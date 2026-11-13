use anyhow::{Result, anyhow};
use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{Mutex, Semaphore},
    task::JoinHandle,
    time::sleep,
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    acme::AcmeManager,
    config::{AppConfig, NodeConfig, RuntimeConfig},
    encryption::EncryptionServer,
    listener::{ListenerConfig, NodeListener},
    panel::{NodeInfo, PanelClient, UserInfo, UserTraffic},
    state::{NodeState, UserEntry},
    tls::TlsManager,
    vless::VlessContext,
};

struct ListenerSlot {
    listener: NodeListener,
    token: CancellationToken,
    config: Arc<ListenerConfig>,
    context: Arc<VlessContext>,
}

pub struct NodeRuntime {
    pub node_id: i32,
    node_config: NodeConfig,
    runtime: Arc<RuntimeConfig>,
    acme: AcmeManager,
    tls: TlsManager,
    node_info: Arc<ArcSwap<NodeInfo>>,
    connection_limit: Option<Arc<Semaphore>>,
    cancel: CancellationToken,
    state: Arc<NodeState>,
    listener_slot: Arc<Mutex<Option<ListenerSlot>>>,
    traffic: Arc<Mutex<TrafficReporter>>,
    tasks: Vec<JoinHandle<()>>,
}

impl NodeRuntime {
    pub async fn start(
        node_config: NodeConfig,
        runtime: Arc<RuntimeConfig>,
        acme: AcmeManager,
        connection_limit: Option<Arc<Semaphore>>,
        parent_cancel: CancellationToken,
    ) -> Result<Self> {
        let cancel = parent_cancel.child_token();
        let client = PanelClient::new(&node_config, &runtime)?;
        let node_info = client
            .get_node_config()
            .await?
            .ok_or_else(|| anyhow!("initial node config is empty"))?;
        let users = client.get_user_list().await?.unwrap_or_default();
        let alive = client.get_alive_list().await;

        let state = Arc::new(NodeState::new());
        let startup_traffic = apply_users(&state, &users);
        if let Some(alive) = alive {
            state.limiter.update_alive(alive);
        }
        let encryption = build_encryption(&node_info)?;

        let tls = TlsManager::new();
        ensure_acme_certificate(&acme, &node_info, &runtime, &tls).await;
        tls.apply_node(&node_info, &runtime)?;
        let node_info = Arc::new(ArcSwap::from_pointee(node_info));
        let vless_context = Arc::new(VlessContext {
            state: state.clone(),
            runtime: runtime.clone(),
            node: node_info.load_full(),
            encryption,
            cancel: cancel.clone(),
        });
        let listener_config = Arc::new(ListenerConfig::from_node(
            &node_info.load_full(),
            &runtime,
            &tls,
            connection_limit.clone(),
        )?);
        let listener_token = cancel.child_token();
        let listener = NodeListener::start(
            listener_config.clone(),
            vless_context.clone(),
            listener_token.clone(),
        )
        .await?;
        let listener_slot = Arc::new(Mutex::new(Some(ListenerSlot {
            listener,
            token: listener_token,
            config: listener_config,
            context: vless_context,
        })));
        let mut reporter = TrafficReporter::new(
            client.clone(),
            runtime.push_idempotency,
            &runtime.traffic_spool_path,
        );
        reporter.load_spool();
        // Fresh store means the diff is normally empty; keep the settlement
        // path explicit so a future pre-seeded store cannot silently drop it.
        reporter.add_all(&startup_traffic);
        let traffic = Arc::new(Mutex::new(reporter));
        let tasks = spawn_background_tasks(
            client,
            state.clone(),
            tls.clone(),
            acme.clone(),
            node_info.clone(),
            listener_slot.clone(),
            runtime.clone(),
            connection_limit.clone(),
            traffic.clone(),
            cancel.clone(),
            node_config.node_id,
        );

        info!(
            node_id = node_config.node_id,
            users = state.users.len(),
            "node runtime started"
        );
        Ok(Self {
            node_id: node_config.node_id,
            node_config,
            runtime,
            acme,
            tls,
            node_info,
            connection_limit,
            cancel,
            state,
            listener_slot,
            traffic,
            tasks,
        })
    }

