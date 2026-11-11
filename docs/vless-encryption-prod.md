# VLESS Encryption 生产配置方法（M1 服务端能力，暂不开启）

> 决策（2026-11-10）：VLESS Encryption 纳入 M1 服务端实现，但**生产环境不默认开启**。全部 M1 工作完成并通过测试后，由 AI 主动提示用户按本文档配置；未经用户明确确认，不修改生产节点。

## 1. 结论

- **协议层兼容现有 `VLESS + WebSocket + TLS`**：`decryption` / `encryption` 是 VLESS 自己的字段，与传输层无关，可以继续走 WS、TLS、Cloudflare、ECH。
- **不能在同一入站上同时兼容加密与非加密客户端**：VLESS 入站的 `decryption` 是单一配置，启用后旧客户端会握手失败。
- 因此生产启用必须走**新增独立节点/端口/订阅 + 单用户灰度**，不要直接改现有 40 条 VLESS/WS 节点。

## 2. 当前生产事实（2026-11-10）

- 40 条 `vless + tls=1 + ws`，主节点 id 1/3/10/30；监听 `127.0.0.1:10001/10001/10001/10004`，WS `/cdn-api`，`cert_mode=none`。
- 客户端到 Cloudflare 是 TLS 1.3 + ECH；CF → cloudflared → 源站是明文 WS，所以 CF 边缘技术上能看到内层 WS/VLESS。
- 生产 `v2_server_v2node.encryption` 全部为空，`encryption_settings` 为空；没有任何节点启用 ACME。
- 面板订阅生成：
  - ClashMeta / ClashVerge / ClashNyanpasu / QuantumultX **会**生成 VLESS `encryption` 字段。
  - Singbox / SingboxOld **没有**生成该字段。

## 3. 服务端/客户端字符串格式

Xray 官方定义：

- 服务端入站 `decryption`：
  ```
  mlkem768x25519plus.<mode>.<ticket>[.<server_padding>].<private_key>
  ```
- 客户端出站 `encryption`：
  ```
  mlkem768x25519plus.<mode>.<rtt>[.<client_padding>].<password>
  ```

字段含义：

| 字段 | 说明 |
|---|---|
| `mode` | `native` / `xorpub` / `random`，两端必须一致 |
| `ticket` | 服务端会话恢复票据有效期，如 `600s`、`100-500s`；`0s` 表示禁用 |
| `rtt` | 客户端 `0rtt`（尝试票据快速连接）或 `1rtt`（强制完整握手） |
| `server_padding` / `client_padding` | 可选 padding/delay 方案；缺省时核心使用默认 padding。面板管理端未填写时会把字段存为 `null`，服务端按空值处理（与 Go 版行为一致） |
| `private_key` | 面板生成的服务端 X25519 私钥（base64url） |
| `password` | 面板生成的客户端公钥（base64url） |

