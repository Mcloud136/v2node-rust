//! 黑盒契约测试：只经公开 API（配置串 → 握手 → 应用流）验证双端协议行为。
//! 与 src 内联单测互补：单测可构造私有字段，这里模拟真实调用方的使用方式。

use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use v2node_proto::encryption::{ClientEncryptionConfig, EncryptionServer};
use x25519_dalek::{PublicKey, StaticSecret};

const ENCODER: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

struct KeyPair {
    decryption: String,
    encryption: String,
}

fn key_pair(mode: &str, padding: Option<&str>) -> KeyPair {
    let secret = StaticSecret::random();
    let public = PublicKey::from(&secret);
    let private_b64 = ENCODER.encode(secret.to_bytes());
    let public_b64 = ENCODER.encode(public.to_bytes());
    let mut server = format!("mlkem768x25519plus.{mode}.600s");
    let mut client = format!("mlkem768x25519plus.{mode}.1rtt");
    if let Some(padding) = padding {
        server.push('.');
        server.push_str(padding);
        client.push('.');
        client.push_str(padding);
    }
    server.push('.');
    server.push_str(&private_b64);
    client.push('.');
    client.push_str(&public_b64);
    KeyPair {
        decryption: server,
        encryption: client,
    }
}

async fn handshake_pair(
    keys: &KeyPair,
) -> (
    tokio::io::DuplexStream,
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
) {
    let server = EncryptionServer::from_decryption(&keys.decryption)
        .expect("valid decryption string")
        .expect("encryption enabled");
    let client_config =
        ClientEncryptionConfig::parse(&keys.encryption).expect("valid encryption string");
    let (client_io, server_io) = tokio::io::duplex(65536);
    let server_task = tokio::spawn(async move { server.handshake(server_io).await });
    let (client_app, client_pump) = client_handshake_with_timeout(client_io, &client_config).await;
    let (server_app, server_pump) = server_task
        .await
        .expect("server task join")
        .expect("server handshake");
    (client_app, server_app, client_pump, server_pump)
}

async fn client_handshake_with_timeout(
    io: tokio::io::DuplexStream,
    config: &ClientEncryptionConfig,
) -> (tokio::io::DuplexStream, tokio::task::JoinHandle<()>) {
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        v2node_proto::encryption::client_handshake(io, config),
    )
    .await
    .expect("client handshake timeout")
    .expect("client handshake")
}

#[tokio::test]
async fn three_modes_roundtrip_via_public_config_strings() {
    for mode in ["native", "xorpub", "random"] {
        let keys = key_pair(mode, None);
        let (client_app, server_app, client_pump, server_pump) = handshake_pair(&keys).await;
        let (mut client_reader, mut client_writer) = tokio::io::split(client_app);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_app);

        let up = format!("up-{mode}");
        client_writer.write_all(up.as_bytes()).await.unwrap();
        let mut buffer = vec![0u8; up.len()];
        server_reader.read_exact(&mut buffer).await.unwrap();
        assert_eq!(buffer, up.as_bytes(), "mode {mode} upstream");

        let down = format!("down-{mode}");
        server_writer.write_all(down.as_bytes()).await.unwrap();
        let mut buffer = vec![0u8; down.len()];
        client_reader.read_exact(&mut buffer).await.unwrap();
        assert_eq!(buffer, down.as_bytes(), "mode {mode} downstream");

        client_pump.abort();
        server_pump.abort();
    }
}

#[tokio::test]
async fn custom_padding_roundtrip() {
    let keys = key_pair("random", Some("100-111-1111.75-0-111.50-0-3333"));
    let (client_app, server_app, client_pump, server_pump) = handshake_pair(&keys).await;
    let (mut client_reader, mut client_writer) = tokio::io::split(client_app);
    let (mut server_reader, mut server_writer) = tokio::io::split(server_app);
    client_writer.write_all(b"padded").await.unwrap();
    let mut buffer = [0u8; 6];
    server_reader.read_exact(&mut buffer).await.unwrap();
    assert_eq!(&buffer, b"padded");
    server_writer.write_all(b"reply").await.unwrap();
    let mut buffer = [0u8; 5];
    client_reader.read_exact(&mut buffer).await.unwrap();
    assert_eq!(&buffer, b"reply");
    client_pump.abort();
    server_pump.abort();
}

#[tokio::test]
async fn large_transfer_crosses_record_boundaries() {
    let keys = key_pair("native", None);
    let (client_app, server_app, client_pump, server_pump) = handshake_pair(&keys).await;
    let (mut client_reader, mut client_writer) = tokio::io::split(client_app);
    let (mut server_reader, mut server_writer) = tokio::io::split(server_app);

    // 300 KiB > 8192B record size, exercises fragmentation and reassembly.
    let payload: Vec<u8> = (0..300 * 1024).map(|i| (i % 251) as u8).collect();
    let expected = payload.clone();
    let writer = tokio::spawn(async move {
        client_writer.write_all(&payload).await.unwrap();
        client_writer.shutdown().await.unwrap();
    });
    let mut received = Vec::new();
    server_reader.read_to_end(&mut received).await.unwrap();
    writer.await.unwrap();
    assert_eq!(received.len(), expected.len());
    assert_eq!(received, expected);

    // Reverse direction after EOF drain must still work.
    server_writer.write_all(b"tail").await.unwrap();
    let mut buffer = [0u8; 4];
    client_reader.read_exact(&mut buffer).await.unwrap();
    assert_eq!(&buffer, b"tail");
    client_pump.abort();
    server_pump.abort();
}

