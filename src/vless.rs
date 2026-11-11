use anyhow::{Context, Result, anyhow, bail};
use std::{
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{Arc, Mutex, atomic::Ordering},
    task::{Context as TaskContext, Poll},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpStream, UdpSocket, lookup_host},
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::debug;
use uuid::Uuid;

use crate::{
    config::RuntimeConfig,
    encryption::EncryptionServer,
    panel::NodeInfo,
    state::{NodeState, RateBucket, TrafficStorage, UserEntry},
};

pub struct VlessContext {
    pub state: Arc<NodeState>,
    pub runtime: Arc<RuntimeConfig>,
    #[allow(dead_code)]
    pub node: Arc<NodeInfo>,
    pub encryption: Option<Arc<EncryptionServer>>,
    pub cancel: CancellationToken,
}

pub async fn serve<S>(
    stream: S,
    early_data: Option<Vec<u8>>,
    client_ip: IpAddr,
    context: Arc<VlessContext>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let stream = PrefixedIo::new(stream, early_data.unwrap_or_default());
    if let Some(encryption) = context.encryption.clone() {
        let handshake_timeout =
            Duration::from_secs(context.runtime.vless_handshake_timeout_secs.max(1));
        let (app, pump) = timeout(handshake_timeout, encryption.handshake(stream))
            .await
            .map_err(|_| anyhow!("VLESS encryption handshake timeout"))?
            .context("VLESS encryption handshake")?;
        let result = serve_plain(app, client_ip, context.clone()).await;
        let drain = Duration::from_secs(context.runtime.drain_timeout_secs.max(1));
        let mut pump = pump;
        if timeout(drain, &mut pump).await.is_err() {
            pump.abort();
        }
        result
    } else {
        serve_plain(stream, client_ip, context).await
    }
}

async fn serve_plain<S>(stream: S, client_ip: IpAddr, context: Arc<VlessContext>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let handshake_timeout =
        Duration::from_secs(context.runtime.vless_handshake_timeout_secs.max(1));
    let (uuid, user, command, port, address) =
        timeout(handshake_timeout, read_vless_request(&mut reader, &context))
            .await
            .map_err(|_| anyhow!("VLESS handshake timeout"))??;

    if !context
        .state
        .limiter
        .check_and_register(uuid, user.uid, client_ip, user.device_limit)
    {
        bail!("device limit exceeded for user {}", user.uid);
    }

    writer.write_all(&[0u8, 0u8]).await?;
    writer.flush().await?;
    debug!(
        user = user.uid,
        client_ip = %client_ip,
        command,
        %address,
        port,
        "VLESS request accepted"
    );

    let link_token = context.cancel.child_token();
    let _registration = context
        .state
        .links
        .register(uuid, link_token.clone(), client_ip);
    let bucket = context.state.limiter.bucket_for(uuid, user.speed_limit);

    match command {
        1 => {
            relay_tcp(
                &mut reader,
                &mut writer,
                uuid,
                address,
                port,
                &context,
                bucket,
                link_token,
            )
            .await
        }
        2 => {
            relay_udp(
                &mut reader,
                &mut writer,
                uuid,
                address,
                port,
                &context,
                bucket,
                link_token,
            )
            .await
        }
        other => bail!("unsupported VLESS command {other}"),
    }
}

impl std::fmt::Display for TargetAddress {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ip(ip) => write!(formatter, "{ip}"),
            Self::Domain(domain) => write!(formatter, "{domain}"),
        }
    }
}

enum TargetAddress {
    Ip(IpAddr),
    Domain(String),
}

async fn read_vless_request<R: AsyncRead + Unpin>(
    reader: &mut R,
    context: &VlessContext,
) -> Result<(Uuid, UserEntry, u8, u16, TargetAddress)> {
    let version = reader.read_u8().await.context("read VLESS version")?;
    if version != 0 {
        bail!("unsupported VLESS version {version}");
    }
    let mut id = [0u8; 16];
    reader
        .read_exact(&mut id)
        .await
        .context("read VLESS user id")?;
    let uuid = Uuid::from_bytes(id);
    let user = context
        .state
        .users
        .get(&uuid)
        .ok_or_else(|| anyhow!("invalid VLESS user"))?;
    let addon_len = reader.read_u8().await.context("read VLESS addons length")?;
    if addon_len != 0 {
        bail!("VLESS addons/flow are not supported in M1");
    }
    let command = reader.read_u8().await.context("read VLESS command")?;
    let port = reader.read_u16().await.context("read VLESS port")?;
    let address = read_address(reader).await?;
    Ok((uuid, user, command, port, address))
}