v2node 的 `buildVLess` 构造方式（[core/inbound.go:198](file:///D:/wxmuma/v2node/core/inbound.go#L198)）：

```
parts = [ "mlkem768x25519plus", mode, ticket ]
if server_padding != "" { parts.push(server_padding) }
parts.push(private_key)
decryption = parts.join(".")
```

Rust 服务端必须按同样顺序拼接，不能把 `rtt` 当成服务端字段。

## 4. 面板配置方法（生产启用时）

1. 进入 V2Board 管理端 → 服务器 → v2node → **编辑节点**。
2. 找到“编辑加密配置”抽屉（`encryption_settings`）。
3. 选择 `mlkem768x25519plus`。
4. 默认值（面板控制器行为）：
   - `mode = native`
   - 未填 `rtt`：`rtt = 0rtt`，`ticket = 600s`
   - `rtt = 1rtt`：`ticket = 0s`
   - 自动生成 `private_key`（服务端私钥）和 `password`（客户端公钥），二者必须成对，不要手工只改一个。
5. 可选：设置 `client_padding` / `server_padding`；不设置时核心自动使用默认 padding。
6. 保存后，节点配置接口 `/api/v2/server/config` 会下发 `encryption` 和 `encryption_settings`；Rust 节点按第 3 节格式解析。

面板代码依据：
- 面板生成 `private_key` / `password` / `mode` / `rtt` / `ticket`：[V2nodeController.php:129](file:///D:/wxmuma/v2board/app/Http/Controllers/V1/Admin/Server/V2nodeController.php#L129)
- Clash 系订阅生成 `encryption`：[ClashMeta.php:321](file:///D:/wxmuma/v2board/app/Protocols/ClashMeta.php#L321)
- Singbox 订阅未生成 `encryption`：[Singbox.php:217](file:///D:/wxmuma/v2board/app/Protocols/Singbox/Singbox.php#L217)

## 5. 客户端支持矩阵

| 客户端/内核 | 订阅是否下发 encryption | 实际支持 | 备注 |
|---|---|---|---|
| ClashMeta / Mihomo | 是 | 需按版本实测 | 面板已生成字段，不代表旧版 Mihomo 支持 |
| Clash Verge / Clash Verge Rev | 是 | 需按版本实测 | 内核是 Mihomo，版本决定能力 |
| ClashNyanpasu | 是 | 需按版本实测 | 同上 |
| QuantumultX | 是 | 需按版本实测 | 面板有生成逻辑 |
| Sing-box / Sing-boxOld | **否** | 需手动配置/版本确认 | 面板订阅不会带该字段，直接导入会失败或降级 |
| Xray-core / v2rayN | 官方支持 | 是 | 官方文档有 VLESS Encryption 字段 |
| Shadowrocket / Loon | 未知 | 必须实测 | 不要假设支持 |

**硬要求**：启用前用真实客户端版本做一次握手测试；面板能生成字段 ≠ 客户端内核能解析。

## 6. 灰度上线步骤

1. **复制一个新节点**（不要改现有 40 条）：
   - 新 `v2_server_v2node` 行，新 node id、新 `server_port`（例如 10005）、新域名/订阅入口。
   - WS 路径、Host、TLS/`cert_mode` 按该节点实际配置。
2. 在新节点上启用 `mlkem768x25519plus`，保存并确认 `/api/v2/server/config` 返回 `encryption` / `encryption_settings`。
3. 只给一个测试用户/测试分组开放新节点，生成独立订阅（Clash 系）。
4. 节点侧验证：
   - Rust 节点启动日志出现“VLESS encryption configured”，不打印 key/password。
   - 客户端 TCP 与 UDP 都能通；流量上报、在线 IP、限速正常。
   - 观察 CPU 与握手延迟，和未加密节点对比。
5. **回滚**：关闭新节点的 encryption，或删除新节点/订阅；现有 40 条不受影响。
6. 只有确认目标客户端覆盖率后，才逐步扩大；始终保留未加密节点给旧客户端/Sing-box 用户。

## 7. 风险与注意事项

- 两端 `mode` 必须一致，否则握手失败。
- 服务端 `ticket` 与客户端 `rtt` 语义不同，不要混用。
- `private_key` / `password` 必须成对；只改一个会导致所有客户端失败。
- padding 会增加带宽和 CPU，先小范围测。
- 不要在现有 40 条节点上直接打开；不要给 Sing-box 订阅下发加密节点。
- 不要把 `private_key`、`password` 写入日志或聊天记录。
- 全量启用前必须确认：Clash 系最低支持版本、是否存在 Sing-box/Shadowrocket 用户、回滚路径可用。

## 7.1 M1 实现边界（2026-11-10 实测）

M1 的服务端 VLESS Encryption 已实现并通过实机测试。生产制品为 static-pie musl：`/opt/v2node-rust-target/x86_64-unknown-linux-musl/release/v2node-rust`（跨 Ubuntu 20.04/Debian 10 可用）。

- 支持 `mlkem768x25519plus` 的 **`native` / `xorpub` / `random`** 三种模式，**1-RTT**。
- 静态 X25519 密钥来自面板 `private_key`；PFS 使用 ML-KEM-768 + X25519；密钥派生使用 BLAKE3 derive-key（支持二进制 context，与 Xray 的 `blake3.DeriveKey("VLESS", key)` 一致）；AEAD 优先 AES-256-GCM，失败回退 ChaCha20-Poly1305；记录使用 `23 03 03` TLS 1.3 头伪装。
- `xorpub`：对客户端静态公钥做 AES-256-CTR 异或（密钥由 BLAKE3 derive-key 从服务端公钥派生、IV 用客户端 IV）。
- `random`：在 `xorpub` 基础上，对每条记录的 5 字节 TLS 头做 AES-256-CTR 异或（上传用客户端 IV、下载用票据；AEAD AAD 仍为明文头）。
- **自定义 `server_padding` / `client_padding`**：支持 `概率%-最小-最大` 三段式，多点用 `.` 分隔，例如 `100-111-1111.75-0-111.50-0-3333`；缺省用默认 padding（111–1111 字节）。Rust 服务端只使用长度分布，不实现 `paddingGaps` 的分段延时（不影响协议互通，只影响流量时序特征）。
- 记录分片与官方一致：应用数据按 **8192 字节**一段加密封装，单记录长度限制 17–16640 字节。
- **服务端票据有效期强制为 0**，客户端不会缓存票据，因此不会发起 0-RTT；客户端配置 `rtt=0rtt` 或 `1rtt` 都会走 1-RTT。生产面板建议显式设置 `rtt=1rtt`。
- **暂不支持**：0-RTT 会话恢复、ML-KEM 静态密钥（64/1184 字节密钥；面板只生成 X25519 32 字节密钥）。Xray 服务端多级 relay 才需要 ML-KEM 静态密钥，当前 v2node 生产链路不涉及。
- **半关闭语义**：加密泵已按服务端/客户端角色在单向 EOF 后继续排空另一方向的剩余数据，避免响应尾部被截断；但 WebSocket（RFC 6455）没有 TCP 半关闭语义，客户端发送 WS Close 即代表整条连接结束，这是传输层限制，不是加密层问题。
- 已实测（`tests/e2e_encryption.sh`）：native、xorpub+自定义 padding、random（TCP/UDP、100KB 多记录）、关闭加密热切换、旧加密客户端被拒绝、PID 不变。
- **官方 Xray 互通已实测**（`tests/interop_xray.sh`，Xray v26.11.9 linux-amd64）：
  - Rust 节点作服务端 + 官方 Xray 作客户端：三种模式均通过 200KB 下载 + 100KB 上传校验。
  - 官方 Xray 作服务端 + Rust `vless_probe` 作客户端：三种模式均通过 100KB TCP 回显 + UDP 回显。

## 8. 完成提示（给 AI 的硬约束）

当 M1/M1.1 全部代码、测试、文档完成，且用户确认可以进入生产时：

1. 主动提示用户“VLESS Encryption 服务端能力已完成，可以按 `docs/vless-encryption-prod.md` 配置生产”。
2. 只提示，不擅自修改生产节点；等用户明确确认后再操作。
3. 提示时同时给出：灰度节点建议、客户端版本检查点、回滚方法。
