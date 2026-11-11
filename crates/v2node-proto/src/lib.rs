//! VLESS 协议层：节点与未来客户端共用的线协议原语。
//!
//! C0b 第一步只包含双向 VLESS Encryption 实现（服务端入站 + 客户端出站，
//! 见 `encryption::EncryptionServer` 与 `encryption::client_handshake`）。
//! `vless.rs` / `tls.rs` 仍持有 `NodeState` / `RuntimeConfig` / `NodeInfo`
//! 等节点状态，需先完成角色解耦（帧编解码与计量/限速分离）后再迁入，
//! 见 `docs/client-design-plan.md` §4。
//!
//! 本 crate 不依赖任何面板或节点业务类型，可被独立复用。

pub mod encryption;
