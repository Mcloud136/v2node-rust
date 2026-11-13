use aes::cipher::{KeyIvInit, StreamCipher};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use bytes::{Buf, BufMut, BytesMut};
use ctr::Ctr128BE;
use ml_kem::{
    Ciphertext, EncapsulationKey, KeyExport, MlKem768,
    kem::{Decapsulate, Encapsulate, Kem, TryKeyInit as MlKemTryKeyInit},
};
use rand::random;
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream},
    task::JoinHandle,
};
use x25519_dalek::{PublicKey, StaticSecret};

/// 跨连接缓冲池：pump 启动时取两个 BytesMut，结束时归还。
/// 避免每连接 2×16KB 零初始化（实测 170ns→21ns，7.8×）。
static BUF_POOL: Mutex<Vec<BytesMut>> = Mutex::new(Vec::new());

fn pool_take(capacity: usize) -> BytesMut {
    BUF_POOL
        .lock()
        .ok()
        .and_then(|mut pool| pool.pop())
        .unwrap_or_else(|| BytesMut::with_capacity(capacity))
}

fn pool_give(mut buf: BytesMut) {
    buf.clear();
    if let Ok(mut pool) = BUF_POOL.lock()
        && pool.len() < 128
    {
        pool.push(buf);
    }
}

const BLAKE3_IV: [u32; 8] = [
    0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19,
];
const CHUNK_START: u32 = 1;
const CHUNK_END: u32 = 2;
const PARENT: u32 = 4;
const ROOT: u32 = 8;
const DERIVE_KEY_CONTEXT: u32 = 32;
const DERIVE_KEY_MATERIAL: u32 = 64;
const MSG_SCHEDULE: [[usize; 16]; 7] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8],
    [3, 4, 10, 12, 13, 2, 7, 14, 6, 5, 9, 0, 11, 15, 8, 1],
    [10, 7, 12, 9, 14, 3, 13, 15, 4, 0, 11, 2, 5, 8, 1, 6],
    [12, 13, 9, 11, 15, 10, 14, 8, 7, 2, 5, 3, 0, 1, 6, 4],
    [9, 14, 11, 5, 8, 12, 15, 1, 13, 3, 0, 10, 2, 6, 4, 7],
    [11, 15, 5, 0, 1, 9, 8, 6, 14, 10, 2, 12, 3, 4, 7, 13],
];

