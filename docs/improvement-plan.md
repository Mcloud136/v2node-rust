# v2node-rust 完善推进计划

> 编制日期：2026-11-14
> 事实基线：本地 `main` @ `ea2875e`；`origin/main` @ `26d0bf4`；生产四节点运行 `v0.2.0`
> 适用范围：本仓库（节点端 + `v2node-proto`），不含面板与独立客户端仓库
> 进度载体：本文件为后续唯一进度载体，取代本地 `TODO.md`（不入库）与 [m1-completion-plan.md](m1-completion-plan.md) 的待办部分
> 规范依据：`rust-best-practices` 红线 + [benchmark-spec.md](benchmark-spec.md) v1.0

---

## 1. 实测基线（2026-11-14，Windows 本机）

### 1.1 硬门禁

| 命令 | 结果 |
|---|---|
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过，0 警告 |
| `cargo test --workspace` | 65 passed / 0 failed（主程序 55 + proto 10） |

Linux 应为 66（多 1 个 Unix-only 测试 `existing_private_key_permissions_are_corrected`）。历史记录的 105 含已拆出的 `client-core`，不再适用。

### 1.2 规范差距（逐项可复现）

| 项 | 实测 | 复现方式 |
|---|---|---|
| 生产代码 panic 点 | 3 | 逐文件按 `#[cfg(test)]` 行号切分后扫描 `unwrap()/expect(/panic!` |
| `unsafe` | 0 | `[workspace.lints.rust] unsafe_code = "deny"` |
| 生产代码 `let _ =` | 35（src 32 + crates 3） | 同上切分后扫描 |
| 无 `///` 的 `pub` 项 | 117（src 110 + crates 7） | 检查 `pub fn/struct/enum/trait` 前一行 |
| `# Examples` 文档测试 | 0 | `grep -rn "# Examples" src crates` |
| cargo 集成测试 | 0 | `tests/` 下只有 shell 与 Python，无 `*.rs` |
| 严格模式（pedantic+nursery） | 本机 143 / Linux CI 147 | `make lint-strict`；CI 见 §1.5 |
| 主包无用依赖 | `blake3` 1 项 | CI clippy `unused dependency`；`grep -rn blake3 src benches examples` 零命中 |

`blake3` 是 C0b 抽 crate 后遗留在主包 `[dependencies]` 的依赖，实际只被 `v2node-proto` 的 dev-dependencies 使用。CI 同时报 `criterion` 未使用，但它被 `benches/proto_bench.rs` 引用，属该 lint 对 bench-only 依赖的已知误报，不删。

**统计口径提醒**：CI 日志带时间戳前缀且 `CARGO_TERM_COLOR=always` 含 ANSI 转义码，直接 `grep -c "^warning: "` 会得到假 0。必须先剥离时间戳与 ANSI 再统计（本轮曾因此误判 Linux 严格模式为 0 条）。

`let _ =` 的 35 处经逐条判读，实际分布为：

| 类别 | 数量 | 判定 |
|---|---|---|
| 关闭/排空/Pong 等 best-effort I/O | 约 19 | 语义正确，对端已断开时失败无意义；保留并写明理由 |
| 幂等 `install_default()`（4 处） | 4 | 重复安装返回 Err 属正常；保留 |
| 清理 `remove_file`（6 处） | 6 | 删除失败不该覆盖原始错误；保留 |
| 已 abort 的 `task.await` | 2 | JoinError 即预期取消；保留 |
| `let _ = private;`（非 Unix 分支占位） | 1 | 不是 Result，不属本红线 |
| 需改为显式处理 | 3 | `node.rs:72` 丢弃 `apply_users` 返回值、`node.rs:169/177/197` 回滚路径丢弃 `tls.apply_node` 的 Err |

结论：35 处中真正需要修的只有 4 处（1 + 3），其余按 best-effort 语义保留。**不采用"清零 `let _ =`"作为验收标准**，改为"无静默丢弃可恢复错误"。

严格模式 143 条构成（按规则计次）：

| 规则 | 条数 | 处置 |
|---|---|---|
| `cast_possible_truncation` | 21 | 逐条判定：协议字段用 `try_from` + 显式错误 |
| `cast_precision_loss` | 11 | 计数转 f64，精度损失可接受，模块级 allow |
| `cast_possible_wrap` | 11 | 逐条判定，多为 u64→i64 计量 |
| `missing_const_for_fn` | 10 | 机械修复 |
| `unreadable_literal` | 9 | 机械修复（BLAKE3 常量表） |
| `semicolon_if_nothing_returned` | 6 | 机械修复 |
| `significant_drop_tightening` | 5 | 逐条判定锁作用域 |
| `missing_errors_doc` | 5 | 随文档任务一并补 |
| `map_unwrap_or` | 5 | 机械修复 |
| `cast_sign_loss` | 5 | 逐条判定 |
| 其余 22 类 | 55 | 逐条判定，多为 pedantic 风格 |

