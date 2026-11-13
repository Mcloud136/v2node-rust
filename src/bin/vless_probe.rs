use v2node_proto::encryption;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use rustls::{
    ClientConfig, DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use std::{net::Ipv4Addr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream},
    net::{TcpListener, UdpSocket},
    task::JoinHandle,
};
use tokio_tungstenite::{
    Connector, WebSocketStream, connect_async, connect_async_tls_with_config,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};
use uuid::Uuid;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long)]
    url: String,
    #[arg(long)]
    host: String,
    #[arg(long)]
    uuid: String,
    #[arg(long, default_value = "tcp")]
    mode: String,
    #[arg(long, default_value = "hello")]
    payload: String,
    #[arg(long, default_value_t = 0)]
    payload_size: usize,
    #[arg(long, default_value_t = 1)]
    roundtrips: usize,
    #[arg(long, default_value_t = 0)]
    hold_seconds: u64,
    #[arg(long, default_value_t = false)]
    early: bool,
    #[arg(long, default_value_t = false)]
    insecure: bool,
    #[arg(long)]
    target: Option<String>,
    #[arg(long)]
    xff: Option<String>,
    #[arg(long)]
    encryption: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let uuid = Uuid::parse_str(&args.uuid).context("parse uuid")?;
    let command = match args.mode.as_str() {
        "tcp" => 1u8,
        "udp" => 2u8,
        other => bail!("unknown mode {other}"),
    };
    let (target_addr, port) = if let Some(target) = &args.target {
        let (host, port) = target
            .rsplit_once(':')
            .ok_or_else(|| anyhow::anyhow!("--target must be IPv4:port"))?;
        let host: Ipv4Addr = host.parse().context("parse --target IPv4")?;
        let port: u16 = port.parse().context("parse --target port")?;
        (host, port)
    } else if command == 1 {
        (Ipv4Addr::LOCALHOST, start_tcp_echo().await?)
    } else {
        (Ipv4Addr::LOCALHOST, start_udp_echo().await?)
    };
    let payload = if args.payload_size > 0 {
        vec![b'x'; args.payload_size]
    } else {
        args.payload.as_bytes().to_vec()
    };
    if command == 2 && payload.len() > u16::MAX as usize {
        bail!("UDP payload exceeds 65535 bytes");
    }
    let started = std::time::Instant::now();
    let request_bytes = vless_request(uuid, command, target_addr, port, &payload);
    let mut request = args.url.clone().into_client_request()?;
    request
        .headers_mut()
        .insert("Host", HeaderValue::from_str(&args.host)?);
    if let Some(xff) = &args.xff {
        request
            .headers_mut()
            .insert("X-Forwarded-For", HeaderValue::from_str(xff)?);
    }
    if args.early {
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&request_bytes);
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", HeaderValue::from_str(&encoded)?);
    }
    let (ws, _) = if args.insecure && args.url.starts_with("wss://") {
        let config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth();
        connect_async_tls_with_config(
            request,
            None,
            false,
            Some(Connector::Rustls(Arc::new(config))),
        )
        .await
        .context("websocket TLS connect")?
    } else {
        connect_async(request).await.context("websocket connect")?
    };
    if args.early && args.encryption.is_some() {
        bail!("--early with --encryption is not supported by this probe");
    }
    let (app, pumps) = if let Some(encryption) = &args.encryption {
        let (ws_app, ws_pump) = ws_to_io(ws, 65536);
        let config = encryption::ClientEncryptionConfig::parse(encryption)?;
        let (plain, enc_pump) = encryption::client_handshake(ws_app, &config).await?;
        (plain, vec![ws_pump, enc_pump])
    } else {
        let (app, ws_pump) = ws_to_io(ws, 65536);
        (app, vec![ws_pump])
    };
    let (mut app_reader, app_writer) = tokio::io::split(app);
    let mut app_writer = Some(app_writer);
    let writer_task = if args.encryption.is_some() || !args.early {
        let mut writer = app_writer
            .take()
            .ok_or_else(|| anyhow!("app writer already consumed"))?;
        let bytes = request_bytes;
        Some(tokio::spawn(async move {
            writer.write_all(&bytes).await?;
            writer.flush().await?;
            Ok::<_, std::io::Error>(writer)
        }))
    } else {
        None
    };
    let mut header = [0u8; 2];
    app_reader.read_exact(&mut header).await?;
    if header != [0u8, 0u8] {
        bail!("bad VLESS response header {header:?}");
    }
    let first = read_echo(&mut app_reader, command, payload.len()).await?;
    if first != payload {
        bail!("payload mismatch");
    }
    let mut app_writer = match writer_task {
        Some(task) => task.await??,
        None => app_writer
            .take()
            .ok_or_else(|| anyhow!("app writer already consumed"))?,
    };
    let mut rtts_us = Vec::new();
    for _ in 1..args.roundtrips {
        let started_rtt = std::time::Instant::now();
        write_echo(&mut app_writer, command, &payload).await?;
        let data = read_echo(&mut app_reader, command, payload.len()).await?;
        if data != payload {
            bail!("payload mismatch on roundtrip");
        }
        rtts_us.push(started_rtt.elapsed().as_micros());
    }
    let elapsed_ms = started.elapsed().as_millis();
    if args.hold_seconds > 0 {
        tokio::time::sleep(Duration::from_secs(args.hold_seconds)).await;
    }
    for pump in pumps {
        pump.abort();
    }
    println!(
        "PROBE_OK mode={} port={} early={} bytes={} roundtrips={} hold_seconds={} elapsed_ms={}",
        args.mode,
        port,
        args.early,
        payload.len(),
        args.roundtrips,
        args.hold_seconds,
        elapsed_ms
    );
    if !rtts_us.is_empty() {
        rtts_us.sort_unstable();
        println!(
            "RTT_STATS count={} min_us={} p50_us={} p95_us={} p99_us={} max_us={}",
            rtts_us.len(),
            rtts_us[0],
            percentile(&rtts_us, 50),
            percentile(&rtts_us, 95),
            percentile(&rtts_us, 99),
            rtts_us[rtts_us.len() - 1]
        );
    }
    Ok(())
}

