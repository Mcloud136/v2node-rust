//! C0e / L3 整机基准：本机 TCP 回环 + 完整 VLESS Encryption 泵。
//!
//! 口径见 `docs/benchmark-spec.md` L3：本机回环、真实 socket、无公网 IO。
//! 运行：`cargo run --release --example bench_l3_loopback`
//!
//! 与 L2（`DuplexStream` 内存流）对比，量化系统调用 / TCP 栈边界开销。
//! 范围说明：本层未接 mock 面板 / 订阅，只覆盖数据面；RSS 采样需平台 API，
//! 列为剩余缺口，见正式开发前测试报告。

use anyhow::{Context, Result};
use base64::Engine;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    runtime::Builder,
};
use v2node_proto::encryption::{ClientEncryptionConfig, EncryptionServer, client_handshake};
use x25519_dalek::{PublicKey, StaticSecret};

const PAYLOAD_BYTES: usize = 1 << 20;
const ROUNDS: usize = 5;
const TIMEOUT: Duration = Duration::from_secs(60);
const LADDER: [usize; 3] = [1, 8, 32];

type ServerCfg = Arc<EncryptionServer>;
type ClientCfg = Arc<ClientEncryptionConfig>;

fn config_pair() -> Result<(ServerCfg, ClientCfg)> {
    let secret_bytes = [7u8; 32];
    let secret = StaticSecret::from(secret_bytes);
    let public = PublicKey::from(&secret);
    let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let decryption = format!(
        "mlkem768x25519plus.native.600s.{}",
        encoder.encode(secret_bytes)
    );
    let encryption = format!(
        "mlkem768x25519plus.native.0rtt.{}",
        encoder.encode(public.to_bytes())
    );
    let server = EncryptionServer::from_decryption(&decryption)
        .context("parse decryption")?
        .context("server config must be present")?;
    let client = ClientEncryptionConfig::parse(&encryption).context("parse encryption")?;
    Ok((server, Arc::new(client)))
}

async fn tcp_pair() -> Result<(TcpStream, TcpStream)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        Ok::<TcpStream, std::io::Error>(stream)
    });
    let client = TcpStream::connect(addr).await?;
    let server = task.await??;
    Ok((client, server))
}

async fn handshake_over_tcp(server: ServerCfg, client: ClientCfg) -> Result<Duration> {
    let (client_io, server_io) = tcp_pair().await?;
    let task = tokio::spawn(async move { server.handshake(server_io).await });
    let start = Instant::now();
    let (client_app, client_pump) = client_handshake(client_io, client.as_ref()).await?;
    let (server_app, server_pump) = task.await??;
    let elapsed = start.elapsed();
    drop(client_app);
    drop(server_app);
    client_pump.abort();
    server_pump.abort();
    Ok(elapsed)
}

async fn transfer_over_tcp(
    server: ServerCfg,
    client: ClientCfg,
    payload: Arc<Vec<u8>>,
) -> Result<Duration> {
    let (client_io, server_io) = tcp_pair().await?;
    let task = tokio::spawn(async move { server.handshake(server_io).await });
    let (client_app, client_pump) = client_handshake(client_io, client.as_ref()).await?;
    let (server_app, server_pump) = task.await??;
    let (_client_reader, mut client_writer) = tokio::io::split(client_app);
    let (mut server_reader, _server_writer) = tokio::io::split(server_app);
    let mut received = vec![0u8; payload.len()];
    let start = Instant::now();
    let (written, read) = tokio::join!(
        client_writer.write_all(payload.as_slice()),
        server_reader.read_exact(&mut received)
    );
    written.context("client write")?;
    read.context("server read")?;
    let elapsed = start.elapsed();
    client_pump.abort();
    server_pump.abort();
    Ok(elapsed)
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn measure_single(
    server: &ServerCfg,
    client: &ClientCfg,
    payload: &Arc<Vec<u8>>,
) -> Result<(f64, f64)> {
    let rt = Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    let mut handshake_us = Vec::with_capacity(ROUNDS);
    let mut transfer_s = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let hs = rt.block_on(async {
            tokio::time::timeout(
                TIMEOUT,
                handshake_over_tcp(Arc::clone(server), Arc::clone(client)),
            )
            .await
        })??;
        handshake_us.push(hs.as_secs_f64() * 1e6);
        let d = rt.block_on(async {
            tokio::time::timeout(
                TIMEOUT,
                transfer_over_tcp(Arc::clone(server), Arc::clone(client), Arc::clone(payload)),
            )
            .await
        })??;
        transfer_s.push(d.as_secs_f64());
    }
    Ok((median(handshake_us), median(transfer_s)))
}

fn measure_ladder(
    concurrency: usize,
    server: &ServerCfg,
    client: &ClientCfg,
    payload: &Arc<Vec<u8>>,
) -> Result<f64> {
    let rt = Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    let mut samples = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let elapsed = rt.block_on(async {
            tokio::time::timeout(TIMEOUT, async {
                let start = Instant::now();
                let mut handles = Vec::with_capacity(concurrency);
                for _ in 0..concurrency {
                    handles.push(tokio::spawn(transfer_over_tcp(
                        Arc::clone(server),
                        Arc::clone(client),
                        Arc::clone(payload),
                    )));
                }
                for handle in handles {
                    handle.await??;
                }
                Ok::<Duration, anyhow::Error>(start.elapsed())
            })
            .await
            .context("ladder round timed out")?
        })?;
        samples.push(elapsed.as_secs_f64());
    }
    Ok(median(samples))
}

fn main() -> Result<()> {
    let (server, client) = config_pair()?;
    let payload = Arc::new(vec![0xABu8; PAYLOAD_BYTES]);

    println!("C0e / L3 loopback benchmark - 127.0.0.1 TCP, no public network IO");
    println!(
        "payload={} MiB  rounds={}  ladder={:?}",
        PAYLOAD_BYTES >> 20,
        ROUNDS,
        LADDER
    );
    println!("{}", "-".repeat(84));

    let (hs_us, single_s) = measure_single(&server, &client, &payload)?;
    println!("W11 connect + handshake       {hs_us:>10.1} us");
    println!(
        "W12 1 MiB single session      {:>10.2} ms   {:>8.1} MiB/s",
        single_s * 1e3,
        1.0 / single_s
    );

    println!("{}", "-".repeat(84));
    println!("W13 connection ladder (4 workers, median of {ROUNDS})");
    for concurrency in LADDER {
        let s = measure_ladder(concurrency, &server, &client, &payload)?;
        println!(
            "  {concurrency:>2} sessions  {:>8.2} ms  {:>8.1} MiB/s",
            s * 1e3,
            concurrency as f64 / s
        );
    }
    Ok(())
}
