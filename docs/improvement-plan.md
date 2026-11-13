# v2node-rust 完善推进计划

> 编制日期：2026-11-14 ｜ 最近修订：2026-11-14（增量补齐：R11、基准豁免条款、归档命名）
> 事实基线：本地 `main` @ `a742bdf`（**ahead origin/main 3**）；`origin/main` @ `ea2875e`；生产四节点运行 `v0.2.0`
> 适用范围：本仓库（节点端 + `v2node-proto`），不含面板与独立客户端仓库
> 进度载体：本文件为唯一进度载体。原 `TODO.md` 已归档为 `.workbuddy/TODO.archived.md`（不入库，首行含归档说明）；[m1-completion-plan.md](m1-completion-plan.md) 为历史归档
> 规范依据：`rust-best-practices` 红线 + [benchmark-spec.md](benchmark-spec.md) v1.0

---

## 1. 当前状态单表（唯一事实来源）

> 本节是全部任务与质量状态的**唯一权威表**。§3 实施方向、§4 证据只引用本表，不重复描述状态。
> 状态取值：`done` / `todo` / `deferred`（延后）/ `optional`（可选）。

### 1.1 质量门禁（2026-11-14 实测，Windows 本机 rustc 1.99.1）

| 项 | 当前值 | 状态 | 复现口径 |
|---|---|---|---|
| `cargo fmt --all -- --check` | 通过 | done | — |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 警告 | done | — |
| `cargo test --workspace` | **77 passed / 0 failed** | done | 55 主 crate + 10 proto + 9 契约 + 3 doc |
| 生产代码 panic 点 | **0** | done | 按 `#[cfg(test)]` 切分，仅扫生产区 |
| `unsafe` | 0 | done | workspace `unsafe_code = "deny"` |
| 生产代码 `let _ =` | 31（均 best-effort，见 §1.3） | done | 同上切分 |
| 无 `///` 的 pub 项 | **110**（全在主 crate；proto 已清零） | todo | 检查 pub 项前一行，**跳过属性行** |
| `# Examples` 文档测试 | 3（proto） | done | `cargo test` doc-test 计数 |
| cargo 集成测试 | 9（`crates/v2node-proto/tests/`） | done | — |
| 严格模式 lint（pedantic+nursery） | **127**（生产区 90 / 测试区 20 / benches+examples+tests 17） | todo | 见 §1.4 计数纪律 |
| 主包无用依赖 | 0（blake3 已移除） | done | CI clippy `unused dependency` |