#[tokio::test]
async fn mismatched_server_key_fails_handshake() {
    let server_keys = key_pair("native", None);
    let wrong_keys = key_pair("native", None);
    let server = EncryptionServer::from_decryption(&server_keys.decryption)
        .unwrap()
        .unwrap();
    let client_config = ClientEncryptionConfig::parse(&wrong_keys.encryption).unwrap();
    let (client_io, server_io) = tokio::io::duplex(65536);
    let server_task = tokio::spawn(async move { server.handshake(server_io).await });
    let client_result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        v2node_proto::encryption::client_handshake(client_io, &client_config),
    )
    .await
    .expect("handshake should fail fast, not hang");
    assert!(
        client_result.is_err(),
        "wrong key must not complete handshake"
    );
    let server_result = server_task.await.unwrap();
    assert!(
        server_result.is_err(),
        "server must reject wrong-key client"
    );
}

#[tokio::test]
async fn mode_mismatch_fails_handshake() {
    let server_keys = key_pair("native", None);
    let client_keys = key_pair("xorpub", None);
    // Same server key material, different mode: reuse server public from server_keys.
    let server_public = server_keys.encryption.rsplit('.').next().unwrap();
    let mismatched = format!("mlkem768x25519plus.xorpub.1rtt.{server_public}");
    let _ = client_keys;
    let server = EncryptionServer::from_decryption(&server_keys.decryption)
        .unwrap()
        .unwrap();
    let client_config = ClientEncryptionConfig::parse(&mismatched).unwrap();
    let (client_io, server_io) = tokio::io::duplex(65536);
    let server_task = tokio::spawn(async move { server.handshake(server_io).await });
    let client_result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        v2node_proto::encryption::client_handshake(client_io, &client_config),
    )
    .await
    .expect("handshake should fail fast, not hang");
    assert!(client_result.is_err(), "mode mismatch must not complete");
    assert!(server_task.await.unwrap().is_err());
}

#[tokio::test]
async fn truncated_client_hello_fails() {
    let keys = key_pair("native", None);
    let server = EncryptionServer::from_decryption(&keys.decryption)
        .unwrap()
        .unwrap();
    let (mut client_io, server_io) = tokio::io::duplex(65536);
    let server_task = tokio::spawn(async move { server.handshake(server_io).await });
    // IV + public key only, then EOF: server must error, not hang.
    client_io.write_all(&[0u8; 48]).await.unwrap();
    client_io.shutdown().await.unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), server_task)
        .await
        .expect("server should fail fast")
        .unwrap();
    assert!(result.is_err());
}

#[test]
fn empty_and_none_disable_encryption() {
    assert!(EncryptionServer::from_decryption("").unwrap().is_none());
    assert!(EncryptionServer::from_decryption("none").unwrap().is_none());
    assert!(
        EncryptionServer::from_decryption("  none  ")
            .unwrap()
            .is_none()
    );
}

#[test]
fn invalid_config_strings_are_rejected() {
    for bad in [
        "mlkem768x25519plus.native.600s",                  // too few parts
        "unknownsuite.native.600s.AAAA",                   // wrong suite
        "mlkem768x25519plus.bogus.600s.AAAA",              // wrong mode
        "mlkem768x25519plus.native.notanumber.AAAA",       // bad ticket
        "mlkem768x25519plus.native.600s.!!!not-base64!!!", // bad key encoding
        "mlkem768x25519plus.native.600s.AAAA",             // key too short
    ] {
        assert!(
            EncryptionServer::from_decryption(bad).is_err(),
            "server accepted {bad:?}"
        );
    }
    for bad in [
        "mlkem768x25519plus.native.1rtt",                  // too few parts
        "unknownsuite.native.1rtt.AAAA",                   // wrong suite
        "mlkem768x25519plus.bogus.1rtt.AAAA",              // wrong mode
        "mlkem768x25519plus.native.1rtt.!!!not-base64!!!", // bad key encoding
        "mlkem768x25519plus.native.1rtt.AAAA",             // key too short
    ] {
        assert!(
            ClientEncryptionConfig::parse(bad).is_err(),
            "client accepted {bad:?}"
        );
    }
}

#[test]
fn derive_key_matches_known_answer() {
    // Cross-checked against the blake3 crate in unit tests; this pins the
    // public API contract for external callers.
    let key = v2node_proto::encryption::derive_key_raw(b"VLESS", &[7u8; 32]).unwrap();
    let expected = blake3::derive_key("VLESS", &[7u8; 32]);
    assert_eq!(key, expected);
}