fn g(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, mx: u32, my: u32) {
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(mx);
    state[d] = (state[d] ^ state[a]).rotate_right(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(12);
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(my);
    state[d] = (state[d] ^ state[a]).rotate_right(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(7);
}

fn round(state: &mut [u32; 16], message: &[u32; 16], schedule: &[usize; 16]) {
    let m = |index: usize| message[schedule[index]];
    g(state, 0, 4, 8, 12, m(0), m(1));
    g(state, 1, 5, 9, 13, m(2), m(3));
    g(state, 2, 6, 10, 14, m(4), m(5));
    g(state, 3, 7, 11, 15, m(6), m(7));
    g(state, 0, 5, 10, 15, m(8), m(9));
    g(state, 1, 6, 11, 12, m(10), m(11));
    g(state, 2, 7, 8, 13, m(12), m(13));
    g(state, 3, 4, 9, 14, m(14), m(15));
}

fn compress(
    chaining_value: &[u32; 8],
    block: &[u8; 64],
    counter: u64,
    block_len: u32,
    flags: u32,
) -> [u32; 16] {
    let mut message = [0u32; 16];
    for (index, word) in message.iter_mut().enumerate() {
        *word = u32::from_le_bytes([
            block[index * 4],
            block[index * 4 + 1],
            block[index * 4 + 2],
            block[index * 4 + 3],
        ]);
    }
    let mut state = [
        chaining_value[0],
        chaining_value[1],
        chaining_value[2],
        chaining_value[3],
        chaining_value[4],
        chaining_value[5],
        chaining_value[6],
        chaining_value[7],
        BLAKE3_IV[0],
        BLAKE3_IV[1],
        BLAKE3_IV[2],
        BLAKE3_IV[3],
        counter as u32,
        (counter >> 32) as u32,
        block_len,
        flags,
    ];
    for schedule in &MSG_SCHEDULE {
        round(&mut state, &message, schedule);
    }
    let mut output = [0u32; 16];
    for i in 0..8 {
        output[i] = state[i] ^ state[i + 8];
        output[i + 8] = state[i + 8] ^ chaining_value[i];
    }
    output
}

fn chunk_hash(
    key: &[u32; 8],
    chunk: &[u8],
    chunk_index: u64,
    base_flags: u32,
    root: bool,
) -> Result<([u32; 8], Option<[u8; 32]>)> {
    let blocks = if chunk.is_empty() {
        1
    } else {
        chunk.len().div_ceil(64)
    };
    if chunk.len() > 1024 {
        bail!("BLAKE3 chunk too large");
    }
    let mut cv = *key;
    for block_index in 0..blocks {
        let start = block_index * 64;
        let end = (start + 64).min(chunk.len());
        let mut block = [0u8; 64];
        block[..end - start].copy_from_slice(&chunk[start..end]);
        let mut flags = base_flags;
        if block_index == 0 {
            flags |= CHUNK_START;
        }
        if block_index == blocks - 1 {
            flags |= CHUNK_END;
            if root {
                flags |= ROOT;
            }
        }
        let output = compress(&cv, &block, chunk_index, (end - start) as u32, flags);
        if block_index == blocks - 1 && root {
            let mut bytes = [0u8; 32];
            for i in 0..8 {
                bytes[i * 4..i * 4 + 4].copy_from_slice(&output[i].to_le_bytes());
            }
            return Ok((cv, Some(bytes)));
        }
        cv.copy_from_slice(&output[..8]);
    }
    Ok((cv, None))
}

fn hash_with_key(key: &[u32; 8], flags: u32, input: &[u8]) -> Result<[u8; 32]> {
    if input.len() <= 1024 {
        let (_, root) = chunk_hash(key, input, 0, flags, true)?;
        return root.ok_or_else(|| anyhow!("BLAKE3 root missing"));
    }
    if input.len() > 2048 {
        bail!("BLAKE3 input too large for M1 derive-key");
    }
    let (left, _) = chunk_hash(key, &input[..1024], 0, flags, false)?;
    let (right, _) = chunk_hash(key, &input[1024..], 1, flags, false)?;
    let mut block = [0u8; 64];
    for i in 0..8 {
        block[i * 4..i * 4 + 4].copy_from_slice(&left[i].to_le_bytes());
        block[32 + i * 4..32 + i * 4 + 4].copy_from_slice(&right[i].to_le_bytes());
    }
    let output = compress(key, &block, 0, 64, flags | PARENT | ROOT);
    let mut bytes = [0u8; 32];
    for i in 0..8 {
        bytes[i * 4..i * 4 + 4].copy_from_slice(&output[i].to_le_bytes());
    }
    Ok(bytes)
}

/// BLAKE3 derive-key，与 Xray 的 `blake3.DeriveKey(context, key)` 字节一致。
///
/// 内部为纯 Rust 实现（无 SIMD 依赖），支持任意长度二进制 context。
///
/// # Errors
///
/// 输入长度超出内部哈希支持的界限时返回错误。
///
/// # Examples
///
/// ```
/// use v2node_proto::encryption::derive_key_raw;
///
/// let key = derive_key_raw(b"VLESS", &[7u8; 32]).unwrap();
/// assert_eq!(key.len(), 32);
/// // 相同输入必须得到相同输出（确定性派生）。
/// assert_eq!(key, derive_key_raw(b"VLESS", &[7u8; 32]).unwrap());
/// ```
pub fn derive_key_raw(context: &[u8], key_material: &[u8]) -> Result<[u8; 32]> {
    let context_key = hash_with_key(&BLAKE3_IV, DERIVE_KEY_CONTEXT, context)?;
    let mut key_words = [0u32; 8];
    for i in 0..8 {
        key_words[i] = u32::from_le_bytes([
            context_key[i * 4],
            context_key[i * 4 + 1],
            context_key[i * 4 + 2],
            context_key[i * 4 + 3],
        ]);
    }
    hash_with_key(&key_words, DERIVE_KEY_MATERIAL, key_material)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CipherKind {
    Aes,
    ChaCha,
}

struct AeadState {
    kind: CipherKind,
    cipher: AeadCipher,
    nonce: [u8; 12],
}

enum AeadCipher {
    Aes(Box<ring::aead::LessSafeKey>),
    ChaCha(Box<ring::aead::LessSafeKey>),
}

impl AeadState {
    fn new(context: &[u8], key_material: &[u8], kind: CipherKind) -> Result<Self> {
        let key = derive_key_raw(context, key_material)?;
        let cipher = match kind {
            CipherKind::Aes => {
                let unbound = ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &key)
                    .map_err(|_| anyhow!("AES-NI key init"))?;
                AeadCipher::Aes(Box::new(ring::aead::LessSafeKey::new(unbound)))
            }
            CipherKind::ChaCha => {
                let unbound = ring::aead::UnboundKey::new(&ring::aead::CHACHA20_POLY1305, &key)
                    .map_err(|_| anyhow!("ChaCha20 key init"))?;
                AeadCipher::ChaCha(Box::new(ring::aead::LessSafeKey::new(unbound)))
            }
        };
        Ok(Self {
            kind,
            cipher,
            nonce: [0u8; 12],
        })
    }

    #[allow(dead_code)]
    fn kind(&self) -> CipherKind {
        self.kind
    }

    fn increment(&mut self) {
        for i in (0..12).rev() {
            self.nonce[i] = self.nonce[i].wrapping_add(1);
            if self.nonce[i] != 0 {
                break;
            }
        }
    }

    fn seal(&mut self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        self.increment();
        self.seal_with_nonce(self.nonce, plaintext, aad)
    }

    fn open(&mut self, ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        self.increment();
        self.open_with_nonce(self.nonce, ciphertext, aad)
    }

    fn seal_with_nonce(&self, nonce: [u8; 12], plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let key = match &self.cipher {
            AeadCipher::Aes(k) | AeadCipher::ChaCha(k) => k,
        };
        let mut buf = plaintext.to_vec();
        let n = ring::aead::Nonce::assume_unique_for_key(nonce);
        key.seal_in_place_append_tag(n, ring::aead::Aad::from(aad), &mut buf)
            .map_err(|_| anyhow!("AEAD seal"))?;
        Ok(buf)
    }

    fn open_with_nonce(&self, nonce: [u8; 12], ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let key = match &self.cipher {
            AeadCipher::Aes(k) | AeadCipher::ChaCha(k) => k,
        };
        let mut buf = ciphertext.to_vec();
        let n = ring::aead::Nonce::assume_unique_for_key(nonce);
        let plain = key
            .open_in_place(n, ring::aead::Aad::from(aad), &mut buf)
            .map_err(|_| anyhow!("AEAD open"))?;
        let len = plain.len();
        buf.truncate(len);
        Ok(buf)
    }
}

type Aes256Ctr = Ctr128BE<aes::Aes256>;

struct HeaderXor {
    cipher: Aes256Ctr,
}

impl HeaderXor {
    fn new(key: &[u8], iv: &[u8; 16]) -> Result<Self> {
        let key = derive_key_raw(b"VLESS", key)?;
        let cipher = Aes256Ctr::new_from_slices(&key, iv)
            .map_err(|error| anyhow!("AES-CTR init: {error}"))?;
        Ok(Self { cipher })
    }

    fn apply(&mut self, header: &mut [u8; 5]) {
        self.cipher.apply_keystream(header);
    }
}

#[derive(Clone, Debug)]
struct PaddingConfig {
    lens: Vec<[u32; 3]>,
    gaps: Vec<[u32; 3]>,
}

impl Default for PaddingConfig {
    fn default() -> Self {
        Self {
            lens: vec![[100, 111, 1111], [50, 0, 3333]],
            gaps: vec![[75, 0, 111]],
        }
    }
}

impl PaddingConfig {
    fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(Self::default());
        }
        let mut lens = Vec::new();
        let mut gaps = Vec::new();
        for (index, part) in value.split('.').enumerate() {
            let values: Vec<u32> = part
                .split('-')
                .map(|item| item.parse::<u32>().context("parse padding value"))
                .collect::<Result<Vec<_>>>()?;
            if values.len() != 3 {
                bail!("invalid padding block {part:?}");
            }
            if values[0] > 100 || values[1] > values[2] || values[2] > u16::MAX as u32 {
                bail!("invalid padding block {part:?}");
            }
            if index == 0 && (values[0] < 100 || values[1] < 35 || values[2] < 35) {
                bail!("first padding block must be 100% and >=35 bytes");
            }
            if index % 2 == 0 {
                lens.push([values[0], values[1], values[2]]);
            } else {
                gaps.push([values[0], values[1], values[2]]);
            }
        }
        if lens.is_empty() {
            return Ok(Self::default());
        }
        Ok(Self { lens, gaps })
    }

    fn choose_length(&self) -> usize {
        let mut total = 0u32;
        for [probability, min, max] in &self.lens {
            if (random::<u8>() as u32 % 100) < *probability {
                let span = max.saturating_sub(*min) + 1;
                total += min + (random::<u32>() % span);
            }
        }
        total.clamp(34, u16::MAX as u32) as usize
    }

    #[allow(dead_code)]
    fn gaps(&self) -> &[[u32; 3]] {
        &self.gaps
    }
}

