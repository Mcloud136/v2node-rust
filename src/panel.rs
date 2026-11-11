use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use tracing::warn;

use crate::config::{NodeConfig, RuntimeConfig};

fn nullable_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

fn nullable_strings<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<Vec<String>>::deserialize(deserializer)?.unwrap_or_default())
}

fn nullable_interval<'de, D>(deserializer: D) -> Result<IntervalValue, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<IntervalValue>::deserialize(deserializer)?.unwrap_or_default())
}

fn nullable_string_map<'de, D>(deserializer: D) -> Result<HashMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(
        Option::<HashMap<String, Option<String>>>::deserialize(deserializer)?
            .unwrap_or_default()
            .into_iter()
            .map(|(key, value)| (key, value.unwrap_or_default()))
            .collect(),
    )
}

fn nullable_object<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Array(items)) if items.is_empty() => Ok(None),
        Some(value) => serde_json::from_value(value)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

fn nullable_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UserInfo {
    #[serde(default)]
    pub id: i32,
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub speed_limit: i32,
    #[serde(default)]
    pub device_limit: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct UserListBody {
    #[serde(default)]
    pub users: Vec<UserInfo>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AliveMap {
    #[serde(default)]
    pub alive: Option<HashMap<i32, i32>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct NetworkSettings {
    #[serde(default, deserialize_with = "nullable_string")]
    pub path: String,
    #[serde(default, deserialize_with = "nullable_string_map")]
    pub headers: HashMap<String, String>,
    #[serde(default, rename = "acceptProxyProtocol")]
    pub accept_proxy_protocol: Option<bool>,
}

impl NetworkSettings {
    pub fn host(&self) -> Option<&str> {
        self.headers
            .get("Host")
            .or_else(|| self.headers.get("host"))
            .map(String::as_str)
            .filter(|host| !host.trim().is_empty())
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TlsSettings {
    #[serde(default, deserialize_with = "nullable_string")]
    pub server_name: String,
    #[serde(default, deserialize_with = "nullable_strings")]
    pub server_names: Vec<String>,
    #[serde(default, deserialize_with = "nullable_string")]
    pub dest: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub server_port: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub short_id: String,
    #[serde(default, deserialize_with = "nullable_strings")]
    pub short_ids: Vec<String>,
    #[serde(default, deserialize_with = "nullable_string")]
    pub private_key: String,
    #[serde(default, rename = "mldsa65Seed", deserialize_with = "nullable_string")]
    pub mldsa65_seed: String,
    #[serde(default)]
    pub xver: serde_json::Value,
    #[serde(default, deserialize_with = "nullable_string")]
    pub cert_mode: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub cert_file: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub key_file: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub tls_cert: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub tls_key: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub provider: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub dns_env: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub reject_unknown_sni: String,
    #[serde(default)]
    pub allow_insecure: serde_json::Value,
    #[serde(default, deserialize_with = "nullable_string")]
    pub fingerprint: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub ech: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub ech_server_name: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub ech_config: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub ech_key: String,
}

impl TlsSettings {
    pub fn effective_server_names(&self) -> Vec<String> {
        if !self.server_names.is_empty() {
            return self.server_names.clone();
        }
        if self.server_name.is_empty() {
            return Vec::new();
        }
        vec![self.server_name.clone()]
    }

    pub fn reject_unknown_sni(&self) -> bool {
        matches!(
            self.reject_unknown_sni.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    }

    pub fn dns_env_map(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        for item in self.dns_env.split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            if let Some((key, value)) = item.split_once('=') {
                map.insert(key.trim().to_owned(), value.trim().to_owned());
            }
        }
        map
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum IntervalValue {
    Int(i64),
    Float(f64),
    Str(String),
}

impl Default for IntervalValue {
    fn default() -> Self {
        Self::Int(0)
    }
}

impl IntervalValue {
    pub fn seconds(&self, fallback: u64) -> u64 {
        let value = match self {
            Self::Int(value) => *value as f64,
            Self::Float(value) => *value,
            Self::Str(value) => value.trim().parse::<f64>().unwrap_or(0.0),
        };
        if value > 0.0 {
            value.round() as u64
        } else {
            fallback
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct BaseConfig {
    #[serde(default, deserialize_with = "nullable_interval")]
    pub push_interval: IntervalValue,
    #[serde(default, deserialize_with = "nullable_interval")]
    pub pull_interval: IntervalValue,
    #[serde(default)]
    pub node_report_min_traffic: i64,
    #[serde(default)]
    pub device_online_min_traffic: i64,
}

impl BaseConfig {
    pub fn push_seconds(&self, fallback: u64) -> u64 {
        self.push_interval.seconds(fallback)
    }

    pub fn pull_seconds(&self, fallback: u64) -> u64 {
        self.pull_interval.seconds(fallback)
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Default)]
pub struct EncryptionSettings {
    #[serde(default, deserialize_with = "nullable_string")]
    pub mode: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub rtt: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub ticket: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub server_padding: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub client_padding: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub private_key: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub password: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Route {
    #[serde(default)]
    pub id: i32,
    #[serde(default, rename = "match")]
    pub matches: Vec<String>,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub action_value: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Default)]
pub struct NodeInfo {
    #[serde(default)]
    pub listen_ip: String,
    #[serde(default)]
    pub server_port: u16,
    #[serde(default)]
    pub network: String,
    #[serde(default, deserialize_with = "nullable_object")]
    pub network_settings: Option<NetworkSettings>,
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub tls: u8,
    #[serde(default, deserialize_with = "nullable_object")]
    pub tls_settings: Option<TlsSettings>,
    #[serde(default)]
    pub encryption: Option<String>,
    #[serde(default, deserialize_with = "nullable_object")]
    pub encryption_settings: Option<EncryptionSettings>,
    #[serde(default)]
    pub flow: Option<String>,
    #[serde(default)]
    pub cipher: Option<String>,
    #[serde(default)]
    pub congestion_control: Option<String>,
    #[serde(default)]
    pub zero_rtt_handshake: bool,
    #[serde(default)]
    pub up_mbps: i32,
    #[serde(default)]
    pub down_mbps: i32,
    #[serde(default)]
    pub obfs: Option<String>,
    #[serde(default)]
    pub obfs_password: Option<String>,
    #[serde(default)]
    pub padding_scheme: Option<Vec<String>>,
    #[serde(default, deserialize_with = "nullable_strings")]
    pub trusted_x_forwarded_for: Vec<String>,
    #[serde(default)]
    pub base_config: Option<BaseConfig>,
    #[serde(default, deserialize_with = "nullable_vec")]
    pub routes: Vec<Route>,
}

impl NodeInfo {
    pub fn ws_path(&self) -> Option<&str> {
        self.network_settings
            .as_ref()
            .map(|settings| settings.path.as_str())
            .filter(|path| !path.is_empty())
    }

    pub fn ws_host(&self) -> Option<&str> {
        self.network_settings
            .as_ref()
            .and_then(NetworkSettings::host)
    }

    pub fn base(&self) -> BaseConfig {
        self.base_config.clone().unwrap_or_default()
    }

    pub fn tls_settings(&self) -> Option<&TlsSettings> {
        self.tls_settings.as_ref()
    }

    pub fn cert_mode(&self) -> &str {
        self.tls_settings
            .as_ref()
            .map(|settings| settings.cert_mode.trim())
            .unwrap_or("")
    }

    pub fn validate_for_m1(&self) -> Result<()> {
        if self.protocol != "vless" {
            bail!("M1 only supports protocol=vless, got {:?}", self.protocol);
        }
        if self.network != "ws" {
            bail!("M1 only supports network=ws, got {:?}", self.network);
        }
        if self.tls > 2 {
            bail!("invalid tls value {}", self.tls);
        }
        if self.tls == 2 {
            bail!("REALITY (tls=2) is not supported in M1");
        }
        if self.flow.as_deref().unwrap_or("").trim() != "" {
            bail!("VLESS flow is not supported in M1");
        }
        let encryption = self.encryption.as_deref().unwrap_or("").trim();
        if !encryption.is_empty() && encryption != "none" {
            if encryption != "mlkem768x25519plus" {
                bail!("unsupported VLESS encryption {encryption:?}");
            }
            let settings = self
                .encryption_settings
                .as_ref()
                .ok_or_else(|| anyhow!("VLESS encryption requires encryption_settings"))?;
            if settings.private_key.trim().is_empty() {
                bail!("VLESS encryption requires encryption_settings.private_key");
            }
            match settings.mode.trim() {
                "" | "native" | "xorpub" | "random" => {}
                other => bail!("unsupported VLESS encryption mode {other:?}"),
            }
        }
        if !self.routes.is_empty() {
            bail!("routes are not supported in M1");
        }
        if self.listen_ip.trim().is_empty() {
            bail!("listen_ip is empty");
        }
        self.listen_ip
            .parse::<std::net::IpAddr>()
            .with_context(|| format!("invalid listen_ip {:?}", self.listen_ip))?;
        if self.server_port == 0 {
            bail!("server_port is empty");
        }
        let settings = self
            .network_settings
            .as_ref()
            .ok_or_else(|| anyhow!("network_settings is empty"))?;
        if settings.path.trim().is_empty() {
            bail!("network_settings.path is empty");
        }
        if settings.accept_proxy_protocol.unwrap_or(false) {
            bail!("PROXY protocol is not supported in M1");
        }
        if self.tls == 1 {
            let tls = self
                .tls_settings
                .as_ref()
                .ok_or_else(|| anyhow!("tls=1 requires tls_settings"))?;
            match tls.cert_mode.trim() {
                "" | "none" => {}
                "file" | "self" => {
                    if tls.cert_file.trim().is_empty() || tls.key_file.trim().is_empty() {
                        bail!(
                            "cert_mode={} requires cert_file and key_file",
                            tls.cert_mode
                        );
                    }
                    if tls.effective_server_names().is_empty() {
                        bail!(
                            "cert_mode={} requires server_name or server_names",
                            tls.cert_mode
                        );
                    }
                }
                "remote" => {
                    if tls.cert_file.trim().is_empty() || tls.key_file.trim().is_empty() {
                        bail!("cert_mode=remote requires cert_file and key_file");
                    }
                    if tls.tls_cert.trim().is_empty() || tls.tls_key.trim().is_empty() {
                        bail!("cert_mode=remote requires tls_cert and tls_key");
                    }
                }
                "dns" | "http" => {
                    if tls.cert_file.trim().is_empty() || tls.key_file.trim().is_empty() {
                        bail!(
                            "cert_mode={} requires cert_file and key_file",
                            tls.cert_mode
                        );
                    }
                    if tls.provider.trim().is_empty() && tls.cert_mode.trim() == "dns" {
                        bail!("cert_mode=dns requires provider");
                    }
                    if tls.effective_server_names().is_empty() {
                        bail!(
                            "cert_mode={} requires server_name or server_names",
                            tls.cert_mode
                        );
                    }
                }
                other => bail!("unsupported cert_mode {other:?}"),
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct UserTraffic {
    pub uid: i32,
    pub upload: i64,
    pub download: i64,
}

#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct TrafficReportError {
    pub definitely_not_sent: bool,
    pub permanent: bool,
    #[source]
    pub error: anyhow::Error,
}

fn classify_traffic_status(status: StatusCode) -> (bool, bool) {
    // (definitely_not_sent, permanent)
    if status == StatusCode::TOO_MANY_REQUESTS {
        // The panel rejects before processing; retrying is safe.
        (true, false)
    } else if status.is_client_error() {
        // 400/401/403/404/413/422 will never succeed by retrying the same payload.
        (false, true)
    } else {
        // 5xx and other responses may have been processed; outcome is unknown.
        (false, false)
    }
}

#[derive(Clone)]
pub struct PanelClient {
    http: Client,
    base_url: String,
    node_id: i32,
    token: String,
    retry_count: usize,
    retry_backoff: Duration,
    node_etag: Arc<Mutex<String>>,
    node_body_hash: Arc<Mutex<String>>,
    user_etag: Arc<Mutex<String>>,
    user_body_hash: Arc<Mutex<String>>,
}

impl PanelClient {
    pub fn new(node: &NodeConfig, runtime: &RuntimeConfig) -> Result<Self> {
        let timeout = Duration::from_secs(node.effective_timeout(runtime));
        let connect_timeout = Duration::from_secs(runtime.panel_connect_timeout_secs);
        let http = Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(timeout)
            .user_agent(format!("v2node-rust/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("build panel http client")?;
        Ok(Self {
            http,
            base_url: node.api_host.trim_end_matches('/').to_owned(),
            node_id: node.node_id,
            token: node.api_key.clone(),
            retry_count: node.effective_retry(runtime),
            retry_backoff: Duration::from_millis(runtime.panel_retry_backoff_ms),
            node_etag: Arc::new(Mutex::new(String::new())),
            node_body_hash: Arc::new(Mutex::new(String::new())),
            user_etag: Arc::new(Mutex::new(String::new())),
            user_body_hash: Arc::new(Mutex::new(String::new())),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn with_query(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.query(&[
            ("node_type", "v2node".to_owned()),
            ("node_id", self.node_id.to_string()),
            ("token", self.token.clone()),
        ])
    }

    async fn send_with_retry<F>(&self, mut build: F) -> Result<reqwest::Response>
    where
        F: FnMut() -> reqwest::RequestBuilder,
    {
        let mut last_error: Option<anyhow::Error> = None;
        for attempt in 0..=self.retry_count {
            match build().send().await {
                Ok(response) => {
                    let retryable = response.status().is_server_error()
                        || response.status() == StatusCode::TOO_MANY_REQUESTS;
                    if retryable && attempt < self.retry_count {
                        last_error = Some(anyhow!("panel returned {}", response.status()));
                    } else {
                        return Ok(response);
                    }
                }
                Err(error) => {
                    last_error = Some(anyhow::Error::new(error));
                    if attempt >= self.retry_count {
                        break;
                    }
                }
            }
            if !self.retry_backoff.is_zero() {
                tokio::time::sleep(self.retry_backoff).await;
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("panel request failed")))
    }

    pub async fn get_node_config(&self) -> Result<Option<NodeInfo>> {
        let etag = self.node_etag.lock().clone();
        let response = self
            .send_with_retry(|| {
                let mut request = self.http.get(self.url("/api/v2/server/config"));
                if !etag.is_empty() {
                    request = request.header(reqwest::header::IF_NONE_MATCH, etag.clone());
                }
                self.with_query(request)
            })
            .await?;
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        if !response.status().is_success() {
            bail!("node config HTTP {}", response.status());
        }
        let new_etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let body = response.bytes().await.context("read node config body")?;
        let hash = sha256_hex(&body);
        if self.node_body_hash.lock().as_str() == hash {
            *self.node_etag.lock() = new_etag;
            return Ok(None);
        }
        let info: NodeInfo = serde_json::from_slice(&body).context("decode node config body")?;
        info.validate_for_m1()?;
        *self.node_body_hash.lock() = hash;
        *self.node_etag.lock() = new_etag;
        Ok(Some(info))
    }

    pub async fn get_user_list(&self) -> Result<Option<Vec<UserInfo>>> {
        let etag = self.user_etag.lock().clone();
        let response = self
            .send_with_retry(|| {
                let mut request = self
                    .http
                    .get(self.url("/api/v1/server/UniProxy/user"))
                    .header("X-Response-Format", "msgpack");
                if !etag.is_empty() {
                    request = request.header(reqwest::header::IF_NONE_MATCH, etag.clone());
                }
                self.with_query(request)
            })
            .await?;
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        if !response.status().is_success() {
            bail!("user list HTTP {}", response.status());
        }
        let new_etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let is_msgpack = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.contains("application/x-msgpack"))
            .unwrap_or(false);
        let body = response.bytes().await.context("read user list body")?;
        let hash = sha256_hex(&body);
        if self.user_body_hash.lock().as_str() == hash {
            *self.user_etag.lock() = new_etag;
            return Ok(None);
        }
        let users = if is_msgpack {
            decode_msgpack_users(&body)?
        } else {
            decode_json_users(&body)?
        };
        *self.user_body_hash.lock() = hash;
        *self.user_etag.lock() = new_etag;
        Ok(Some(users))
    }

    pub async fn get_alive_list(&self) -> Option<HashMap<i32, i32>> {
        let result = self
            .send_with_retry(|| {
                self.with_query(self.http.get(self.url("/api/v1/server/UniProxy/alivelist")))
            })
            .await;
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                warn!(error = %error, "get alive list failed; keeping previous counts");
                return None;
            }
        };
        if !response.status().is_success() {
            warn!(status = %response.status(), "get alive list failed; keeping previous counts");
            return None;
        }
        match response.json::<AliveMap>().await {
            Ok(map) => Some(map.alive.unwrap_or_default()),
            Err(error) => {
                warn!(error = %error, "decode alive list failed; keeping previous counts");
                None
            }
        }
    }

    pub fn invalidate_node_cache(&self) {
        self.node_etag.lock().clear();
        self.node_body_hash.lock().clear();
    }

    pub async fn report_traffic(
        &self,
        traffic: &[UserTraffic],
        batch_id: Option<&str>,
    ) -> Result<(), TrafficReportError> {
        let mut data: HashMap<i32, [i64; 2]> = HashMap::with_capacity(traffic.len());
        for item in traffic {
            data.insert(item.uid, [item.upload, item.download]);
        }
        let mut request = self.with_query(
            self.http
                .post(self.url("/api/v1/server/UniProxy/push"))
                .json(&data),
        );
        if let Some(batch_id) = batch_id {
            request = request.header("X-V2Node-Batch-Id", batch_id);
        }
        let response = request.send().await.map_err(|error| {
            let definitely_not_sent = error.is_connect() || error.is_dns() || error.is_request();
            let reason = if error.is_timeout() {
                "timeout"
            } else if error.is_connect() {
                "connect"
            } else if error.is_dns() {
                "dns"
            } else {
                "request"
            };
            // Do not include the URL: it carries the panel token in its query string.
            TrafficReportError {
                definitely_not_sent,
                permanent: false,
                error: anyhow!("report traffic request failed ({reason})"),
            }
        })?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let (definitely_not_sent, permanent) = classify_traffic_status(status);
        Err(TrafficReportError {
            definitely_not_sent,
            permanent,
            error: anyhow!("report traffic HTTP {status}"),
        })
    }

    pub async fn report_alive(&self, data: &HashMap<i32, Vec<String>>) -> Result<()> {
        let response = self
            .send_with_retry(|| {
                self.with_query(
                    self.http
                        .post(self.url("/api/v1/server/UniProxy/alive"))
                        .json(data),
                )
            })
            .await?;
        if !response.status().is_success() {
            bail!("report alive HTTP {}", response.status());
        }
        Ok(())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn decode_msgpack_users(bytes: &[u8]) -> Result<Vec<UserInfo>> {
    if let Ok(body) = rmp_serde::from_slice::<UserListBody>(bytes) {
        return Ok(body.users);
    }
    rmp_serde::from_slice::<Vec<UserInfo>>(bytes).context("decode msgpack user list")
}

fn decode_json_users(bytes: &[u8]) -> Result<Vec<UserInfo>> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).context("decode JSON user list")?;
    if value.is_array() {
        return serde_json::from_value::<Vec<UserInfo>>(value).context("decode JSON user array");
    }
    serde_json::from_value::<UserListBody>(value)
        .map(|body| body.users)
        .context("decode JSON user object")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn production_like_node() -> serde_json::Value {
        serde_json::json!({
            "listen_ip": "127.0.0.1",
            "server_port": 10001,
            "network": "ws",
            "network_settings": {
                "path": "/cdn-api",
                "headers": {"Host": "hk2.wxmuma.cc.cd"}
            },
            "protocol": "vless",
            "tls": 1,
            "tls_settings": {
                "server_name": "hk2.wxmuma.cc.cd",
                "cert_mode": "none",
                "ech": "cloudflare",
                "ech_server_name": "",
                "ech_config": "",
                "ech_key": ""
            },
            "flow": "",
            "encryption": "",
            "trusted_x_forwarded_for": ["X-Forwarded-For"],
            "base_config": {
                "push_interval": 60,
                "pull_interval": "60",
                "node_report_min_traffic": 0,
                "device_online_min_traffic": 0
            },
            "routes": []
        })
    }

    #[test]
    fn interval_value_handles_int_float_string() {
        assert_eq!(IntervalValue::Int(60).seconds(1), 60);
        assert_eq!(IntervalValue::Float(60.4).seconds(1), 60);
        assert_eq!(IntervalValue::Str("60".to_owned()).seconds(1), 60);
        assert_eq!(IntervalValue::Str("bad".to_owned()).seconds(7), 7);
        assert_eq!(IntervalValue::Int(0).seconds(7), 7);
    }

    #[test]
    fn production_like_node_is_accepted() {
        let info: NodeInfo = serde_json::from_value(production_like_node()).unwrap();
        assert_eq!(info.ws_path(), Some("/cdn-api"));
        assert_eq!(info.ws_host(), Some("hk2.wxmuma.cc.cd"));
        assert_eq!(info.base().push_seconds(1), 60);
        assert_eq!(info.base().pull_seconds(1), 60);
        assert_eq!(info.trusted_x_forwarded_for, vec!["X-Forwarded-For"]);
        assert_eq!(info.cert_mode(), "none");
        info.validate_for_m1().unwrap();
    }

    #[test]
    fn panel_null_fields_are_tolerated() {
        let mut value = production_like_node();
        value["network_settings"] = serde_json::json!({
            "path": "/cdn-api",
            "headers": {"Host": null},
            "acceptProxyProtocol": null
        });
        value["tls_settings"] = serde_json::json!({
            "server_name": "hk2.wxmuma.cc.cd",
            "server_names": null,
            "dest": null,
            "short_id": null,
            "short_ids": null,
            "private_key": null,
            "cert_mode": "none",
            "cert_file": null,
            "key_file": null,
            "ech": null
        });
        value["encryption"] = serde_json::json!("mlkem768x25519plus");
        value["encryption_settings"] = serde_json::json!({
            "mode": "native",
            "rtt": "0rtt",
            "ticket": "600s",
            "server_padding": null,
            "client_padding": null,
            "private_key": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "password": null
        });
        value["base_config"] = serde_json::json!({
            "push_interval": null,
            "pull_interval": null,
            "node_report_min_traffic": 0,
            "device_online_min_traffic": 0
        });
        let info: NodeInfo = serde_json::from_value(value).unwrap();
        assert_eq!(info.ws_host(), None);
        assert_eq!(info.tls_settings.as_ref().unwrap().cert_mode, "none");
        let settings = info.encryption_settings.as_ref().unwrap();
        assert_eq!(settings.server_padding, "");
        assert_eq!(settings.client_padding, "");
        assert_eq!(settings.password, "");
        assert!(!settings.private_key.is_empty());
        assert_eq!(info.base().push_seconds(60), 60);
        assert_eq!(info.base().pull_seconds(60), 60);
        info.validate_for_m1().unwrap();
    }

    #[test]
    fn empty_array_settings_decode_as_none() {
        let mut value = production_like_node();
        value["tls"] = serde_json::json!(0);
        value["tls_settings"] = serde_json::json!([]);
        value["network_settings"] = serde_json::json!([]);
        value["encryption"] = serde_json::json!(null);
        value["encryption_settings"] = serde_json::json!([]);
        let info: NodeInfo = serde_json::from_value(value).unwrap();
        assert!(info.tls_settings.is_none());
        assert!(info.network_settings.is_none());
        assert!(info.encryption_settings.is_none());
        assert!(info.encryption.is_none());
    }

    #[test]
    fn null_collection_fields_are_tolerated() {
        let mut value = production_like_node();
        value["trusted_x_forwarded_for"] = serde_json::json!(null);
        value["routes"] = serde_json::json!(null);
        let info: NodeInfo = serde_json::from_value(value).unwrap();
        assert!(info.trusted_x_forwarded_for.is_empty());
        assert!(info.routes.is_empty());
        let alive: AliveMap = serde_json::from_str(r#"{"alive":null}"#).unwrap();
        assert!(alive.alive.is_none());
    }

    #[tokio::test]
    async fn traffic_report_to_unreachable_panel_is_not_sent() {
        let node = NodeConfig {
            api_host: "http://127.0.0.1:1".to_owned(),
            node_id: 1,
            api_key: "test".to_owned(),
            timeout: 2,
            retry_count: Some(0),
        };
        let client = PanelClient::new(&node, &RuntimeConfig::default()).unwrap();
        let error = client
            .report_traffic(
                &[UserTraffic {
                    uid: 1,
                    upload: 10,
                    download: 20,
                }],
                None,
            )
            .await
            .unwrap_err();
        assert!(error.definitely_not_sent, "{error}");
        assert!(!error.permanent, "{error}");
    }

    #[test]
    fn traffic_status_classification_matches_retry_policy() {
        assert_eq!(
            classify_traffic_status(StatusCode::TOO_MANY_REQUESTS),
            (true, false)
        );
        assert_eq!(
            classify_traffic_status(StatusCode::PAYLOAD_TOO_LARGE),
            (false, true)
        );
        assert_eq!(
            classify_traffic_status(StatusCode::UNAUTHORIZED),
            (false, true)
        );
        assert_eq!(
            classify_traffic_status(StatusCode::INTERNAL_SERVER_ERROR),
            (false, false)
        );
    }

    #[test]
    fn routes_are_rejected_in_m1() {
        let mut value = production_like_node();
        value["routes"] = serde_json::json!([{"id": 1, "match": ["x"], "action": "block"}]);
        let info: NodeInfo = serde_json::from_value(value).unwrap();
        assert!(info.validate_for_m1().is_err());
    }

    #[test]
    fn flow_is_rejected_in_m1() {
        let mut value = production_like_node();
        value["flow"] = serde_json::json!("xtls-rprx-vision");
        let info: NodeInfo = serde_json::from_value(value).unwrap();
        assert!(info.validate_for_m1().is_err());
    }

    #[test]
    fn user_lists_decode_from_json_and_msgpack() {
        let users = vec![UserInfo {
            id: 1,
            uuid: "11111111-1111-1111-1111-111111111111".to_owned(),
            speed_limit: 100,
            device_limit: 3,
        }];
        let json = serde_json::to_vec(&UserListBody {
            users: users.clone(),
        })
        .unwrap();
        assert_eq!(decode_json_users(&json).unwrap().len(), 1);

        let msgpack = rmp_serde::to_vec(&UserListBody {
            users: users.clone(),
        })
        .unwrap();
        assert_eq!(decode_msgpack_users(&msgpack).unwrap().len(), 1);
    }
}
