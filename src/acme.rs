use anyhow::{Context, Result, anyhow, bail};
use dashmap::DashMap;
use instant_acme::{
    Account, AccountBuilder, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier,
    NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use reqwest::{Client, RequestBuilder};
use serde::Deserialize;
use std::{
    fs,
    io::BufReader,
    net::SocketAddr,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex as AsyncMutex,
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use x509_parser::parse_x509_certificate;

use crate::{
    config::{AcmeConfig, RuntimeConfig},
    panel::TlsSettings,
    tls::TlsManager,
};

#[derive(Clone)]
pub struct AcmeManager {
    config: AcmeConfig,
    http: Client,
    renew_lock: Arc<AsyncMutex<()>>,
}

impl AcmeManager {
    pub fn new(config: AcmeConfig) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("build ACME http client")?;
        Ok(Self {
            config,
            http,
            renew_lock: Arc::new(AsyncMutex::new(())),
        })
    }

    pub fn config(&self) -> &AcmeConfig {
        &self.config
    }

    pub async fn ensure_certificate(
        &self,
        tls: &TlsSettings,
        runtime: &RuntimeConfig,
        tls_manager: &TlsManager,
    ) -> Result<()> {
        let mode = tls.cert_mode.trim();
        if mode != "dns" && mode != "http" {
            return Ok(());
        }
        if tls.cert_file.trim().is_empty() || tls.key_file.trim().is_empty() {
            bail!("ACME requires cert_file and key_file");
        }
        let _renew_guard = self.renew_lock.lock().await;
        if !cert_needs_renewal(&tls.cert_file, &tls.key_file, self.config.renew_before_days)? {
            return Ok(());
        }
        let domains = tls.effective_server_names();
        if domains.is_empty() {
            bail!("ACME requires server_name or server_names");
        }
        let account = self.account().await?;
        let identifiers: Vec<Identifier> = domains.into_iter().map(Identifier::Dns).collect();
        let mut order = account
            .new_order(&NewOrder::new(&identifiers))
            .await
            .map_err(|error| anyhow!("create ACME order: {error}"))?;
        let mut cleanup = Cleanup::default();
        let result = self
            .issue(
                &account,
                &mut order,
                tls,
                runtime,
                tls_manager,
                &mut cleanup,
            )
            .await;
        cleanup.run().await;
        result
    }

    async fn issue(
        &self,
        _account: &Account,
        order: &mut instant_acme::Order,
        tls: &TlsSettings,
        runtime: &RuntimeConfig,
        tls_manager: &TlsManager,
        cleanup: &mut Cleanup,
    ) -> Result<()> {
        let mode = tls.cert_mode.trim();
        if mode == "http" {
            let server =
                Http01Server::start(&self.config.http01_bind, cleanup.tokens.clone()).await?;
            info!(addr = %server.local_addr, "ACME HTTP-01 challenge server started");
            cleanup.http_server = Some(server);
        } else {
            cleanup.cloudflare = Some(Cloudflare::new(&self.http, &self.config, tls)?);
        }

        let mut authorizations = order.authorizations();
        while let Some(result) = authorizations.next().await {
            let mut authz = result.map_err(|error| anyhow!("read ACME authorization: {error}"))?;
            match authz.status {
                AuthorizationStatus::Pending => {}
                AuthorizationStatus::Valid => continue,
                other => bail!("unexpected ACME authorization status {other:?}"),
            }
            let domain = authz.identifier().to_string();
            if mode == "http" {
                let mut challenge = authz
                    .challenge(ChallengeType::Http01)
                    .ok_or_else(|| anyhow!("no HTTP-01 challenge for {domain}"))?;
                let token = challenge.token.clone();
                let key_authorization = challenge.key_authorization().as_str().to_owned();
                cleanup.tokens.insert(token, key_authorization);
                challenge
                    .set_ready()
                    .await
                    .map_err(|error| anyhow!("set HTTP-01 challenge ready: {error}"))?;
                info!(domain = %domain, "ACME HTTP-01 challenge ready");
            } else {
                let mut challenge = authz
                    .challenge(ChallengeType::Dns01)
                    .ok_or_else(|| anyhow!("no DNS-01 challenge for {domain}"))?;
                let value = challenge.key_authorization().dns_value();
                let fqdn = format!("_acme-challenge.{domain}");
                let record = {
                    let cloudflare = cleanup
                        .cloudflare
                        .as_ref()
                        .ok_or_else(|| anyhow!("cloudflare provider not initialized"))?;
                    cloudflare.create_txt(&domain, &fqdn, &value).await?
                };
                cleanup.dns_records.push(record);
                if self.config.dns_propagation_delay_secs > 0 {
                    sleep(Duration::from_secs(self.config.dns_propagation_delay_secs)).await;
                }
                challenge
                    .set_ready()
                    .await
                    .map_err(|error| anyhow!("set DNS-01 challenge ready: {error}"))?;
                info!(domain = %domain, record = %fqdn, "ACME DNS-01 challenge ready");
            }
        }

        let status = order
            .poll_ready(&RetryPolicy::default())
            .await
            .map_err(|error| anyhow!("poll ACME order ready: {error}"))?;
        if status != OrderStatus::Ready {
            bail!("ACME order ended in status {status:?}");
        }
        let key_pem = order
            .finalize()
            .await
            .map_err(|error| anyhow!("finalize ACME order: {error}"))?;
        let cert_pem = order
            .poll_certificate(&RetryPolicy::default())
            .await
            .map_err(|error| anyhow!("download ACME certificate: {error}"))?;
        crate::tls::write_pem_files(
            &tls.cert_file,
            &tls.key_file,
            cert_pem.as_bytes(),
            key_pem.as_bytes(),
        )?;
        tls_manager.apply_acme_pem(cert_pem.as_bytes(), key_pem.as_bytes(), tls, runtime)?;
        info!(domains = ?tls.effective_server_names(), "ACME certificate issued and applied");
        Ok(())
    }

    async fn account(&self) -> Result<Account> {
        let directory = Path::new(&self.config.account_dir);
        let account_path = directory.join("account.json");
        if account_path.exists() {
            let credentials: AccountCredentials = serde_json::from_slice(
                &fs::read(&account_path).context("read ACME account credentials")?,
            )
            .context("parse ACME account credentials")?;
            return self
                .builder()?
                .from_credentials(credentials)
                .await
                .map_err(|error| anyhow!("load ACME account: {error}"));
        }
        let contacts: Vec<String> = if self.config.email.trim().is_empty() {
            Vec::new()
        } else {
            vec![format!("mailto:{}", self.config.email.trim())]
        };
        let contact_refs: Vec<&str> = contacts.iter().map(String::as_str).collect();
        let (account, credentials) = self
            .builder()?
            .create(
                &NewAccount {
                    contact: &contact_refs,
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                self.config.directory_url.clone(),
                None,
            )
            .await
            .map_err(|error| anyhow!("create ACME account: {error}"))?;
        fs::create_dir_all(directory).context("create ACME account directory")?;
        crate::tls::write_private_file(
            &account_path.display().to_string(),
            &serde_json::to_vec_pretty(&credentials).context("serialize ACME credentials")?,
        )
        .context("write ACME account credentials")?;
        Ok(account)
    }

    fn builder(&self) -> Result<AccountBuilder> {
        if self.config.root_cert.trim().is_empty() {
            Account::builder().map_err(|error| anyhow!("create ACME account builder: {error}"))
        } else {
            Account::builder_with_root(self.config.root_cert.trim())
                .map_err(|error| anyhow!("create ACME account builder with root: {error}"))
        }
    }
}

#[derive(Default)]
struct Cleanup {
    tokens: Arc<DashMap<String, String>>,
    http_server: Option<Http01Server>,
    cloudflare: Option<Cloudflare>,
    dns_records: Vec<DnsRecord>,
}

impl Cleanup {
    async fn run(&mut self) {
        self.http_server.take();
        if let Some(cloudflare) = &self.cloudflare {
            for record in self.dns_records.drain(..) {
                if let Err(error) = cloudflare.delete_txt(&record).await {
                    warn!(error = %error, "delete ACME DNS record failed");
                }
            }
        }
    }
}

struct Http01Server {
    local_addr: SocketAddr,
    cancel: CancellationToken,
    join: tokio::task::JoinHandle<()>,
}

impl Http01Server {
    async fn start(bind: &str, tokens: Arc<DashMap<String, String>>) -> Result<Self> {
        let listener = TcpListener::bind(bind)
            .await
            .with_context(|| format!("bind ACME HTTP-01 server {bind}"))?;
        let local_addr = listener.local_addr()?;
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let join = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = token.cancelled() => break,
                    accepted = listener.accept() => {
                        let (stream, _) = match accepted {
                            Ok(value) => value,
                            Err(error) => {
                                warn!(error = %error, "ACME HTTP-01 accept failed");
                                tokio::time::sleep(Duration::from_millis(100)).await;
                                continue;
                            }
                        };
                        let tokens = tokens.clone();
                        tokio::spawn(async move {
                            serve_http01(stream, tokens).await;
                        });
                    }
                }
            }
        });
        Ok(Self {
            local_addr,
            cancel,
            join,
        })
    }
}