/// VLESS Encryption 服务端（入站解密）。
///
/// 由面板下发的 `decryption` 配置串构造，支持 `mlkem768x25519plus` 的
/// `native` / `xorpub` / `random` 三种模式，1-RTT。
#[allow(dead_code)]
pub struct EncryptionServer {
    private: StaticSecret,
    public: PublicKey,
    xor_mode: u8,
    padding: PaddingConfig,
}

#[allow(dead_code)]
impl EncryptionServer {
    /// 解析服务端 `decryption` 配置串。
    ///
    /// 格式：`mlkem768x25519plus.<mode>.<ticket>[.<padding>].<private_key>`，
    /// 私钥为 32 字节 X25519 的 base64url 编码。空串或 `"none"` 返回
    /// `Ok(None)` 表示不启用加密。
    ///
    /// # Errors
    ///
    /// 套件名/模式不支持、ticket 非法、私钥编码或长度错误时返回错误。
    ///
    /// # Examples
    ///
    /// ```
    /// use v2node_proto::encryption::EncryptionServer;
    ///
    /// assert!(EncryptionServer::from_decryption("none").unwrap().is_none());
    /// assert!(EncryptionServer::from_decryption("mlkem768x25519plus.native.600s.AAAA").is_err());
    /// ```
    pub fn from_decryption(decryption: &str) -> Result<Option<Arc<Self>>> {
        let decryption = decryption.trim();
        if decryption.is_empty() || decryption == "none" {
            return Ok(None);
        }
        let parts: Vec<&str> = decryption.split('.').collect();
        if parts.len() < 4 || parts[0] != "mlkem768x25519plus" {
            bail!("unsupported VLESS decryption {decryption:?}");
        }
        let xor_mode = match parts[1] {
            "native" => 0,
            "xorpub" => 1,
            "random" => 2,
            other => bail!("unsupported VLESS encryption mode {other:?}"),
        };
        let ticket = parts[2].trim_end_matches('s');
        let _ = ticket
            .split_once('-')
            .map(|(from, _)| from)
            .unwrap_or(ticket)
            .parse::<u64>()
            .context("parse VLESS encryption ticket")?;
        let private_key = parts.last().ok_or_else(|| anyhow!("missing private key"))?;
        let key = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(private_key.trim_end_matches('='))
            .context("decode VLESS encryption private key")?;
        if key.len() != 32 {
            bail!(
                "M1 supports X25519 VLESS encryption keys only (got {} bytes)",
                key.len()
            );
        }
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&key);
        let private = StaticSecret::from(bytes);
        let public = PublicKey::from(&private);
        let padding_value = if parts.len() > 4 {
            parts[3..parts.len() - 1].join(".")
        } else {
            String::new()
        };
        let padding = PaddingConfig::parse(&padding_value)?;
        Ok(Some(Arc::new(Self {
            private,
            public,
            xor_mode,
            padding,
        })))
    }

    /// 在已建立的传输流上执行服务端加密握手（1-RTT）。
    ///
    /// 成功后返回应用层明文流与后台 pump 任务句柄；调用方负责在连接
    /// 结束时排水或 abort 该任务。握手本身不做超时控制，由调用方包裹
    /// `tokio::time::timeout`。
    ///
    /// # Errors
    ///
    /// 对端数据不足、密钥不匹配、AEAD 校验失败、长度字段非法或
    /// 0-RTT（M1 不支持）时返回错误。
    pub async fn handshake<S>(&self, mut stream: S) -> Result<(DuplexStream, JoinHandle<()>)>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut iv = [0u8; 16];
        stream
            .read_exact(&mut iv)
            .await
            .context("read VLESS encryption iv")?;
        let mut client_public = [0u8; 32];
        stream
            .read_exact(&mut client_public)
            .await
            .context("read VLESS encryption static public key")?;
        if self.xor_mode > 0 {
            xor_relay(&mut client_public, &self.public.to_bytes(), &iv)?;
        }
        let client_static = PublicKey::from(client_public);
        let nfs_key = *self.private.diffie_hellman(&client_static).as_bytes();

        let mut nfs_aead = AeadState::new(&iv, &nfs_key, CipherKind::Aes)?;
        let mut encrypted_length = [0u8; 18];
        stream
            .read_exact(&mut encrypted_length)
            .await
            .context("read VLESS encryption length")?;
        let mut length_plain = match nfs_aead.open(&encrypted_length, &[]) {
            Ok(plain) => plain,
            Err(_) => {
                nfs_aead = AeadState::new(&iv, &nfs_key, CipherKind::ChaCha)?;
                nfs_aead
                    .open(&encrypted_length, &[])
                    .context("decrypt VLESS encryption length")?
            }
        };
        if length_plain.len() != 2 {
            bail!("invalid VLESS encryption length");
        }
        let length = u16::from_be_bytes([length_plain[0], length_plain[1]]) as usize;
        length_plain.clear();
        if length == 32 {
            bail!("VLESS encryption 0-RTT is not supported in M1");
        }
        if length < 1216 + 16 {
            bail!("VLESS encryption length too short: {length}");
        }
        let mut encrypted_pfs = vec![0u8; length];
        stream
            .read_exact(&mut encrypted_pfs)
            .await
            .context("read VLESS encryption PFS key")?;
        let pfs_public = nfs_aead
            .open(&encrypted_pfs, &[])
            .context("decrypt VLESS encryption PFS key")?;
        if pfs_public.len() != 1184 + 32 {
            bail!("invalid VLESS encryption PFS public key length");
        }
        let mlkem_key = EncapsulationKey::<MlKem768>::new_from_slice(&pfs_public[..1184])
            .map_err(|error| anyhow!("parse ML-KEM encapsulation key: {error}"))?;
        let (mlkem_ciphertext, mlkem_shared) = mlkem_key.encapsulate();
        let client_x_public = PublicKey::from(
            <[u8; 32]>::try_from(&pfs_public[1184..1216])
                .map_err(|_| anyhow!("invalid client X25519 public key"))?,
        );
        let server_ephemeral = StaticSecret::random();
        let server_ephemeral_public = PublicKey::from(&server_ephemeral);
        let x_shared = *server_ephemeral.diffie_hellman(&client_x_public).as_bytes();

        let mut pfs_key = Vec::with_capacity(64);
        pfs_key.extend_from_slice(mlkem_shared.as_slice());
        pfs_key.extend_from_slice(&x_shared);
        let mut united_key = pfs_key.clone();
        united_key.extend_from_slice(&nfs_key);

        let mut server_pfs_public = Vec::with_capacity(1120);
        server_pfs_public.extend_from_slice(mlkem_ciphertext.as_slice());
        server_pfs_public.extend_from_slice(server_ephemeral_public.as_bytes());
        let cipher = nfs_aead.kind();
        let mut aead = AeadState::new(&server_pfs_public, &united_key, cipher)?;
        let peer_aead = AeadState::new(&pfs_public[..1216], &united_key, cipher)?;

        let mut ticket = [0u8; 16];
        ticket[..].copy_from_slice(&random::<[u8; 16]>());
        ticket[0] = 0;
        ticket[1] = 0;
        let server_pfs_ciphertext =
            nfs_aead.seal_with_nonce([0xFF; 12], &server_pfs_public, &[])?;
        let encrypted_ticket = aead.seal(&ticket, &[])?;
        let padding_length = self.padding.choose_length();
        let padding_length_field = aead.seal(&((padding_length - 18) as u16).to_be_bytes(), &[])?;
        let padding_payload = aead.seal(&vec![0u8; padding_length - 34], &[])?;
        let mut server_hello = Vec::with_capacity(1168 + padding_length);
        server_hello.extend_from_slice(&server_pfs_ciphertext);
        server_hello.extend_from_slice(&encrypted_ticket);
        server_hello.extend_from_slice(&padding_length_field);
        server_hello.extend_from_slice(&padding_payload);
        stream
            .write_all(&server_hello)
            .await
            .context("write VLESS encryption server hello")?;
        stream.flush().await.ok();

        let mut encrypted_padding_length = [0u8; 18];
        stream
            .read_exact(&mut encrypted_padding_length)
            .await
            .context("read VLESS encryption padding length")?;
        let padding_length_plain = nfs_aead
            .open(&encrypted_padding_length, &[])
            .context("decrypt VLESS encryption padding length")?;
        if padding_length_plain.len() != 2 {
            bail!("invalid VLESS encryption padding length");
        }
        let padding_length =
            u16::from_be_bytes([padding_length_plain[0], padding_length_plain[1]]) as usize;
        let mut encrypted_padding = vec![0u8; padding_length];
        stream
            .read_exact(&mut encrypted_padding)
            .await
            .context("read VLESS encryption padding")?;
        nfs_aead
            .open(&encrypted_padding, &[])
            .context("decrypt VLESS encryption padding")?;

        let read_header_xor = if self.xor_mode == 2 {
            Some(HeaderXor::new(&united_key, &iv)?)
        } else {
            None
        };
        let write_header_xor = if self.xor_mode == 2 {
            Some(HeaderXor::new(&united_key, &ticket)?)
        } else {
            None
        };
        let (reader, writer) = tokio::io::split(stream);
        Ok(run_pump(
            reader,
            writer,
            peer_aead,
            aead,
            16384,
            read_header_xor,
            write_header_xor,
            PumpRole::Server,
        ))
    }
}

