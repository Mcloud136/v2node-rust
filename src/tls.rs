use anyhow::{Context, Result, anyhow, bail};
use arc_swap::ArcSwap;
use parking_lot::Mutex;
use rustls::{
    ServerConfig,
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivateKeyDer},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::sync::atomic::AtomicU64;
use std::{
    fs,
    io::BufReader,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio_rustls::TlsAcceptor;

use crate::config::RuntimeConfig;
use crate::panel::{NodeInfo, TlsSettings};

#[cfg(unix)]
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
enum CertSource {
    File { cert: PathBuf, key: PathBuf },
    SelfSigned { cert: PathBuf, key: PathBuf },
    Remote { cert: PathBuf, key: PathBuf },
    Acme { cert: PathBuf, key: PathBuf },
}

impl CertSource {
    fn paths(&self) -> (&Path, &Path) {
        match self {
            Self::File { cert, key }
            | Self::SelfSigned { cert, key }
            | Self::Remote { cert, key }
            | Self::Acme { cert, key } => (cert, key),
        }
    }
}

struct DynamicCertResolver {
    cert: ArcSwap<Option<Arc<CertifiedKey>>>,
    server_names: ArcSwap<Vec<String>>,
    reject_unknown_sni: AtomicBool,
}

impl std::fmt::Debug for DynamicCertResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DynamicCertResolver")
            .finish_non_exhaustive()
    }
}

impl DynamicCertResolver {
    fn new() -> Self {
        Self {
            cert: ArcSwap::from_pointee(None),
            server_names: ArcSwap::from_pointee(Vec::new()),
            reject_unknown_sni: AtomicBool::new(false),
        }
    }
}

impl ResolvesServerCert for DynamicCertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let cert = self.cert.load_full();
        let cert = cert.as_ref().clone()?;
        let names = self.server_names.load();
        if self.reject_unknown_sni.load(Ordering::Relaxed) && !names.is_empty() {
            let sni = client_hello.server_name()?;
            if !names.iter().any(|name| name.eq_ignore_ascii_case(sni)) {
                return None;
            }
        }
        Some(cert)
    }
}

#[derive(Clone)]
pub struct TlsManager {
    resolver: Arc<DynamicCertResolver>,
    server_config: Arc<ArcSwap<Option<Arc<ServerConfig>>>>,
    source: Arc<Mutex<Option<CertSource>>>,
    last_hash: Arc<Mutex<Option<[u8; 32]>>>,
}