impl Drop for Http01Server {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.join.abort();
    }
}

async fn serve_http01(mut stream: TcpStream, tokens: Arc<DashMap<String, String>>) {
    let mut buffer = Vec::new();
    loop {
        let mut chunk = [0u8; 1024];
        let Ok(Ok(read)) = timeout(Duration::from_secs(5), stream.read(&mut chunk)).await else {
            return;
        };
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > 8192 || buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let request = String::from_utf8_lossy(&buffer);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let token = path
        .strip_prefix("/.well-known/acme-challenge/")
        .unwrap_or("");
    let body = tokens.get(token).map(|value| value.clone());
    let (status, body) = match body {
        Some(body) => ("200 OK", body),
        None => ("404 Not Found", String::new()),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

struct DnsRecord {
    zone_id: String,
    record_id: String,
}

struct Cloudflare {
    http: Client,
    base: String,
    token: Option<String>,
    email: Option<String>,
    key: Option<String>,
}

#[derive(Deserialize)]
struct CloudflareResponse<T> {
    success: bool,
    result: Option<T>,
    errors: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Zone {
    id: String,
}

#[derive(Deserialize)]
struct Record {
    id: String,
}

impl Cloudflare {
    fn new(http: &Client, config: &AcmeConfig, tls: &TlsSettings) -> Result<Self> {
        let provider = if tls.provider.trim().is_empty() {
            config.dns_provider.trim()
        } else {
            tls.provider.trim()
        };
        if !provider.eq_ignore_ascii_case("cloudflare") {
            bail!("unsupported ACME DNS provider {provider:?}; M1 supports cloudflare");
        }
        let mut env = config.dns_env.clone();
        for (key, value) in tls.dns_env_map() {
            env.insert(key, value);
        }
        let token = [
            "CLOUDFLARE_DNS_API_TOKEN",
            "CF_API_TOKEN",
            "CLOUDFLARE_API_TOKEN",
        ]
        .iter()
        .find_map(|key| env.get(*key).cloned().filter(|value| !value.is_empty()));
        let email = env
            .get("CLOUDFLARE_EMAIL")
            .cloned()
            .filter(|value| !value.is_empty());
        let key = ["CLOUDFLARE_API_KEY", "CF_API_KEY"]
            .iter()
            .find_map(|key| env.get(*key).cloned().filter(|value| !value.is_empty()));
        if token.is_none() && (email.is_none() || key.is_none()) {
            bail!(
                "Cloudflare credentials missing: set CLOUDFLARE_DNS_API_TOKEN or CLOUDFLARE_EMAIL + CLOUDFLARE_API_KEY"
            );
        }
        Ok(Self {
            http: http.clone(),
            base: config.cloudflare_api_base.trim_end_matches('/').to_owned(),
            token,
            email,
            key,
        })
    }

    fn auth(&self, request: RequestBuilder) -> RequestBuilder {
        if let Some(token) = &self.token {
            request.header("Authorization", format!("Bearer {token}"))
        } else {
            request
                .header("X-Auth-Email", self.email.as_deref().unwrap_or(""))
                .header("X-Auth-Key", self.key.as_deref().unwrap_or(""))
        }
    }

    async fn find_zone(&self, domain: &str) -> Result<String> {
        let parts: Vec<&str> = domain.split('.').collect();
        if parts.len() < 2 {
            bail!("invalid ACME domain {domain:?}");
        }
        for index in 0..parts.len() - 1 {
            let candidate = parts[index..].join(".");
            let url = format!("{}/zones?name={candidate}", self.base);
            let response = self
                .auth(self.http.get(url))
                .send()
                .await
                .context("query Cloudflare zones")?;
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if !status.is_success() {
                continue;
            }
            let parsed: CloudflareResponse<Vec<Zone>> = match serde_json::from_str(&body) {
                Ok(parsed) => parsed,
                Err(_) => continue,
            };
            if parsed.success
                && let Some(zones) = parsed.result
                && let Some(zone) = zones.first()
            {
                return Ok(zone.id.clone());
            }
        }
        bail!("Cloudflare zone not found for {domain}");
    }

    async fn create_txt(&self, domain: &str, fqdn: &str, value: &str) -> Result<DnsRecord> {
        let zone_id = self.find_zone(domain).await?;
        let url = format!("{}/zones/{zone_id}/dns_records", self.base);
        let body = serde_json::json!({
            "type": "TXT",
            "name": fqdn,
            "content": value,
            "ttl": 120,
        });
        let response = self
            .auth(self.http.post(url).json(&body))
            .send()
            .await
            .context("create Cloudflare TXT record")?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!(
                "create Cloudflare TXT failed HTTP {status}: {}",
                truncate(&text)
            );
        }
        let parsed: CloudflareResponse<Record> =
            serde_json::from_str(&text).context("parse Cloudflare TXT response")?;
        if !parsed.success {
            bail!("create Cloudflare TXT failed: {:?}", parsed.errors);
        }
        let record = parsed
            .result
            .ok_or_else(|| anyhow!("Cloudflare TXT response has no record"))?;
        Ok(DnsRecord {
            zone_id,
            record_id: record.id,
        })
    }

    async fn delete_txt(&self, record: &DnsRecord) -> Result<()> {
        let url = format!(
            "{}/zones/{}/dns_records/{}",
            self.base, record.zone_id, record.record_id
        );
        let response = self
            .auth(self.http.delete(url))
            .send()
            .await
            .context("delete Cloudflare TXT record")?;
        if !response.status().is_success() {
            bail!("delete Cloudflare TXT failed HTTP {}", response.status());
        }
        Ok(())
    }
}

fn cert_needs_renewal(cert_path: &str, key_path: &str, days: u64) -> Result<bool> {
    if !Path::new(cert_path).exists() || !Path::new(key_path).exists() {
        return Ok(true);
    }
    let data = fs::read(cert_path).context("read certificate for renewal check")?;
    let Ok(key_data) = fs::read(key_path) else {
        return Ok(true);
    };
    if !crate::tls::certificate_matches_key(&data, &key_data) {
        return Ok(true);
    }
    let certificates: Vec<_> = rustls_pemfile::certs(&mut BufReader::new(&data[..]))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse certificate for renewal check")?;
    let Some(certificate) = certificates.first() else {
        return Ok(true);
    };
    let (_, parsed) = parse_x509_certificate(certificate.as_ref())
        .map_err(|error| anyhow!("parse certificate for renewal check: {error}"))?;
    let not_after = parsed.validity().not_after.timestamp();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before UNIX epoch")?
        .as_secs() as i64;
    Ok(not_after - now < days as i64 * 86400)
}

/// 返回证书剩余有效天数：未到期按整天向上取整，已过期返回 -1。
///
/// 只读取本地 PEM 的第一个证书，不校验证书链与主机名；用于到期告警，
/// 不参与信任决策（信任由 rustls 在握手阶段完成）。
pub fn certificate_days_left(cert_path: &str) -> Result<i64> {
    let data = fs::read(cert_path).with_context(|| format!("read certificate {cert_path}"))?;
    let certificates: Vec<_> = rustls_pemfile::certs(&mut BufReader::new(&data[..]))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse certificate")?;
    let certificate = certificates
        .first()
        .ok_or_else(|| anyhow!("certificate file {cert_path} contains no certificate"))?;
    let (_, parsed) = parse_x509_certificate(certificate.as_ref())
        .map_err(|error| anyhow!("parse certificate {cert_path}: {error}"))?;
    let not_after = parsed.validity().not_after.timestamp();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before UNIX epoch")?
        .as_secs() as i64;
    let remaining = not_after - now;
    Ok(if remaining < 0 {
        -1
    } else {
        (remaining + 86_399) / 86_400
    })
}

fn truncate(value: &str) -> String {
    let mut truncated: String = value.chars().take(500).collect();
    if value.chars().count() > 500 {
        truncated.push_str("...");
    }
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http01_server_serves_known_token() {
        let tokens = Arc::new(DashMap::new());
        tokens.insert("token-1".to_owned(), "key-auth".to_owned());
        let server = Http01Server::start("127.0.0.1:0", tokens).await.unwrap();
        let url = format!(
            "http://{}/.well-known/acme-challenge/token-1",
            server.local_addr
        );
        let response = Client::new().get(url).send().await.unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(response.text().await.unwrap(), "key-auth");
        let unknown = Client::new()
            .get(format!(
                "http://{}/.well-known/acme-challenge/missing",
                server.local_addr
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(unknown.status().as_u16(), 404);
    }

    #[tokio::test]
    async fn cloned_manager_shares_renew_lock() {
        let manager = AcmeManager::new(AcmeConfig::default()).unwrap();
        let clone = manager.clone();
        let guard = manager.renew_lock.lock().await;
        assert!(clone.renew_lock.try_lock().is_err());
        drop(guard);
        assert!(clone.renew_lock.try_lock().is_ok());
    }

    #[test]
    fn mismatched_certificate_and_key_need_renewal() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let directory = std::env::temp_dir().join(format!(
            "v2node-acme-pair-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let cert_path = directory.join("node.crt");
        let key_path = directory.join("node.key");
        let first = rcgen::generate_simple_self_signed(vec!["first.local".to_owned()]).unwrap();
        let second = rcgen::generate_simple_self_signed(vec!["second.local".to_owned()]).unwrap();
        fs::write(&cert_path, first.cert.pem()).unwrap();
        fs::write(&key_path, second.signing_key.serialize_pem()).unwrap();
        assert!(
            cert_needs_renewal(cert_path.to_str().unwrap(), key_path.to_str().unwrap(), 0).unwrap()
        );
        fs::write(&key_path, first.signing_key.serialize_pem()).unwrap();
        assert!(
            !cert_needs_renewal(cert_path.to_str().unwrap(), key_path.to_str().unwrap(), 0)
                .unwrap()
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn missing_certificate_needs_renewal() {
        assert!(
            cert_needs_renewal(
                "/tmp/does-not-exist-v2node.crt",
                "/tmp/does-not-exist-v2node.key",
                30
            )
            .unwrap()
        );
    }

    #[test]
    fn truncate_handles_multibyte_text() {
        let text = "错误".repeat(400);
        let result = truncate(&text);
        assert!(result.ends_with("..."));
        assert_eq!(result.chars().count(), 503);
    }

    fn write_test_cert(
        path: &std::path::Path,
        not_before: (i32, u8, u8),
        not_after: (i32, u8, u8),
    ) {
        let key_pair = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["test.local".to_owned()]).unwrap();
        params.not_before = rcgen::date_time_ymd(not_before.0, not_before.1, not_before.2);
        params.not_after = rcgen::date_time_ymd(not_after.0, not_after.1, not_after.2);
        let cert = params.self_signed(&key_pair).unwrap();
        std::fs::write(path, cert.pem()).unwrap();
    }

    #[test]
    fn certificate_days_left_reports_expired_and_future() {
        let dir = std::env::temp_dir().join(format!("v2node-cert-days-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let expired = dir.join("expired.pem");
        write_test_cert(&expired, (2020, 1, 1), (2020, 1, 11));
        assert_eq!(
            certificate_days_left(expired.to_str().unwrap()).unwrap(),
            -1,
            "expired certificate must report -1"
        );

        let future = dir.join("future.pem");
        write_test_cert(&future, (2035, 1, 1), (2035, 1, 11));
        let days = certificate_days_left(future.to_str().unwrap()).unwrap();
        assert!(days > 2000, "expected a large positive value, got {days}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cloudflare_requires_credentials() {
        let config = AcmeConfig::default();
        let settings = TlsSettings::default();
        let Err(error) = Cloudflare::new(&Client::new(), &config, &settings) else {
            panic!("Cloudflare without credentials must fail");
        };
        assert!(error.to_string().contains("credentials"));
    }
}