按位置切分：137 处可定位中，生产代码 99、测试代码 22、其他 16。**测试代码的 pedantic 不纳入收敛目标**，只对生产代码的 99 处分批处理。

### 1.3 仓库与发布状态（B1 已执行，2026-11-14）

| 事实 | 状态 |
|---|---|
| 3 个性能提交 | **已推送**，远端 `main` = `ea2875e`，SHA 与本地逐一相同、无分叉 |
| 推送通道 | `github.com:443` 不通（连接重置 / 21s 超时），`api.github.com` 200；改用 Git Data API（blob → tree(`base_tree`) → commit → PATCH ref），每步校验 SHA，不一致即在更新 ref 前中止 |
| `v0.2.1` 本地标签 | **已删除**（原 tag 对象 `c40ba76` → commit `ea2875e`，SHA 已记录，如需可重建） |
| 远端标签 | 仅 `v0.2.0`；性能改动按用户决策并入下一版 |
| CHANGELOG | 保持顶部 v0.2.0，未添加 v0.2.1 条目（与决策一致） |
| 远端 CI | `ea2875e` 三作业全 success：`quality` / `strict-report` / `bench-observe` |
| 生产版本 | 仍 0.2.0，不含上述性能改动；是否部署待定 |
| 脱敏 | 推送前扫描 diff 277 行，敏感模式零命中；临时凭据文件已清理 |

### 1.4 文档一致性欠账

| 问题 | 位置 |
|---|---|
| 死链 `docs/client-design-plan.md` | README.md、`crates/v2node-proto/src/lib.rs`、benchmark-spec.md |
| 死链 `docs/client-handoff-thinking.md` | README.md |
| 严格基线三处不一致 | README 写 248、Makefile/Cargo.toml 注释写 215、实测本机 143 / Linux 147（且工具链不同） |
| 进度载体冲突 | 本地 TODO.md 标 P1-3/P2-3/P2-4 未完成，计划附录 C 记为已完成 |
| `RelayBufferSize` 口径 | 配置默认 32768，proto 加密层 pump 已改 16384，未说明二者不同层 |

### 1.5 双平台基线差异（CI 实测）

