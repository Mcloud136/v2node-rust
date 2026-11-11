//! C0e / L2 组件基准：完整 VLESS Encryption pump（零外部依赖 harness）。
//!
//! 口径见 `docs/benchmark-spec.md`：`DuplexStream` 内存流，走完整握手与加密泵，
//! 不经任何网络 IO。覆盖四维中的「吞吐」与「多线程能力」。
//!
//! 运行：`cargo run --release --example bench_l2_pump`
//!
//! 用途：回答 L1 遗留问题——零拷贝 / 缓冲池在完整 pump 中是否值得优先优化。

use anyhow::{Context, Result};
use base64::Engine;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, duplex},
    runtime::Builder,
};
use v2node_proto::encryption::{ClientEncryptionConfig, EncryptionServer, client_handshake};
use x25519_dalek::{PublicKey, StaticSecret};

const PAYLOAD_BYTES: usize = 1 << 20;
const DUPLEX_CAP: usize = 65_536;
const ROUNDS: usize = 5;
const TIMEOUT: Duration = Duration::from_secs(30);
const CONCURRENCY: usize = 8;

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

async fn handshake_only(server: ServerCfg, client: ClientCfg) -> Result<Duration> {
    let (client_io, server_io) = duplex(DUPLEX_CAP);
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

async fn transfer(server: ServerCfg, client: ClientCfg, payload: Arc<Vec<u8>>) -> Result<Duration> {
    let (client_io, server_io) = duplex(DUPLEX_CAP);
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
    workers: usize,
    server: &ServerCfg,
    client: &ClientCfg,
    payload: &Arc<Vec<u8>>,
) -> Result<(f64, f64)> {
    let rt = Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()?;
    let mut handshake_us = Vec::with_capacity(ROUNDS);
    let mut transfer_s = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let hs = rt.block_on(async {
            tokio::time::timeout(
                TIMEOUT,
                handshake_only(Arc::clone(server), Arc::clone(client)),
            )
            .await
        })??;
        handshake_us.push(hs.as_secs_f64() * 1e6);
        let d = rt.block_on(async {
            tokio::time::timeout(
                TIMEOUT,
                transfer(Arc::clone(server), Arc::clone(client), Arc::clone(payload)),
            )
            .await
        })??;
        transfer_s.push(d.as_secs_f64());
    }
    Ok((median(handshake_us), median(transfer_s)))
}

fn measure_scaling(
    workers: usize,
    server: &ServerCfg,
    client: &ClientCfg,
    payload: &Arc<Vec<u8>>,
) -> Result<f64> {
    let rt = Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()?;
    let mut samples = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let elapsed = rt.block_on(async {
            tokio::time::timeout(TIMEOUT, async {
                let start = Instant::now();
                let mut handles = Vec::with_capacity(CONCURRENCY);
                for _ in 0..CONCURRENCY {
                    handles.push(tokio::spawn(transfer(
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
            .context("scaling round timed out")?
        })?;
        samples.push(elapsed.as_secs_f64());
    }
    Ok(median(samples))
}

fn main() -> Result<()> {
    let (server, client) = config_pair()?;
    let payload = Arc::new(vec![0xABu8; PAYLOAD_BYTES]);

    println!("C0e / L2 component benchmark - DuplexStream, no network IO");
    println!(
        "payload={} MiB  duplex_cap={}B  rounds={}  concurrency={}",
        PAYLOAD_BYTES >> 20,
        DUPLEX_CAP,
        ROUNDS,
        CONCURRENCY
    );
    println!("{}", "-".repeat(84));

    let (hs_us, single_s) = measure_single(4, &server, &client, &payload)?;
    println!("W7  handshake median          {hs_us:>10.1} us");
    println!(
        "W9  1 MiB single session      {:>10.2} ms   {:>8.1} MiB/s",
        single_s * 1e3,
        1.0 / single_s
    );

    println!("{}", "-".repeat(84));
    println!("W10 worker scaling ({CONCURRENCY} concurrent x 1 MiB, median of {ROUNDS})");
    let mut baseline: Option<f64> = None;
    for workers in [1, 2, 4, 8] {
        let s = measure_scaling(workers, &server, &client, &payload)?;
        let throughput = CONCURRENCY as f64 / s;
        let speedup = *baseline.get_or_insert(s) / s;
        println!(
            "  {workers:>2} workers  {:>8.2} ms  {:>8.1} MiB/s  speedup {:>5.2}x",
            s * 1e3,
            throughput,
            speedup
        );
    }
    Ok(())
}