fn percentile(sorted: &[u128], percent: usize) -> u128 {
    sorted[(sorted.len() - 1) * percent / 100]
}

async fn write_echo<W>(app: &mut W, command: u8, payload: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    if command == 2 {
        app.write_all(&(payload.len() as u16).to_be_bytes()).await?;
    }
    app.write_all(payload).await?;
    app.flush().await?;
    Ok(())
}

async fn read_echo<R>(app: &mut R, command: u8, expected: usize) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    if command == 2 {
        let mut length = [0u8; 2];
        app.read_exact(&mut length).await?;
        let length = u16::from_be_bytes(length) as usize;
        let mut data = vec![0u8; length];
        app.read_exact(&mut data).await?;
        Ok(data)
    } else {
        let mut data = vec![0u8; expected];
        app.read_exact(&mut data).await?;
        Ok(data)
    }
}

fn vless_request(uuid: Uuid, command: u8, addr: Ipv4Addr, port: u16, payload: &[u8]) -> Vec<u8> {
    let mut data = Vec::new();
    data.push(0);
    data.extend_from_slice(uuid.as_bytes());
    data.push(0);
    data.push(command);
    data.extend_from_slice(&port.to_be_bytes());
    data.push(1);
    data.extend_from_slice(&addr.octets());
    if command == 2 {
        let length = (payload.len() as u16).to_be_bytes();
        data.extend_from_slice(&length);
    }
    data.extend_from_slice(payload);
    data
}

#[derive(Debug)]
struct NoVerify;

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn ws_to_io<S>(mut ws: WebSocketStream<S>, capacity: usize) -> (DuplexStream, JoinHandle<()>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (app, pump) = tokio::io::duplex(capacity);
    let (mut pump_reader, mut pump_writer) = tokio::io::split(pump);
    let join = tokio::spawn(async move {
        let mut buffer = vec![0u8; capacity.max(1024)];
        loop {
            tokio::select! {
                message = ws.next() => {
                    match message {
                        Some(Ok(Message::Binary(data))) => {
                            if pump_writer.write_all(&data).await.is_err() { break; }
                        }
                        Some(Ok(Message::Text(text))) => {
                            if pump_writer.write_all(text.as_str().as_bytes()).await.is_err() { break; }
                        }
                        Some(Ok(Message::Ping(data))) => {
                            let _ = ws.send(Message::Pong(data)).await;
                        }
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Ok(_)) => {}
                        Some(Err(_)) => break,
                    }
                }
                read = pump_reader.read(&mut buffer) => {
                    match read {
                        Ok(0) => {
                            let _ = ws.send(Message::Close(None)).await;
                            break;
                        }
                        Ok(read) => {
                            if ws.send(Message::Binary(buffer[..read].to_vec().into())).await.is_err() { break; }
                        }
                        Err(_) => break,
                    }
                }
            }
        }
        let _ = pump_writer.shutdown().await;
    });
    (app, join)
}

async fn start_tcp_echo() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buffer = [0u8; 4096];
                while let Ok(read) = stream.read(&mut buffer).await {
                    if read == 0 || stream.write_all(&buffer[..read]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    Ok(port)
}

async fn start_udp_echo() -> Result<u16> {
    let socket = UdpSocket::bind("127.0.0.1:0").await?;
    let port = socket.local_addr()?.port();
    eprintln!("UDP_ECHO_PORT {port}");
    let socket = std::sync::Arc::new(socket);
    tokio::spawn(async move {
        let mut buffer = [0u8; 4096];
        loop {
            let Ok((read, peer)) = socket.recv_from(&mut buffer).await else {
                break;
            };
            eprintln!("UDP_ECHO_RECV {read} from {peer}");
            let _ = socket.send_to(&buffer[..read], peer).await;
        }
    });
    Ok(port)
}
