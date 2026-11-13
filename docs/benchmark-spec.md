# 跨语言性能基准规范

> 版本：v1.0（2026-11-11）
> 适用范围：**本项目所有后续编程语言之间的性能对比**（Rust / Go / C / 汇编 / 其他），涵盖节点侧、客户端侧、协议层与未来新增模块。
> 依据：用户 2026-11-11 确认「所有后续编程语言之间的测试都按照这个标准」；指标源自原客户端设计规划附录 C.5（该文档已随客户端拆分迁至 McloudClient 仓库）。
> 状态：**已生效**。任何声称「A 语言比 B 语言更快 / 更省 / 吞吐更高」的结论，必须引用本规范的测量结果。

---

## 0. 四条原则

1. **唯一标准是代码执行效率**，四个维度：更低延迟、更低内存占用、更高吞吐、更强多线程能力。
2. 端到端公网测速、Cloudflare 链路、真实 DNS 查询、wintun / IOCP 系统调用开销 **不计入验收**，只作回归冒烟。
3. **测量必须隔离网络**。否则测到的是链路，不是代码。
4. **无基线不启动优化**；每个优化必须附同环境 before/after，否则不合并。

「延迟」两分：端到端延迟（不计入）与代码执行延迟（单次解析 / 封装 / 决策的 CPU 时间，核心指标）。

---

## 1. 统一 workload（跨语言可复现）

任何语言实现必须跑同一组 workload，输入与输出语义一致。

| ID | Workload | 输入 | 主要观察维度 |
|---|---|---|---|
| W1 | 密钥派生 | 固定 32 字节 key material + `VLESS` context | 延迟 |
| W2 | 配置解析 | 固定 `mlkem768x25519plus.*` 配置串 | 延迟 |
| W3 | 服务端配置构造 | 固定 `decryption` 串（含 X25519 公钥计算） | 延迟 |
| W4 | AEAD 封装 | 单条 8192 字节记录 | 延迟 / 吞吐 |
| W5 | AEAD 解封 | W4 的密文 | 延迟 / 吞吐 |
| W6 | 分段分配与拷贝 | 8192 字节记录分片 | 延迟 / 内存 |
| W7 | VLESS 请求头解析 | 固定合法请求头 | 延迟 |
| W8 | 规则匹配单次决策 | 3 万条固定规则集 | 延迟 / 吞吐 |
| W9 | 本机回环 echo | 1 MiB 单连接、16 并发 × 1 MiB | 吞吐 / 多线程 |
| W10 | 并发阶梯 | 1 / 2 / 4 / 8 worker；1 / 40 / 200 / 1000 连接 | 多线程 / 内存 |

W1–W8 属 L1 / L2 层；W9–W10 属 L3 层。新增 workload 需在本表登记后再纳入对比。

---

## 2. 四维度量定义

| 维度 | 指标 | 单位 | 统计口径 |
|---|---|---|---|
| 延迟 | 单次操作耗时 | ns/op、µs/op | **P50 与 P99 同时报**，不只看均值 |
| 内存 | 空载 / 稳态 / 峰值 RSS | MiB | 进程采样，区分启动后与长稳压测 |
| 内存 | 每连接增量、每 MiB 分配次数与字节数 | KiB/conn、次/MiB、B/MiB | 分配计数器 |
| 吞吐 | MB/s、conn/s、decisions/s、records/s、ops/s | 每秒 | 固定时窗内完成量 |
| 多线程 | 加速比、P99 随并发变化、锁等待、争用次数、任务数、CPU% | 倍 / ns / 次 / % | 1/2/4/8 worker 曲线 |

四维均须记录；**不得只报有利维度**。

---

## 3. 三层测量方法

| 层 | 环境 | 覆盖 |
|---|---|---|
| **L1 微基准** | `criterion` 或等价零依赖 harness；纯函数、无 IO、无网络 | W1–W8 的延迟与 ops/s |
| **L2 组件基准** | `DuplexStream` / 内存流，走完整协议栈但不进网络 | 分配、缓冲池、记录吞吐 |
| **L3 整机基准** | 本机回环 + mock 面板 / 节点，复用 `vless_probe` 与 `tests/e2e*.sh` 探针方法 | 并发扩展、RSS、长稳压测、分段打点 |

公网 CF 链路**只做回归冒烟，不作验收数据**。

> C0e 首版 L1 harness 为 `examples/bench_proto.rs`（零依赖，用 `#[path]` 复用 `src/encryption.rs`，不改 `Cargo.toml`）。正式纳入 CI 观测时切换 `criterion` + `cargo bench`，先积累约 2 周基线，不设硬阈值。

---

## 4. 跨语言对照组硬约束