    pub async fn apply_config(
        &mut self,
        node_config: NodeConfig,
        runtime: Arc<RuntimeConfig>,
        acme: AcmeManager,
        connection_limit: Option<Arc<Semaphore>>,
    ) -> Result<()> {
        if node_config != self.node_config {
            return Err(anyhow!(
                "node {} connection config changed; restart required",
                node_config.node_id
            ));
        }
        let info = self.node_info.load_full();
        ensure_acme_certificate(&acme, &info, &runtime, &self.tls).await;
        self.tls.apply_node(&info, &runtime)?;
        let encryption = match build_encryption(&info) {
            Ok(value) => value,
            Err(error) => {
                if let Err(restore_error) = self.tls.apply_node(&info, &self.runtime) {
                    warn!(node_id = self.node_id, error = %restore_error, "restore previous TLS config failed");
                }
                return Err(error);
            }
        };
        let listener_config = match ListenerConfig::from_node(
            &info,
            &runtime,
            &self.tls,
            connection_limit.clone(),
        ) {
            Ok(value) => Arc::new(value),
            Err(error) => {
                if let Err(restore_error) = self.tls.apply_node(&info, &self.runtime) {
                    warn!(node_id = self.node_id, error = %restore_error, "restore previous TLS config failed");
                }
                return Err(error);
            }
        };
        let context = Arc::new(VlessContext {
            state: self.state.clone(),
            runtime: runtime.clone(),
            node: info,
            encryption,
            cancel: self.cancel.clone(),
        });

        // Stop background tasks first so poll_loop cannot apply a competing
        // update while the listener state is being swapped.
        self.abort_tasks().await;
        self.drain_traffic(Duration::from_secs(self.runtime.drain_timeout_secs.max(1)))
            .await;

        if let Err(error) = self.apply_listener(listener_config, context).await {
            let current = self.node_info.load_full();
            if let Err(restore_error) = self.tls.apply_node(&current, &self.runtime) {
                warn!(node_id = self.node_id, error = %restore_error, "restore previous TLS config failed");
            }
            let old_runtime = self.runtime.clone();
            let old_acme = self.acme.clone();
            let old_limit = self.connection_limit.clone();
            if let Err(restart_error) = self.restart_tasks(old_runtime, old_acme, old_limit).await {
                warn!(node_id = self.node_id, error = %restart_error, "restore background tasks failed");
            }
            return Err(error);
        }

        self.runtime = runtime;
        self.acme = acme;
        self.connection_limit = connection_limit;
        let new_runtime = self.runtime.clone();
        let new_acme = self.acme.clone();
        let new_limit = self.connection_limit.clone();
        self.restart_tasks(new_runtime, new_acme, new_limit).await?;
        info!(node_id = self.node_id, "node config applied in place");
        Ok(())
    }

    async fn apply_listener(
        &mut self,
        config: Arc<ListenerConfig>,
        context: Arc<VlessContext>,
    ) -> Result<()> {
        let mut slot = self.listener_slot.lock().await;
        let Some(existing) = slot.take() else {
            let token = self.cancel.child_token();
            let listener =
                NodeListener::start(config.clone(), context.clone(), token.clone()).await?;
            *slot = Some(ListenerSlot {
                listener,
                token,
                config,
                context,
            });
            return Ok(());
        };
        if existing.config.bind == config.bind && existing.config.backlog == config.backlog {
            existing.listener.update(config.clone(), context.clone());
            *slot = Some(ListenerSlot {
                listener: existing.listener,
                token: existing.token,
                config,
                context,
            });
            return Ok(());
        }
        existing.token.cancel();
        existing.listener.wait().await;
        let token = self.cancel.child_token();
        match NodeListener::start(config.clone(), context.clone(), token.clone()).await {
            Ok(listener) => {
                *slot = Some(ListenerSlot {
                    listener,
                    token,
                    config,
                    context,
                });
                Ok(())
            }
            Err(error) => {
                let restore_token = self.cancel.child_token();
                match NodeListener::start(
                    existing.config.clone(),
                    existing.context.clone(),
                    restore_token.clone(),
                )
                .await
                {
                    Ok(listener) => {
                        *slot = Some(ListenerSlot {
                            listener,
                            token: restore_token,
                            config: existing.config,
                            context: existing.context,
                        });
                        warn!(node_id = self.node_id, error = %error, "listener rebind failed; previous listener restored");
                    }
                    Err(restore_error) => {
                        warn!(node_id = self.node_id, error = %restore_error, "listener rebind failed and previous listener could not be restored");
                    }
                }
                Err(error)
            }
        }
    }

    async fn abort_tasks(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
            let _ = task.await;
        }
    }

    async fn drain_traffic(&self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let mut reporter = self.traffic.lock().await;
                reporter.deliver().await;
                if reporter.is_empty() {
                    return;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            sleep(Duration::from_millis(500)).await;
        }
        let reporter = self.traffic.lock().await;
        warn!(
            node_id = self.node_id,
            bytes = reporter.pending_bytes(),
            "traffic pending remains after drain; spooled for next start"
        );
    }

    async fn restart_tasks(
        &mut self,
        runtime: Arc<RuntimeConfig>,
        acme: AcmeManager,
        connection_limit: Option<Arc<Semaphore>>,
    ) -> Result<()> {
        let client = PanelClient::new(&self.node_config, &runtime)?;
        {
            let mut reporter = self.traffic.lock().await;
            reporter.rebind(
                client.clone(),
                runtime.push_idempotency,
                &runtime.traffic_spool_path,
            );
        }
        self.tasks = spawn_background_tasks(
            client,
            self.state.clone(),
            self.tls.clone(),
            acme,
            self.node_info.clone(),
            self.listener_slot.clone(),
            runtime,
            connection_limit,
            self.traffic.clone(),
            self.cancel.clone(),
            self.node_id,
        );
        Ok(())
    }

    pub async fn stop(self) {
        self.cancel.cancel();
        if let Some(slot) = self.listener_slot.lock().await.take() {
            slot.token.cancel();
            slot.listener.wait().await;
        }
        self.state.links.close_all();
        self.drain_traffic(Duration::from_secs(self.runtime.drain_timeout_secs.max(1)))
            .await;
        for task in self.tasks {
            task.abort();
            let _ = task.await;
        }
        info!(node_id = self.node_id, "node runtime stopped");
    }
}

