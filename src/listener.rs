use anyhow::{Context, Result, anyhow};
use arc_swap::ArcSwap;
use base64::Engine;
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use std::{
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context as TaskContext, Poll},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpSocket, TcpStream},
    sync::Semaphore,
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async_with_config,
    tungstenite::{
        Message,
        handshake::server::{ErrorResponse, Request, Response},
        http::{HeaderMap, HeaderValue, StatusCode, header::HeaderName},
        protocol::WebSocketConfig,
    },
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    config::RuntimeConfig,
    panel::NodeInfo,
    tls::TlsManager,
    vless::{self, VlessContext},
};

#[derive(Clone)]
pub struct ListenerConfig {
    pub bind: SocketAddr,
    pub path: String,
    pub host: Option<String>,
    pub trusted_xff: Vec<String>,
    pub early_data_max: usize,
    pub max_header_bytes: usize,
    pub max_message_bytes: usize,
    pub backlog: usize,
    pub connection_limit: Option<Arc<Semaphore>>,
    pub ws_handshake_timeout: Duration,
    pub tls_handshake_timeout: Duration,
    pub drain_timeout: Duration,
    pub heartbeat_interval: Duration,
    pub relay_buffer_size: usize,
    pub tls: Option<tokio_rustls::TlsAcceptor>,
}

impl ListenerConfig {
    pub fn from_node(
        node: &NodeInfo,
        runtime: &RuntimeConfig,
        tls: &TlsManager,
        connection_limit: Option<Arc<Semaphore>>,
    ) -> Result<Self> {
        let ip: IpAddr = node
            .listen_ip
            .parse()
            .with_context(|| format!("invalid listen_ip {:?}", node.listen_ip))?;
        let path = node
            .ws_path()
            .ok_or_else(|| anyhow!("network_settings.path is empty"))?
            .to_owned();
        Ok(Self {
            bind: SocketAddr::new(ip, node.server_port),
            path,
            host: node.ws_host().map(ToOwned::to_owned),
            trusted_xff: node.trusted_x_forwarded_for.clone(),
            early_data_max: runtime.ws_early_data_max_bytes,
            max_header_bytes: runtime.ws_max_header_bytes,
            max_message_bytes: runtime.ws_max_message_bytes,
            backlog: runtime.listen_backlog,
            connection_limit,
            ws_handshake_timeout: Duration::from_secs(runtime.ws_handshake_timeout_secs),
            tls_handshake_timeout: Duration::from_secs(runtime.tls_handshake_timeout_secs),
            drain_timeout: Duration::from_secs(runtime.drain_timeout_secs.max(1)),
            heartbeat_interval: Duration::from_secs(runtime.ws_heartbeat_secs),
            relay_buffer_size: runtime.relay_buffer_size,
            tls: tls.acceptor(),
        })
    }
}

pub struct ListenerState {
    pub config: Arc<ListenerConfig>,
    pub context: Arc<VlessContext>,
}

pub struct NodeListener {
    #[allow(dead_code)]
    pub local_addr: SocketAddr,
    state: Arc<ArcSwap<ListenerState>>,
    join: tokio::task::JoinHandle<()>,
}