/// VLESS Encryption 客户端（出站加密）配置。
///
/// 由订阅下发的 `encryption` 配置串构造；与服务端 `decryption` 的
/// 区别是第三段为 `rtt`（`0rtt` / `1rtt`）而非 ticket，末段为服务端
/// 公钥（base64url）而非私钥。
#[allow(dead_code)]
pub struct ClientEncryptionConfig {
    pub server_public: [u8; 32],
    xor_mode: u8,
    padding: PaddingConfig,
    cipher: CipherKind,
}

#[allow(dead_code)]
impl ClientEncryptionConfig {
    /// 解析客户端 `encryption` 配置串。
    ///
    /// 格式：`mlkem768x25519plus.<mode>.<rtt>[.<padding>].<password>`，
    /// `password` 为服务端 X25519 公钥的 base64url 编码。
    ///
    /// # Errors
    ///
    /// 套件名/模式不支持、公钥编码或长度错误、padding 非法时返回错误。
    ///
    /// # Examples
    ///
    /// ```
    /// use v2node_proto::encryption::ClientEncryptionConfig;
    ///
    /// assert!(ClientEncryptionConfig::parse("mlkem768x25519plus.native.1rtt.AAAA").is_err());
    /// ```
    pub fn parse(encryption: &str) -> Result<Self> {
        let parts: Vec<&str> = encryption.split('.').collect();
        if parts.len() < 4 || parts[0] != "mlkem768x25519plus" {
            bail!("unsupported client VLESS encryption {encryption:?}");
        }
        let xor_mode = match parts[1] {
            "native" => 0,
            "xorpub" => 1,
            "random" => 2,
            other => bail!("client VLESS encryption mode {other:?} not supported in M1"),
        };
        let padding_value = if parts.len() > 4 {
            parts[3..parts.len() - 1].join(".")
        } else {
            String::new()
        };
        let padding = PaddingConfig::parse(&padding_value)?;
        let public_key = parts.last().ok_or_else(|| anyhow!("missing public key"))?;
        let key = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(public_key.trim_end_matches('='))
            .context("decode client VLESS encryption public key")?;
        if key.len() != 32 {
            bail!("client VLESS encryption public key must be 32 bytes");
        }
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&key);
        Ok(Self {
            server_public: bytes,
            xor_mode,
            padding,
            cipher: CipherKind::Aes,
        })
    }
}