async fn read_address<R: AsyncRead + Unpin>(reader: &mut R) -> Result<TargetAddress> {
    let kind = reader.read_u8().await.context("read VLESS address type")?;
    match kind {
        1 => {
            let mut bytes = [0u8; 4];
            reader.read_exact(&mut bytes).await?;
            Ok(TargetAddress::Ip(IpAddr::from(bytes)))
        }
        2 => {
            let length = reader.read_u8().await? as usize;
            if length == 0 {
                bail!("empty VLESS domain");
            }
            let mut bytes = vec![0u8; length];
            reader.read_exact(&mut bytes).await?;
            Ok(TargetAddress::Domain(String::from_utf8(bytes)?))
        }
        3 => {
            let mut bytes = [0u8; 16];
            reader.read_exact(&mut bytes).await?;
            Ok(TargetAddress::Ip(IpAddr::from(bytes)))
        }
        other => bail!("unsupported VLESS address type {other}"),
    }
}

async fn resolve_targets(
    address: &TargetAddress,
    port: u16,
    context: &VlessContext,
) -> Result<Vec<SocketAddr>> {
    let candidates: Vec<SocketAddr> = match address {
        TargetAddress::Ip(ip) => vec![SocketAddr::new(*ip, port)],
        TargetAddress::Domain(domain) => {
            let addrs = timeout(
                Duration::from_secs(context.runtime.tcp_connect_timeout_secs),
                lookup_host((domain.as_str(), port)),
            )
            .await
            .map_err(|_| anyhow!("DNS lookup timeout for {domain}"))??;
            addrs.collect()
        }
    };
    let targets: Vec<SocketAddr> = candidates
        .into_iter()
        .filter(|target| context.runtime.allow_private_targets || is_public_target(target.ip()))
        .collect();
    if targets.is_empty() {
        bail!("target {address}:{port} is private or unavailable");
    }
    Ok(targets)
}

fn is_public_target(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_unspecified()
                || v4.is_documentation()
                || v4.octets()[0] == 0
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_target(IpAddr::V4(v4));
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_multicast())
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn relay_tcp<R, W>(
    reader: &mut R,
    writer: &mut W,
    uuid: Uuid,
    address: TargetAddress,
    port: u16,
    context: &Arc<VlessContext>,
    bucket: Option<Arc<RateBucket>>,
    token: CancellationToken,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let targets = resolve_targets(&address, port, context.as_ref()).await?;
    let mut stream = None;
    let mut last_error = None;
    for target in targets {
        match timeout(
            Duration::from_secs(context.runtime.tcp_connect_timeout_secs),
            TcpStream::connect(target),
        )
        .await
        {
            Ok(Ok(connected)) => {
                stream = Some(connected);
                break;
            }
            Ok(Err(error)) => {
                last_error = Some(anyhow!("connect {target}: {error}"));
            }
            Err(_) => {
                last_error = Some(anyhow!("TCP connect timeout for {target}"));
            }
        }
    }
    let stream = stream.ok_or_else(|| {
        last_error.unwrap_or_else(|| anyhow!("no usable address for {address}:{port}"))
    })?;
    stream.set_nodelay(true).ok();
    let (mut outbound_reader, mut outbound_writer) = stream.into_split();
    let up_storage = context.state.counters.storage(uuid);
    let down_storage = context.state.counters.storage(uuid);
    let activity = Arc::new(Mutex::new(Instant::now()));
    let up = pump(
        reader,
        &mut outbound_writer,
        &up_storage,
        Direction::Up,
        bucket.as_deref(),
        &token,
        context.runtime.relay_buffer_size,
        &activity,
    );
    let down = pump(
        &mut outbound_reader,
        writer,
        &down_storage,
        Direction::Down,
        bucket.as_deref(),
        &token,
        context.runtime.relay_buffer_size,
        &activity,
    );
    let idle_secs = context.runtime.connection_idle_timeout_secs;
    let watchdog_activity = activity.clone();
    let watchdog_token = token.clone();
    let watchdog = (idle_secs > 0).then(|| {
        tokio::spawn(async move {
            let interval = Duration::from_secs((idle_secs / 2).clamp(1, 30));
            loop {
                tokio::select! {
                    () = watchdog_token.cancelled() => break,
                    () = sleep(interval) => {
                        let idle = watchdog_activity
                            .lock()
                            .map(|last| last.elapsed())
                            .unwrap_or_default();
                        if idle >= Duration::from_secs(idle_secs) {
                            watchdog_token.cancel();
                            break;
                        }
                    }
                }
            }
        })
    });
    tokio::pin!(up, down);
    let drain = Duration::from_secs(context.runtime.drain_timeout_secs.max(1));
    let first = tokio::select! {
        result = &mut up => Some((true, result)),
        result = &mut down => Some((false, result)),
        () = token.cancelled() => None,
    };
    let result = match first {
        None => Ok(()),
        Some((true, Ok(PumpEnd::Eof))) => {
            let _ = timeout(drain, &mut down).await;
            Ok(())
        }
        Some((false, Ok(PumpEnd::Eof))) => {
            let _ = timeout(drain, &mut up).await;
            Ok(())
        }
        Some((_, Ok(_))) => Ok(()),
        Some((_, Err(error))) => Err(error),
    };
    if let Some(watchdog) = watchdog {
        watchdog.abort();
    }
    result
}

