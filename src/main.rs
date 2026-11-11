mod acme;
mod config;
mod listener;
mod logging;
mod node;
mod panel;
mod state;
mod tls;
mod vless;

// C0b：协议实现已抽到 `v2node-proto`。此处 re-export 保留 `crate::encryption::*`
// 路径，避免一次性改动 vless.rs / node.rs / 单测中的既有引用；角色解耦完成后移除。
pub use v2node_proto::encryption;

use clap::Parser;
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Parser)]
#[command(
    name = "v2node-rust",
    version,
    about = "V2Board node backend (Rust M1)"
)]
struct Cli {
    #[arg(short, long, default_value = "/etc/v2node-rust/config.json")]
    config: PathBuf,
    #[arg(short, long, default_value_t = true, action = clap::ArgAction::Set)]
    watch: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let config = Arc::new(config::load(&cli.config)?);
    let logging = logging::init(&config.log)?;
    tracing::info!(
        config = %cli.config.display(),
        nodes = config.nodes.len(),
        "v2node-rust starting"
    );
    if config.pprof_port != 0 {
        tracing::warn!(
            port = config.pprof_port,
            "PprofPort is kept for config compatibility but Go pprof is not available in the Rust node"
        );
    }
    if !config.log.access.trim().is_empty() && config.log.access.trim() != "none" {
        tracing::warn!(
            path = %config.log.access,
            "Log.Access is kept for config compatibility but access logging is not implemented"
        );
    }
    let mut manager = Some(node::NodeManager::start(&config).await?);
    let mut active_config = (*config).clone();

    let store = Arc::new(arc_swap::ArcSwap::from_pointee((*config).clone()));
    let (sender, mut receiver) = tokio::sync::watch::channel(config.clone());
    let cancel = CancellationToken::new();
    let watcher = if cli.watch {
        Some(config::spawn_watcher(
            cli.config.clone(),
            store,
            sender.clone(),
            cancel.clone(),
        ))
    } else {
        None
    };
    let _keep_sender_alive = sender;

    loop {
        tokio::select! {
            () = wait_signal() => {
                tracing::info!("shutdown signal received");
                break;
            }
            changed = receiver.changed() => {
                if changed.is_err() {
                    break;
                }
                let new_config = receiver.borrow_and_update().clone();
                if new_config.nodes == active_config.nodes
                    && new_config.runtime == active_config.runtime
                    && new_config.acme == active_config.acme
                {
                    if let Err(error) = logging.reload(&new_config.log) {
                        tracing::error!(error = %error, "reload logging failed");
                    }
                    active_config = (*new_config).clone();
                    tracing::info!("logging config applied");
                    continue;
                }
                let applied = match manager.as_mut() {
                    Some(active) => active.apply_config(&new_config).await,
                    None => Err(anyhow::anyhow!("no active node manager")),
                };
                match applied {
                    Ok(()) => {
                        if let Err(error) = logging.reload(&new_config.log) {
                            tracing::error!(error = %error, "reload logging failed");
                        }
                        tracing::info!("config applied in place");
                        active_config = (*new_config).clone();
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "in-place config apply failed; rebuilding");
                        if let Some(old) = manager.take() {
                            old.stop().await;
                        }
                        match node::NodeManager::start(&new_config).await {
                            Ok(new_manager) => {
                                tracing::info!(nodes = new_manager.len(), "config applied");
                                manager = Some(new_manager);
                                if let Err(log_error) = logging.reload(&new_config.log) {
                                    tracing::error!(error = %log_error, "reload logging failed");
                                }
                                active_config = (*new_config).clone();
                            }
                            Err(start_error) => {
                                tracing::error!(error = %start_error, "apply config failed; restoring previous config");
                                if let Err(log_error) = logging.reload(&active_config.log) {
                                    tracing::error!(error = %log_error, "restore logging config failed");
                                }
                                match node::NodeManager::start(&active_config).await {
                                    Ok(restored) => {
                                        tracing::warn!(nodes = restored.len(), "previous config restored");
                                        manager = Some(restored);
                                    }
                                    Err(restore_error) => {
                                        tracing::error!(error = %restore_error, "previous config restore failed; no node is running");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    cancel.cancel();
    if let Some(manager) = manager.take() {
        manager.stop().await;
    }
    if let Some(handle) = watcher {
        handle.abort();
    }
    Ok(())
}

#[cfg(unix)]
async fn wait_signal() {
    let Ok(mut terminate) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_flag_can_be_disabled() {
        let default = Cli::try_parse_from(["v2node-rust"]).unwrap();
        assert!(default.watch);
        let disabled = Cli::try_parse_from(["v2node-rust", "--watch", "false"]).unwrap();
        assert!(!disabled.watch);
    }
}