async fn ensure_acme_certificate(
    acme: &AcmeManager,
    info: &NodeInfo,
    runtime: &RuntimeConfig,
    tls: &TlsManager,
) {
    if !matches!(info.cert_mode(), "dns" | "http") {
        return;
    }
    let Some(settings) = info.tls_settings() else {
        warn!("ACME node has no tls_settings");
        return;
    };
    if let Err(error) = acme.ensure_certificate(settings, runtime, tls).await {
        // A transient ACME failure must not take down a node that still has a
        // usable certificate on disk; tls.apply_node will fail if none exists.
        warn!(error = %error, "ACME certificate check failed");
    }
}

const MAX_BATCH_USERS: usize = 500;
const UNKNOWN_REPLAY_SECS: u64 = 240;
const SPOOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpoolEntry {
    uid: i32,
    upload: i64,
    download: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SpoolState {
    NotSent,
    Unknown,
    Sending,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpoolBatch {
    batch_id: String,
    entries: Vec<SpoolEntry>,
    state: SpoolState,
    unknown_since_unix: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct SpoolFile {
    version: u32,
    batches: Vec<SpoolBatch>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

struct TrafficReporter {
    client: PanelClient,
    pending: HashMap<i32, (i64, i64)>,
    batches: Vec<SpoolBatch>,
    idempotency: bool,
    spool_path: Option<PathBuf>,
}

impl TrafficReporter {
    fn new(client: PanelClient, idempotency: bool, spool_path: &str) -> Self {
        let spool_path = if spool_path.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(spool_path.trim()))
        };
        Self {
            client,
            pending: HashMap::new(),
            batches: Vec::new(),
            idempotency,
            spool_path,
        }
    }

    fn rebind(&mut self, client: PanelClient, idempotency: bool, spool_path: &str) {
        self.client = client;
        self.idempotency = idempotency;
        let new_path = if spool_path.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(spool_path.trim()))
        };
        if self.spool_path != new_path {
            self.spool_path = new_path;
            self.write_spool();
        }
    }

    fn add(&mut self, uid: i32, upload: i64, download: i64) {
        let entry = self.pending.entry(uid).or_insert((0, 0));
        entry.0 = entry.0.saturating_add(upload);
        entry.1 = entry.1.saturating_add(download);
    }

    fn add_all(&mut self, traffic: &[UserTraffic]) {
        for item in traffic {
            self.add(item.uid, item.upload, item.download);
        }
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.batches.is_empty()
    }

    fn pending_bytes(&self) -> i64 {
        let pending = self
            .pending
            .values()
            .map(|(upload, download)| upload.saturating_add(*download))
            .fold(0i64, i64::saturating_add);
        let batches = self
            .batches
            .iter()
            .flat_map(|batch| batch.entries.iter())
            .map(|entry| entry.upload.saturating_add(entry.download))
            .fold(0i64, i64::saturating_add);
        pending.saturating_add(batches)
    }

    fn enqueue_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let entries: Vec<SpoolEntry> = self
            .pending
            .drain()
            .map(|(uid, (upload, download))| SpoolEntry {
                uid,
                upload,
                download,
            })
            .collect();
        for chunk in entries.chunks(MAX_BATCH_USERS) {
            self.batches.push(SpoolBatch {
                batch_id: Uuid::new_v4().to_string(),
                entries: chunk.to_vec(),
                state: SpoolState::NotSent,
                unknown_since_unix: 0,
            });
        }
        self.write_spool();
    }

    fn should_drop_batch(&self, batch: &SpoolBatch) -> bool {
        match batch.state {
            SpoolState::NotSent => false,
            SpoolState::Unknown | SpoolState::Sending => {
                if !self.idempotency || batch.unknown_since_unix == 0 {
                    return true;
                }
                now_unix().saturating_sub(batch.unknown_since_unix) > UNKNOWN_REPLAY_SECS
            }
        }
    }

    fn load_spool(&mut self) {
        let Some(path) = &self.spool_path else {
            return;
        };
        let Ok(bytes) = fs::read(path) else {
            return;
        };
        let spool: SpoolFile = match serde_json::from_slice(&bytes) {
            Ok(spool) => spool,
            Err(error) => {
                warn!(error = %error, "traffic spool is invalid; dropping it");
                let _ = fs::remove_file(path);
                return;
            }
        };
        if spool.version != SPOOL_VERSION {
            warn!(
                version = spool.version,
                "traffic spool version mismatch; dropping it"
            );
            let _ = fs::remove_file(path);
            return;
        }
        let mut kept = Vec::new();
        for mut batch in spool.batches {
            if batch.state == SpoolState::Sending {
                batch.state = SpoolState::Unknown;
                if batch.unknown_since_unix == 0 {
                    batch.unknown_since_unix = now_unix();
                }
            }
            if self.should_drop_batch(&batch) {
                warn!(
                    batch = %batch.batch_id,
                    users = batch.entries.len(),
                    "dropping stale traffic batch from spool"
                );
            } else {
                kept.push(batch);
            }
        }
        self.batches = kept;
        info!(
            batches = self.batches.len(),
            bytes = self.pending_bytes(),
            "traffic spool loaded"
        );
        self.write_spool();
    }

    fn write_spool(&self) {
        let Some(path) = &self.spool_path else {
            return;
        };
        if self.batches.is_empty() {
            let _ = fs::remove_file(path);
            return;
        }
        let data = SpoolFile {
            version: SPOOL_VERSION,
            batches: self.batches.clone(),
        };
        let bytes = match serde_json::to_vec(&data) {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!(error = %error, "serialize traffic spool failed");
                return;
            }
        };
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            warn!(error = %error, path = %parent.display(), "create traffic spool directory failed");
            return;
        }
        let temporary = path.with_extension("tmp");
        let result = (|| -> std::io::Result<()> {
            let mut file = fs::File::create(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
            }
            fs::rename(&temporary, path)?;
            Ok(())
        })();
        if let Err(error) = result {
            warn!(error = %error, path = %path.display(), "write traffic spool failed");
            let _ = fs::remove_file(&temporary);
        }
    }

    async fn deliver(&mut self) {
        self.enqueue_pending();
        if self.batches.is_empty() {
            self.write_spool();
            return;
        }
        let mut index = 0;
        while index < self.batches.len() {
            if self.should_drop_batch(&self.batches[index]) {
                let batch = self.batches.remove(index);
                warn!(
                    batch = %batch.batch_id,
                    users = batch.entries.len(),
                    "dropping stale traffic batch"
                );
                self.write_spool();
                continue;
            }
            if self.batches[index].unknown_since_unix == 0 {
                self.batches[index].unknown_since_unix = now_unix();
            }
            self.batches[index].state = SpoolState::Sending;
            self.write_spool();
            let entries = self.batches[index].entries.clone();
            let batch_id = self.batches[index].batch_id.clone();
            let traffic: Vec<UserTraffic> = entries
                .iter()
                .map(|entry| UserTraffic {
                    uid: entry.uid,
                    upload: entry.upload,
                    download: entry.download,
                })
                .collect();
            let total = entries
                .iter()
                .map(|entry| entry.upload.saturating_add(entry.download))
                .fold(0i64, i64::saturating_add);
            let batch_id_arg = self.idempotency.then_some(batch_id.as_str());
            match self.client.report_traffic(&traffic, batch_id_arg).await {
                Ok(()) => {
                    self.batches.remove(index);
                    self.write_spool();
                }
                Err(error) if error.permanent => {
                    tracing::error!(
                        batch = %batch_id,
                        bytes = total,
                        users = traffic.len(),
                        error = %error,
                        "panel permanently rejected traffic batch; dropping it"
                    );
                    self.batches.remove(index);
                    self.write_spool();
                }
                Err(error) if error.definitely_not_sent => {
                    if let Some(batch) = self.batches.get_mut(index) {
                        batch.state = SpoolState::NotSent;
                        batch.unknown_since_unix = 0;
                    }
                    warn!(
                        batch = %batch_id,
                        bytes = total,
                        users = traffic.len(),
                        error = %error,
                        "panel traffic push was not delivered; keeping it for retry"
                    );
                    self.write_spool();
                    index += 1;
                }
                Err(error) => {
                    if self.idempotency {
                        if let Some(batch) = self.batches.get_mut(index) {
                            batch.state = SpoolState::Unknown;
                        }
                        warn!(
                            batch = %batch_id,
                            bytes = total,
                            users = traffic.len(),
                            error = %error,
                            "panel traffic push outcome is unknown; retrying with the same batch id"
                        );
                        self.write_spool();
                        index += 1;
                    } else {
                        tracing::error!(
                            batch = %batch_id,
                            bytes = total,
                            users = traffic.len(),
                            error = %error,
                            "panel traffic push outcome is unknown; dropping the batch to avoid double billing"
                        );
                        self.batches.remove(index);
                        self.write_spool();
                    }
                }
            }
        }
    }
}