Linux CI 测试数为 **78**（多 1 个 Unix-only 测试 `existing_private_key_permissions_are_corrected`，[tls.rs:462](../src/tls.rs#L462)）。历史记录的 105 含已拆出的 `client-core`，不再适用。

### 1.2 任务状态

| ID | 事项 | 优先级 | 状态 | 证据 / 备注 |
|---|---|---|---|---|
| A4 | Encryption 生产启用决策 | P0 | done | 保持启用，兼容性由自研客户端承接（§2 决策 3） |
| B1 | 发布收口（第一轮） | P0 | done | 3 提交推送、SHA 一致、CI 三作业全绿、v0.2.1 标签已删（§4.1） |
| B2 | 文档一致性 | P0 | done | 死链清零、基线绑工具链、RelayBufferSize 双层口径（§4.2） |
| B3a | 消除生产 panic 点 | P1 | done | 3 处改 let-else / ok_or_else，复扫零命中（§4.2） |
| B3b | 显式处理可恢复错误 | P1 | done | 启动期流量显式结算 + 3 处 TLS 回滚记 warn（§4.2） |
| B8 | 清理主包无用依赖 | P1 | done | blake3 移除，Cargo.lock 仅删 1 行 |
| B4 | proto 黑盒契约测试 | P2 | done | 9 例全过，错误断言经变异测试验证（§4.2） |
| B5a-proto | proto 公开项文档 | P2 | done | 7 项补 ///（含 # Errors）+ 3 个 # Examples |
| B5a-main | 主 crate 公开项文档 | P2 | todo | 110 项；估时 **4–5d**（全量，非 2d） |
| B5c | 严格模式分批收敛 | P2 | todo | 目标以**生产区 90** 为准；分批见 §3 |
| B6 | 其余三台 journald + Apport | P1 | todo | srv85 已完成；需运维窗口 |
| C1a | 热更新字段分类 | 后续 | todo | 监听类/运行类/面板连接类三组 |
| C1b | 节点集增删原位应用 | 后续 | todo | 依赖 C1a |
| C1c | 面板连接参数热替换 | 后续 | todo | 依赖 C1a |
| C2 | proto 角色解耦 | 后续 | todo | 依赖 B4；移除 main.rs 兼容 pub use |
| C3 | M2 立项（REALITY/routes/0-RTT） | 后续 | todo | M1 关闭后独立立项 |
| A1 | 真实 ACME staging DNS-01 | — | deferred | 非强制（§2 决策 2）；重启需 CF Token+子域+授权 |
| A2 | 真实 ACME staging HTTP-01 | — | deferred | 同上，另需 DNS-only A 记录 + 公网 80 |
| A3 | 生产 cert_mode 决策与灰度 | — | deferred | 当前维持 none |
| A5 | 生产日志级别调 warn | — | deferred | 与 A1 同窗口；当前无 ACME 告警需求 |
| B7 | 握手失败分类可观测性 | — | optional | 兼容既由自研客户端承接，老客户端失败属预期 |
| D1 | arm64 制品 | P3 | optional | 出现 arm 节点时 |
| D2 | 访问日志 / 性能采样 | P3 | optional | 运维确有依赖时 |
| D3 | 面板幂等补丁正式入库 | P3 | optional | 面板侧排期 |
| D4 | 清理 .workbuddy/tmp 临时文件 | P3 | optional | 确认不再需要（先备份） |

### 1.3 `let _ =` 判读结论（31 处保留项）

35 处中 4 处已修（B3b），其余 31 处经逐条判读为语义正确的 best-effort，**不机械清零**：

| 类别 | 数量 | 判定 |
|---|---|---|
| 关闭/排空/Pong 等 best-effort I/O | 约 19 | 对端已断开时失败无意义；保留 |
| 幂等 `install_default()` | 4 | 重复安装返回 Err 属正常；保留 |
| 清理 `remove_file` | 6 | 删除失败不该覆盖原始错误；保留 |
| 已 abort 的 `task.await` | 2 | JoinError 即预期取消；保留 |

验收标准为"**无静默丢弃可恢复错误**"，而非"清零 `let _ =`"。强制加日志只会引入噪声、降低信噪比。

### 1.4 计数纪律（避免脏数字）

严格模式 lint 计数**必须**用以下口径，否则会得到虚高或假 0：

```bash
cargo clippy --workspace --all-targets --message-format=short \
  -- -W clippy::pedantic -W clippy::nursery 2>&1 \
  | grep "warning:" | grep -v "generated" \
  | grep -v "lock file" | grep -v "incremental" | wc -l
```

排除三类非 lint 行：
1. `generated N warnings` 汇总行（每个 crate/target 一条）；
2. **增量编译锁文件噪声**（`error deleting lock file ... os error 5`，Windows 并发编译时出现，审查报告的命令也漏了此项）；
3. CI 日志的时间戳前缀与 ANSI 色码（`CARGO_TERM_COLOR=always`），直接 `grep "^warning: "` 会得假 0。

本机干净口径（2026-11-14，short 格式强制完整输出）：**总 127 = 生产区 90 + 测试区 20 + benches/examples/tests 17**。**收敛目标只针对生产区 90**，测试区与基准/示例不纳入。

**缓存陷阱**：默认格式在 crate 已编译缓存时只输出增量 warning（曾测得 11、155 等不稳定值），`--message-format=short` 强制完整输出，是唯一可靠口径。早先记录的 143/140/129 均为含汇总行、锁噪声或缓存不完整的脏数字，已废弃。

无文档 pub 项计数须**跳过属性行**（`#[allow(...)]` 等）再找 `///`，否则会把"文档在属性上方"的项误判为缺失（proto 的 `EncryptionServer`/`ClientEncryptionConfig` 曾因此被误报）。

**审查报告引用**：本文件的状态单表重构与计数纪律源自外部审查报告 `improvement-plan-review-2026-11-14.md`（留在工作区 `C:\Users\Administrator\WorkBuddy\2026-11-14-02-20-58\`，不入库）。该报告复核了 panic/let_/测试数等断言并指出双时间点冲突；其严格模式计数 142 含锁噪声，本文件以 clean 构建 short 口径 127 为准。

---

## 2. 目标与关闭标准

> **用户决策（2026-11-14）**：
> 1. **ACME 非强制项**，A1/A2/A3/A5 全部延后，不作为 M1 关闭条件；生产维持 `cert_mode=none`。
> 2. **加密保持启用，不为老客户端做兼容**；兼容性由后续**自研客户端**（McloudClient，复用 `v2node-proto`）解决。B7 降级为可选观测项。
> 3. 其余按本计划建议推进（轨道 B 自主项优先）。

### 2.1 三条并行轨道

```text
轨道 A（延后，非强制）：ACME 真实验收与 cert_mode 切换
    └─ 待未来需要源站 TLS 时再启动；脚本已就绪，瓶颈是外部输入

轨道 B（自主推进，当前主线）：
    └─ B1 发布收口[完成] ─ B2 文档一致性 ─ B3 规范红线 ─ B8 依赖清理 ─ B4 测试补齐 ─ B5 文档与严格收敛

轨道 C（下阶段）：
    └─ C1 热更新剩余范围 ─ C2 proto 角色解耦 ─ C3 M2 立项（REALITY/routes/0-RTT）
    └─ 自研客户端在独立仓库推进，加密兼容性由其承接
```

### 2.2 M1 关闭门控（修订版）

1. B1 完成：远端与本地一致、标签与 CHANGELOG 自洽、远端 CI 全绿。**已满足**。
2. B2 完成：无死链、基线数字一致并注明工具链、单一进度载体。**已满足**。
3. A4 决策落库：加密保持启用、兼容性由自研客户端承接。**已满足**。

**三项门控均已满足，M1 可宣布关闭**（待用户确认，见 §6）。ACME 不再是关闭条件。C1/C2/C3 与规范债不纳入。

### 2.3 发布门控（每次发版都必须满足）

1. `cargo fmt --all -- --check` 通过。
2. `cargo clippy --workspace --all-targets -- -D warnings` 零警告。
3. `cargo test --workspace` 全绿。
4. Linux 构建机 **8 套 e2e + 1 套 interop_xray** 全绿。
5. 涉及协议或性能改动时，附 [benchmark-spec.md](benchmark-spec.md) 要求的同机 before/after，四维无未说明回退、P99 恶化不超过 5%。
6. 生产代码 panic 点为 0。

---

## 3. 待办任务实施方向

> 仅列 `todo` 项。`done` 项的实施细节见 §4.2 已实施附录；`deferred`/`optional` 项见 §1.2 备注。

### B5a-main 主 crate 公开项文档（110 项）

全量 110 项需 **4–5d**（§7 自审已承认，非早期估的 2d）。分批：先关键路径（`NodeRuntime`/`PanelClient`/`TlsManager` 的公开方法），再配置结构体字段。`missing_errors_doc` 严格项随本任务一并补。

### B5c 严格模式分批收敛（生产区 90）

| 批次 | 规则 | 处置 |
|---|---|---|
| 第一批 | `unreadable_literal`、`semicolon_if_nothing_returned`、`map_unwrap_or`、`missing_const_for_fn` | 纯机械，改完升 `deny` |
| 第二批 | `cast_possible_truncation`、`cast_possible_wrap`、`cast_sign_loss` | 协议字段用 `try_from` + 显式错误；计量转换逐条判定 |
| 第三批 | `cast_precision_loss`、`significant_drop_tightening`、`too_many_lines`、其余风格类 | 逐条评审，锁作用域改动需异步安全复核 |

**目标以生产区 90 为准**（§1.4）。测试区 20 处不纳入，可在 `tests` 模块加 `#[allow(clippy::pedantic)]` 并注明理由。每批开工前按 §1.4 口径重新计数，避免按全量数字虚估工作量。基线随工具链变化（0.1.99 与 1.100 规则集不同），**引用时必须带工具链版本**。

**验收口径（不做绝对数字门控）**：每批改动后生产区计数**只减不增**即通过；某规则归零后立即升 `deny` 锁定，防止回流。不预设"必须降到 N 条"的硬阈值——基线随工具链漂移，绝对数字门控会误伤。

### B6 其余三台 journald + Apport

srv85 已完成（journald 200M/50M、Apport enabled=0）。其余三台需统一运维窗口，验收 `journalctl --disk-usage` < 200M。

### C1 热更新剩余范围

`NodeManager::apply_config` 目前对"同节点集"原位应用，节点集变化整体重建。拆三步独立验收：

1. **C1a 字段分类**：监听类（listen_ip/server_port/ws_path/tls/backlog）、运行类（超时/限速/日志/pull-push 间隔）、面板连接类（ApiHost/ApiKey/Timeout/Retry）。
2. **C1b 节点集增删**：按 node_id diff，只 start/stop 差异节点，保留其余 `NodeRuntime`。
3. **C1c 面板连接类热替换**：`PanelClient` 已 `Clone` + 内部 `Arc` 缓存，让 `NodeRuntime` 持可替换句柄，后台循环每轮读当前句柄。需定清热替换时 ETag/body hash 缓存是否保留。

风险点是后台任务与 listener 状态竞态。现有"先 abort 任务再交换 listener"不变量必须保持，并保留失败回滚。

### C2 proto 角色解耦

`vless.rs` 请求解析与地址编解码依赖 `VlessContext`（持 `NodeState`/`RuntimeConfig`/`NodeInfo`）。把纯编解码（version/UUID/addons/command/port/ATYP）抽成 proto 无状态函数，节点侧只留鉴权/限额/计量。完成后删 `main.rs` 兼容 `pub use`。按 benchmark-spec §5，结构性搬运后必须重跑 L1（历史 C0b 曾现 W1 +10.5% 偏差）。

### C3 M2 立项要点

| 能力 | 现状 | 立项前必须先解决 |
|---|---|---|
| REALITY | 未实现，配置层未接受 | 不能复用 rustls 标准 TLS 路径；先定客户端版本互通矩阵与证书链伪装方案 |
| routes | `Route` 已能反序列化，`validate_for_m1` 拒绝 | 规则匹配位置（relay 前）、规则集规模基准（W8）、默认动作语义 |
| 0-RTT | 握手层拒绝 `length == 32`，ticket 前两字节置 0 即禁用 | ticket 生命周期、重放防护、降级路径；不能只放开解析分支 |

### 原 TODO.md 遗留项处置

`TODO.md` 已归档为 `.workbuddy/TODO.archived.md`（不入库，首行含归档说明，保留备查不删）。条目处置：

| 原条目 | 处置 |
|---|---|
| P1-1 关闭 sshd 密码登录 | 用户明确不做；运维备忘，不排期 |
| P1-2 node3 迁出 Debian 10 | 用户明确不做；风险 R8，30 天复查（2026-12-14） |
| P1-3 可配置 backlog | done（v0.2.0） |
| P2-1 发布 v0.1.1 | 已过时（0.1.1→0.2.0 均已发布） |
| P2-2 真实 ACME | deferred（§2 决策 2） |
| P2-3 配置原位重载 | 大部分 done（v0.2.0）；剩余 = C1a/b/c |
| P2-4 journald | srv85 done；其余 = B6 |
| P3-1 Apport | srv85 done；其余随 B6 |
| P3-2 arm64 | = D1 |
| P3-3 pprof/访问日志 | = D2 |
| P3-4 幂等键 | 节点侧 done；面板补丁入库 = D3 |
| P3-5 清理 stash | done（2026-11-11，patch 备份后删除） |

---

## 4. 证据与已实施附录

### 4.1 第一轮：发布收口（B1，2026-11-14）

原状况"本地领先 3 提交 + 本地独有 v0.2.1 标签 + CHANGELOG 无条目"。用户决策：删标签、性能并入下一版、授权推送。

| 步骤 | 结果 |
|---|---|
| 脱敏扫描 | 待推送 diff 277 行，敏感模式零命中 |
| 凭据核实 | GCM OAuth 凭据，先查 `permissions.push=true` |
| 通道 | `github.com:443` 不通、`api.github.com` 200 → Git Data API |
| 推送 | blob → tree(`base_tree`) → commit → PATCH ref，每步校验 SHA |
| SHA 一致 | `604646b`/`a71bbcf`/`ea2875e` 与本地完全相同，无分叉 |
| 标签 | 本地 v0.2.1 已删（原 `c40ba76`→`ea2875e`，可重建）；远端仅 v0.2.0 |
| CI | quality / strict-report / bench-observe 三作业全 success |

**当前偏离**：第二轮起产生 3 个新提交 `9f79d34`（代码红线修复）、`f83c511`（文档同步）、`a742bdf`（按审查报告重构为状态单表），**本地 ahead origin/main 3，尚未推送**（推送需用户授权，见 R11）。

### 4.2 第二轮：自主推进（2026-11-14，已实施）

| 任务 | 实施内容 | 验证 |
|---|---|---|
| B2 | README 死链清零、基线绑工具链、Makefile/Cargo.toml 注释同步、lib.rs 与 benchmark-spec 引用修正、RelayBufferSize 双层口径、TODO.md 并入本文件 | 死链逐路径检查无缺失 |
| B3a | encryption.rs 记录层 `unwrap()`→let-else；vless_probe 两处 `take().unwrap()`→`ok_or_else` | 生产区复扫零命中 |
| B3b | node.rs 启动期流量显式 `add_all`；3 处 TLS 回滚 Err 记 warn | 门禁全绿 |
| B8 | 主包 blake3 移除 | Cargo.lock 仅删 1 行 |
| B4 | `crates/v2node-proto/tests/handshake_contract.rs` 9 例（三模式握手/300KiB 跨记录/错误路径） | 全过；错误断言经变异测试（反转即失败，已还原复跑） |
| B5a-proto | 7 个 pub 项补 ///（含 # Errors）+ 3 个 # Examples | doc-test 3 全过 |

L1 基准证据：[2026-11-14-b3-redline-fix.md](benchmark-results/2026-11-14-b3-redline-fix.md)。W3/W4 顺序跑 P99 偏高，经同时段交替 A/B 三轮判定为环境噪声（差值 -13.5%~+14.0% 双向波动，P50 持平，该路径代码零改动），**无回退**。

### 4.3 灰度失败根因（B7 背景，已定性）

**根因（非代码缺陷）**：VLESS 入站 `decryption` 是单一配置，同一入站无法兼容加密与明文客户端。老客户端明文请求被当加密握手解析：服务端先读 16B 当 IV、再读 32B 当公钥，而明文客户端发的是 `version+UUID+addons+command+port+ATYP`，派生 AEAD 密钥必错，解密长度字段失败；字节不足者等 10s 超时。两路径都归入 `VLESS encryption handshake`。

**排除代码问题的证据**：面板配置与本地逐字段一致；密钥对复核 `password == X25519(private_key)` 为 True；真实密钥握手单测通过；`wss://` 经 CF 与 `ws://` 直连均 PROBE_OK；用户刷新订阅后节点全绿、失败速率 ~1.5/s→~0.47/s；WS 层 350 次握手全成功。

**订阅侧**：ClashMeta/ClashVerge/ClashNyanpasu 下发 encryption；Singbox/SingboxOld **不生成**；`flag=clash` 旧模板返回 `proxies: []`（不可用）。

**52% 口径问题**：失败行是 `debug!`（[listener.rs:163](../src/listener.rs#L163)）而生产记录为 `Log.Level=error`，按此配置不应出现——观测期级别与记录不一致，需实机核实。且 `accepted` 只计完成 VLESS 解析的连接，`closed_err` 计所有带错关闭（含探测/保活），故 52% 是**连接失败率**非用户失败率。

**处置（用户决策）**：加密保持启用，兼容性由自研客户端承接，老客户端失败属预期。B7 降级可选。

### 4.4 双平台基线差异

| 项 | Windows 本机 | Linux CI |
|---|---|---|
| 工具链 | rustc 1.99.1 / clippy 0.1.99 | clippy rust-1.100.0 |
| 测试数 | 77 | 78（多 1 Unix-only） |
| 严格 lint（干净口径） | 127（生产区 90） | 待按 §1.4 口径重测 |

严格条数随工具链版本变化，收敛必须固定并记录工具链版本。

---

## 5. 风险登记

| ID | 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|---|
| R1 | 外部输入延迟 | — | — | 已消解：ACME 延后，不阻塞关闭 |
| R2 | 无隔离测试环境，E2E 需借生产机 | 高 | 高 | srv85 隔离实例（独立端口/目录/spool，不碰生产配置与 systemd）；留 kill 与回滚 |
| R3 | 性能改动未经 Linux E2E | 中 | 高 | 远端 CI 已验证；部署前必须跑 8 e2e + 1 interop |
| R4 | 记录层修复引入 P99 回退 | 中 | 中 | 附 L1 before/after，交替 A/B 判定，P99 >5% 不合并。**L1 仅覆盖 Windows 本机**，Linux 侧待构建机复测；路径未改动的 workload 按 benchmark-spec §5.7 豁免 |
| R5 | C1 热更新竞态 | 中 | 高 | 保持"先停任务后换 listener"不变量；三子任务独立验收；保留回滚 |
| R6 | Log.Level=error 吞证书告警 | — | — | 已消解：ACME 延后；重启 A1 时再调 warn |
| R7 | 面板幂等补丁被 update.sh 覆盖 | 中 | 中 | 现网 PushIdempotency=true，补丁失效退化为无去重；D3 入库前面板升级后复核 |
| R8 | node3 Debian 10 EOL | 高 | 中 | 显式接受风险，30 天复查（2026-12-14） |
| R9 | 文档与实际漂移 | 中 | 中 | 本文件§1 单表为唯一事实来源；发版同步 CHANGELOG 与基线 |
| R10 | 老客户端持续握手失败 | 高 | 低 | 已决策接受：兼容由自研客户端承接，失败属预期 |
| R11 | 本地 3 提交未推送，远端 CI 未验证第二轮改动 | 中 | 中 | `9f79d34`/`f83c511`/`a742bdf` 仅在本地；推送需用户授权，推送后远端 CI 自动验证。未推送前不得宣称第二轮改动"已上线/已验证" |

---

## 6. 决策记录与待定事项

**已决策（2026-11-14）**：

1. 发布路径：删 v0.2.1 标签、性能并入下一版、授权推送——已执行（B1）。
2. ACME 非强制：A1/A2/A3/A5 延后，生产维持 cert_mode=none。
3. Encryption 保持启用：兼容由自研客户端承接，B7 降级可选。
4. 其余按建议推进：轨道 B 自主项连续执行。
5. TODO.md 移至 .workbuddy 保留；按审查报告重构本文件为状态单表。

**仍待定**：

1. 第二轮 2 个提交（`9f79d34`/`f83c511`）是否推送远端？
2. 性能改动（AEAD 复用 + 16KB pump）何时部署生产？部署前需构建机跑 8 e2e + 1 interop。
3. M1 关闭：§2.2 三项门控均已满足，是否确认关闭？
4. Debian 10 风险：是否接受 node3 继续运行并设 30 天复查（2026-12-14）？

---

## 7. 自我审查

### 7.1 与实测证据的一致性

| 断言 | 证据 | 核实 |
|---|---|---|
| 门禁全绿、Win 77 / Linux 78 | 本机三关 + CI quality 日志逐 target 计数 | 是（双平台） |
| 生产 panic 0 | 按 cfg(test) 切分扫描 + 逐处阅读 | 是 |
| let _ = 31 处保留 | 逐条判读语义 | 是 |
| 无文档 pub 110（全主 crate） | 脚本统计，跳过属性行修正误报 | 是 |
| 严格 lint 127（生产区 90） | §1.4 干净口径，排除汇总行+锁噪声 | 是 |
| 本地 ahead 2 | git branch -vv | 是 |
| 灰度根因 | 握手读取顺序 + 历史互通证据 + 订阅模板差异 | 是 |
| 生产 0.2.0 / 日志级别矛盾 | 引自历史记录，本轮未连生产复核 | 部分 |

### 7.2 合理性

1. 状态单表（§1）消除了原 §1/§3/§8.6 三处重复事实，是 P-1 类冲突的根治。
2. let _ = 不机械清零正确：31 处确为 best-effort，强制加日志降低信噪比。
3. 严格收敛只针对生产区 90，测试区不纳入，避免虚估工作量。
4. C1 拆三子任务必要：涉及生命周期与竞态，拆分后每步可独立验收回滚。
5. 不合理之处：轨道 B 估时未计 Linux E2E 往返，墙钟时间长于人日估算。

### 7.3 可行性

1. B5a-main 全量 110 项需 4–5d（早期 2d 偏乐观，已修正）。
2. C1c 有设计不确定性：PanelClient 热替换时 ETag/body hash 缓存保留策略需在 C1a 定清。
3. A1/A2 瓶颈是输入非开发：脚本就绪，staging 账户创建已验证。

### 7.4 测量陷阱备忘

| 陷阱 | 表现 | 纠正 |
|---|---|---|
| CI 日志时间戳 + ANSI | `grep "^warning: "` 假 0 | 剥离前缀与 `\x1b[…m` |
| 严格计数混入噪声 | 含 generated 汇总行 + 锁文件噪声 | §1.4 口径排除三类非 lint 行 |
| 默认格式缓存增量 | 已编译 crate 只输出增量 warning（11/155 等不稳定值） | 用 `--message-format=short` 强制完整输出 |
| pub 文档计数误报 | 文档在属性上方被判缺失 | 跳过属性行再找 /// |
| Python 不认 /tmp | FileNotFoundError | cygpath -w 转换 |
| Git Data API 时区 | +0800 归一 UTC 致 SHA 不同 | 保留原始偏移，逐步校验 SHA |
| 本机 P99 尾噪 ±15% | 单次顺序跑误判回退 | 同时段交替 A/B |

### 7.5 本计划未做的事

- 未创建远端标签、未发布 Release、未部署生产；
- 未执行 Linux E2E（本机 Windows 不具备条件，CI 只跑单测）；
- 未连接生产复核运行版本与日志级别（引自历史记录，需实机核实）；
- 第二轮 2 个提交未推送（待用户授权）。