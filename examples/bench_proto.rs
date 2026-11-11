//! C0e / L1 微基准：代码执行效率（零外部依赖 harness）。
//!
//! 口径见 `docs/benchmark-spec.md`：只测代码执行时间，不含任何网络 IO。
//! 运行：`cargo run --release --example bench_proto`
//!
//! 本层覆盖四维中的「延迟」与「吞吐」；内存与多线程能力由 L2 / L3 层测量。
//! 协议实现来自 `v2node-proto` crate（C0b 已抽出），节点与客户端共用同一份代码。

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use base64::Engine;
use std::{
    env::consts::ARCH,
    hint::black_box,
    time::{Duration, Instant},
};
use v2node_proto::encryption::{ClientEncryptionConfig, EncryptionServer, derive_key_raw};

const WARMUP: usize = 2_000;
const ITERS: usize = 20_000;
const RECORD: usize = 8_192;

fn measure<T, F>(iters: usize, mut f: F) -> (Duration, f64)
where
    F: FnMut() -> T,
{
    for _ in 0..WARMUP {
        let _ = black_box(f());
    }
    let start = Instant::now();
    for _ in 0..iters {
        let _ = black_box(f());
    }
    let elapsed = start.elapsed();
    let ns_per_op = elapsed.as_nanos() as f64 / iters as f64;
    (elapsed, ns_per_op)
}

fn report(name: &str, bytes: usize, iters: usize, elapsed: Duration, ns: f64) {
    let ops = iters as f64 / elapsed.as_secs_f64();
    let mib = ops * bytes as f64 / (1024.0 * 1024.0);
    println!("{name:<38} {ns:>9.1} ns/op {ops:>12.0} ops/s {mib:>9.1} MiB/s");
}

fn main() {
    let key_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7u8; 32]);
    let client_cfg = format!("mlkem768x25519plus.native.0rtt.{key_b64}");
    let server_cfg = format!("mlkem768x25519plus.native.600s.{key_b64}");
    let material = [9u8; 32];
    let key = [3u8; 32];
    let nonce_bytes = [0u8; 12];
    let aad: &[u8] = &[];
    let nonce = Nonce::from(nonce_bytes);
    let cipher = Aes256Gcm::new_from_slice(&key).expect("valid AES-256 key");
    let plaintext = vec![0xABu8; RECORD];
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: &plaintext,
                aad,
            },
        )
        .expect("seal baseline");

    println!("C0e / L1 micro benchmark - no network IO");
    println!("arch={ARCH} iters={ITERS} record={RECORD}B warmup={WARMUP}");
    println!("{}", "-".repeat(84));

    let (t, ns) = measure(ITERS, || {
        derive_key_raw(b"VLESS", &material).expect("derive")
    });
    report("W1 derive_key_raw (BLAKE3)", 0, ITERS, t, ns);

    let (t, ns) = measure(ITERS, || {
        ClientEncryptionConfig::parse(&client_cfg).expect("parse")
    });
    report("W2 ClientEncryptionConfig::parse", 0, ITERS, t, ns);

    let (t, ns) = measure(ITERS, || {
        EncryptionServer::from_decryption(&server_cfg).expect("from_decryption")
    });
    report("W3 EncryptionServer::from_decryption", 0, ITERS, t, ns);

    let (t, ns) = measure(ITERS, || {
        cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: &plaintext,
                    aad,
                },
            )
            .expect("seal")
    });
    report("W4 AES-256-GCM seal 8192B (crate)", RECORD, ITERS, t, ns);

    let (t, ns) = measure(ITERS, || {
        cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: &ciphertext,
                    aad,
                },
            )
            .expect("open")
    });
    report("W5 AES-256-GCM open 8192B (crate)", RECORD, ITERS, t, ns);

    let (t, ns) = measure(ITERS, || {
        let mut out = Vec::with_capacity(RECORD);
        for chunk in plaintext.chunks(RECORD) {
            let mut buf = vec![0u8; chunk.len()];
            buf.copy_from_slice(chunk);
            out.extend_from_slice(&buf);
        }
        out.len()
    });
    report("W6 pump-style alloc+copy 8192B", RECORD, ITERS, t, ns);

    println!("{}", "-".repeat(84));
    println!("W1-W3 / W6 = v2node-rust code; W4-W5 = dependency baseline.");
    println!("Memory and concurrency dimensions are measured at L2 / L3.");
}