#[derive(Clone, Copy)]
enum Direction {
    Up,
    Down,
}

enum PumpEnd {
    Eof,
    Cancelled,
}

#[allow(clippy::too_many_arguments)]
async fn pump<R, W>(
    reader: &mut R,
    writer: &mut W,
    storage: &TrafficStorage,
    direction: Direction,
    bucket: Option<&RateBucket>,
    token: &CancellationToken,
    buffer_size: usize,
    activity: &Arc<Mutex<Instant>>,
) -> Result<PumpEnd>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0u8; buffer_size.max(1024)];
    loop {
        let read = tokio::select! {
            () = token.cancelled() => return Ok(PumpEnd::Cancelled),
            result = reader.read(&mut buffer) => {
                match result {
                    Ok(0) => {
                        let _ = writer.shutdown().await;
                        return Ok(PumpEnd::Eof);
                    }
                    Ok(n) => n,
                    Err(error) => return Err(error.into()),
                }
            }
        };
        if let Ok(mut last) = activity.lock() {
            *last = Instant::now();
        }
        if let Some(bucket) = bucket {
            bucket.acquire(read).await;
        }
        match direction {
            Direction::Up => storage.up.fetch_add(read as u64, Ordering::Relaxed),
            Direction::Down => storage.down.fetch_add(read as u64, Ordering::Relaxed),
        };
        tokio::select! {
            () = token.cancelled() => return Ok(PumpEnd::Cancelled),
            result = writer.write_all(&buffer[..read]) => result?,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn relay_udp<R, W>(
    reader: &mut R,
    writer: &mut W,
    uuid: Uuid,
    address: TargetAddress,
    port: u16,
    context: &Arc<VlessContext>,
    bucket: Option<Arc<RateBucket>>,
    token: CancellationToken,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let targets = resolve_targets(&address, port, context.as_ref()).await?;
    let target = targets
        .iter()
        .find(|target| target.is_ipv4())
        .or_else(|| targets.first())
        .copied()
        .ok_or_else(|| anyhow!("no usable address for {address}:{port}"))?;
    let bind = if target.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind).await.context("bind UDP socket")?;
    socket
        .connect(target)
        .await
        .with_context(|| format!("connect UDP {target}"))?;
    debug!(%target, "udp relay started");
    let socket = Arc::new(socket);
    let storage = context.state.counters.storage(uuid);
    let activity = Arc::new(Mutex::new(Instant::now()));
    let up = udp_up(
        reader,
        socket.clone(),
        &storage,
        bucket.as_deref(),
        &token,
        &activity,
    );
    let down = udp_down(
        socket,
        writer,
        &storage,
        bucket.as_deref(),
        &token,
        &activity,
    );
    let idle_secs = context.runtime.udp_idle_timeout_secs.max(1);
    let watchdog_activity = activity.clone();
    let watchdog_token = token.clone();
    let watchdog = tokio::spawn(async move {
        let interval = Duration::from_secs((idle_secs / 2).clamp(1, 30));
        loop {
            tokio::select! {
                () = watchdog_token.cancelled() => break,
                () = sleep(interval) => {
                    let idle = watchdog_activity
                        .lock()
                        .map(|last| last.elapsed())
                        .unwrap_or_default();
                    if idle >= Duration::from_secs(idle_secs) {
                        watchdog_token.cancel();
                        break;
                    }
                }
            }
        }
    });
    let result = tokio::select! {
        result = up => result,
        result = down => result,
        () = token.cancelled() => Ok(()),
    };
    watchdog.abort();
    result
}

async fn udp_up<R>(
    reader: &mut R,
    socket: Arc<UdpSocket>,
    storage: &TrafficStorage,
    bucket: Option<&RateBucket>,
    token: &CancellationToken,
    activity: &Arc<Mutex<Instant>>,
) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    loop {
        let length = tokio::select! {
            () = token.cancelled() => return Ok(()),
            result = reader.read_u16() => result? as usize,
        };
        if length == 0 {
            continue;
        }
        debug!(length, "udp up packet");
        let mut payload = vec![0u8; length];
        tokio::select! {
            () = token.cancelled() => return Ok(()),
            result = reader.read_exact(&mut payload) => {
                result?;
            }
        }
        if let Ok(mut last) = activity.lock() {
            *last = Instant::now();
        }
        if let Some(bucket) = bucket {
            bucket.acquire(length).await;
        }
        storage.up.fetch_add(length as u64, Ordering::Relaxed);
        tokio::select! {
            () = token.cancelled() => return Ok(()),
            result = socket.send(&payload) => {
                result?;
            }
        }
        debug!(length, "udp up sent");
    }
}