**同机对比是结论成立的前提**：

1. 同一物理机 / 同一 OS、固定 CPU 亲和、同一规则集、同一输入数据、同一并发模型。
2. 关闭无关进程；每组 ≥5 轮，报中位数与 P99；预热轮单独剔除，不计入。
3. 必须记录：`commit`、编译器版本（rustc / go version）、编译 flags、CPU 型号、内存、OS、时间戳、复现命令。
4. Go 侧：固定 `GOMAXPROCS`，记录 `GOGC`，不得使用区别于生产构建的特殊 `-gcflags`。
5. Rust 侧：`--release`，`lto` / `codegen-units` / `strip` 与发布配置一致（见 `Cargo.toml` `[profile.release]`）。
6. 汇编实现：仅在门控通过后参与对比，且必须附双平台结果与 `unsafe` 不变量说明。

**禁止**：跨机器、跨规则集、跨并发数直接比数字；拿端到端公网结果证明代码效率；只报均值不报 P99；用 debug 构建数字代表性能。

---

## 5. 判定规则

1. 每项记录 `baseline → 优化后 → 变化率`；**目标数值在基线建立后确定，不预设数字**。
2. **四维不得出现未说明的回退**（避免用内存换吞吐却无人察觉）。
3. **P99 恶化 >5% 视为回退**，不予合并。
4. 吞吐 / 内存提升须附原始数字与复现命令。
5. `cargo bench` 纳入 CI **观测**，不设硬阈值；`tracing` 分段打点常驻。
6. **汇编门控**：仅当 L1 微基准证明某热点占 CPU 时间 >10%、且现有库与编译器自动向量化已无空间时，才允许单点引入。原「占端到端总耗时 >10%」在用户态占比 <1% 的链路下永不触发，已废弃。

---

## 6. 结果记录格式

统一字段，便于跨语言汇总：

```text
date, commit, language, compiler, workload, layer, metric, unit, p50, p99, samples, notes
```

- 结果归档到 `docs/benchmark-results/`（首批数据产生时创建），只存聚合数字与复现命令。
- **不得**归档订阅 token、用户 ID、IP、原始日志行或私钥。
- 每组结果必须能被一条命令复现；不可复现的数字不进入对比结论。

---

## 7. 落地状态（2026-11-11）

| 项 | 状态 |
|---|---|
| 本规范 | 已生效，适用于所有后续跨语言测试 |
| C0e（性能基线） | **已批准，与 C0b 并行启动** |
| C0b（提前重构） | **已批准，与 C0e 并行启动**；范围锁定 N0–N2，N3 为长期目标 |
| L1 微基准 harness | `examples/bench_proto.rs`，覆盖 W1–W6，首批结果已归档 `docs/benchmark-results/2026-11-11-l1-baseline.md`（baseline 种子，不可用于跨语言结论） |
| L2 组件层 | **已落地**：`examples/bench_l2_pump.rs`（W7 握手 / W9 单会话吞吐 / W10 worker 扩展），结果归档 `docs/benchmark-results/2026-11-11-l2-baseline.md` |
| **正式基准工具链** | **已接入**：`criterion 0.8.2`（`[dev-dependencies]` + `[[bench]]`），`benches/proto_bench.rs` 提供区间估计与逐次 P50/P99/P99.9 探针；基线见 `docs/benchmark-results/2026-11-11-criterion-baseline.md`。后续代码执行效率判定一律以其为准 |
| L3 整机层 | **已落地（数据面范围）**：`examples/bench_l3_loopback.rs`，本机 TCP 回环 + 完整加密泵 + 连接阶梯；结果见 `docs/benchmark-results/2026-11-11-l3-baseline.md`。未接 mock 面板 / 订阅、未采 RSS |
| Go 内核对照组 | **已批准纳入，当前阻塞**：本机 `go: command not found`，需在 `<BUILD_HOST_IP>` 构建机执行或先安装 Go；约 2–3 人日 |
| CI 观测 | **已接入**：`.github/workflows/rust.yml` 新增非阻断 `bench-observe` job（编译 + 运行 `cargo bench --bench proto_bench`），不设硬阈值 |
| 正式开发前测试总报告 | `docs/benchmark-results/2026-11-11-predev-test-report.md` |

---

## 8. 与项目质量门禁的关系

新增基准代码同样受质量门禁约束，不得引入新 clippy 警告：

```bash
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

E2E 与 clippy **以 Linux 构建机（`<BUILD_HOST_IP>`）结果为准**——本地 Windows 与 Linux 的 clippy 版本存在差异，已有误报先例。`[lints]` 为 package 级，新增 lib crate 后需改 `[workspace.lints]` 并让 CI `quality` 覆盖。