fn build_encryption(node: &NodeInfo) -> Result<Option<Arc<EncryptionServer>>> {
    let scheme = node.encryption.as_deref().unwrap_or("").trim();
    if scheme.is_empty() || scheme == "none" {
        return Ok(None);
    }
    let settings = node
        .encryption_settings
        .as_ref()
        .ok_or_else(|| anyhow!("VLESS encryption requires encryption_settings"))?;
    let mode = if settings.mode.trim().is_empty() {
        "native"
    } else {
        settings.mode.trim()
    };
    let ticket = if settings.ticket.trim().is_empty() {
        "0s"
    } else {
        settings.ticket.trim()
    };
    let mut parts = vec![scheme.to_owned(), mode.to_owned(), ticket.to_owned()];
    if !settings.server_padding.trim().is_empty() {
        parts.push(settings.server_padding.trim().to_owned());
    }
    parts.push(settings.private_key.trim().to_owned());
    info!(mode = %mode, "VLESS encryption enabled");
    EncryptionServer::from_decryption(&parts.join("."))
}

fn apply_users(state: &NodeState, users: &[UserInfo]) -> Vec<UserTraffic> {
    let entries: Vec<(Uuid, UserEntry)> = users
        .iter()
        .filter_map(|user| {
            Uuid::parse_str(&user.uuid).ok().map(|uuid| {
                (
                    uuid,
                    UserEntry {
                        uid: user.id,
                        speed_limit: user.speed_limit,
                        device_limit: user.device_limit,
                    },
                )
            })
        })
        .collect();
    let diff = state.users.apply(entries);
    let mut final_traffic = Vec::new();
    for (uuid, entry) in &diff.removed {
        let (up, down) = state.counters.take(uuid);
        if up + down > 0 {
            final_traffic.push(UserTraffic {
                uid: entry.uid,
                upload: up as i64,
                download: down as i64,
            });
        }
        state.links.close_user(uuid);
        state.counters.remove(uuid);
        state.limiter.remove_user(uuid);
    }
    for (uuid, _) in &diff.modified {
        state.limiter.reset_bucket(uuid);
    }
    for (uuid, _) in &diff.added {
        state.counters.storage(*uuid);
    }
    final_traffic
}

