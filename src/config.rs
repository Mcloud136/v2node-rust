use anyhow::{Context, Result, bail};
use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct AppConfig {
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub nodes: Vec<NodeConfig>,
    #[serde(default)]
    pub pprof_port: u16,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub acme: AcmeConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct LogConfig {
    pub level: String,
    pub output: String,
    pub access: String,
    pub ansi: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".to_owned(),
            output: String::new(),
            access: "none".to_owned(),
            ansi: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct NodeConfig {
    #[serde(rename = "ApiHost")]
    pub api_host: String,
    #[serde(rename = "NodeID")]
    pub node_id: i32,
    #[serde(rename = "ApiKey")]
    pub api_key: String,
    #[serde(default = "default_node_timeout")]
    pub timeout: u64,
    #[serde(rename = "RetryCount", default)]
    pub retry_count: Option<usize>,
}

fn default_node_timeout() -> u64 {
    15
}

impl NodeConfig {
    pub fn effective_timeout(&self, runtime: &RuntimeConfig) -> u64 {
        if self.timeout > 0 {
            self.timeout
        } else {
            runtime.panel_request_timeout_secs
        }
    }

    pub fn effective_retry(&self, runtime: &RuntimeConfig) -> usize {
        self.retry_count.unwrap_or(runtime.panel_retry_count)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct RuntimeConfig {
    pub config_watch_interval_secs: u64,
    pub panel_connect_timeout_secs: u64,
    pub panel_request_timeout_secs: u64,
    pub panel_retry_count: usize,
    pub panel_retry_backoff_ms: u64,
    pub push_idempotency: bool,
    pub traffic_spool_path: String,
    pub default_pull_interval_secs: u64,
    pub default_push_interval_secs: u64,
    pub ws_handshake_timeout_secs: u64,
    pub vless_handshake_timeout_secs: u64,
    pub ws_max_header_bytes: usize,
    pub ws_max_message_bytes: usize,
    pub ws_early_data_max_bytes: usize,
    pub ws_heartbeat_secs: u64,
    pub connection_idle_timeout_secs: u64,
    pub max_connections: usize,
    pub listen_backlog: usize,
    pub drain_timeout_secs: u64,
    pub tcp_connect_timeout_secs: u64,
    pub allow_private_targets: bool,
    pub udp_idle_timeout_secs: u64,
    pub relay_buffer_size: usize,
    pub tls_handshake_timeout_secs: u64,
    pub tls_min_version: String,
    pub tls_max_version: String,
    pub tls_alpn: Vec<String>,
    pub cert_reload_interval_secs: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            config_watch_interval_secs: 2,
            panel_connect_timeout_secs: 10,
            panel_request_timeout_secs: 15,
            panel_retry_count: 1,
            panel_retry_backoff_ms: 200,
            push_idempotency: false,
            traffic_spool_path: String::new(),
            default_pull_interval_secs: 60,
            default_push_interval_secs: 60,
            ws_handshake_timeout_secs: 4,
            vless_handshake_timeout_secs: 10,
            ws_max_header_bytes: 8192,
            ws_max_message_bytes: 1048576,
            ws_early_data_max_bytes: 2048,
            ws_heartbeat_secs: 0,
            connection_idle_timeout_secs: 0,
            max_connections: 4096,
            listen_backlog: 4096,
            drain_timeout_secs: 10,
            tcp_connect_timeout_secs: 10,
            allow_private_targets: false,
            udp_idle_timeout_secs: 60,
            relay_buffer_size: 32768,
            tls_handshake_timeout_secs: 4,
            tls_min_version: "1.2".to_owned(),
            tls_max_version: "1.3".to_owned(),
            tls_alpn: vec!["http/1.1".to_owned()],
            cert_reload_interval_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct AcmeConfig {
    pub directory_url: String,
    pub email: String,
    pub account_dir: String,
    pub http01_bind: String,
    pub renew_before_days: u64,
    pub renew_check_interval_secs: u64,
    pub dns_provider: String,
    pub dns_env: BTreeMap<String, String>,
    pub dns_propagation_delay_secs: u64,
    pub cloudflare_api_base: String,
    pub root_cert: String,
}

impl Default for AcmeConfig {
    fn default() -> Self {
        Self {
            directory_url: "https://acme-v02.api.letsencrypt.org/directory".to_owned(),
            email: String::new(),
            account_dir: "/etc/v2node-rust/acme".to_owned(),
            http01_bind: "0.0.0.0:80".to_owned(),
            renew_before_days: 30,
            renew_check_interval_secs: 86400,
            dns_provider: "cloudflare".to_owned(),
            dns_env: BTreeMap::new(),
            dns_propagation_delay_secs: 10,
            cloudflare_api_base: "https://api.cloudflare.com/client/v4".to_owned(),
            root_cert: String::new(),
        }
    }
}

impl AppConfig {
    pub fn validate(&self) -> Result<()> {
        if self.nodes.is_empty() {
            bail!("Nodes must not be empty");
        }
        for (i, node) in self.nodes.iter().enumerate() {
            if !(node.api_host.starts_with("http://") || node.api_host.starts_with("https://")) {
                bail!("Nodes[{i}].ApiHost must start with http:// or https://");
            }
            if node.node_id <= 0 {
                bail!("Nodes[{i}].NodeID must be > 0");
            }
            if node.api_key.trim().is_empty() {
                bail!("Nodes[{i}].ApiKey must not be empty");
            }
        }
        let rt = &self.runtime;
        if rt.config_watch_interval_secs == 0
            || rt.panel_connect_timeout_secs == 0
            || rt.panel_request_timeout_secs == 0
            || rt.ws_handshake_timeout_secs == 0
            || rt.vless_handshake_timeout_secs == 0
            || rt.tcp_connect_timeout_secs == 0
            || rt.udp_idle_timeout_secs == 0
            || rt.tls_handshake_timeout_secs == 0
            || rt.cert_reload_interval_secs == 0
        {
            bail!("Runtime timeouts and intervals must be > 0");
        }
        if !(128..=65535).contains(&rt.listen_backlog) {
            bail!("Runtime.ListenBacklog must be between 128 and 65535");
        }
        if rt.relay_buffer_size < 1024 {
            bail!("Runtime.RelayBufferSize must be >= 1024");
        }
        if rt.ws_max_header_bytes < 1024 {
            bail!("Runtime.WsMaxHeaderBytes must be >= 1024");
        }
        if rt.ws_max_message_bytes < 1024 {
            bail!("Runtime.WsMaxMessageBytes must be >= 1024");
        }
        if rt.tls_min_version != "1.2" && rt.tls_min_version != "1.3" {
            bail!("Runtime.TlsMinVersion must be 1.2 or 1.3");
        }
        if rt.tls_max_version != "1.2" && rt.tls_max_version != "1.3" {
            bail!("Runtime.TlsMaxVersion must be 1.2 or 1.3");
        }
        if rt.tls_min_version == "1.3" && rt.tls_max_version == "1.2" {
            bail!("Runtime.TlsMinVersion must not exceed TlsMaxVersion");
        }
        if self.acme.renew_before_days == 0 || self.acme.renew_check_interval_secs == 0 {
            bail!("Acme renewal values must be > 0");
        }
        Ok(())
    }
}

pub fn load(path: &Path) -> Result<AppConfig> {
    let (bytes, _) = read_and_hash(path)?;
    let config: AppConfig = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse config {}", path.display()))?;
    config.validate()?;
    Ok(config)
}

fn read_and_hash(path: &Path) -> Result<(Vec<u8>, [u8; 32])> {
    let bytes = std::fs::read(path).with_context(|| format!("read config {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&digest);
    Ok((bytes, hash))
}

pub fn spawn_watcher(
    path: PathBuf,
    store: Arc<ArcSwap<AppConfig>>,
    sender: watch::Sender<Arc<AppConfig>>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_hash = read_and_hash(&path).ok().map(|(_, hash)| hash);
        loop {
            let interval = store.load().runtime.config_watch_interval_secs.max(1);
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(interval)) => {}
                () = cancel.cancelled() => break,
            }
            let (bytes, hash) = match read_and_hash(&path) {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(error = %error, "config file read failed");
                    continue;
                }
            };
            if last_hash.as_ref() == Some(&hash) {
                continue;
            }
            let parsed = serde_json::from_slice::<AppConfig>(&bytes)
                .map_err(anyhow::Error::from)
                .and_then(|config| {
                    config.validate()?;
                    Ok(config)
                });
            match parsed {
                Ok(config) => {
                    last_hash = Some(hash);
                    let config = Arc::new(config);
                    store.store(config.clone());
                    if sender.send(config).is_err() {
                        break;
                    }
                    tracing::info!(path = %path.display(), "config file reloaded");
                }
                Err(error) => {
                    tracing::error!(
                        error = %error,
                        path = %path.display(),
                        "config reload rejected; keeping previous config"
                    );
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_config() -> AppConfig {
        AppConfig {
            log: LogConfig::default(),
            nodes: vec![NodeConfig {
                api_host: "http://127.0.0.1:8080".to_owned(),
                node_id: 1,
                api_key: "test".to_owned(),
                timeout: 15,
                retry_count: None,
            }],
            pprof_port: 0,
            runtime: RuntimeConfig::default(),
            acme: AcmeConfig::default(),
        }
    }

    #[test]
    fn listen_backlog_defaults_and_validates() {
        let mut config = minimal_config();
        assert_eq!(config.runtime.listen_backlog, 4096);
        config.runtime.listen_backlog = 127;
        assert!(config.validate().is_err());
        config.runtime.listen_backlog = 128;
        assert!(config.validate().is_ok());
        config.runtime.listen_backlog = 65535;
        assert!(config.validate().is_ok());
        config.runtime.listen_backlog = 65536;
        assert!(config.validate().is_err());
    }
}