impl TlsManager {
    pub fn new() -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        Self {
            resolver: Arc::new(DynamicCertResolver::new()),
            server_config: Arc::new(ArcSwap::from_pointee(None)),
            source: Arc::new(Mutex::new(None)),
            last_hash: Arc::new(Mutex::new(None)),
        }
    }

    pub fn apply_node(&self, node: &NodeInfo, runtime: &RuntimeConfig) -> Result<()> {
        let Some(tls) = node.tls_settings() else {
            self.disable();
            return Ok(());
        };
        let mode = tls.cert_mode.trim();
        if node.tls != 1 || mode.is_empty() || mode == "none" {
            self.disable();
            return Ok(());
        }
        match mode {
            "file" => {
                let cert = fs::read(&tls.cert_file)
                    .with_context(|| format!("read cert_file {}", tls.cert_file))?;
                let key = fs::read(&tls.key_file)
                    .with_context(|| format!("read key_file {}", tls.key_file))?;
                self.apply_pem(
                    &cert,
                    &key,
                    tls,
                    runtime,
                    CertSource::File {
                        cert: PathBuf::from(&tls.cert_file),
                        key: PathBuf::from(&tls.key_file),
                    },
                )
            }
            "self" => {
                let (cert, key) = ensure_self_signed(tls)?;
                self.apply_pem(
                    &cert,
                    &key,
                    tls,
                    runtime,
                    CertSource::SelfSigned {
                        cert: PathBuf::from(&tls.cert_file),
                        key: PathBuf::from(&tls.key_file),
                    },
                )
            }
            "remote" => {
                write_file(&tls.cert_file, tls.tls_cert.as_bytes(), false)?;
                write_file(&tls.key_file, tls.tls_key.as_bytes(), true)?;
                self.apply_pem(
                    tls.tls_cert.as_bytes(),
                    tls.tls_key.as_bytes(),
                    tls,
                    runtime,
                    CertSource::Remote {
                        cert: PathBuf::from(&tls.cert_file),
                        key: PathBuf::from(&tls.key_file),
                    },
                )
            }
            "dns" | "http" => {
                let cert_path = Path::new(&tls.cert_file);
                let key_path = Path::new(&tls.key_file);
                if !cert_path.exists() || !key_path.exists() {
                    bail!(
                        "cert_mode={} has no certificate yet at {} / {}; run ACME first",
                        mode,
                        tls.cert_file,
                        tls.key_file
                    );
                }
                let cert = fs::read(cert_path)
                    .with_context(|| format!("read cert_file {}", tls.cert_file))?;
                let key = fs::read(key_path)
                    .with_context(|| format!("read key_file {}", tls.key_file))?;
                self.apply_pem(
                    &cert,
                    &key,
                    tls,
                    runtime,
                    CertSource::Acme {
                        cert: cert_path.to_path_buf(),
                        key: key_path.to_path_buf(),
                    },
                )
            }
            other => bail!("unsupported cert_mode {other:?}"),
        }
    }

    pub fn apply_acme_pem(
        &self,
        cert_pem: &[u8],
        key_pem: &[u8],
        tls: &TlsSettings,
        runtime: &RuntimeConfig,
    ) -> Result<()> {
        self.apply_pem(
            cert_pem,
            key_pem,
            tls,
            runtime,
            CertSource::Acme {
                cert: PathBuf::from(&tls.cert_file),
                key: PathBuf::from(&tls.key_file),
            },
        )
    }

    fn apply_pem(
        &self,
        cert_pem: &[u8],
        key_pem: &[u8],
        tls: &TlsSettings,
        runtime: &RuntimeConfig,
        source: CertSource,
    ) -> Result<()> {
        let certified = parse_certified_key(cert_pem, key_pem)?;
        self.resolver.cert.store(Arc::new(Some(certified)));
        self.resolver
            .server_names
            .store(Arc::new(tls.effective_server_names()));
        self.resolver
            .reject_unknown_sni
            .store(tls.reject_unknown_sni(), Ordering::Relaxed);
        let server_config = self.build_server_config(runtime)?;
        self.server_config.store(Arc::new(Some(server_config)));
        *self.source.lock() = Some(source);
        *self.last_hash.lock() = Some(hash_files(cert_pem, key_pem));
        Ok(())
    }

    fn build_server_config(&self, runtime: &RuntimeConfig) -> Result<Arc<ServerConfig>> {
        let provider = CryptoProvider::get_default()
            .cloned()
            .ok_or_else(|| anyhow!("rustls crypto provider is not installed"))?;
        let mut versions: Vec<&'static rustls::SupportedProtocolVersion> = Vec::new();
        if runtime.tls_max_version == "1.3" {
            versions.push(&rustls::version::TLS13);
        }
        if runtime.tls_min_version == "1.2" {
            versions.push(&rustls::version::TLS12);
        }
        if versions.is_empty() {
            bail!("no TLS versions enabled");
        }
        let mut config = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&versions)
            .map_err(|error| anyhow!("build TLS versions: {error}"))?
            .with_no_client_auth()
            .with_cert_resolver(self.resolver.clone());
        config.alpn_protocols = runtime
            .tls_alpn
            .iter()
            .map(|value| value.as_bytes().to_vec())
            .collect();
        Ok(Arc::new(config))
    }

    pub fn acceptor(&self) -> Option<TlsAcceptor> {
        self.server_config
            .load_full()
            .as_ref()
            .clone()
            .map(TlsAcceptor::from)
    }

    pub fn reload_if_changed(&self) -> Result<bool> {
        let source = self.source.lock().clone();
        let Some(source) = source else {
            return Ok(false);
        };
        let (cert_path, key_path) = source.paths();
        let cert = fs::read(cert_path).with_context(|| format!("read {}", cert_path.display()))?;
        let key = fs::read(key_path).with_context(|| format!("read {}", key_path.display()))?;
        let hash = hash_files(&cert, &key);
        if self.last_hash.lock().as_ref() == Some(&hash) {
            return Ok(false);
        }
        let certified = parse_certified_key(&cert, &key)?;
        self.resolver.cert.store(Arc::new(Some(certified)));
        *self.last_hash.lock() = Some(hash);
        Ok(true)
    }

    fn disable(&self) {
        self.resolver.cert.store(Arc::new(None));
        self.resolver.server_names.store(Arc::new(Vec::new()));
        self.resolver
            .reject_unknown_sni
            .store(false, Ordering::Relaxed);
        self.server_config.store(Arc::new(None));
        *self.source.lock() = None;
        *self.last_hash.lock() = None;
    }
}

pub(crate) fn certificate_matches_key(cert_pem: &[u8], key_pem: &[u8]) -> bool {
    parse_certified_key(cert_pem, key_pem).is_ok()
}