#[allow(clippy::too_many_arguments)]
async fn poll_loop(
    client: PanelClient,
    state: Arc<NodeState>,
    tls: TlsManager,
    acme: AcmeManager,
    node_info: Arc<ArcSwap<NodeInfo>>,
    listener_slot: Arc<Mutex<Option<ListenerSlot>>>,
    runtime: Arc<RuntimeConfig>,
    connection_limit: Option<Arc<Semaphore>>,
    traffic: Arc<Mutex<TrafficReporter>>,
    cancel: CancellationToken,
    node_id: i32,
) {
    loop {
        let interval = node_info
            .load()
            .base()
            .pull_seconds(runtime.default_pull_interval_secs)
            .max(1);
        tokio::select! {
            () = sleep(Duration::from_secs(interval)) => {}
            () = cancel.cancelled() => break,
        }

        match client.get_node_config().await {
            Ok(Some(info)) => {
                if let Err(error) = apply_node_update(
                    &state,
                    &tls,
                    &acme,
                    &node_info,
                    &listener_slot,
                    &runtime,
                    connection_limit.clone(),
                    &cancel,
                    info,
                )
                .await
                {
                    warn!(node_id, error = %error, "apply node config failed");
                    // The failed config was already cached by the panel client;
                    // clear it so the next poll retries the same config.
                    client.invalidate_node_cache();
                }
            }
            Ok(None) => {}
            Err(error) => warn!(node_id, error = %error, "poll node config failed"),
        }

        match client.get_user_list().await {
            Ok(Some(users)) => {
                let removed_traffic = apply_users(&state, &users);
                if !removed_traffic.is_empty() {
                    let mut reporter = traffic.lock().await;
                    reporter.add_all(&removed_traffic);
                    reporter.deliver().await;
                }
                info!(node_id, users = state.users.len(), "user list updated");
            }
            Ok(None) => {}
            Err(error) => warn!(node_id, error = %error, "poll user list failed"),
        }

        if let Some(alive) = client.get_alive_list().await {
            state.limiter.update_alive(alive);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn apply_node_update(
    state: &Arc<NodeState>,
    tls: &TlsManager,
    acme: &AcmeManager,
    node_info: &Arc<ArcSwap<NodeInfo>>,
    listener_slot: &Arc<Mutex<Option<ListenerSlot>>>,
    runtime: &Arc<RuntimeConfig>,
    connection_limit: Option<Arc<Semaphore>>,
    cancel: &CancellationToken,
    info: NodeInfo,
) -> Result<()> {
    let old_info = node_info.load_full();
    ensure_acme_certificate(acme, &info, runtime, tls).await;
    tls.apply_node(&info, runtime)?;
    let prepared = build_encryption(&info).and_then(|encryption| {
        let context = Arc::new(VlessContext {
            state: state.clone(),
            runtime: runtime.clone(),
            node: Arc::new(info.clone()),
            encryption,
            cancel: cancel.clone(),
        });
        let config = Arc::new(ListenerConfig::from_node(
            &info,
            runtime,
            tls,
            connection_limit,
        )?);
        Ok((context, config))
    });
    let result = match prepared {
        Ok((context, config)) => {
            apply_listener_update(node_info, listener_slot, cancel, info, context, config).await
        }
        Err(error) => Err(error),
    };
    if result.is_err()
        && let Err(error) = tls.apply_node(&old_info, runtime)
    {
        warn!(error = %error, "restore previous TLS config failed");
    }
    result
}

async fn apply_listener_update(
    node_info: &Arc<ArcSwap<NodeInfo>>,
    listener_slot: &Arc<Mutex<Option<ListenerSlot>>>,
    cancel: &CancellationToken,
    info: NodeInfo,
    context: Arc<VlessContext>,
    config: Arc<ListenerConfig>,
) -> Result<()> {
    let mut slot = listener_slot.lock().await;
    if let Some(existing) = slot.take() {
        if existing.config.bind == config.bind {
            existing.listener.update(config.clone(), context.clone());
            *slot = Some(ListenerSlot {
                listener: existing.listener,
                token: existing.token,
                config,
                context,
            });
            drop(slot);
            node_info.store(Arc::new(info));
            info!("node config applied");
            return Ok(());
        }
        existing.token.cancel();
        existing.listener.wait().await;
        let listener_token = cancel.child_token();
        match NodeListener::start(config.clone(), context.clone(), listener_token.clone()).await {
            Ok(listener) => {
                *slot = Some(ListenerSlot {
                    listener,
                    token: listener_token,
                    config,
                    context,
                });
                drop(slot);
                node_info.store(Arc::new(info));
                info!("node config applied");
                Ok(())
            }
            Err(error) => {
                warn!(error = %error, "new listener failed; restoring previous listener");
                let token = cancel.child_token();
                match NodeListener::start(
                    existing.config.clone(),
                    existing.context.clone(),
                    token.clone(),
                )
                .await
                {
                    Ok(listener) => {
                        *slot = Some(ListenerSlot {
                            listener,
                            token,
                            config: existing.config,
                            context: existing.context,
                        });
                        warn!("previous listener restored");
                    }
                    Err(restore_error) => {
                        warn!(error = %restore_error, "previous listener restore failed; no listener running");
                    }
                }
                Err(error)
            }
        }
    } else {
        let listener_token = cancel.child_token();
        match NodeListener::start(config.clone(), context.clone(), listener_token.clone()).await {
            Ok(listener) => {
                *slot = Some(ListenerSlot {
                    listener,
                    token: listener_token,
                    config,
                    context,
                });
                drop(slot);
                node_info.store(Arc::new(info));
                info!("node config applied");
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

async fn report_loop(
    client: PanelClient,
    state: Arc<NodeState>,
    node_info: Arc<ArcSwap<NodeInfo>>,
    runtime: Arc<RuntimeConfig>,
    traffic: Arc<Mutex<TrafficReporter>>,
    cancel: CancellationToken,
) {
    let mut pending_online: HashMap<i32, Vec<String>> = HashMap::new();
    loop {
        let interval = node_info
            .load()
            .base()
            .push_seconds(runtime.default_push_interval_secs)
            .max(1);
        tokio::select! {
            () = sleep(Duration::from_secs(interval)) => {}
            () = cancel.cancelled() => break,
        }

        let base = node_info.load().base();
        let min_traffic = base.node_report_min_traffic.max(0) as u64 * 1000;
        let mut totals: HashMap<i32, i64> = HashMap::new();
        {
            let mut reporter = traffic.lock().await;
            for (uuid, user) in state.users.all() {
                let (up, down) = state.counters.take(&uuid);
                if up + down > min_traffic {
                    totals.insert(user.uid, (up + down) as i64);
                    reporter.add(user.uid, up as i64, down as i64);
                } else {
                    state.counters.add_up(uuid, up as usize);
                    state.counters.add_down(uuid, down as usize);
                }
            }
            reporter.deliver().await;
        }

        // Recent connections (possibly already closed) and currently open
        // links both count as online for this window. Open links are reported
        // again every interval, so long-lived connections stay visible.
        let recent = state.limiter.take_online();
        let active = state.links.active_online();
        let min_online = base.device_online_min_traffic.max(0) * 1000;
        let mut data = std::mem::take(&mut pending_online);
        for (uuid, ips) in recent.into_iter().chain(active) {
            let Some(user) = state.users.get(&uuid) else {
                continue;
            };
            let total = totals.get(&user.uid).copied().unwrap_or(0);
            if total < min_online {
                continue;
            }
            let entry = data.entry(user.uid).or_default();
            for ip in ips {
                let text = ip.to_string();
                if !entry.contains(&text) {
                    entry.push(text);
                }
            }
        }
        if !data.is_empty()
            && let Err(error) = client.report_alive(&data).await
        {
            warn!(error = %error, "report online users failed; will retry next interval");
            pending_online = data;
        }
    }
}

async fn acme_loop(
    acme: AcmeManager,
    tls: TlsManager,
    node_info: Arc<ArcSwap<NodeInfo>>,
    runtime: Arc<RuntimeConfig>,
    cancel: CancellationToken,
) {
    let mut consecutive_failures: u32 = 0;
    loop {
        let interval = acme.config().renew_check_interval_secs.max(1);
        tokio::select! {
            () = sleep(Duration::from_secs(interval)) => {}
            () = cancel.cancelled() => break,
        }
        let info = node_info.load_full();
        if !matches!(info.cert_mode(), "dns" | "http") {
            continue;
        }
        let Some(settings) = info.tls_settings() else {
            continue;
        };
        match acme.ensure_certificate(settings, &runtime, &tls).await {
            Ok(()) => {
                consecutive_failures = 0;
                info!("ACME renewal check completed");
            }
            Err(error) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                if consecutive_failures >= 3 {
                    // 连续失败升级为 error，但每 3 轮报一次，避免每小时刷屏。
                    if consecutive_failures.is_multiple_of(3) {
                        tracing::error!(
                            consecutive_failures,
                            error = %error,
                            "ACME renewal keeps failing; certificate may expire"
                        );
                    }
                } else {
                    warn!(
                        consecutive_failures,
                        error = %error,
                        "ACME renewal check failed"
                    );
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_background_tasks(
    client: PanelClient,
    state: Arc<NodeState>,
    tls: TlsManager,
    acme: AcmeManager,
    node_info: Arc<ArcSwap<NodeInfo>>,
    listener_slot: Arc<Mutex<Option<ListenerSlot>>>,
    runtime: Arc<RuntimeConfig>,
    connection_limit: Option<Arc<Semaphore>>,
    traffic: Arc<Mutex<TrafficReporter>>,
    cancel: CancellationToken,
    node_id: i32,
) -> Vec<JoinHandle<()>> {
    let poll = tokio::spawn(poll_loop(
        client.clone(),
        state.clone(),
        tls.clone(),
        acme.clone(),
        node_info.clone(),
        listener_slot,
        runtime.clone(),
        connection_limit,
        traffic.clone(),
        cancel.clone(),
        node_id,
    ));
    let report = tokio::spawn(report_loop(
        client,
        state,
        node_info.clone(),
        runtime.clone(),
        traffic,
        cancel.clone(),
    ));
    let cert = tokio::spawn(cert_loop(
        tls.clone(),
        node_info.clone(),
        runtime.clone(),
        cancel.clone(),
    ));
    let acme_task = tokio::spawn(acme_loop(acme, tls, node_info, runtime, cancel));
    vec![poll, report, cert, acme_task]
}

async fn cert_loop(
    tls: TlsManager,
    node_info: Arc<ArcSwap<NodeInfo>>,
    runtime: Arc<RuntimeConfig>,
    cancel: CancellationToken,
) {
    // 同一天只报一次到期告警，避免日志被每轮检查淹没。
    let mut last_alert_day: i64 = -1;
    loop {
        let interval = runtime.cert_reload_interval_secs.max(1);
        tokio::select! {
            () = sleep(Duration::from_secs(interval)) => {}
            () = cancel.cancelled() => break,
        }
        match tls.reload_if_changed() {
            Ok(true) => info!("TLS certificate reloaded"),
            Ok(false) => {}
            Err(error) => warn!(error = %error, "TLS certificate reload failed"),
        }

        let info = node_info.load_full();
        if !matches!(info.cert_mode(), "file" | "self" | "dns" | "http") {
            continue;
        }
        let Some(settings) = info.tls_settings() else {
            continue;
        };
        if settings.cert_file.trim().is_empty() {
            continue;
        }
        match crate::acme::certificate_days_left(&settings.cert_file) {
            Ok(days) if days <= 14 => {
                let today = unix_day();
                if last_alert_day != today {
                    last_alert_day = today;
                    if days < 0 {
                        tracing::error!(
                            cert_file = %settings.cert_file,
                            "TLS certificate has expired; clients will reject it"
                        );
                    } else if days <= 7 {
                        tracing::error!(
                            days_left = days,
                            cert_file = %settings.cert_file,
                            "TLS certificate expires within 7 days"
                        );
                    } else {
                        warn!(
                            days_left = days,
                            cert_file = %settings.cert_file,
                            "TLS certificate expires within 14 days"
                        );
                    }
                }
            }
            Ok(_) => {}
            Err(error) => {
                // 尚未签发或 cert_mode 无本地文件时会出现，降为 debug 避免噪声。
                tracing::debug!(error = %error, "check certificate expiry failed");
            }
        }
    }
}

fn unix_day() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| (elapsed.as_secs() / 86_400) as i64)
        .unwrap_or(-1)
}

pub struct NodeManager {
    runtimes: Vec<NodeRuntime>,
    runtime: Arc<RuntimeConfig>,
    connection_limit: Option<Arc<Semaphore>>,
}

impl NodeManager {
    pub async fn start(config: &AppConfig) -> Result<Self> {
        let parent = CancellationToken::new();
        let runtime = Arc::new(config.runtime.clone());
        // One semaphore and one ACME manager per process: MaxConnections is a
        // global cap, and ACME account creation/HTTP-01 binding must be serialized
        // across all nodes.
        let connection_limit = (runtime.max_connections > 0)
            .then(|| Arc::new(Semaphore::new(runtime.max_connections)));
        let acme = AcmeManager::new(config.acme.clone())?;
        let mut runtimes = Vec::new();
        for node in &config.nodes {
            match NodeRuntime::start(
                node.clone(),
                runtime.clone(),
                acme.clone(),
                connection_limit.clone(),
                parent.clone(),
            )
            .await
            {
                Ok(node_runtime) => runtimes.push(node_runtime),
                Err(error) => {
                    for runtime in runtimes {
                        runtime.stop().await;
                    }
                    return Err(error);
                }
            }
        }
        Ok(Self {
            runtimes,
            runtime,
            connection_limit,
        })
    }

    /// Apply a changed configuration without rebuilding the whole manager.
    ///
    /// Node set changes and per-node panel connection changes still require a
    /// full rebuild; runtime/ACME changes are applied per node, keeping the
    /// listener and existing connections alive when the bind address is stable.
    pub async fn apply_config(&mut self, config: &AppConfig) -> Result<()> {
        if config.nodes.len() != self.runtimes.len() {
            return Err(anyhow!("node set changed; full restart required"));
        }
        for (index, node) in config.nodes.iter().enumerate() {
            if self.runtimes[index].node_id != node.node_id
                || self.runtimes[index].node_config != *node
            {
                return Err(anyhow!(
                    "node {} connection config changed; restart required",
                    node.node_id
                ));
            }
        }
        let runtime = Arc::new(config.runtime.clone());
        let acme = AcmeManager::new(config.acme.clone())?;
        let connection_limit = if runtime.max_connections == self.runtime.max_connections {
            self.connection_limit.clone()
        } else {
            (runtime.max_connections > 0).then(|| Arc::new(Semaphore::new(runtime.max_connections)))
        };
        for (index, node) in config.nodes.iter().enumerate() {
            self.runtimes[index]
                .apply_config(
                    node.clone(),
                    runtime.clone(),
                    acme.clone(),
                    connection_limit.clone(),
                )
                .await?;
        }
        self.runtime = runtime;
        self.connection_limit = connection_limit;
        Ok(())
    }

    pub async fn stop(self) {
        for runtime in self.runtimes {
            runtime.stop().await;
        }
    }

    pub fn len(&self) -> usize {
        self.runtimes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_user_traffic_is_returned() {
        let state = NodeState::new();
        let uuid = Uuid::new_v4();
        let user = UserInfo {
            id: 7,
            uuid: uuid.to_string(),
            speed_limit: 0,
            device_limit: 0,
        };
        assert!(apply_users(&state, &[user]).is_empty());
        state.counters.add_up(uuid, 10);
        state.counters.add_down(uuid, 20);
        let traffic = apply_users(&state, &[]);
        assert_eq!(traffic.len(), 1);
        assert_eq!(traffic[0].uid, 7);
        assert_eq!(traffic[0].upload, 10);
        assert_eq!(traffic[0].download, 20);
        assert_eq!(state.counters.take(&uuid), (0, 0));
    }

    fn test_client() -> PanelClient {
        PanelClient::new(
            &NodeConfig {
                api_host: "http://127.0.0.1:1".to_owned(),
                node_id: 1,
                api_key: "test".to_owned(),
                timeout: 1,
                retry_count: Some(0),
            },
            &RuntimeConfig::default(),
        )
        .unwrap()
    }

    #[test]
    fn chunking_splits_by_user_limit() {
        let mut reporter = TrafficReporter::new(test_client(), true, "");
        for uid in 0..(MAX_BATCH_USERS as i32 * 2 + 1) {
            reporter.add(uid, 1, 1);
        }
        reporter.enqueue_pending();
        assert_eq!(reporter.batches.len(), 3);
        assert!(
            reporter
                .batches
                .iter()
                .all(|batch| batch.entries.len() <= MAX_BATCH_USERS)
        );
        assert_eq!(
            reporter.pending_bytes(),
            (MAX_BATCH_USERS as i64 * 2 + 1) * 2
        );
    }

    #[test]
    fn spool_keeps_not_sent_and_drops_stale_unknown() {
        let path = std::env::temp_dir().join(format!("v2node-spool-{}.json", Uuid::new_v4()));
        let spool = SpoolFile {
            version: SPOOL_VERSION,
            batches: vec![
                SpoolBatch {
                    batch_id: "keep".to_owned(),
                    entries: vec![SpoolEntry {
                        uid: 1,
                        upload: 1,
                        download: 1,
                    }],
                    state: SpoolState::NotSent,
                    unknown_since_unix: 0,
                },
                SpoolBatch {
                    batch_id: "drop".to_owned(),
                    entries: vec![SpoolEntry {
                        uid: 2,
                        upload: 1,
                        download: 1,
                    }],
                    state: SpoolState::Unknown,
                    unknown_since_unix: 1,
                },
            ],
        };
        fs::write(&path, serde_json::to_vec(&spool).unwrap()).unwrap();
        let mut reporter = TrafficReporter::new(test_client(), true, path.to_str().unwrap());
        reporter.load_spool();
        assert_eq!(reporter.batches.len(), 1);
        assert_eq!(reporter.batches[0].batch_id, "keep");
        assert!(path.exists());
        let _ = fs::remove_file(path);
    }
}