impl NodeListener {
    pub async fn start(
        config: Arc<ListenerConfig>,
        context: Arc<VlessContext>,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let socket = match config.bind {
            SocketAddr::V4(_) => TcpSocket::new_v4(),
            SocketAddr::V6(_) => TcpSocket::new_v6(),
        }
        .with_context(|| format!("create socket for {}", config.bind))?;
        #[cfg(unix)]
        socket
            .set_reuseaddr(true)
            .with_context(|| format!("set reuseaddr {}", config.bind))?;
        socket
            .bind(config.bind)
            .with_context(|| format!("bind {}", config.bind))?;
        let listener = socket
            .listen(u32::try_from(config.backlog).unwrap_or(u32::MAX))
            .with_context(|| format!("listen {}", config.bind))?;
        let local_addr = listener.local_addr().context("get listener address")?;
        info!(
            addr = %local_addr,
            path = %config.path,
            tls = config.tls.is_some(),
            "listener started"
        );
        let state = Arc::new(ArcSwap::from_pointee(ListenerState { config, context }));
        let accept_state = state.clone();
        let join = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, peer)) => {
                                let current = accept_state.load_full();
                                let config = current.config.clone();
                                let context = current.context.clone();
                                let permit = match config.connection_limit.as_ref() {
                                    Some(limit) => match limit.clone().try_acquire_owned() {
                                        Ok(permit) => Some(permit),
                                        Err(_) => {
                                            warn!(peer = %peer, "connection limit reached; dropping connection");
                                            continue;
                                        }
                                    },
                                    None => None,
                                };
                                let connection_cancel = context.cancel.clone();
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    tokio::select! {
                                        () = connection_cancel.cancelled() => {}
                                        result = handle_connection(stream, peer, config, context) => {
                                            if let Err(error) = result {
                                                debug!(peer = %peer, error = %error, "connection closed with error");
                                            }
                                        }
                                    }
                                });
                            }
                            Err(error) => {
                                warn!(error = %error, "accept failed");
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                        }
                    }
                }
            }
            info!(addr = %local_addr, "listener stopped");
        });
        Ok(Self {
            local_addr,
            state,
            join,
        })
    }

    pub fn update(&self, config: Arc<ListenerConfig>, context: Arc<VlessContext>) {
        self.state
            .store(Arc::new(ListenerState { config, context }));
    }

    pub async fn wait(self) {
        let _ = self.join.await;
    }
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    config: Arc<ListenerConfig>,
    context: Arc<VlessContext>,
) -> Result<()> {
    let peer_ip = peer.ip();
    if let Some(acceptor) = config.tls.clone() {
        let stream = tokio::time::timeout(config.tls_handshake_timeout, acceptor.accept(stream))
            .await
            .map_err(|_| anyhow!("TLS handshake timeout"))?
            .context("TLS handshake failed")?;
        run_ws(stream, peer_ip, config, context).await
    } else {
        run_ws(stream, peer_ip, config, context).await
    }
}

async fn run_ws<S>(
    stream: S,
    peer_ip: IpAddr,
    config: Arc<ListenerConfig>,
    context: Arc<VlessContext>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (ws, early, client_ip) = ws_handshake(stream, peer_ip, config.clone()).await?;
    let (io, pump) = ws_to_io(ws, config.relay_buffer_size, config.heartbeat_interval);
    let result = vless::serve(io, early, client_ip, context).await;
    let mut pump = pump;
    if tokio::time::timeout(config.drain_timeout, &mut pump)
        .await
        .is_err()
    {
        pump.abort();
    }
    result
}

#[derive(Default)]
struct HandshakeCapture {
    early_data: Option<Vec<u8>>,
    client_ip: Option<IpAddr>,
}

struct HeaderLimitState {
    read: AtomicUsize,
    done: AtomicBool,
    limit: usize,
}

struct HeaderLimitedStream<S> {
    inner: S,
    state: Arc<HeaderLimitState>,
}