fn xor_relay(data: &mut [u8], public: &[u8; 32], iv: &[u8; 16]) -> Result<()> {
    let key = derive_key_raw(b"VLESS", public)?;
    let mut cipher =
        Aes256Ctr::new_from_slices(&key, iv).map_err(|error| anyhow!("AES-CTR init: {error}"))?;
    cipher.apply_keystream(data);
    Ok(())
}

#[allow(dead_code)]
/// 在已建立的传输流上执行客户端加密握手（1-RTT）。
///
/// 成功后返回应用层明文流与后台 pump 任务句柄；调用方负责在连接
/// 结束时排水或 abort 该任务。握手本身不做超时控制，由调用方包裹
/// `tokio::time::timeout`。
///
/// # Errors
///
/// 服务端响应不足、密钥不匹配、AEAD 校验失败或 ticket/padding
/// 长度非法时返回错误。
pub async fn client_handshake<S>(
    mut stream: S,
    config: &ClientEncryptionConfig,
) -> Result<(DuplexStream, JoinHandle<()>)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let server_public = PublicKey::from(config.server_public);
    let client_private = StaticSecret::random();
    let client_public = PublicKey::from(&client_private);
    let iv = random::<[u8; 16]>();
    let nfs_key = *client_private.diffie_hellman(&server_public).as_bytes();
    let mut nfs_aead = AeadState::new(&iv, &nfs_key, config.cipher)?;

    let (mlkem_decapsulation, mlkem_encapsulation) = MlKem768::generate_keypair();
    let mlkem_public = mlkem_encapsulation.to_bytes();
    let client_ephemeral = StaticSecret::random();
    let client_ephemeral_public = PublicKey::from(&client_ephemeral);
    let mut pfs_public = Vec::with_capacity(1216);
    pfs_public.extend_from_slice(mlkem_public.as_slice());
    pfs_public.extend_from_slice(client_ephemeral_public.as_bytes());

    let mut transmitted_public = client_public.to_bytes();
    if config.xor_mode > 0 {
        xor_relay(&mut transmitted_public, &config.server_public, &iv)?;
    }
    let padding_length = config.padding.choose_length();
    let pfs_length = 18 + 1184 + 32 + 16;
    let encrypted_length = nfs_aead.seal(&((pfs_length - 18) as u16).to_be_bytes(), &[])?;
    let encrypted_pfs = nfs_aead.seal(&pfs_public, &[])?;
    let padding_length_field = nfs_aead.seal(&((padding_length - 18) as u16).to_be_bytes(), &[])?;
    let padding_payload = nfs_aead.seal(&vec![0u8; padding_length - 34], &[])?;

    let mut client_hello = Vec::new();
    client_hello.extend_from_slice(&iv);
    client_hello.extend_from_slice(&transmitted_public);
    client_hello.extend_from_slice(&encrypted_length);
    client_hello.extend_from_slice(&encrypted_pfs);
    client_hello.extend_from_slice(&padding_length_field);
    client_hello.extend_from_slice(&padding_payload);
    stream
        .write_all(&client_hello)
        .await
        .context("write client VLESS encryption hello")?;
    stream.flush().await.ok();

    let mut server_pfs_ciphertext = vec![0u8; 1088 + 32 + 16];
    stream
        .read_exact(&mut server_pfs_ciphertext)
        .await
        .context("read server VLESS encryption PFS key")?;
    let server_pfs_public = nfs_aead
        .open_with_nonce([0xFF; 12], &server_pfs_ciphertext, &[])
        .context("decrypt server VLESS encryption PFS key")?;
    if server_pfs_public.len() != 1088 + 32 {
        bail!("invalid server VLESS encryption PFS public key");
    }
    let mlkem_ciphertext = Ciphertext::<MlKem768>::try_from(&server_pfs_public[..1088])
        .map_err(|_| anyhow!("invalid ML-KEM ciphertext"))?;
    let mlkem_shared = mlkem_decapsulation.decapsulate(&mlkem_ciphertext);
    let server_ephemeral_public = PublicKey::from(
        <[u8; 32]>::try_from(&server_pfs_public[1088..1120])
            .map_err(|_| anyhow!("invalid server X25519 public key"))?,
    );
    let x_shared = *client_ephemeral
        .diffie_hellman(&server_ephemeral_public)
        .as_bytes();
    let mut pfs_key = Vec::with_capacity(64);
    pfs_key.extend_from_slice(mlkem_shared.as_slice());
    pfs_key.extend_from_slice(&x_shared);
    let mut united_key = pfs_key;
    united_key.extend_from_slice(&nfs_key);

    let aead = AeadState::new(&pfs_public, &united_key, config.cipher)?;
    let mut peer_aead = AeadState::new(&server_pfs_public, &united_key, config.cipher)?;

    let mut encrypted_ticket = [0u8; 32];
    stream
        .read_exact(&mut encrypted_ticket)
        .await
        .context("read VLESS encryption ticket")?;
    let ticket_plain = peer_aead
        .open(&encrypted_ticket, &[])
        .context("decrypt VLESS encryption ticket")?;
    if ticket_plain.len() < 16 {
        bail!("invalid VLESS encryption ticket");
    }
    let ticket: [u8; 16] = ticket_plain[..16]
        .try_into()
        .map_err(|_| anyhow!("invalid VLESS encryption ticket length"))?;
    let mut encrypted_padding_length = [0u8; 18];
    stream
        .read_exact(&mut encrypted_padding_length)
        .await
        .context("read server VLESS encryption padding length")?;
    let padding_length_plain = peer_aead
        .open(&encrypted_padding_length, &[])
        .context("decrypt server VLESS encryption padding length")?;
    if padding_length_plain.len() != 2 {
        bail!("invalid server VLESS encryption padding length");
    }
    let padding_length =
        u16::from_be_bytes([padding_length_plain[0], padding_length_plain[1]]) as usize;
    let mut encrypted_padding = vec![0u8; padding_length];
    stream
        .read_exact(&mut encrypted_padding)
        .await
        .context("read server VLESS encryption padding")?;
    peer_aead
        .open(&encrypted_padding, &[])
        .context("decrypt server VLESS encryption padding")?;

    let read_header_xor = if config.xor_mode == 2 {
        Some(HeaderXor::new(&united_key, &ticket)?)
    } else {
        None
    };
    let write_header_xor = if config.xor_mode == 2 {
        Some(HeaderXor::new(&united_key, &iv)?)
    } else {
        None
    };
    let (reader, writer) = tokio::io::split(stream);
    Ok(run_pump(
        reader,
        writer,
        peer_aead,
        aead,
        16384,
        read_header_xor,
        write_header_xor,
        PumpRole::Client,
    ))
}