fn parse_certified_key(cert_pem: &[u8], key_pem: &[u8]) -> Result<Arc<CertifiedKey>> {
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut BufReader::new(cert_pem))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse certificate PEM")?;
    if certs.is_empty() {
        bail!("certificate PEM contains no certificates");
    }
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut BufReader::new(key_pem))
        .context("parse private key PEM")?
        .ok_or_else(|| anyhow!("private key PEM contains no key"))?;
    let provider = CryptoProvider::get_default()
        .cloned()
        .ok_or_else(|| anyhow!("rustls crypto provider is not installed"))?;
    let signing_key = provider
        .key_provider
        .load_private_key(key)
        .map_err(|error| anyhow!("load private key: {error}"))?;
    let certified = CertifiedKey::new(certs, signing_key);
    certified
        .keys_match()
        .map_err(|error| anyhow!("certificate private key mismatch: {error}"))?;
    Ok(Arc::new(certified))
}

fn ensure_self_signed(tls: &TlsSettings) -> Result<(Vec<u8>, Vec<u8>)> {
    if Path::new(&tls.cert_file).exists() && Path::new(&tls.key_file).exists() {
        let cert = fs::read(&tls.cert_file)
            .with_context(|| format!("read cert_file {}", tls.cert_file))?;
        let key =
            fs::read(&tls.key_file).with_context(|| format!("read key_file {}", tls.key_file))?;
        return Ok((cert, key));
    }
    let names = tls.effective_server_names();
    if names.is_empty() {
        bail!("self-signed certificate requires server_name or server_names");
    }
    let generated = rcgen::generate_simple_self_signed(names)
        .map_err(|error| anyhow!("generate self-signed certificate: {error}"))?;
    let cert_pem = generated.cert.pem().into_bytes();
    let key_pem = generated.signing_key.serialize_pem().into_bytes();
    write_file(&tls.cert_file, &cert_pem, false)?;
    write_file(&tls.key_file, &key_pem, true)?;
    Ok((cert_pem, key_pem))
}

fn write_file(path: &str, data: &[u8], private: bool) -> Result<()> {
    let path = Path::new(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mode = if private { 0o600 } else { 0o644 };
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("v2node");
        let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = path.with_file_name(format!(
            ".{file_name}.tmp.{}.{}",
            std::process::id(),
            sequence
        ));
        let result = (|| -> Result<()> {
            let mut options = fs::OpenOptions::new();
            options.create(true).truncate(true).write(true).mode(mode);
            let mut file = options
                .open(&temporary)
                .with_context(|| format!("open {}", temporary.display()))?;
            file.write_all(data)
                .with_context(|| format!("write {}", temporary.display()))?;
            file.sync_all()
                .with_context(|| format!("sync {}", temporary.display()))?;
            drop(file);
            fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))
                .with_context(|| format!("set permissions on {}", temporary.display()))?;
            fs::rename(&temporary, path)
                .with_context(|| format!("rename {} to {}", temporary.display(), path.display()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
    }
    #[cfg(not(unix))]
    {
        let _ = private;
        fs::write(path, data).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

pub(crate) fn write_private_file(path: &str, data: &[u8]) -> Result<()> {
    write_file(path, data, true)
}

pub(crate) fn write_pem_files(
    cert_path: &str,
    key_path: &str,
    cert: &[u8],
    key: &[u8],
) -> Result<()> {
    write_file(cert_path, cert, false)?;
    write_file(key_path, key, true)
}

fn hash_files(cert: &[u8], key: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(cert);
    hasher.update([0u8]);
    hasher.update(key);
    let digest = hasher.finalize();
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&digest);
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "v2node-tls-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn runtime() -> RuntimeConfig {
        RuntimeConfig::default()
    }

    #[test]
    fn mismatched_certificate_and_key_are_rejected() {
        let first = rcgen::generate_simple_self_signed(vec!["first.local".to_owned()]).unwrap();
        let second = rcgen::generate_simple_self_signed(vec!["second.local".to_owned()]).unwrap();
        let result = parse_certified_key(
            first.cert.pem().as_bytes(),
            second.signing_key.serialize_pem().as_bytes(),
        );
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn existing_private_key_permissions_are_corrected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        let path = dir.join("node.key");
        write_file(&path.display().to_string(), b"first", true).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_file(&path.display().to_string(), b"second", true).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn self_signed_certificates_are_generated_and_loaded() {
        let dir = temp_dir("self");
        let cert = dir.join("node.crt");
        let key = dir.join("node.key");
        let tls = TlsSettings {
            server_name: "test.local".to_owned(),
            cert_mode: "self".to_owned(),
            cert_file: cert.display().to_string(),
            key_file: key.display().to_string(),
            ..Default::default()
        };
        let manager = TlsManager::new();
        let (cert_pem, key_pem) = ensure_self_signed(&tls).unwrap();
        let result = manager.apply_pem(
            &cert_pem,
            &key_pem,
            &tls,
            &runtime(),
            CertSource::SelfSigned {
                cert: cert.clone(),
                key: key.clone(),
            },
        );
        assert!(result.is_ok(), "{result:?}");
        assert!(manager.acceptor().is_some());
        assert!(cert.exists());
        assert!(key.exists());
        let _ = fs::remove_dir_all(dir);
    }
}
