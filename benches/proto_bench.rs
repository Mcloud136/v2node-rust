//! 正式基准：L1 微基准（criterion）+ 逐次采样 P50/P99/P99.9 探针。
//!
//! 口径见 `docs/benchmark-spec.md`：纯函数、无 IO、无网络，只测代码执行时间。
//! 运行：`cargo bench --bench proto_bench`
//!
//! criterion 提供 mean / median / std dev / MAD；自定义 `probe` 补齐规范 §2
//! 要求的逐次分布分位数。单次 `Instant` 计时约 20–30 ns，亚微秒项仅作上界参考。

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use base64::Engine;
use criterion::{Criterion, criterion_group, criterion_main};
use std::{hint::black_box, time::Instant};
use v2node_proto::encryption::{ClientEncryptionConfig, EncryptionServer, derive_key_raw};

const RECORD: usize = 8_192;
const PROBE_SAMPLES: usize = 5_000;
const PROBE_WARMUP: usize = 500;

struct Fixture {
    client_cfg: String,
    server_cfg: String,
    material: [u8; 32],
    key: [u8; 32],
    nonce_bytes: [u8; 12],
    plaintext: Vec<u8>,
    ciphertext: Vec<u8>,
}

fn fixtures() -> Fixture {
    let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let key_b64 = encoder.encode([7u8; 32]);
    let aad: &[u8] = &[];
    let cipher = Aes256Gcm::new_from_slice(&[3u8; 32]).expect("valid AES-256 key");
    let plaintext = vec![0xABu8; RECORD];
    let ciphertext = cipher
        .encrypt(
            &Nonce::from([0u8; 12]),
            Payload {
                msg: &plaintext,
                aad,
            },
        )
        .expect("seal baseline");
    Fixture {
        client_cfg: format!("mlkem768x25519plus.native.0rtt.{key_b64}"),
        server_cfg: format!("mlkem768x25519plus.native.600s.{key_b64}"),
        material: [9u8; 32],
        key: [3u8; 32],
        nonce_bytes: [0u8; 12],
        plaintext,
        ciphertext,
    }
}

fn percentile(mut values: Vec<f64>, p: f64) -> f64 {
    values.sort_by(f64::total_cmp);
    let idx = ((values.len() - 1) as f64 * p).round() as usize;
    values[idx]
}

fn probe<T, F>(name: &str, mut f: F)
where
    F: FnMut() -> T,
{
    for _ in 0..PROBE_WARMUP {
        let _ = black_box(f());
    }
    let mut samples = Vec::with_capacity(PROBE_SAMPLES);
    for _ in 0..PROBE_SAMPLES {
        let start = Instant::now();
        let _ = black_box(f());
        samples.push(start.elapsed().as_nanos() as f64);
    }
    let p50 = percentile(samples.clone(), 0.50);
    let p99 = percentile(samples.clone(), 0.99);
    let p999 = percentile(samples, 0.999);
    println!(
        "P99PROBE {name:<34} p50={p50:>8.1}ns p99={p99:>8.1}ns p999={p999:>8.1}ns n={PROBE_SAMPLES}"
    );
}

fn probe_all() {
    let f = fixtures();
    let cipher = Aes256Gcm::new_from_slice(&f.key).expect("valid AES-256 key");
    let nonce = Nonce::from(f.nonce_bytes);
    let aad: &[u8] = &[];
    println!("=== per-iteration percentile probe (timer overhead ~20-30ns) ===");

    probe("W1 derive_key_raw", || {
        derive_key_raw(b"VLESS", &f.material).expect("derive")
    });
    probe("W2 ClientEncryptionConfig::parse", || {
        ClientEncryptionConfig::parse(&f.client_cfg).expect("parse")
    });
    probe("W3 EncryptionServer::from_decryption", || {
        EncryptionServer::from_decryption(&f.server_cfg).expect("from_decryption")
    });
    probe("W4 AES-256-GCM seal 8192B", || {
        cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: &f.plaintext,
                    aad,
                },
            )
            .expect("seal")
    });
    probe("W5 AES-256-GCM open 8192B", || {
        cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: &f.ciphertext,
                    aad,
                },
            )
            .expect("open")
    });
    probe("W6 pump-style alloc+copy 8192B", || {
        let mut out = Vec::with_capacity(RECORD);
        for chunk in f.plaintext.chunks(RECORD) {
            let mut buf = vec![0u8; chunk.len()];
            buf.copy_from_slice(chunk);
            out.extend_from_slice(&buf);
        }
        out.len()
    });
    println!("=== criterion groups ===");
}

fn bench_l1(c: &mut Criterion) {
    probe_all();
    let f = fixtures();
    let cipher = Aes256Gcm::new_from_slice(&f.key).expect("valid AES-256 key");
    let nonce = Nonce::from(f.nonce_bytes);
    let aad: &[u8] = &[];

    c.bench_function("W1 derive_key_raw", |b| {
        b.iter(|| black_box(derive_key_raw(b"VLESS", &f.material).expect("derive")))
    });
    c.bench_function("W2 ClientEncryptionConfig::parse", |b| {
        b.iter(|| black_box(ClientEncryptionConfig::parse(&f.client_cfg).expect("parse")))
    });
    c.bench_function("W3 EncryptionServer::from_decryption", |b| {
        b.iter(|| {
            black_box(EncryptionServer::from_decryption(&f.server_cfg).expect("from_decryption"))
        })
    });
    c.bench_function("W4 AES-256-GCM seal 8192B", |b| {
        b.iter(|| {
            black_box(
                cipher
                    .encrypt(
                        &nonce,
                        Payload {
                            msg: &f.plaintext,
                            aad,
                        },
                    )
                    .expect("seal"),
            )
        })
    });
    c.bench_function("W5 AES-256-GCM open 8192B", |b| {
        b.iter(|| {
            black_box(
                cipher
                    .decrypt(
                        &nonce,
                        Payload {
                            msg: &f.ciphertext,
                            aad,
                        },
                    )
                    .expect("open"),
            )
        })
    });
    // AES-NI 对照组（ring，与生产 AeadState 同路径）。
    let ni_key = ring::aead::LessSafeKey::new(
        ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &f.key).expect("NI key"),
    );
    c.bench_function("W4-NI AES-256-GCM seal 8192B (ring)", |b| {
        b.iter(|| {
            let mut buf = f.plaintext.clone();
            ni_key
                .seal_in_place_append_tag(
                    ring::aead::Nonce::assume_unique_for_key(f.nonce_bytes),
                    ring::aead::Aad::empty(),
                    &mut buf,
                )
                .expect("NI seal");
            black_box(buf)
        })
    });
    c.bench_function("W5-NI AES-256-GCM open 8192B (ring)", |b| {
        b.iter(|| {
            let mut buf = f.ciphertext.clone();
            let plain = ni_key
                .open_in_place(
                    ring::aead::Nonce::assume_unique_for_key(f.nonce_bytes),
                    ring::aead::Aad::empty(),
                    &mut buf,
                )
                .expect("NI open");
            black_box(plain.len())
        })
    });
    c.bench_function("W6 pump-style alloc+copy 8192B", |b| {
        b.iter(|| {
            let mut out = Vec::with_capacity(RECORD);
            for chunk in f.plaintext.chunks(RECORD) {
                let mut buf = vec![0u8; chunk.len()];
                buf.copy_from_slice(chunk);
                out.extend_from_slice(&buf);
            }
            black_box(out.len())
        })
    });
}

criterion_group!(proto_benches, bench_l1);
criterion_main!(proto_benches);