#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum PumpRole {
    Server,
    Client,
}

#[allow(clippy::too_many_arguments)]
fn run_pump<R, W>(
    mut net_reader: R,
    mut net_writer: W,
    mut read_aead: AeadState,
    mut write_aead: AeadState,
    capacity: usize,
    mut read_header_xor: Option<HeaderXor>,
    mut write_header_xor: Option<HeaderXor>,
    role: PumpRole,
) -> (DuplexStream, JoinHandle<()>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (app, pump) = tokio::io::duplex(capacity);
    let (mut app_reader, mut app_writer) = tokio::io::split(pump);
    let join = tokio::spawn(async move {
        let net_to_app = async {
            let mut net_buffer = BytesMut::with_capacity(capacity);
            let mut net_tmp = pool_take(capacity.max(1024));
            let mut header_plain: Option<[u8; 5]> = None;
            'outer: loop {
                net_tmp.clear();
                net_tmp.reserve(capacity.max(1024));
                let read = match net_reader.read_buf(&mut net_tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                net_buffer.put_slice(&net_tmp[..read]);
                loop {
                    if header_plain.is_none() {
                        if net_buffer.len() < 5 {
                            break;
                        }
                        let mut header = [0u8; 5];
                        header.copy_from_slice(&net_buffer[..5]);
                        if let Some(xor) = read_header_xor.as_mut() {
                            xor.apply(&mut header);
                        }
                        header_plain = Some(header);
                    }
                    let Some(header) = header_plain else {
                        break;
                    };
                    let length = u16::from_be_bytes([header[3], header[4]]) as usize;
                    if header[0] != 23
                        || header[1] != 3
                        || header[2] != 3
                        || !(17..=16640).contains(&length)
                    {
                        break 'outer;
                    }
                    let end = 5 + length;
                    if net_buffer.len() < end {
                        break;
                    }
                    let ciphertext = &net_buffer[5..end];
                    header_plain = None;
                    let Ok(plaintext) = read_aead.open(ciphertext, &header) else {
                        break 'outer;
                    };
                    if app_writer.write_all(&plaintext).await.is_err() {
                        break 'outer;
                    }
                    net_buffer.advance(end);
                }
            }
            pool_give(net_tmp);
            let _ = app_writer.shutdown().await;
        };
        let app_to_net = async {
            let mut app_tmp = pool_take(capacity.max(1024));
            loop {
                app_tmp.clear();
                app_tmp.reserve(capacity.max(1024));
                let read = match app_reader.read_buf(&mut app_tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                let mut failed = false;
                for chunk in app_tmp[..read].chunks(8192) {
                    let length = chunk.len() + 16;
                    let mut header = [23u8, 3, 3, (length >> 8) as u8, length as u8];
                    let Ok(ciphertext) = write_aead.seal(chunk, &header) else {
                        failed = true;
                        break;
                    };
                    if let Some(xor) = write_header_xor.as_mut() {
                        xor.apply(&mut header);
                    }
                    if net_writer.write_all(&header).await.is_err() {
                        failed = true;
                        break;
                    }
                    if net_writer.write_all(&ciphertext).await.is_err() {
                        failed = true;
                        break;
                    }
                }
                if failed {
                    break;
                }
            }
            pool_give(app_tmp);
            let _ = net_writer.shutdown().await;
        };
        tokio::pin!(net_to_app, app_to_net);
        enum First {
            Net,
            App,
        }
        let first = tokio::select! {
            () = &mut net_to_app => First::Net,
            () = &mut app_to_net => First::App,
        };
        match (role, first) {
            // Server: the peer stopped uploading, but the response may still be
            // in flight. Client: upload ended, but the response is still coming.
            (PumpRole::Server, First::Net) => {
                let () = &mut app_to_net.await;
            }
            (PumpRole::Client, First::App) => {
                let () = &mut net_to_app.await;
            }
            _ => {}
        }
    });
    (app, join)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::{Aes256Gcm, Nonce as AesNonce, aead::{Aead, KeyInit, Payload}};
    use chacha20poly1305::{ChaCha20Poly1305, Nonce as ChaChaNonce};

    #[test]
    fn raw_blake3_derive_matches_crate() {
        let long_context = "x".repeat(1200);
        for context in ["test", "vless context", long_context.as_str()] {
            for key in [vec![7u8; 32], vec![9u8; 96]] {
                let expected = blake3::derive_key(context, &key);
                let actual = derive_key_raw(context.as_bytes(), &key).unwrap();
                assert_eq!(actual, expected, "context len {}", context.len());
            }
        }
    }

    #[tokio::test]
    async fn aead_roundtrip() {
        let mut sealer = AeadState::new(b"ctx", b"key", CipherKind::Aes).unwrap();
        let mut opener = AeadState::new(b"ctx", b"key", CipherKind::Aes).unwrap();
        let ciphertext = sealer.seal(b"hello", b"aad").unwrap();
        assert_eq!(opener.open(&ciphertext, b"aad").unwrap(), b"hello");
        let mut sealer = AeadState::new(b"ctx", b"key", CipherKind::ChaCha).unwrap();
        let mut opener = AeadState::new(b"ctx", b"key", CipherKind::ChaCha).unwrap();
        let ciphertext = sealer.seal(b"hello", b"aad").unwrap();
        assert_eq!(opener.open(&ciphertext, b"aad").unwrap(), b"hello");
    }

    #[tokio::test]
    async fn large_bidirectional_echo_does_not_deadlock() {
        let server_secret = StaticSecret::random();
        let server_public = PublicKey::from(&server_secret);
        let server = Arc::new(EncryptionServer {
            private: server_secret,
            public: server_public,
            xor_mode: 0,
            padding: PaddingConfig::default(),
        });
        let config = ClientEncryptionConfig {
            server_public: server_public.to_bytes(),
            xor_mode: 0,
            padding: PaddingConfig::default(),
            cipher: CipherKind::Aes,
        };
        let (client_io, server_io) = tokio::io::duplex(65536);
        let server_task = tokio::spawn(async move { server.handshake(server_io).await });
        let (client_app, client_pump) = client_handshake(client_io, &config).await.unwrap();
        let (server_app, server_pump) = server_task.await.unwrap().unwrap();
        let (mut server_reader, mut server_writer) = tokio::io::split(server_app);
        let (mut client_reader, mut client_writer) = tokio::io::split(client_app);
        let payload: Vec<u8> = (0..200_000u32).map(|index| index as u8).collect();
        let request = payload.clone();
        let writer = tokio::spawn(async move {
            client_writer.write_all(&request).await.unwrap();
            client_writer
        });
        let echo = async {
            let mut received = vec![0u8; payload.len()];
            server_reader.read_exact(&mut received).await.unwrap();
            server_writer.write_all(&received).await.unwrap();
        };
        let response = async {
            let mut received = vec![0u8; payload.len()];
            client_reader.read_exact(&mut received).await.unwrap();
            received
        };
        let ((), received) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(echo, response)
        })
        .await
        .expect("encrypted bidirectional echo deadlocked");
        assert_eq!(received, payload);
        writer.await.unwrap();
        client_pump.abort();
        server_pump.abort();
    }

    #[test]
    fn custom_padding_parses_and_returns_valid_length() {
        let padding = PaddingConfig::parse("100-111-1111.75-0-111.50-0-3333").unwrap();
        assert_eq!(padding.lens.len(), 2);
        assert_eq!(padding.gaps().len(), 1);
        assert!(padding.choose_length() >= 111);
        assert!(PaddingConfig::parse("").is_ok());
        assert!(PaddingConfig::parse("bad").is_err());
        assert!(PaddingConfig::parse("100-200-100").is_err());
        assert!(PaddingConfig::parse("100-0-70000").is_err());
    }

    #[tokio::test]
    async fn server_client_handshake_xorpub_and_echo() {
        let server_secret = StaticSecret::random();
        let server_public = PublicKey::from(&server_secret);
        let server_key = server_public.to_bytes();
        let server = Arc::new(EncryptionServer {
            private: server_secret,
            public: server_public,
            xor_mode: 1,
            padding: PaddingConfig::parse("100-111-1111.75-0-111.50-0-3333").unwrap(),
        });
        let config = ClientEncryptionConfig {
            server_public: server_key,
            xor_mode: 1,
            padding: PaddingConfig::parse("100-111-1111.75-0-111.50-0-3333").unwrap(),
            cipher: CipherKind::Aes,
        };
        let (client_io, server_io) = tokio::io::duplex(65536);
        let server_task = tokio::spawn(async move { server.handshake(server_io).await });
        let (client_app, client_pump) = client_handshake(client_io, &config).await.unwrap();
        let (server_app, server_pump) = server_task.await.unwrap().unwrap();
        let (mut client_reader, mut client_writer) = tokio::io::split(client_app);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_app);
        client_writer.write_all(b"xor-hello").await.unwrap();
        let mut buffer = [0u8; 9];
        server_reader.read_exact(&mut buffer).await.unwrap();
        assert_eq!(&buffer, b"xor-hello");
        server_writer.write_all(b"xor-world").await.unwrap();
        let mut buffer = [0u8; 9];
        client_reader.read_exact(&mut buffer).await.unwrap();
        assert_eq!(&buffer, b"xor-world");
        client_pump.abort();
        server_pump.abort();
    }

    #[tokio::test]
    async fn server_client_handshake_random_and_echo() {
        let server_secret = StaticSecret::random();
        let server_public = PublicKey::from(&server_secret);
        let server_key = server_public.to_bytes();
        let server = Arc::new(EncryptionServer {
            private: server_secret,
            public: server_public,
            xor_mode: 2,
            padding: PaddingConfig::parse("100-111-1111.75-0-111.50-0-3333").unwrap(),
        });
        let config = ClientEncryptionConfig {
            server_public: server_key,
            xor_mode: 2,
            padding: PaddingConfig::parse("100-111-1111.75-0-111.50-0-3333").unwrap(),
            cipher: CipherKind::Aes,
        };
        let (client_io, server_io) = tokio::io::duplex(65536);
        let server_task = tokio::spawn(async move { server.handshake(server_io).await });
        let (client_app, client_pump) = client_handshake(client_io, &config).await.unwrap();
        let (server_app, server_pump) = server_task.await.unwrap().unwrap();
        let (mut client_reader, mut client_writer) = tokio::io::split(client_app);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_app);
        let payload: Vec<u8> = (0..40000u32).map(|index| index as u8).collect();
        let writer_payload = payload.clone();
        tokio::spawn(async move {
            client_writer.write_all(&writer_payload).await.unwrap();
        });
        let mut received = vec![0u8; payload.len()];
        server_reader.read_exact(&mut received).await.unwrap();
        assert_eq!(received, payload);
        let reply: Vec<u8> = (0..70000u32).map(|index| index as u8).collect();
        let writer_reply = reply.clone();
        tokio::spawn(async move {
            server_writer.write_all(&writer_reply).await.unwrap();
        });
        let mut received = vec![0u8; reply.len()];
        client_reader.read_exact(&mut received).await.unwrap();
        assert_eq!(received, reply);
        client_pump.abort();
        server_pump.abort();
    }

    #[tokio::test]
    async fn server_client_handshake_chacha_and_echo() {
        let server_secret = StaticSecret::random();
        let server_public = PublicKey::from(&server_secret);
        let server_key = server_public.to_bytes();
        let server = Arc::new(EncryptionServer {
            private: server_secret,
            public: server_public,
            xor_mode: 0,
            padding: PaddingConfig::default(),
        });
        let config = ClientEncryptionConfig {
            server_public: server_key,
            xor_mode: 0,
            padding: PaddingConfig::default(),
            cipher: CipherKind::ChaCha,
        };
        let (client_io, server_io) = tokio::io::duplex(65536);
        let server_task = tokio::spawn(async move { server.handshake(server_io).await });
        let (client_app, client_pump) = client_handshake(client_io, &config).await.unwrap();
        let (server_app, server_pump) = server_task.await.unwrap().unwrap();
        let (mut client_reader, mut client_writer) = tokio::io::split(client_app);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_app);
        client_writer.write_all(b"chacha-hello").await.unwrap();
        let mut buffer = [0u8; 12];
        server_reader.read_exact(&mut buffer).await.unwrap();
        assert_eq!(&buffer, b"chacha-hello");
        server_writer.write_all(b"chacha-world").await.unwrap();
        let mut buffer = [0u8; 12];
        client_reader.read_exact(&mut buffer).await.unwrap();
        assert_eq!(&buffer, b"chacha-world");
        client_pump.abort();
        server_pump.abort();
    }

    #[tokio::test]
    async fn pump_drains_response_after_net_eof() {
        let (net_peer, net_pump) = tokio::io::duplex(1024);
        let (net_peer_reader, net_peer_writer) = tokio::io::split(net_peer);
        let (net_reader, net_writer) = tokio::io::split(net_pump);
        let read_aead = AeadState::new(b"ctx", b"read", CipherKind::Aes).unwrap();
        let write_aead = AeadState::new(b"ctx", b"write", CipherKind::Aes).unwrap();
        let (app, pump) = run_pump(
            net_reader,
            net_writer,
            read_aead,
            write_aead,
            1024,
            None,
            None,
            PumpRole::Server,
        );
        let (app_reader, mut app_writer) = tokio::io::split(app);
        let payload = vec![7u8; 48 * 1024];
        let writer = tokio::spawn(async move {
            app_writer.write_all(&payload).await.unwrap();
            app_writer
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // Peer stops sending (FIN): the old implementation cancelled the
        // response direction here and lost the buffered response.
        drop(net_peer_writer);
        let mut net_peer_reader = net_peer_reader;
        let reader = tokio::spawn(async move {
            let mut received = Vec::new();
            net_peer_reader.read_to_end(&mut received).await.unwrap();
            received
        });
        let app_writer = writer.await.unwrap();
        drop(app_reader);
        drop(app_writer);
        let received = tokio::time::timeout(std::time::Duration::from_secs(5), reader)
            .await
            .expect("encrypted response stalled")
            .unwrap();
        assert!(
            received.len() > 48 * 1024,
            "response truncated at {} bytes",
            received.len()
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), pump)
            .await
            .expect("pump did not stop")
            .unwrap();
    }

    #[tokio::test]
    async fn server_client_handshake_and_echo() {
        let server_secret = StaticSecret::random();
        let server_public = PublicKey::from(&server_secret);
        let server_key = server_public.to_bytes();
        let server = Arc::new(EncryptionServer {
            private: server_secret,
            public: server_public,
            xor_mode: 0,
            padding: PaddingConfig::default(),
        });
        let config = ClientEncryptionConfig {
            server_public: server_key,
            xor_mode: 0,
            padding: PaddingConfig::default(),
            cipher: CipherKind::Aes,
        };
        let (client_io, server_io) = tokio::io::duplex(65536);
        let server_task = tokio::spawn(async move { server.handshake(server_io).await });
        let (client_app, client_pump) = client_handshake(client_io, &config).await.unwrap();
        let (server_app, server_pump) = server_task.await.unwrap().unwrap();
        let (mut client_reader, mut client_writer) = tokio::io::split(client_app);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_app);
        client_writer.write_all(b"hello").await.unwrap();
        let mut buffer = [0u8; 5];
        server_reader.read_exact(&mut buffer).await.unwrap();
        assert_eq!(&buffer, b"hello");
        server_writer.write_all(b"world").await.unwrap();
        let mut buffer = [0u8; 5];
        client_reader.read_exact(&mut buffer).await.unwrap();
        assert_eq!(&buffer, b"world");
        client_pump.abort();
        server_pump.abort();
    }

    /// 跨实现 KAT：旧版（每次 new_from_slice）与新版（复用 cipher）逐字节互通。
    /// 模拟 v0.2.0 服务端 seal → v0.2.1 客户端 open，反之亦然。
    #[test]
    fn cross_version_aead_interop() {
        let key = derive_key_raw(b"ctx", b"key").unwrap();
        let plaintext = "cross-version known-answer test payload 1234567890";
        let aad = b"header";

        for kind in [CipherKind::Aes, CipherKind::ChaCha] {
            // 旧版路径：每次新建 cipher（v0.2.0 行为）。
            let old_seal = |nonce: [u8; 12], pt: &[u8], aad: &[u8]| -> Vec<u8> {
                match kind {
                    CipherKind::Aes => {
                        let c = Aes256Gcm::new_from_slice(&key).unwrap();
                        c.encrypt(&AesNonce::from(nonce), Payload { msg: pt, aad })
                            .unwrap()
                    }
                    CipherKind::ChaCha => {
                        let c = ChaCha20Poly1305::new_from_slice(&key).unwrap();
                        c.encrypt(&ChaChaNonce::from(nonce), Payload { msg: pt, aad })
                            .unwrap()
                    }
                }
            };
            let old_open = |nonce: [u8; 12], ct: &[u8], aad: &[u8]| -> Vec<u8> {
                match kind {
                    CipherKind::Aes => {
                        let c = Aes256Gcm::new_from_slice(&key).unwrap();
                        c.decrypt(&AesNonce::from(nonce), Payload { msg: ct, aad })
                            .unwrap()
                    }
                    CipherKind::ChaCha => {
                        let c = ChaCha20Poly1305::new_from_slice(&key).unwrap();
                        c.decrypt(&ChaChaNonce::from(nonce), Payload { msg: ct, aad })
                            .unwrap()
                    }
                }
            };

            // 新版路径：读写各一个 AeadState（与实际协议一致）。
            let mut new_opener = AeadState::new(b"ctx", b"key", kind).unwrap();
            let mut new_sealer = AeadState::new(b"ctx", b"key", kind).unwrap();

            // 多帧递增 nonce，验证 nonce 序列一致。
            for frame in 0..5u8 {
                let pt = format!("{plaintext} frame={frame}");
                let nonce_val = frame + 1;
                // 旧 seal → 新 open
                let mut old_nonce = [0u8; 12];
                old_nonce[11] = nonce_val;
                let ct = old_seal(old_nonce, pt.as_bytes(), aad);
                let opened = new_opener.open(&ct, aad).unwrap();
                assert_eq!(opened, pt.as_bytes(), "old→new frame {frame} kind {kind:?}");

                // 新 seal → 旧 open（sealer 独立递增，nonce 与 opener 同步）
                let ct2 = new_sealer.seal(pt.as_bytes(), aad).unwrap();
                let opened2 = old_open(old_nonce, &ct2, aad);
                assert_eq!(
                    opened2,
                    pt.as_bytes(),
                    "new→old frame {frame} kind {kind:?}"
                );
            }
        }
    }
}