async fn udp_down<W>(
    socket: Arc<UdpSocket>,
    writer: &mut W,
    storage: &TrafficStorage,
    bucket: Option<&RateBucket>,
    token: &CancellationToken,
    activity: &Arc<Mutex<Instant>>,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0u8; 65535];
    loop {
        let read = tokio::select! {
            () = token.cancelled() => return Ok(()),
            result = socket.recv(&mut buffer) => result?,
        };
        if read == 0 {
            continue;
        }
        if let Ok(mut last) = activity.lock() {
            *last = Instant::now();
        }
        if let Some(bucket) = bucket {
            bucket.acquire(read).await;
        }
        debug!(read, "udp down packet");
        storage.down.fetch_add(read as u64, Ordering::Relaxed);
        let length = (read.min(u16::MAX as usize) as u16).to_be_bytes();
        tokio::select! {
            () = token.cancelled() => return Ok(()),
            result = async {
                writer.write_all(&length).await?;
                writer.write_all(&buffer[..read]).await?;
                writer.flush().await
            } => result?,
        }
    }
}

struct PrefixedIo<T> {
    prefix: Vec<u8>,
    offset: usize,
    inner: T,
}

impl<T> PrefixedIo<T> {
    fn new(inner: T, prefix: Vec<u8>) -> Self {
        Self {
            prefix,
            offset: 0,
            inner,
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for PrefixedIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.offset < self.prefix.len() {
            let remaining = &self.prefix[self.offset..];
            let length = remaining.len().min(buffer.remaining());
            buffer.put_slice(&remaining[..length]);
            self.offset += length;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buffer)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for PrefixedIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::state::{NodeState, UserEntry};
    use base64::Engine;
    use tokio::net::{TcpListener, UdpSocket};

    fn context(uuid: Uuid) -> Arc<VlessContext> {
        let state = Arc::new(NodeState::new());
        state.users.apply(vec![(
            uuid,
            UserEntry {
                uid: 1,
                speed_limit: 0,
                device_limit: 0,
            },
        )]);
        let mut runtime = RuntimeConfig::default();
        runtime.allow_private_targets = true;
        Arc::new(VlessContext {
            state,
            runtime: Arc::new(runtime),
            node: Arc::new(NodeInfo::default()),
            encryption: None,
            cancel: CancellationToken::new(),
        })
    }

    fn ipv4_request(
        uuid: Uuid,
        command: u8,
        port: u16,
        target: std::net::Ipv4Addr,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut data = Vec::new();
        data.push(0);
        data.extend_from_slice(uuid.as_bytes());
        data.push(0);
        data.push(command);
        data.extend_from_slice(&port.to_be_bytes());
        data.push(1);
        data.extend_from_slice(&target.octets());
        data.extend_from_slice(payload);
        data
    }

    #[test]
    fn private_targets_are_classified() {
        for blocked in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public_target(blocked.parse().unwrap()), "{blocked}");
        }
        for allowed in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(is_public_target(allowed.parse().unwrap()), "{allowed}");
        }
    }

    #[tokio::test]
    async fn handshake_times_out_without_header() {
        let uuid = Uuid::new_v4();
        let state = Arc::new(NodeState::new());
        state.users.apply(vec![(
            uuid,
            UserEntry {
                uid: 1,
                speed_limit: 0,
                device_limit: 0,
            },
        )]);
        let mut runtime = RuntimeConfig::default();
        runtime.vless_handshake_timeout_secs = 1;
        let context = Arc::new(VlessContext {
            state,
            runtime: Arc::new(runtime),
            node: Arc::new(NodeInfo::default()),
            encryption: None,
            cancel: CancellationToken::new(),
        });
        let (_client, server) = tokio::io::duplex(1024);
        let started = std::time::Instant::now();
        let error = serve(server, None, "203.0.113.7".parse().unwrap(), context)
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(error.to_string().contains("handshake timeout"));
    }

    #[tokio::test]
    async fn private_target_is_rejected_by_default() {
        let uuid = Uuid::new_v4();
        let state = Arc::new(NodeState::new());
        state.users.apply(vec![(
            uuid,
            UserEntry {
                uid: 1,
                speed_limit: 0,
                device_limit: 0,
            },
        )]);
        let context = Arc::new(VlessContext {
            state,
            runtime: Arc::new(RuntimeConfig::default()),
            node: Arc::new(NodeInfo::default()),
            encryption: None,
            cancel: CancellationToken::new(),
        });
        let (mut client, server) = tokio::io::duplex(65536);
        let task = tokio::spawn(async move {
            serve(server, None, "203.0.113.7".parse().unwrap(), context).await
        });
        let request = ipv4_request(uuid, 1, 9, std::net::Ipv4Addr::LOCALHOST, b"");
        client.write_all(&request).await.unwrap();
        let error = task.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("private"), "{error}");
    }

    #[tokio::test]
    async fn udp_payload_read_times_out() {
        let uuid = Uuid::new_v4();
        let state = Arc::new(NodeState::new());
        state.users.apply(vec![(
            uuid,
            UserEntry {
                uid: 1,
                speed_limit: 0,
                device_limit: 0,
            },
        )]);
        let mut runtime = RuntimeConfig::default();
        runtime.allow_private_targets = true;
        runtime.udp_idle_timeout_secs = 1;
        let context = Arc::new(VlessContext {
            state,
            runtime: Arc::new(runtime),
            node: Arc::new(NodeInfo::default()),
            encryption: None,
            cancel: CancellationToken::new(),
        });
        let (mut client, server) = tokio::io::duplex(65536);
        let task = tokio::spawn(async move {
            serve(server, None, "203.0.113.7".parse().unwrap(), context).await
        });
        let request = ipv4_request(uuid, 2, 9, std::net::Ipv4Addr::LOCALHOST, b"");
        client.write_all(&request).await.unwrap();
        client.write_all(&(5u16).to_be_bytes()).await.unwrap();
        let started = std::time::Instant::now();
        let result = timeout(Duration::from_secs(3), task).await;
        assert!(result.is_ok(), "udp payload read did not time out");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn malformed_vless_requests_do_not_hang_or_panic() {
        let uuid = Uuid::new_v4();
        let state = Arc::new(NodeState::new());
        state.users.apply(vec![(
            uuid,
            UserEntry {
                uid: 1,
                speed_limit: 0,
                device_limit: 0,
            },
        )]);
        let mut runtime = RuntimeConfig::default();
        runtime.allow_private_targets = true;
        runtime.vless_handshake_timeout_secs = 1;
        let context = Arc::new(VlessContext {
            state,
            runtime: Arc::new(runtime),
            node: Arc::new(NodeInfo::default()),
            encryption: None,
            cancel: CancellationToken::new(),
        });
        let mut cases: Vec<Vec<u8>> = Vec::new();
        cases.push(vec![1]); // bad version
        cases.push(vec![0, 1, 2]); // truncated uuid
        let mut bad_user = vec![0];
        bad_user.extend_from_slice(&[9u8; 16]);
        cases.push(bad_user); // unknown user
        let mut addons = vec![0];
        addons.extend_from_slice(uuid.as_bytes());
        addons.push(1); // addon length
        cases.push(addons);
        let mut bad_command = vec![0];
        bad_command.extend_from_slice(uuid.as_bytes());
        bad_command.extend_from_slice(&[0, 9]);
        cases.push(bad_command); // unsupported command
        let mut bad_address = vec![0];
        bad_address.extend_from_slice(uuid.as_bytes());
        bad_address.extend_from_slice(&[0, 1, 0, 80, 4]);
        cases.push(bad_address); // unsupported address type
        let mut empty_domain = vec![0];
        empty_domain.extend_from_slice(uuid.as_bytes());
        empty_domain.extend_from_slice(&[0, 1, 0, 80, 2, 0]);
        cases.push(empty_domain); // empty domain
        let mut truncated_domain = vec![0];
        truncated_domain.extend_from_slice(uuid.as_bytes());
        truncated_domain.extend_from_slice(&[0, 1, 0, 80, 2, 8]);
        truncated_domain.extend_from_slice(b"abc");
        cases.push(truncated_domain); // truncated domain
        for bytes in cases {
            let (mut client, server) = tokio::io::duplex(4096);
            let task = tokio::spawn({
                let context = context.clone();
                async move { serve(server, None, "203.0.113.7".parse().unwrap(), context).await }
            });
            client.write_all(&bytes).await.unwrap();
            let result = timeout(Duration::from_secs(2), task).await;
            assert!(result.is_ok(), "malformed request hung: {bytes:?}");
        }
    }

    #[tokio::test]
    async fn tcp_relay_echo() {
        let uuid = Uuid::new_v4();
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = echo.accept().await.unwrap();
            let mut buffer = [0u8; 4096];
            loop {
                let read = stream.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                stream.write_all(&buffer[..read]).await.unwrap();
            }
        });

        let (client, server) = tokio::io::duplex(65536);
        let context = context(uuid);
        let task = tokio::spawn(async move {
            serve(server, None, "203.0.113.7".parse().unwrap(), context).await
        });
        let (mut client_reader, mut client_writer) = tokio::io::split(client);
        let request = ipv4_request(uuid, 1, port, std::net::Ipv4Addr::LOCALHOST, b"hello");
        client_writer.write_all(&request).await.unwrap();
        let mut response = [0u8; 2];
        client_reader.read_exact(&mut response).await.unwrap();
        assert_eq!(response, [0, 0]);
        let mut payload = [0u8; 5];
        client_reader.read_exact(&mut payload).await.unwrap();
        assert_eq!(&payload, b"hello");
        drop(client_writer);
        let _ = timeout(Duration::from_secs(2), task).await;
    }

    #[tokio::test]
    async fn tcp_half_close_drains_response() {
        let uuid = Uuid::new_v4();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 1024];
            while stream.read(&mut buffer).await.unwrap() != 0 {}
            stream.write_all(b"bye").await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let state = Arc::new(NodeState::new());
        state.users.apply(vec![(
            uuid,
            UserEntry {
                uid: 1,
                speed_limit: 0,
                device_limit: 0,
            },
        )]);
        let mut runtime = RuntimeConfig::default();
        runtime.allow_private_targets = true;
        runtime.drain_timeout_secs = 2;
        let context = Arc::new(VlessContext {
            state,
            runtime: Arc::new(runtime),
            node: Arc::new(NodeInfo::default()),
            encryption: None,
            cancel: CancellationToken::new(),
        });
        let (client, server) = tokio::io::duplex(65536);
        let task = tokio::spawn(async move {
            serve(server, None, "203.0.113.7".parse().unwrap(), context).await
        });
        let (mut reader, mut writer) = tokio::io::split(client);
        let request = ipv4_request(uuid, 1, port, std::net::Ipv4Addr::LOCALHOST, b"hello");
        writer.write_all(&request).await.unwrap();
        writer.shutdown().await.unwrap();
        let mut header = [0u8; 2];
        reader.read_exact(&mut header).await.unwrap();
        assert_eq!(header, [0, 0]);
        let mut response = [0u8; 3];
        timeout(Duration::from_secs(3), reader.read_exact(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&response, b"bye");
        let _ = task.await;
    }

    #[tokio::test]
    async fn one_way_download_does_not_trip_idle_timeout() {
        let uuid = Uuid::new_v4();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            for _ in 0..20 {
                if stream.write_all(b"x").await.is_err() {
                    return;
                }
                sleep(Duration::from_millis(100)).await;
            }
            let _ = stream.shutdown().await;
        });
        let state = Arc::new(NodeState::new());
        state.users.apply(vec![(
            uuid,
            UserEntry {
                uid: 1,
                speed_limit: 0,
                device_limit: 0,
            },
        )]);
        let mut runtime = RuntimeConfig::default();
        runtime.allow_private_targets = true;
        runtime.connection_idle_timeout_secs = 1;
        runtime.drain_timeout_secs = 1;
        let context = Arc::new(VlessContext {
            state,
            runtime: Arc::new(runtime),
            node: Arc::new(NodeInfo::default()),
            encryption: None,
            cancel: CancellationToken::new(),
        });
        let (client, server) = tokio::io::duplex(65536);
        let task = tokio::spawn(async move {
            serve(server, None, "203.0.113.7".parse().unwrap(), context).await
        });
        let (mut reader, mut writer) = tokio::io::split(client);
        let request = ipv4_request(uuid, 1, port, std::net::Ipv4Addr::LOCALHOST, b"");
        writer.write_all(&request).await.unwrap();
        let mut header = [0u8; 2];
        reader.read_exact(&mut header).await.unwrap();
        assert_eq!(header, [0, 0]);
        let started = std::time::Instant::now();
        let mut received = 0;
        let result = timeout(Duration::from_secs(4), async {
            loop {
                let mut byte = [0u8; 1];
                reader.read_exact(&mut byte).await.unwrap();
                received += 1;
                if received == 20 {
                    break;
                }
            }
        })
        .await;
        assert!(result.is_ok(), "download was cut short at {received} bytes");
        assert_eq!(received, 20);
        assert!(started.elapsed() >= Duration::from_millis(1500));
        drop(writer);
        let _ = task.await;
    }

    #[tokio::test]
    async fn idle_timeout_zero_keeps_idle_connection() {
        let uuid = Uuid::new_v4();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            sleep(Duration::from_millis(1500)).await;
            let _ = stream.write_all(b"ok").await;
        });
        let state = Arc::new(NodeState::new());
        state.users.apply(vec![(
            uuid,
            UserEntry {
                uid: 1,
                speed_limit: 0,
                device_limit: 0,
            },
        )]);
        let mut runtime = RuntimeConfig::default();
        runtime.allow_private_targets = true;
        runtime.connection_idle_timeout_secs = 0;
        let context = Arc::new(VlessContext {
            state,
            runtime: Arc::new(runtime),
            node: Arc::new(NodeInfo::default()),
            encryption: None,
            cancel: CancellationToken::new(),
        });
        let (client, server) = tokio::io::duplex(65536);
        tokio::spawn(
            async move { serve(server, None, "203.0.113.7".parse().unwrap(), context).await },
        );
        let (mut reader, mut writer) = tokio::io::split(client);
        let request = ipv4_request(uuid, 1, port, std::net::Ipv4Addr::LOCALHOST, b"");
        writer.write_all(&request).await.unwrap();
        let mut header = [0u8; 2];
        reader.read_exact(&mut header).await.unwrap();
        assert_eq!(header, [0, 0]);
        let mut response = [0u8; 2];
        timeout(Duration::from_secs(4), reader.read_exact(&mut response))
            .await
            .expect("idle connection was closed with idle timeout disabled")
            .unwrap();
        assert_eq!(&response, b"ok");
        drop(writer);
    }

    #[tokio::test]
    async fn udp_one_way_download_does_not_trip_idle_timeout() {
        let uuid = Uuid::new_v4();
        let target = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = target.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut buffer = [0u8; 64];
            let (_, peer) = target.recv_from(&mut buffer).await.unwrap();
            for _ in 0..20 {
                if target.send_to(b"y", peer).await.is_err() {
                    return;
                }
                sleep(Duration::from_millis(100)).await;
            }
        });
        let state = Arc::new(NodeState::new());
        state.users.apply(vec![(
            uuid,
            UserEntry {
                uid: 1,
                speed_limit: 0,
                device_limit: 0,
            },
        )]);
        let mut runtime = RuntimeConfig::default();
        runtime.allow_private_targets = true;
        runtime.udp_idle_timeout_secs = 1;
        let context = Arc::new(VlessContext {
            state,
            runtime: Arc::new(runtime),
            node: Arc::new(NodeInfo::default()),
            encryption: None,
            cancel: CancellationToken::new(),
        });
        let (client, server) = tokio::io::duplex(65536);
        let task = tokio::spawn(async move {
            serve(server, None, "203.0.113.7".parse().unwrap(), context).await
        });
        let (mut reader, mut writer) = tokio::io::split(client);
        let request = ipv4_request(uuid, 2, port, std::net::Ipv4Addr::LOCALHOST, b"");
        writer.write_all(&request).await.unwrap();
        let mut header = [0u8; 2];
        reader.read_exact(&mut header).await.unwrap();
        assert_eq!(header, [0, 0]);
        writer.write_all(&(5u16).to_be_bytes()).await.unwrap();
        writer.write_all(b"start").await.unwrap();
        let started = std::time::Instant::now();
        let mut received = 0;
        let result = timeout(Duration::from_secs(4), async {
            loop {
                let mut length = [0u8; 2];
                reader.read_exact(&mut length).await.unwrap();
                let length = u16::from_be_bytes(length) as usize;
                let mut payload = vec![0u8; length];
                reader.read_exact(&mut payload).await.unwrap();
                received += 1;
                if received == 20 {
                    break;
                }
            }
        })
        .await;
        assert!(result.is_ok(), "udp download cut short at {received}");
        assert_eq!(received, 20);
        assert!(started.elapsed() >= Duration::from_millis(1500));
        drop(writer);
        let _ = task.await;
    }

    #[tokio::test]
    async fn encrypted_tcp_half_close_drains_response() {
        let uuid = Uuid::new_v4();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 1024];
            while stream.read(&mut buffer).await.unwrap() != 0 {}
            stream.write_all(b"bye").await.unwrap();
            stream.shutdown().await.unwrap();
        });

        let server_secret = x25519_dalek::StaticSecret::random();
        let server_public = x25519_dalek::PublicKey::from(&server_secret);
        let private =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(server_secret.to_bytes());
        let server =
            EncryptionServer::from_decryption(&format!("mlkem768x25519plus.native.0s.{private}"))
                .unwrap()
                .unwrap();
        let public =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(server_public.to_bytes());
        let client_config = crate::encryption::ClientEncryptionConfig::parse(&format!(
            "mlkem768x25519plus.native.1rtt.{public}"
        ))
        .unwrap();

        let state = Arc::new(NodeState::new());
        state.users.apply(vec![(
            uuid,
            UserEntry {
                uid: 1,
                speed_limit: 0,
                device_limit: 0,
            },
        )]);
        let mut runtime = RuntimeConfig::default();
        runtime.allow_private_targets = true;
        runtime.drain_timeout_secs = 2;
        let context = Arc::new(VlessContext {
            state,
            runtime: Arc::new(runtime),
            node: Arc::new(NodeInfo::default()),
            encryption: Some(server),
            cancel: CancellationToken::new(),
        });

        let (client_net, server_net) = tokio::io::duplex(65536);
        let server_task = tokio::spawn({
            let context = context.clone();
            async move { serve(server_net, None, "203.0.113.7".parse().unwrap(), context).await }
        });
        let (client_app, client_pump) =
            crate::encryption::client_handshake(client_net, &client_config)
                .await
                .unwrap();
        let (mut reader, mut writer) = tokio::io::split(client_app);
        let request = ipv4_request(uuid, 1, port, std::net::Ipv4Addr::LOCALHOST, b"hello");
        writer.write_all(&request).await.unwrap();
        writer.shutdown().await.unwrap();
        let mut header = [0u8; 2];
        timeout(Duration::from_secs(5), reader.read_exact(&mut header))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(header, [0, 0]);
        let mut response = [0u8; 3];
        timeout(Duration::from_secs(5), reader.read_exact(&mut response))
            .await
            .expect("encrypted half-close response was truncated")
            .unwrap();
        assert_eq!(&response, b"bye");
        timeout(Duration::from_secs(5), server_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        client_pump.abort();
    }

    #[tokio::test]
    async fn udp_relay_to_ipv6_target() {
        let uuid = Uuid::new_v4();
        let Ok(target) = UdpSocket::bind("[::1]:0").await else {
            return;
        };
        let port = target.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut buffer = [0u8; 64];
            let (read, peer) = target.recv_from(&mut buffer).await.unwrap();
            target.send_to(&buffer[..read], peer).await.unwrap();
        });
        let state = Arc::new(NodeState::new());
        state.users.apply(vec![(
            uuid,
            UserEntry {
                uid: 1,
                speed_limit: 0,
                device_limit: 0,
            },
        )]);
        let mut runtime = RuntimeConfig::default();
        runtime.allow_private_targets = true;
        let context = Arc::new(VlessContext {
            state,
            runtime: Arc::new(runtime),
            node: Arc::new(NodeInfo::default()),
            encryption: None,
            cancel: CancellationToken::new(),
        });
        let (client, server) = tokio::io::duplex(65536);
        tokio::spawn(
            async move { serve(server, None, "203.0.113.7".parse().unwrap(), context).await },
        );
        let (mut client_reader, mut client_writer) = tokio::io::split(client);
        let mut request = vec![0];
        request.extend_from_slice(uuid.as_bytes());
        request.extend_from_slice(&[0, 2]);
        request.extend_from_slice(&port.to_be_bytes());
        request.push(3);
        request.extend_from_slice(&std::net::Ipv6Addr::LOCALHOST.octets());
        client_writer.write_all(&request).await.unwrap();
        let mut header = [0u8; 2];
        client_reader.read_exact(&mut header).await.unwrap();
        assert_eq!(header, [0, 0]);
        client_writer
            .write_all(&(4u16).to_be_bytes())
            .await
            .unwrap();
        client_writer.write_all(b"ping").await.unwrap();
        let mut length = [0u8; 2];
        timeout(
            Duration::from_secs(3),
            client_reader.read_exact(&mut length),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(u16::from_be_bytes(length), 4);
        let mut payload = [0u8; 4];
        client_reader.read_exact(&mut payload).await.unwrap();
        assert_eq!(&payload, b"ping");
    }

    #[tokio::test]
    async fn udp_relay_echo() {
        let uuid = Uuid::new_v4();
        let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut buffer = [0u8; 4096];
            loop {
                let (read, peer) = echo.recv_from(&mut buffer).await.unwrap();
                echo.send_to(&buffer[..read], peer).await.unwrap();
            }
        });

        let (client, server) = tokio::io::duplex(65536);
        let context = context(uuid);
        tokio::spawn(
            async move { serve(server, None, "203.0.113.7".parse().unwrap(), context).await },
        );
        let (mut client_reader, mut client_writer) = tokio::io::split(client);
        let request = ipv4_request(uuid, 2, port, std::net::Ipv4Addr::LOCALHOST, b"");
        client_writer.write_all(&request).await.unwrap();
        let mut response = [0u8; 2];
        client_reader.read_exact(&mut response).await.unwrap();
        assert_eq!(response, [0, 0]);
        client_writer
            .write_all(&(4u16.to_be_bytes()))
            .await
            .unwrap();
        client_writer.write_all(b"ping").await.unwrap();
        let mut length = [0u8; 2];
        client_reader.read_exact(&mut length).await.unwrap();
        assert_eq!(u16::from_be_bytes(length), 4);
        let mut payload = [0u8; 4];
        client_reader.read_exact(&mut payload).await.unwrap();
        assert_eq!(&payload, b"ping");
    }
}