| 项 | Windows 本机 | Linux CI |
|---|---|---|
| 工具链 | rustc 1.99.1 / clippy 0.1.99 | clippy rust-1.100.0 |
| 测试数 | 65（proto 10 + 主程序 55） | **66**（proto 10 + 主程序 56） |
| 差值来源 | 少 1 个 Unix-only 测试 `existing_private_key_permissions_are_corrected`（[tls.rs:462](../src/tls.rs#L462)） | — |
| 严格模式 warning | 143 | 147 |
| 严格模式可定位处 | 137（生产 99 / 测试 22 / 其他 16） | 140（生产 118） |

Linux 66 与编制计划时的推断一致，已证实。严格模式条数随工具链版本变化（1.99 → 1.100 新增规则），**收敛时必须固定并记录工具链版本**，否则基线数字不可比——这也是三处基线数字不一致的部分成因。生产代码待收敛量以 Linux 的 118 处为准（比本机 99 多，因新版 clippy 规则更全）。

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

1. B1 完成：远端与本地一致、标签与 CHANGELOG 自洽、远端 CI 全绿。**已满足（2026-11-14）**。
2. B2 完成：无死链、基线数字一致并注明工具链、单一进度载体。
3. A4 决策落库：加密保持启用、兼容性由自研客户端承接。**已决策（2026-11-14）**。

ACME（A1/A2/A3/A5）不再是关闭条件。C1/C2/C3 与规范债不纳入 M1 关闭门控。

### 2.3 发布门控（每次发版都必须满足）

1. `cargo fmt --all -- --check` 通过。
2. `cargo clippy --workspace --all-targets -- -D warnings` 零警告。
3. `cargo test --workspace` 全绿。
4. Linux 构建机 9 套 E2E + `interop_xray.sh` 全绿。
5. 涉及协议或性能改动时，附 [benchmark-spec.md](benchmark-spec.md) 要求的同机 before/after，四维无未说明回退、P99 恶化不超过 5%。
6. 生产代码 panic 点为 0。

---

## 3. 任务清单

### 3.1 P0 — M1 关闭必需

| ID | 事项 | 类型 | 依赖 | 验收标准 | 估时 |
|---|---|---|---|---|---|
| A4 | Encryption 生产启用决策 | 决策 | 用户裁决 | **已决策（2026-11-14）**：保持启用，不为老客户端做兼容；兼容性由自研客户端承接 | 已交付 |
| B1 | 发布收口 | 发布 | 用户授权推送 | **已完成（2026-11-14）**：3 提交推送、SHA 一致、远端 CI 三作业全绿、本地 v0.2.1 标签已删、性能改动并入下一版 | 已交付 |
| B2 | 文档一致性 | 文档 | 无 | 无死链；严格基线数字三处一致并注明工具链版本；TODO.md 内容并入本文件后归档 | 0.5d |

### 3.1a 延后项（非强制，用户决策 2026-11-14）

| ID | 事项 | 状态 | 重启条件 |
|---|---|---|---|
| A1 | 真实 ACME staging，DNS-01 | 延后 | 需要源站 TLS 时；CF Token + 测试子域 + 运行授权到位 |
| A2 | 真实 ACME staging，HTTP-01 | 延后 | 同上，另需 DNS-only A 记录 + 公网 80 入站 |
| A3 | 生产 `cert_mode` 决策与灰度 | 延后 | A1/A2 完成后；当前维持 `none`（CF 边缘终止 TLS + 内层 VLESS Encryption） |
| A5 | 生产日志级别调 `warn` | 延后 | 与 A1 同窗口；当前无 ACME 证书告警需求 |
| B7 | 握手失败分类可观测性 | 可选 | 兼容性既由自研客户端承接，老客户端失败属预期；仅当需要量化失败构成时再做 |

### 3.2 P1 — 放量前收尾

| ID | 事项 | 类型 | 依赖 | 验收标准 | 估时 |
|---|---|---|---|---|---|
| A5 | 生产日志级别调为 `warn` | 运维 | 与 A1 同窗口 | 14 天证书 WARN 在 journald 可见（现为 `error` 会吞掉） | 0.1d |
| C1a | 热更新字段分类 | 重构 | 无 | 明确监听类/运行类/面板连接类三组，写入本文件与代码注释 | 1d |
| C1b | 节点集增删原位应用 | 重构 | C1a | 增删单个节点不影响其余节点，PID 不变 | 3d |
| C1c | 面板连接参数热替换 | 重构 | C1a | 改 ApiHost/ApiKey/Timeout/Retry 后 PID 不变、存量连接不断 | 2d |
| B6 | 其余三台 journald + Apport | 运维 | 运维窗口 | `journalctl --disk-usage` < 200M；Apport 决策留档 | 0.5d |
| B3a | 消除生产 panic 点 | 质量 | 无 | 3 处改为显式错误；扫描零命中 | 0.3d |
| B3b | 显式处理可恢复错误 | 质量 | 无 | `node.rs:72` 结算启动期流量；3 处回滚路径 Err 记日志 | 0.5d |
| B8 | 清理主包无用依赖 | 质量 | 无 | 移除主包 `blake3`；CI `unused dependency` 仅剩 criterion 误报 | 0.1d |

### 3.3 P2 — 工程质量与能力扩展

| ID | 事项 | 类型 | 依赖 | 验收标准 | 估时 |
|---|---|---|---|---|---|
| B4 | cargo 集成测试 | 测试 | 无 | 新增 `tests/*.rs` 黑盒用例，覆盖握手/relay/限额/热更新；`cargo test` 计数上升 | 2d |
| B5a | 公开项文档 | 质量 | 无 | proto 7 项 + 主 crate 关键路径优先；`missing_errors_doc` 5 条清零 | 2d |
| B5b | `# Examples` 文档测试 | 质量 | B5a | proto 公开 API 至少 3 个可运行示例，`cargo test` 出现 doc-test 且全绿 | 1d |
| B5c | 严格模式分批收敛 | 质量 | 无 | 生产代码 99 条中机械类先清零并升 `deny`；逐批记录基线 | 3d |
| C2 | proto 角色解耦 | 重构 | B4 | VLESS 帧/地址编解码迁入 proto；移除 `main.rs` 兼容 `pub use`；proto 不依赖节点类型 | 3d |
| C3 | M2 立项 | 功能 | M1 关闭 | REALITY/routes/0-RTT 独立规格与验收矩阵 | 待评估 |

### 3.4 P3 — 可选

| ID | 事项 | 触发条件 | 估时 |
|---|---|---|---|
| D1 | arm64 制品 | 出现 arm 节点 | 2d |
| D2 | 访问日志 / 性能采样 | 运维确有依赖 | 2–3d |
| D3 | 面板幂等补丁正式入库 | 面板侧排期 | 1d + 面板 |
| D4 | 清理 `.workbuddy/tmp` 临时文件 | 确认不再需要（先备份） | 0.2d |

### 3.5 原 TODO.md 遗留项处置（2026-11-14 并入）

本地 `TODO.md`（不入库）自本计划生效起归档，不再维护。其条目处置如下：

| 原条目 | 处置 |
|---|---|
| P1-1 逐台关闭 sshd 密码登录 | 用户此前明确不做；保留为运维备忘，不在本计划排期 |
| P1-2 node3 迁出 Debian 10 | 用户此前明确不做；风险登记 R8，30 天复查（2026-12-14） |
| P1-3 可配置 listen backlog | **已完成**（v0.2.0，`Runtime.ListenBacklog`） |
| P2-1 发布 v0.1.1 | **已过时**（0.1.1 → 0.2.0 均已发布部署） |
| P2-2 真实 ACME 验证 | 延后（用户决策 2026-11-14，见 §3.1a） |
| P2-3 配置原位重载 | **大部分完成**（v0.2.0）；剩余范围 = C1a/b/c |
| P2-4 journald 限额 | srv85 已完成；其余三台 = B6 |
| P3-1 关闭 Apport | srv85 已完成；其余三台随 B6 窗口决策 |
| P3-2 arm64 制品 | = D1 |
| P3-3 pprof / 访问日志 | = D2 |
| P3-4 流量推送幂等键 | 节点侧已完成（v0.2.0）；面板补丁正式入库 = D3 |
| P3-5 清理旧仓库 stash | **已完成**（2026-11-11，patch 已备份后删除） |

---

## 4. 关键任务实施方向

### B1 发布收口（已完成）

原状况是"本地领先 3 个提交 + 本地独有 v0.2.1 标签 + CHANGELOG 无对应条目"，三者叠加会让远端出现无法追溯的版本。

**用户决策（2026-11-14）**：删除本地标签、性能改动并入下一版、授权推送。

**执行结果**：

| 步骤 | 结果 |
|---|---|
| 脱敏扫描 | 待推送 diff 277 行，敏感模式零命中 |
| 凭据核实 | GCM 中 OAuth 凭据，先查 `permissions.push=true` 再动手 |
| 通道选择 | `github.com:443` 不通、`api.github.com` 200 → Git Data API |
| 推送 | blob → tree(`base_tree` 增量) → commit → PATCH ref，每步校验 SHA |
| SHA 一致性 | `604646b` / `a71bbcf` / `ea2875e` 与本地完全相同，无分叉 |
| 标签 | 本地 `v0.2.1` 已删；远端仅 `v0.2.0` |
| CI | `quality` / `strict-report` / `bench-observe` 三作业全 success |
| 清理 | 临时凭据与脚本文件已删除 |

**剩余**：这些性能改动何时部署生产由用户决定；部署需按 §2.3 在构建机跑 9 套 E2E。下一版发布时再把这批改动写入 CHANGELOG。

### B3a 消除 panic 点

| 位置 | 现状 | 方向 |
|---|---|---|
| `crates/v2node-proto/src/encryption.rs:811` | `header_plain.unwrap()`，上方分支已保证 Some | 改 `let Some(header) = header_plain else { break 'outer; }`，去掉冗余 Option 往返 |
| `src/bin/vless_probe.rs:132` | `app_writer.take().unwrap()` | 该分支下必为 Some，但改为 `ok_or_else` 返回错误 |
| `src/bin/vless_probe.rs:153` | 同上 | 同上 |

注意 `encryption.rs:811` 处于记录层热路径，改动必须附 L1 基准 before/after（W4/W5/W6），确认无 P99 回退。

### B3b 显式处理可恢复错误

`src/node.rs:72` 的 `let _ = apply_users(&state, &users)` 丢弃了返回的 `Vec<UserTraffic>`。启动期用户表为空、diff 必然无 removed，因此当前**不会真的丢流量**；但这是隐式依赖"初始状态为空"的写法。方向：改为显式判断非空则送入 reporter，并加注释说明该分支预期为空。

`src/node.rs:169/177/197` 三处在回滚路径丢弃 `tls.apply_node` 的 Err。回滚失败意味着节点可能没有可用证书，属需要告警的状态。方向：`if let Err(e) = ... { warn!(...) }`，与同文件既有的 `restore previous TLS config failed` 写法保持一致。

### B7 握手失败可观测性与灰度失败根因

**根因（已定性，非代码缺陷）**：VLESS 入站的 `decryption` 是单一配置，同一入站无法同时兼容加密与明文客户端。启用加密后，老客户端的明文请求被当作加密握手解析：服务端先读 16B 当 IV、再读 32B 当客户端静态公钥（`encryption.rs:430`），而明文客户端实际发的是 `version(1)+UUID(16)+addons(1)+command(1)+port(2)+ATYP…`，派生出的 AEAD 密钥必然错误，在解密长度字段时失败（`encryption.rs:452`）；字节数不足者等到 10s 握手超时。两条路径都归入 `VLESS encryption handshake`。

**排除代码问题的既有证据**：面板下发配置与本地逐字段一致；密钥对经复核 `password == X25519(private_key)` 为 True；真实密钥握手单测通过；`wss://` 经 CF 与 `ws://` 直连均 `PROBE_OK`；用户刷新订阅后节点全绿、失败速率由 ~1.5/s 降至 ~0.47/s；WS 层 350 次握手全部成功（失败严格发生在加密层）。

**订阅侧事实**：ClashMeta / ClashVerge / ClashNyanpasu 模板会下发 `encryption`；Singbox / SingboxOld **不生成该字段**；`flag=clash` 旧模板返回 `proxies: []`（0 节点，不可用）。因此未刷新订阅者、sing-box 用户、旧 clash 链接用户必然失败。

**新发现的统计口径问题（52% 不可直接采信）**：

1. 失败行是 `debug!` 级别（[listener.rs:163](../src/listener.rs#L163)），而记录的生产配置是 `Log.Level=error`（同一约束导致 Q-2 的 14 天 WARN 不可见）。按该配置这些行不应出现，说明观测期日志级别与记录不一致，需核实。
2. 分母被污染：`accepted` 只计完成 VLESS 请求解析的连接，`closed_err` 计所有带错关闭的连接，包含健康检查、探测、扫描、CF Tunnel 保活。历史已证实混入过本地探针（`AllowPrivateTargets=false` 导致 `target … is private`，但同连接已 `accepted`）。故 52% 是**连接失败率**，不等于真实用户受损比例。
3. 日志粒度不足：连接级错误只记 `debug!` 且不带分类，"IV 读取失败 / AEAD 校验失败 / 超时"在日志里是同一句话，无法远程区分老客户端与配置错误。

**B7 实施方向**：在 `vless::serve` 的加密分支把握手错误按原因分类后向上传递，`listener.rs` 侧对握手类失败用 `warn!` 并附 `client_ip` 与分类标签；保持数据面其余错误仍为 `debug!` 以免刷屏。随后以"完成 WS 握手且首字节符合 VLESS/加密特征"的连接为分母重算真实用户失败率。

**处置（用户决策 2026-11-14）**：加密保持启用，不为老客户端做兼容；兼容性由后续自研客户端（McloudClient，复用 `v2node-proto` 的 `client_handshake`）承接。老客户端握手失败属预期行为，不计为缺陷。B7 降级为可选观测项，仅当需要量化失败构成时再做。

### B4 集成测试设计

现有单测全部内联在各模块 `mod tests`，`tests/` 下只有 shell。缺的是**通过公开 API 的黑盒测试**。因为主 crate 是纯 bin，集成测试只能覆盖 `v2node-proto`；主程序侧的黑盒覆盖继续由 shell E2E 承担。

方向：新增 `tests/proto_integration.rs`，用 `tokio::io::duplex` 构造内存链路，覆盖：

- 服务端 `handshake` × 客户端 `client_handshake`，三种模式（native/xorpub/random）各一例；
- 双向大块传输与 EOF 排水（断言尾部字节不丢）；
- 错误路径：错误私钥、长度非法、0-RTT 拒绝、非 `mlkem768x25519plus` 前缀；
- 与既有跨版本 AEAD KAT 互补，不重复。

这也让 `v2node-proto` 作为可复用 crate 具备对外契约测试。

### B5c 严格模式收敛分批

| 批次 | 规则 | 条数 | 处置 |
|---|---|---|---|
| 第一批 | `unreadable_literal`、`semicolon_if_nothing_returned`、`map_unwrap_or`、`missing_const_for_fn` | 30 | 纯机械，改完升 `deny` |
| 第二批 | `cast_possible_truncation`、`cast_possible_wrap`、`cast_sign_loss` | 37 | 协议字段用 `try_from` + 显式错误；计量转换逐条判定 |
| 第三批 | `cast_precision_loss`、`significant_drop_tightening`、`too_many_lines`、其余风格类 | 76 | 逐条评审，锁作用域改动需异步安全复核 |

上述条数为**全量 143 的按规则计次**（含测试代码）。按位置切分，137 处可定位中生产代码 99、测试代码 22、其他 16。因此每批实际改动量小于表中条数：收敛目标只针对生产代码的 99 处，测试代码的 22 处不纳入，可在 `tests` 模块加 `#[allow(clippy::pedantic)]` 并注明理由。每批开工前先按位置重新切分计数，避免按全量数字虚估工作量。

### C1 热更新剩余范围

`NodeManager::apply_config` 目前对"同节点集"做原位应用，节点集变化则整体重建。C1 把它拆成三步独立验收：

1. **字段分类**：监听类（listen_ip/server_port/ws_path/tls/backlog）、运行类（超时/限速/日志/pull-push 间隔）、面板连接类（ApiHost/ApiKey/Timeout/Retry）。
2. **节点集增删**：按 node_id 做 diff，只 start/stop 差异节点，保留其余 `NodeRuntime`。
3. **面板连接类热替换**：`PanelClient` 已是 `Clone` + 内部 `Arc` 缓存，方向是让 `NodeRuntime` 持有可替换句柄，后台循环每轮读取当前句柄，避免重建 runtime。

风险点是后台任务与 listener 状态的竞态。现有实现已用"先 abort 任务再交换 listener"的顺序，C1 必须保持该不变量，并保留失败回滚路径。

### C2 proto 角色解耦

`vless.rs` 的请求解析与地址编解码目前依赖 `VlessContext`（持有 `NodeState`/`RuntimeConfig`/`NodeInfo`）。方向是把纯编解码（version/UUID/addons/command/port/ATYP）抽成 proto 内的无状态函数，节点侧只保留鉴权、限额、计量。完成后删除 `main.rs:13` 的兼容 `pub use`。

按 [benchmark-spec.md](benchmark-spec.md) §5，结构性搬运后必须重跑 L1，历史 C0b 曾出现 W1 +10.5% 偏差，需同批复核。

### C3 M2 立项要点

| 能力 | 现状 | 立项前必须先解决 |
|---|---|---|
| REALITY | 未实现，配置层未接受 | 不能复用 rustls 标准 TLS 路径；需先定客户端版本互通矩阵与证书链伪装方案 |
| routes | `Route` 已能反序列化，`validate_for_m1` 明确拒绝 | 规则匹配位置（relay 前）、规则集规模基准（W8 已定义）、默认动作语义 |
| 0-RTT | 握手层拒绝 `length == 32`，ticket 前两字节置 0 即禁用 | ticket 生命周期、重放防护、降级路径；不能只放开解析分支 |

---

## 5. 排期

### 5.1 轨道 A（延后，非强制）

ACME 真实验收与 `cert_mode` 切换已按用户决策（2026-11-14）延后。未来重启时以三项外部输入到位日为 T0：preflight（0.5d）→ DNS-01（1d）→ HTTP-01（条件，1.5d）→ 决策 + 日志级别 + 回滚演练（2d）→ 7 天观察。脚本 `tests/e2e_acme_real.sh` 已就绪。

### 5.2 轨道 B（自主推进，当前主线）

| 阶段 | 任务 | 里程碑 |
|---|---|---|
| 第 1 步 | B1[完成]、B2 | 远端与本地自洽，文档无矛盾 |
| 第 2 步 | B3a、B3b、B8 | 生产 panic 点归零，可恢复错误不静默，无冗余依赖 |
| 第 3 步 | B4 | 协议 crate 具备黑盒契约测试 |
| 第 4 步 | B5a、B5b | 公开 API 有文档与可运行示例 |
| 第 5 步 | B5c 第一批 | 机械类严格项清零并升 deny |

### 5.3 轨道 C

C1a → C1b/C1c 可在轨道 B 第 3 步之后并行；C2 依赖 B4 完成；C3 待 M1 关闭后独立立项。

---

## 6. 风险登记

| ID | 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|---|
| R1 | 外部输入延迟 | — | — | 已消解：ACME 延后（用户决策 2026-11-14），不再阻塞任何关闭门控 |
| R2 | 无隔离测试环境，E2E 需借生产机 | 高 | 高 | 沿用 srv85 隔离实例做法（独立端口/目录/spool，不碰生产配置与 systemd）；每次留 kill 与回滚步骤 |
| R3 | 性能改动未经 Linux E2E | 中 | 高 | 远端 CI 已验证（B1）；部署前必须跑 9 套 E2E |
| R4 | 记录层 panic 点修复引入 P99 回退 | 中 | 中 | 强制附 L1 before/after，P99 恶化 >5% 不合并 |
| R5 | C1 热更新竞态导致连接中断 | 中 | 高 | 保持"先停任务后换 listener"不变量；三子任务独立验收；保留回滚 |
| R6 | 生产 `Log.Level=error` 吞掉证书告警 | — | — | 已消解：ACME 延后，当前无证书告警需求；重启 A1 时再调 `warn` |
| R7 | 面板幂等补丁被 `update.sh` 覆盖 | 中 | 中 | 现网已开 `PushIdempotency=true`，补丁失效会退化为无去重；D3 正式入库前需在面板升级后复核 |
| R8 | node3 Debian 10 EOL（已决定不做） | 高 | 中 | 显式接受风险，设 30 天复查（2026-12-14），不因"不做"而消失 |
| R9 | 文档与实际再次漂移 | 中 | 中 | 本文件为唯一进度载体；每次发版同步 CHANGELOG 与基线数字 |
| R10 | 老客户端持续握手失败 | 高 | 低 | 已决策接受（2026-11-14）：加密保持启用，兼容性由自研客户端承接；失败属预期，不计为缺陷 |

---

## 7. 决策记录与待定事项

**已决策（2026-11-14）**：

1. **发布路径**：删本地 v0.2.1 标签、性能改动并入下一版、授权推送——已执行（B1 完成）。
2. **ACME 非强制**：A1/A2/A3/A5 延后，生产维持 `cert_mode=none`，不作为 M1 关闭条件。
3. **Encryption 保持启用**：不为老客户端做兼容；兼容性由后续自研客户端（McloudClient）承接。B7 降级为可选。
4. **其余按建议推进**：轨道 B 自主项（B2 → B3/B8 → B4 → B5）连续执行。

**仍待定**：

1. 性能改动（AEAD 复用 + 16KB pump）何时部署生产？部署前需构建机跑 9 套 E2E。
2. M1 关闭评审：修订版门控（§2.2）三项中 B2 完成后即满足，是否确认关闭？
3. Debian 10 风险：是否接受 node3 继续运行并设 30 天复查（2026-12-14）？

---

## 8. 自我审查

### 8.1 与实测证据的一致性

| 计划中的断言 | 证据 | 是否核实 |
|---|---|---|
| 硬门禁全绿、Windows 65 / Linux 66 | 本机三关 + CI quality 作业日志逐 target 计数 | 是（双平台） |
| 3 处生产 panic 点 | 按 `#[cfg(test)]` 切分扫描 + 逐处阅读确认 | 是 |
| 35 处 `let _ =`，仅 4 处需修 | 逐条列出并判读语义 | 是 |
| 117 个 pub 项缺文档 | 脚本统计（src 110 + crates 7） | 是 |
| 严格模式本机 143 / Linux 147 | 本机 `make lint-strict`；CI strict-report 日志剥离时间戳与 ANSI 后统计 | 是 |
| 主包 `blake3` 无用 | CI clippy 报警 + `grep` 主包源码零命中 | 是 |
| 推送后 SHA 一致、CI 三作业全绿 | Git Data API 每步 SHA 校验 + Actions jobs API | 是 |
| 灰度失败根因为老客户端不兼容 | 握手读取顺序代码 + 历史真实密钥互通证据 + 订阅模板差异 | 是 |
| 52% 是连接失败率而非用户失败率 | 日志级别与分母构成分析（见 B7） | 是（口径问题已定性，真实比例待 B7 后测） |
| 文档死链 | 逐路径 `[ -f ]` 检查 | 是 |
| 生产运行 0.2.0、四节点 | 引自 m1-completion-plan 附录 C（历史记录，本轮未连生产复核） | 部分 |

### 8.2 合理性

1. **把发布收口列入 P0 是必要的**：本地独有标签 + CHANGELOG 缺条目 + 未推送，三者叠加会让"发布出去的东西"无法追溯，比规范债更紧急。该项已执行完毕。
2. **不把"清零 `let _ =`"当验收标准是正确的**：逐条判读后 35 处里 31 处是语义正确的 best-effort。机械清零会引入无意义日志噪声，反而降低可观测性。
3. **测试代码的 pedantic 不纳入目标是合理的**：22 条集中在测试断言风格，收敛收益低于改动风险。
4. **C1 拆三子任务是必要的**：原计划 5–8d 估算偏乐观，涉及生命周期与竞态，拆分后每步可独立验收与回滚。
5. **不合理之处**：轨道 B 的估时未计入 Linux E2E 往返时间；每次涉及协议改动都要借构建机，实际墙钟时间会明显长于人日估算。

### 8.3 可行性

1. **B3a/B3b 可行性高**：改动点明确、局部，0.8d 可完成；唯一风险是记录层热路径，已用基准门控约束。
2. **B4 可行性高**：proto 已有 `client_handshake`，双端内存对接无需网络，现有单测可作模板。
3. **B5a 的 117 项估时偏乐观**：2d 只够覆盖 proto 7 项与主 crate 关键路径；全量 117 项需 4–5d。**已在验收标准中限定为"proto + 关键路径优先"，不承诺一次清零。**
4. **C1c 存在设计不确定性**：`PanelClient` 内部持有 ETag/body hash 缓存，热替换时缓存是否保留会影响一次多余全量拉取，需在 C1a 阶段定清。
5. **A1/A2 的瓶颈是输入不是开发**：脚本已就绪，历史已验证 staging 账户创建成功。

### 8.4 潜在遗漏与补充

| 遗漏 | 补充 |
|---|---|
| 未列 Linux E2E 的常态化环境 | 已记为 R2；建议在轨道 B 第 3 步前明确一台可长期使用的测试机，否则每次回归都借生产 |
| 未列 `RelayBufferSize` 双层口径说明 | 并入 B2：文档需区分配置层 32768 与 proto 加密层 16384 |
| 未列面板补丁失效的监测 | 已记为 R7；建议在面板升级后复核 `PushIdempotency` 是否仍真实去重 |
| 未列 v0.2.1 性能改动的生产验证 | B1 只保证 CI 与制品自洽；是否部署生产由用户决定，部署需按 §2.3 跑 E2E |
| 未列 Windows/Linux 测试数差异的说明 | 已写入 §1.1，避免后续误判为覆盖减少 |
| 未列 `x509-parser`/`hex` 依赖复核 | 使用面窄但必要（证书到期解析、密钥打印），不列为待删；若后续做依赖瘦身再评估 |

### 8.5 与技能红线的对照

| 红线 | 当前 | 计划覆盖 |
|---|---|---|
| 生产代码零 panic | **0（第二轮已清零）** | B3a 完成 |
| 零 unsafe | 0 | 已达标，workspace deny 锁定 |
| 错误显式处理 | **4 处已修（第二轮）** | B3b 完成；其余 31 处保留 best-effort |
| clippy 全绿 | 通过 | §2.3 每次发版门控 |
| fmt 全绿 | 通过 | 同上 |
| 公开项有文档 | proto 7 项已补齐；主 crate 110 项待后续 | B5a 分批 |
| `# Examples` | **3 个（第二轮）** | B5b 首批完成 |
| 惯用 API | 基本符合 | 随 B5c 顺带收敛 |

### 8.6 执行记录

**第一轮（2026-11-14，发布收口）**：
- 推送 3 个性能提交到 `origin/main`（Git Data API，SHA 逐一校验一致）；
- 删除本地 `v0.2.1` 标签（SHA 已记录，可重建）；
- 远端 CI 三作业全绿，Linux 66 tests 证实。

**第二轮（2026-11-14，用户决策后自主推进）**：

| 任务 | 状态 | 证据 |
|---|---|---|
| 决策落库（ACME 延后 / 加密保持 / 自研客户端承接兼容） | 完成 | 本文件 §2 / §3.1a / §7 |
| B2 文档一致性 | 完成 | README 死链清零、基线绑工具链（0.1.99=143 / 1.100=147）、Makefile/Cargo.toml 注释同步、lib.rs 与 benchmark-spec 引用修正、RelayBufferSize 双层口径、TODO.md 并入 §3.5 |
| B3a 生产 panic 点 | 完成 | encryption.rs let-else 替换；vless_probe 两处 ok_or_else；复扫零命中（文档示例内的 unwrap 属测试代码） |
| B3b 可恢复错误 | 完成 | node.rs 启动期流量显式 add_all；3 处 TLS 回滚 Err 记 warn |
| B8 无用依赖 | 完成 | 主包 blake3 移除，Cargo.lock 仅删 1 行 |
| B4 黑盒契约测试 | 完成 | `crates/v2node-proto/tests/handshake_contract.rs` 9 例全过；错误路径断言经变异测试验证（反转即失败，已还原复跑） |
| B5a/B5b proto 文档 | 完成 | 7 个 pub 项全部补 ///（含 # Errors）；3 个 # Examples 文档测试全过 |
| 质量门禁 | 全绿 | fmt / clippy -D warnings / **77 tests**（65 原有 + 9 契约 + 3 文档） |

**未做**：
- 未创建远端标签、未发布 Release、未部署生产；
- 未执行 Linux E2E（本机 Windows 不具备条件，CI 只跑单测）；
- 未连接生产复核运行版本与日志级别，§1.3 生产版本与 B7 的级别矛盾均引自历史记录，需实机核实；
- 主 crate 110 个 pub 项文档与 B5c 严格收敛未开始（后续批次）。

### 8.7 本轮踩到并纠正的测量陷阱

| 陷阱 | 表现 | 纠正 |
|---|---|---|
| CI 日志带时间戳 + ANSI 色码 | `grep -c "^warning: "` 得 0，误判 Linux 严格模式无问题 | 先剥离时间戳前缀与 `\x1b[…m`，再统计，得 147 |
| Python 不认 Git Bash 的 `/tmp` | `FileNotFoundError: '\tmp\...'` | 统一用 `cygpath -w` 转换后传参 |
| Git Data API 时区处理 | 初版把 `+0800` 归一成 UTC，会使 commit SHA 必然不同 | 保留原始偏移构造 ISO 时间；推送前逐步校验 SHA，不一致即中止 |