impl<S: AsyncRead + Unpin> AsyncRead for HeaderLimitedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buffer.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buffer) {
            Poll::Ready(Ok(())) => {
                let added = buffer.filled().len() - before;
                if added > 0 && !this.state.done.load(Ordering::Relaxed) {
                    let total = this.state.read.fetch_add(added, Ordering::Relaxed) + added;
                    if total > this.state.limit {
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "websocket header size limit exceeded",
                        )));
                    }
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for HeaderLimitedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[allow(clippy::result_large_err)]
async fn ws_handshake<S>(
    stream: S,
    peer_ip: IpAddr,
    config: Arc<ListenerConfig>,
) -> Result<(
    WebSocketStream<HeaderLimitedStream<S>>,
    Option<Vec<u8>>,
    IpAddr,
)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let capture = Arc::new(Mutex::new(HandshakeCapture::default()));
    let capture_callback = capture.clone();
    let config_callback = config.clone();
    let limit_state = Arc::new(HeaderLimitState {
        read: AtomicUsize::new(0),
        done: AtomicBool::new(false),
        limit: config.max_header_bytes,
    });
    let limit_callback = limit_state.clone();
    let callback = move |request: &Request,
                         mut response: Response|
          -> Result<Response, ErrorResponse> {
        if limit_callback.read.load(Ordering::Relaxed) > limit_callback.limit {
            return Err(error_response(
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
                "request headers too large",
            ));
        }
        if request.uri().path() != config_callback.path {
            return Err(error_response(StatusCode::NOT_FOUND, "not found"));
        }
        if let Some(expected_host) = config_callback.host.as_deref() {
            let request_host = request
                .headers()
                .get("host")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            if !is_valid_http_host(request_host, expected_host) {
                return Err(error_response(StatusCode::NOT_FOUND, "not found"));
            }
        }
        let early_header = request
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|value| value.to_str().ok());
        let early_data = early_header.and_then(|raw| {
            match decode_early_data(raw, config_callback.early_data_max) {
                Ok(data) => data,
                Err(error) => {
                    warn!(error = %error, "ignore invalid websocket early data");
                    None
                }
            }
        });
        if early_data.is_some()
            && let Some(raw) = early_header
        {
            let header_value = HeaderValue::from_str(raw)
                .map_err(|_| error_response(StatusCode::BAD_REQUEST, "bad websocket protocol"))?;
            response.headers_mut().insert(
                HeaderName::from_static("sec-websocket-protocol"),
                header_value,
            );
        }
        let client_ip = apply_trusted_xff(request.headers(), &config_callback.trusted_xff, peer_ip);
        let mut captured = capture_callback.lock();
        captured.early_data = early_data;
        captured.client_ip = Some(client_ip);
        limit_callback.done.store(true, Ordering::Relaxed);
        Ok(response)
    };
    let mut ws_config = WebSocketConfig::default();
    ws_config.max_message_size = Some(config.max_message_bytes);
    ws_config.max_frame_size = Some(config.max_message_bytes);
    let limited = HeaderLimitedStream {
        inner: stream,
        state: limit_state,
    };
    let ws = tokio::time::timeout(
        config.ws_handshake_timeout,
        accept_hdr_async_with_config(limited, callback, Some(ws_config)),
    )
    .await
    .map_err(|_| anyhow!("websocket handshake timeout"))?
    .map_err(|error| anyhow!("websocket handshake failed: {error}"))?;
    let captured = capture.lock();
    let early = captured.early_data.clone();
    let client_ip = captured.client_ip.unwrap_or(peer_ip);
    Ok((ws, early, client_ip))
}

async fn heartbeat_tick(timer: &mut Option<tokio::time::Interval>) {
    match timer {
        Some(timer) => {
            timer.tick().await;
        }
        None => std::future::pending::<()>().await,
    }
}

fn ws_to_io<S>(
    ws: WebSocketStream<S>,
    buffer_size: usize,
    heartbeat: Duration,
) -> (tokio::io::DuplexStream, tokio::task::JoinHandle<()>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let capacity = buffer_size.max(1024);
    let (app, pump_io) = tokio::io::duplex(capacity);
    let join = tokio::spawn(async move {
        let (mut app_reader, mut app_writer) = tokio::io::split(pump_io);
        let (mut ws_sink, mut ws_stream) = ws.split();
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel(32);
        let last_activity = Arc::new(Mutex::new(Instant::now()));
        let inbound_activity = last_activity.clone();
        let inbound = async move {
            loop {
                let message = ws_stream.next().await;
                if message.is_some() {
                    *inbound_activity.lock() = Instant::now();
                }
                match message {
                    Some(Ok(Message::Binary(data))) => {
                        if app_writer.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        if app_writer
                            .write_all(text.as_str().as_bytes())
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = control_tx.try_send(Message::Pong(data));
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
            let _ = app_writer.shutdown().await;
        };
        let outbound_activity = last_activity.clone();
        let outbound = async move {
            let mut buffer = vec![0u8; capacity];
            let mut heartbeat_timer = (!heartbeat.is_zero()).then(|| {
                tokio::time::interval_at(tokio::time::Instant::now() + heartbeat, heartbeat)
            });
            loop {
                tokio::select! {
                    read = app_reader.read(&mut buffer) => {
                        match read {
                            Ok(0) => {
                                let _ = ws_sink.send(Message::Close(None)).await;
                                break;
                            }
                            Ok(read) => {
                                if ws_sink
                                    .send(Message::Binary(buffer[..read].to_vec().into()))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                                *outbound_activity.lock() = Instant::now();
                            }
                            Err(_) => break,
                        }
                    }
                    control = control_rx.recv() => {
                        match control {
                            Some(message) => {
                                if ws_sink.send(message).await.is_err() {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    () = heartbeat_tick(&mut heartbeat_timer) => {
                        let idle = outbound_activity.lock().elapsed();
                        if idle >= heartbeat.saturating_mul(3) {
                            let _ = tokio::time::timeout(
                                Duration::from_secs(1),
                                ws_sink.send(Message::Close(None)),
                            )
                            .await;
                            break;
                        }
                        if ws_sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = ws_sink.close().await;
        };
        tokio::select! {
            () = inbound => {}
            () = outbound => {}
        }
    });
    (app, join)
}

fn error_response(status: StatusCode, message: &str) -> ErrorResponse {
    let mut response =
        tokio_tungstenite::tungstenite::http::Response::new(Some(message.to_owned()));
    *response.status_mut() = status;
    response
}

fn is_valid_http_host(request: &str, config: &str) -> bool {
    let request = request.to_ascii_lowercase();
    let config = config.to_ascii_lowercase();
    if request.contains(':') {
        match request.rsplit_once(':') {
            Some((host, _)) => host == config,
            None => false,
        }
    } else {
        request == config
    }
}

fn apply_trusted_xff(headers: &HeaderMap, trusted: &[String], peer_ip: IpAddr) -> IpAddr {
    for name in trusted {
        let Some(value) = headers
            .get(name.as_str())
            .and_then(|value| value.to_str().ok())
        else {
            continue;
        };
        let first = value.split(',').next().unwrap_or("").trim();
        return first.parse::<IpAddr>().unwrap_or(peer_ip);
    }
    if headers.contains_key("x-forwarded-for") {
        if trusted.is_empty() {
            warn!(
                "received X-Forwarded-For but trusted_x_forwarded_for is not configured; ignoring"
            );
        } else {
            warn!("ignored potentially forged X-Forwarded-For");
        }
    }
    peer_ip
}

fn decode_early_data(raw: &str, max: usize) -> Result<Option<Vec<u8>>> {
    let normalized = raw.replace('+', "-").replace('/', "_");
    let trimmed = normalized.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(trimmed)
        .context("decode websocket early data")?;
    if bytes.len() > max {
        return Err(anyhow!(
            "websocket early data {} exceeds limit {}",
            bytes.len(),
            max
        ));
    }
    Ok(if bytes.is_empty() { None } else { Some(bytes) })
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;
    use crate::panel::NodeInfo;
    use crate::state::{NodeState, UserEntry};
    use crate::tls::TlsManager;
    use crate::vless::VlessContext;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue};
    use uuid::Uuid;

    fn runtime() -> RuntimeConfig {
        let mut runtime = RuntimeConfig::default();
        runtime.allow_private_targets = true;
        runtime
    }

    fn node_fixture() -> NodeInfo {
        serde_json::from_value(serde_json::json!({
            "listen_ip": "127.0.0.1",
            "server_port": 0,
            "network": "ws",
            "network_settings": {
                "path": "/cdn-api",
                "headers": {"Host": "hk2.wxmuma.cc.cd"}
            },
            "protocol": "vless",
            "tls": 1,
            "tls_settings": {
                "server_name": "hk2.wxmuma.cc.cd",
                "cert_mode": "none"
            },
            "flow": "",
            "encryption": "",
            "trusted_x_forwarded_for": ["X-Forwarded-For"],
            "base_config": {},
            "routes": []
        }))
        .unwrap()
    }

    fn vless_context(uuid: Uuid) -> Arc<VlessContext> {
        let state = Arc::new(NodeState::new());
        state.users.apply(vec![(
            uuid,
            UserEntry {
                uid: 1,
                speed_limit: 0,
                device_limit: 0,
            },
        )]);
        Arc::new(VlessContext {
            state,
            runtime: Arc::new(runtime()),
            node: Arc::new(node_fixture()),
            encryption: None,
            cancel: CancellationToken::new(),
        })
    }

    fn ipv4_request(uuid: Uuid, command: u8, port: u16, payload: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        data.push(0);
        data.extend_from_slice(uuid.as_bytes());
        data.push(0);
        data.push(command);
        data.extend_from_slice(&port.to_be_bytes());
        data.push(1);
        data.extend_from_slice(&[127, 0, 0, 1]);
        data.extend_from_slice(payload);
        data
    }

    #[test]
    fn host_validation_matches_xray() {
        assert!(is_valid_http_host("hk2.wxmuma.cc.cd", "hk2.wxmuma.cc.cd"));
        assert!(is_valid_http_host("HK2.WXMUMA.CC.CD", "hk2.wxmuma.cc.cd"));
        assert!(is_valid_http_host(
            "hk2.wxmuma.cc.cd:443",
            "hk2.wxmuma.cc.cd"
        ));
        assert!(!is_valid_http_host("evil.example.com", "hk2.wxmuma.cc.cd"));
        assert!(!is_valid_http_host(
            "evil.example.com:443",
            "hk2.wxmuma.cc.cd"
        ));
    }

    #[test]
    fn xff_uses_first_ip_only_when_trusted_header_present() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.7, 10.0.0.1"),
        );
        let trusted = vec!["X-Forwarded-For".to_owned()];
        assert_eq!(
            apply_trusted_xff(&headers, &trusted, "127.0.0.1".parse().unwrap()),
            "203.0.113.7".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            apply_trusted_xff(&headers, &[], "127.0.0.1".parse().unwrap()),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn xff_uses_the_trusted_header_value() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", HeaderValue::from_static("198.51.100.9"));
        let trusted = vec!["X-Real-IP".to_owned()];
        assert_eq!(
            apply_trusted_xff(&headers, &trusted, "127.0.0.1".parse().unwrap()),
            "198.51.100.9".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn early_data_decodes_base64url() {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"hello");
        assert_eq!(
            decode_early_data(&raw, 2048).unwrap(),
            Some(b"hello".to_vec())
        );
        assert!(decode_early_data(&raw, 2).is_err());
    }

    #[tokio::test]
    async fn websocket_vless_tcp_and_early_data() {
        let uuid = Uuid::new_v4();
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
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

        let manager = TlsManager::new();
        let node = node_fixture();
        manager.apply_node(&node, &runtime()).unwrap();
        let config =
            Arc::new(ListenerConfig::from_node(&node, &runtime(), &manager, None).unwrap());
        let cancel = CancellationToken::new();
        let listener = NodeListener::start(config, vless_context(uuid), cancel.clone())
            .await
            .unwrap();
        let port = listener.local_addr.port();
        let early = ipv4_request(uuid, 1, echo_port, b"hello");
        let early_raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&early);
        let mut request = format!("ws://127.0.0.1:{port}/cdn-api")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Host", HeaderValue::from_static("hk2.wxmuma.cc.cd"));
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_str(&early_raw).unwrap(),
        );
        let (mut ws, response) = tokio_tungstenite::connect_async(request).await.unwrap();
        assert_eq!(
            response
                .headers()
                .get("sec-websocket-protocol")
                .unwrap()
                .to_str()
                .unwrap(),
            early_raw
        );
        let mut received = Vec::new();
        while received.len() < 7 {
            let Some(message) = ws.next().await else {
                break;
            };
            let Ok(Message::Binary(data)) = message else {
                continue;
            };
            received.extend_from_slice(&data);
        }
        assert_eq!(&received[..2], &[0, 0]);
        assert_eq!(&received[2..7], b"hello");
        ws.send(Message::Binary(b"world".to_vec().into()))
            .await
            .unwrap();
        let mut received = Vec::new();
        while received.len() < 5 {
            let Some(message) = ws.next().await else {
                break;
            };
            let Ok(Message::Binary(data)) = message else {
                continue;
            };
            received.extend_from_slice(&data);
        }
        assert_eq!(&received[..5], b"world");
        drop(ws);
        cancel.cancel();
        listener.wait().await;
    }

    #[tokio::test]
    async fn websocket_large_bidirectional_transfer_does_not_deadlock() {
        let uuid = Uuid::new_v4();
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = echo.accept().await.unwrap();
            let mut buffer = [0u8; 32768];
            loop {
                let read = match stream.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                if stream.write_all(&buffer[..read]).await.is_err() {
                    break;
                }
            }
        });
        let manager = TlsManager::new();
        let node = node_fixture();
        manager.apply_node(&node, &runtime()).unwrap();
        let config =
            Arc::new(ListenerConfig::from_node(&node, &runtime(), &manager, None).unwrap());
        let cancel = CancellationToken::new();
        let listener = NodeListener::start(config, vless_context(uuid), cancel.clone())
            .await
            .unwrap();
        let port = listener.local_addr.port();
        let mut request = format!("ws://127.0.0.1:{port}/cdn-api")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Host", HeaderValue::from_static("hk2.wxmuma.cc.cd"));
        let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let payload: Vec<u8> = (0..512_000u32).map(|index| index as u8).collect();
        let mut bytes = ipv4_request(uuid, 1, echo_port, b"");
        bytes.extend_from_slice(&payload);
        ws.send(Message::Binary(bytes.into())).await.unwrap();
        let mut received = Vec::new();
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            while received.len() < payload.len() + 2 {
                let message = ws.next().await.unwrap().unwrap();
                if let Message::Binary(data) = message {
                    received.extend_from_slice(&data);
                }
            }
        })
        .await;
        assert!(result.is_ok(), "large websocket transfer deadlocked");
        assert_eq!(&received[..2], &[0, 0]);
        assert_eq!(&received[2..], payload.as_slice());
        drop(ws);
        cancel.cancel();
        listener.wait().await;
    }

    #[tokio::test]
    async fn websocket_bidirectional_streaming_does_not_deadlock() {
        let uuid = Uuid::new_v4();
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = echo.accept().await.unwrap();
            let mut buffer = [0u8; 16384];
            loop {
                let read = match stream.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                if stream.write_all(&buffer[..read]).await.is_err() {
                    break;
                }
            }
        });
        let manager = TlsManager::new();
        let node = node_fixture();
        manager.apply_node(&node, &runtime()).unwrap();
        let config =
            Arc::new(ListenerConfig::from_node(&node, &runtime(), &manager, None).unwrap());
        let cancel = CancellationToken::new();
        let listener = NodeListener::start(config, vless_context(uuid), cancel.clone())
            .await
            .unwrap();
        let port = listener.local_addr.port();
        let mut request = format!("ws://127.0.0.1:{port}/cdn-api")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Host", HeaderValue::from_static("hk2.wxmuma.cc.cd"));
        let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let (mut ws_sink, mut ws_stream) = ws.split();
        let payload: Vec<u8> = (0..1_048_576u32).map(|index| index as u8).collect();
        let mut first = ipv4_request(uuid, 1, echo_port, b"");
        first.extend_from_slice(&payload);
        let writer = tokio::spawn(async move {
            for chunk in first.chunks(32 * 1024) {
                ws_sink
                    .send(Message::Binary(chunk.to_vec().into()))
                    .await
                    .unwrap();
            }
            ws_sink
        });
        let result = tokio::time::timeout(Duration::from_secs(20), async {
            let mut received = Vec::new();
            while received.len() < payload.len() + 2 {
                let message = ws_stream.next().await.unwrap().unwrap();
                if let Message::Binary(data) = message {
                    received.extend_from_slice(&data);
                }
            }
            received
        })
        .await;
        let received = result.expect("bidirectional websocket transfer deadlocked");
        assert_eq!(&received[..2], &[0, 0]);
        assert_eq!(&received[2..], payload.as_slice());
        let sink = writer.await.unwrap();
        drop(sink);
        drop(ws_stream);
        cancel.cancel();
        listener.wait().await;
    }

    #[tokio::test]
    async fn connection_limit_rejects_extra_connections() {
        let manager = TlsManager::new();
        let node = node_fixture();
        manager.apply_node(&node, &runtime()).unwrap();
        let mut runtime = runtime();
        runtime.max_connections = 1;
        let config = Arc::new(
            ListenerConfig::from_node(&node, &runtime, &manager, Some(Arc::new(Semaphore::new(1))))
                .unwrap(),
        );
        let cancel = CancellationToken::new();
        let listener = NodeListener::start(config, vless_context(Uuid::new_v4()), cancel.clone())
            .await
            .unwrap();
        let port = listener.local_addr.port();
        let mut first = format!("ws://127.0.0.1:{port}/cdn-api")
            .into_client_request()
            .unwrap();
        first
            .headers_mut()
            .insert("Host", HeaderValue::from_static("hk2.wxmuma.cc.cd"));
        let (first_ws, _) = tokio_tungstenite::connect_async(first).await.unwrap();
        let mut second = format!("ws://127.0.0.1:{port}/cdn-api")
            .into_client_request()
            .unwrap();
        second
            .headers_mut()
            .insert("Host", HeaderValue::from_static("hk2.wxmuma.cc.cd"));
        assert!(tokio_tungstenite::connect_async(second).await.is_err());
        drop(first_ws);
        cancel.cancel();
        listener.wait().await;
    }

    #[tokio::test]
    async fn oversized_websocket_headers_are_rejected() {
        let manager = TlsManager::new();
        let node = node_fixture();
        manager.apply_node(&node, &runtime()).unwrap();
        let mut runtime = runtime();
        runtime.ws_max_header_bytes = 1024;
        let config = Arc::new(ListenerConfig::from_node(&node, &runtime, &manager, None).unwrap());
        let cancel = CancellationToken::new();
        let listener = NodeListener::start(config, vless_context(Uuid::new_v4()), cancel.clone())
            .await
            .unwrap();
        let port = listener.local_addr.port();
        let request = format!("ws://127.0.0.1:{port}/cdn-api")
            .into_client_request()
            .unwrap();
        let mut request = request;
        request
            .headers_mut()
            .insert("Cookie", HeaderValue::from_str(&"a".repeat(4096)).unwrap());
        assert!(tokio_tungstenite::connect_async(request).await.is_err());
        cancel.cancel();
        listener.wait().await;
    }

    #[tokio::test]
    async fn heartbeat_closes_silent_peer() {
        let manager = TlsManager::new();
        let node = node_fixture();
        manager.apply_node(&node, &runtime()).unwrap();
        let mut runtime = runtime();
        runtime.ws_heartbeat_secs = 1;
        runtime.drain_timeout_secs = 1;
        let config = Arc::new(ListenerConfig::from_node(&node, &runtime, &manager, None).unwrap());
        let cancel = CancellationToken::new();
        let listener = NodeListener::start(config, vless_context(Uuid::new_v4()), cancel.clone())
            .await
            .unwrap();
        let port = listener.local_addr.port();
        let mut request = format!("ws://127.0.0.1:{port}/cdn-api")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Host", HeaderValue::from_static("hk2.wxmuma.cc.cd"));
        let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        tokio::time::sleep(Duration::from_millis(3500)).await;
        let mut closed = false;
        for _ in 0..5 {
            match tokio::time::timeout(Duration::from_secs(1), ws.next()).await {
                Ok(Some(Ok(Message::Ping(_)))) => continue,
                Ok(None | Some(Ok(Message::Close(_))) | Some(Err(_))) => {
                    closed = true;
                    break;
                }
                Ok(Some(Ok(_))) => continue,
                Err(_) => break,
            }
        }
        assert!(closed, "heartbeat did not close the silent peer");
        cancel.cancel();
        listener.wait().await;
    }

    #[tokio::test]
    async fn shared_connection_limit_spans_listeners() {
        let manager = TlsManager::new();
        let node = node_fixture();
        manager.apply_node(&node, &runtime()).unwrap();
        let limit = Some(Arc::new(Semaphore::new(1)));
        let first_config = Arc::new(
            ListenerConfig::from_node(&node, &runtime(), &manager, limit.clone()).unwrap(),
        );
        let second_config =
            Arc::new(ListenerConfig::from_node(&node, &runtime(), &manager, limit).unwrap());
        let cancel = CancellationToken::new();
        let first = NodeListener::start(
            first_config,
            vless_context(Uuid::new_v4()),
            cancel.child_token(),
        )
        .await
        .unwrap();
        let second = NodeListener::start(
            second_config,
            vless_context(Uuid::new_v4()),
            cancel.child_token(),
        )
        .await
        .unwrap();
        let connect = |port: u16| {
            let mut request = format!("ws://127.0.0.1:{port}/cdn-api")
                .into_client_request()
                .unwrap();
            request
                .headers_mut()
                .insert("Host", HeaderValue::from_static("hk2.wxmuma.cc.cd"));
            tokio_tungstenite::connect_async(request)
        };
        let (first_ws, _) = connect(first.local_addr.port()).await.unwrap();
        assert!(connect(second.local_addr.port()).await.is_err());
        drop(first_ws);
        cancel.cancel();
        first.wait().await;
        second.wait().await;
    }

    #[tokio::test]
    async fn node_shutdown_closes_inflight_connections() {
        let manager = TlsManager::new();
        let node = node_fixture();
        manager.apply_node(&node, &runtime()).unwrap();
        let config =
            Arc::new(ListenerConfig::from_node(&node, &runtime(), &manager, None).unwrap());
        let context = vless_context(Uuid::new_v4());
        let node_cancel = context.cancel.clone();
        let listener_cancel = CancellationToken::new();
        let listener = NodeListener::start(config, context, listener_cancel.clone())
            .await
            .unwrap();
        let port = listener.local_addr.port();
        let mut request = format!("ws://127.0.0.1:{port}/cdn-api")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Host", HeaderValue::from_static("hk2.wxmuma.cc.cd"));
        let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        node_cancel.cancel();
        let closed = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("connection survived node shutdown");
        assert!(matches!(
            closed,
            None | Some(Ok(Message::Close(_))) | Some(Err(_))
        ));
        listener_cancel.cancel();
        listener.wait().await;
    }

    #[tokio::test]
    async fn listener_update_swaps_config_in_place() {
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = echo.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    loop {
                        let read = match stream.read(&mut buffer).await {
                            Ok(0) | Err(_) => break,
                            Ok(read) => read,
                        };
                        if stream.write_all(&buffer[..read]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        let uuid = Uuid::new_v4();
        let manager = TlsManager::new();
        let node = node_fixture();
        manager.apply_node(&node, &runtime()).unwrap();
        let config =
            Arc::new(ListenerConfig::from_node(&node, &runtime(), &manager, None).unwrap());
        let cancel = CancellationToken::new();
        let listener = NodeListener::start(config, vless_context(uuid), cancel.child_token())
            .await
            .unwrap();
        let port = listener.local_addr.port();
        let mut request = format!("ws://127.0.0.1:{port}/cdn-api")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Host", HeaderValue::from_static("hk2.wxmuma.cc.cd"));
        let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let bytes = ipv4_request(uuid, 1, echo_port, b"hello");
        ws.send(Message::Binary(bytes.into())).await.unwrap();
        let mut header_and_echo = Vec::new();
        while header_and_echo.len() < 7 {
            if let Some(Ok(Message::Binary(data))) = ws.next().await {
                header_and_echo.extend_from_slice(&data);
            }
        }
        assert_eq!(&header_and_echo[2..7], b"hello");

        let mut updated = node_fixture();
        updated.network_settings.as_mut().unwrap().path = "/new-api".to_owned();
        let new_config =
            Arc::new(ListenerConfig::from_node(&updated, &runtime(), &manager, None).unwrap());
        listener.update(new_config, vless_context(uuid));
        assert_eq!(listener.local_addr.port(), port);

        ws.send(Message::Binary(b"world".to_vec().into()))
            .await
            .unwrap();
        let mut echo = Vec::new();
        while echo.len() < 5 {
            if let Some(Ok(Message::Binary(data))) = ws.next().await {
                echo.extend_from_slice(&data);
            }
        }
        assert_eq!(&echo[..5], b"world");

        let mut old_path = format!("ws://127.0.0.1:{port}/cdn-api")
            .into_client_request()
            .unwrap();
        old_path
            .headers_mut()
            .insert("Host", HeaderValue::from_static("hk2.wxmuma.cc.cd"));
        assert!(tokio_tungstenite::connect_async(old_path).await.is_err());
        let mut new_path = format!("ws://127.0.0.1:{port}/new-api")
            .into_client_request()
            .unwrap();
        new_path
            .headers_mut()
            .insert("Host", HeaderValue::from_static("hk2.wxmuma.cc.cd"));
        let (new_ws, _) = tokio_tungstenite::connect_async(new_path).await.unwrap();
        drop(new_ws);
        drop(ws);
        cancel.cancel();
        listener.wait().await;
    }

    #[tokio::test]
    async fn wrong_path_and_host_are_rejected() {
        let manager = TlsManager::new();
        let node = node_fixture();
        manager.apply_node(&node, &runtime()).unwrap();
        let config =
            Arc::new(ListenerConfig::from_node(&node, &runtime(), &manager, None).unwrap());
        let cancel = CancellationToken::new();
        let listener = NodeListener::start(config, vless_context(Uuid::new_v4()), cancel.clone())
            .await
            .unwrap();
        let port = listener.local_addr.port();
        let request = format!("ws://127.0.0.1:{port}/wrong")
            .into_client_request()
            .unwrap();
        assert!(tokio_tungstenite::connect_async(request).await.is_err());
        let mut request = format!("ws://127.0.0.1:{port}/cdn-api")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Host", HeaderValue::from_static("evil.example.com"));
        assert!(tokio_tungstenite::connect_async(request).await.is_err());
        cancel.cancel();
        listener.wait().await;
    }
}
