# 正式开发前测试流程报告

> 版本：v1.0（2026-11-11）
> 范围：**C0b（抽 `v2node-proto` crate）启动前**的全部测试流程工作
> 依据：`docs/benchmark-spec.md` v1.0、`docs/client-design-plan.md` v0.7、项目质量门禁（`Makefile` / `.github/workflows/rust.yml`）
> 环境：Windows 本地开发机，rustc 1.99.1 / x86_64，提交 `0d5a063`（含本轮未提交改动）
> **准入判定：有条件通过（Conditional GO）**

---

## 0. 结论摘要

本机可完成的测试流程**已全部执行并留档**：7 项门禁全绿、`criterion 0.8.2` 正式基准链建立、三层基准 L1/L2/L3 全部落地、四维指标覆盖 3 项完整 + 1 项部分。

三项受环境阻塞，如实登记：**Go 语言对照组**（本机无 Go 工具链）、**Linux 8 套 E2E + Xray 互通**（需 musl 二进制与 mock 面板）、**C0a 面板 UA 采集**（无访问路径）。其中 E2E 是 C0b **完成验收**的硬前置，不阻塞其启动。

**C0b 可以启动**，但须锁定本报告的 criterion 基线作为重构前对照，并在完成后到 `<BUILD_HOST_IP>` 跑全量 E2E。所有基准数字**不得用于跨语言结论**。

---

## 1. 范围界定

「正式开发前测试流程」在本报告中被界定为以下七类，避免范围歧义：

| 类别 | 内容 | 本轮状态 |
|---|---|---|
| 回归门禁 | fmt / clippy / test / 全 target 编译 / bench 编译 / diff | **完成**，7 项全绿 |
| 测试资产盘点 | 单测、探针、E2E、互通脚本、CI 作业 | **完成**（E2E 本机不可执行，已盘点） |
| 基准工具链 | criterion 接入、CI 观测、结果归档格式 | **完成** |
| 三层基准 | L1 微基准 / L2 组件层 / L3 整机层 | **完成** |
| 四维指标 | 延迟 / 内存 / 吞吐 / 多线程 | **3 完整 + 1 部分**（内存缺 RSS） |
| 跨语言对照组 | Go 内核同机同负载对比 | **阻塞**（本机无 Go） |
| 用户侧数据采集 | C0a 面板 UA 统计 | **阻塞**（无访问路径），属 G1 门控输入 |

不包含：客户端立项（C1）相关测试、生产变更、灰度演练、真实 ACME 验收（P2-2，仍等外部输入）。

---

## 2. 门禁执行结果（本机实测）

| # | 命令 | 结果 |
|---|---|---|
| 1 | `cargo fmt -- --check` | **通过** |
| 2 | `cargo clippy --all-targets -- -D warnings` | **通过**，0 warning |
| 3 | `cargo clippy --all-targets -- -W clippy::pedantic -W clippy::nursery` | 观测 **215 条**（非阻断，严格模式基线，逐条收敛用） |
| 4 | `cargo test` | **通过**：主程序 64 / 探针 9 / 集成 0，共 73 tests，0 failed |
| 5 | `cargo check --all-targets --all-features` | **通过** |
| 6 | `cargo bench --bench proto_bench --no-run` | **通过**（CI `bench-observe` 的编译步骤） |
| 7 | `git diff --check` | **通过**；期间把 `.github/workflows/rust.yml` 规范化为 LF（符合 `.gitattributes` 的 `eol=lf`） |

补充：`cargo bench --bench proto_bench` 全量运行成功，生成 criterion 正式基线与逐次分位数；`cargo run --release --example bench_l2_pump` / `bench_l3_loopback` 均运行成功。

> `make` 在 Git Bash 不可用，门禁以 `Makefile` 中 `quality` 的等价三条命令执行；CI 上的 `quality` job 仍是硬门禁。

---

## 3. 测试资产盘点

| 资产 | 数量 / 位置 | 本轮状态 |
|---|---|---|
| 主程序单测 | 64 | 全绿 |
| `vless_probe` 探针单测 | 9 | 全绿 |
| 集成测试（Rust） | 0 | 无 |
| E2E 脚本 | 8 套：`e2e.sh`、`e2e_acme.sh`、`e2e_config.sh`、`e2e_encryption.sh`、`e2e_limits.sh`、`e2e_report.sh`、`e2e_tls.sh`、`e2e_acme_real.sh` | **本机不可执行**（依赖 `/tmp/v2node-e2e`、`/opt/v2node-rust-target/debug`、Python mock 面板） |
| 官方互通 | `interop_xray.sh`（Xray v26.11.9 双向） | 同上，需 Linux 构建机 |
| Python mock | `mock_panel.py`、`mock_cloudflare.py`、`http_echo.py` | 已盘点 |
| CI 作业 | `quality`（硬门禁）、`strict-report`（非阻断）、**`bench-observe`（本轮新增，非阻断）** | 硬门禁保持不阻断新代码 |

---

## 4. 三层基准结果汇总

| 层 | Workload | 关键结果 | 归档 |
|---|---|---|---|
| **L1 正式** | criterion 区间估计 | W1 165.63 ns、W2 165.69 ns、W3 13.128 µs、W4 2.1862 µs、W5 2.2060 µs、W6 193.19 ns | `2026-11-11-criterion-baseline.md` |
| **L1 正式** | 逐次分位数探针 | W3 P99 17.4 µs / P99.9 34.2 µs；W4 P99 2.7 µs；W5 P99 2.8 µs | 同上 |
| L1 零依赖 | 5 轮中位数（交叉验证） | 与 criterion 偏差 2–11%，方向一致 | `2026-11-11-l1-baseline.md` |
| **L2 组件** | W7 握手 / W9 吞吐 / W10 worker 扩展 | 484.1 µs；0.90 ms / 1,115 MiB/s；1→8 worker 加速比 1.00→4.10x | `2026-11-11-l2-baseline.md` |
| **L3 整机** | W11 连接+握手 / W12 吞吐 / W13 连接阶梯 | 607.8 µs；1.76 ms / 568.5 MiB/s；1/8/32 会话 = 332/955/901 MiB/s | `2026-11-11-l3-baseline.md` |

**三条跨层结论**：

1. **系统边界成本已量化**：L2（`DuplexStream`）→ L3（真实 TCP 回环），1 MiB 吞吐 1,115 → 568.5 MiB/s（约减半），握手 484.1 → 607.8 µs（+25.6%）。这部分不受用户态语言影响，印证 `client-design-plan.md` 附录 C.1 的收益边界。
2. **L1 结论曾反转**：L1 据 W6（193 ns 分配+拷贝）判断「零拷贝收益上限约 10%」；L2 显示完整 pump 仅为裸 AEAD（约 3.5 GiB/s）的 1/3，差额来自分片、输出 `Vec`、双层流拷贝与调度。已在 L1 结果文件加修正注记——**微基准不足以判断管线级优化优先级**。
3. **多线程扩展在 4 worker 后饱和**：L2 的 1/2/4/8 = 1.00/2.00/3.37/4.10x；L3 的 32 并发相对 8 并发聚合吞吐略降。桌面端 4 worker 是经济区间。

---

## 5. 四维覆盖矩阵

| 维度 | 覆盖 | 采集方式 | 剩余缺口 |
|---|---|---|---|
| 更低延迟 | **完整** | criterion 区间估计 + 逐次 P50/P99/P99.9 探针；L2/L3 握手中位数 | Windows `Instant` 约 100 ns 粒度，亚微秒项分位数仅作上界；正式判定以 criterion 为准 |
| 更低内存占用 | **部分** | W6 分配+拷贝模拟 | **RSS / 每连接增量 / 每 MiB 分配字节数未采**（需平台 API 或外部采样） |
| 更高吞吐 | **完整** | L1 ops/s、L2 单会话与聚合、L3 回环与阶梯 | 数字含 harness 开销，不可跨层直接相减归因 |
| 更强多线程能力 | **完整** | L2 worker 扩展曲线、L3 连接阶梯 | 并发阶梯为 1/8/32，未达规范 W10 的 1/40/200/1000 |
| 跨语言对比 | **未覆盖** | — | Go 对照组阻塞 |

---

## 6. 阻塞项与风险登记

| 项 | 原因 | 影响 | 解除条件 | 是否阻断 C0b |
|---|---|---|---|---|
| Go 内核对照组 | 本机 `go: command not found` | 无法证明「Rust 比 Go 高效」 | 在 `<BUILD_HOST_IP>` 构建机执行或安装 Go，约 2–3 人日 | **否**（规范允许先建 Rust 侧基线） |
| 8 套 E2E + `interop_xray.sh` | 需 Linux 路径、musl 二进制、Python mock | 无法验证重构后协议与面板语义无回归 | 构建机执行全套并留档 | **否（启动）/ 是（完成验收）** |
| C0a 面板 UA 采集 | 未提供面板机访问方式与 access log 路径 | G1 立项门控缺用户侧数据 | 提供只读访问；方案见 `client-design-plan.md` §6 | **否**（属 G1 输入） |
| RSS / 每连接内存采样 | 需平台 API 或外部进程采样 | 内存维度不完整 | 接入平台 API 或 PowerShell 采样 | **否** |
| P99 探针精度 | Windows 计时器 100 ns 粒度 | 亚微秒项分位数台阶化 | 以 criterion 区间估计为准，或在 Linux 重跑探针 | **否** |

---

## 7. 准入判定（Go / No-Go）

**有条件通过（Conditional GO）——允许启动 C0b，须满足三项前置：**

1. **锁定基线**：以 `2026-11-11-criterion-baseline.md` 六项数值作为重构前对照。C0b 完成后必须重跑 `cargo bench --bench proto_bench`，按 `benchmark-spec.md` §5 判定：四维不得有未说明回退，**P99 恶化 >5% 视为回退不予合并**。
2. **完成验收须上构建机**：8 套 E2E + `interop_xray.sh` 在 `<BUILD_HOST_IP>` 全量跑通；本机结果不能替代。
3. **工程质量同步**：新增 lib crate 后 `[lints]` 由 package 级改 `[workspace.lints]`，CI `quality` 覆盖新 crate；每步 `make quality` 等价三条全绿。

**不建议**在 Go 对照组与 C0a 数据到位前进入 C1 立项（与 G1 门控一致）。本报告不构成跨语言性能结论。

---

## 8. 本轮变更清单

| 文件 | 变更 |
|---|---|
| `Cargo.toml` | 新增 `[dev-dependencies] criterion = "0.8.2"` 与 `[[bench]] proto_bench`（`harness = false`） |
| `Cargo.lock` | 新增 criterion 依赖链（离线解析，缓存齐全） |
| `benches/proto_bench.rs` | **新建**：6 组 criterion 基准 + P50/P99/P99.9 逐次探针 |
| `examples/bench_l3_loopback.rs` | **新建**：L3 本机 TCP 回环基准 |
| `.github/workflows/rust.yml` | 新增非阻断 `bench-observe` job；CRLF → LF 规范化 |
| `docs/benchmark-results/` | 新增 criterion / L3 / 本报告；L1 结果补 L2 修正注记；L2 结果已在库 |
| `docs/benchmark-spec.md` | §7 落地状态更新：正式工具链、L3、CI、报告索引 |
| `docs/client-design-plan.md` | 升 v0.7，附录 B 记录本轮收敛 |
| `examples/bench_proto.rs`、`bench_l2_pump.rs` | 前轮已建，本轮保留为零依赖快速校验入口 |

**未改动**：`src/` 下任何节点源码、生产配置、面板文件。

---

## 9. 复现命令清单

```bash
# 全部质量门禁（Makefile quality 的等价三条）
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test

# 全 target / 全 feature 编译
cargo check --all-targets --all-features

# 正式基准（区间估计 + 逐次分位数）
cargo bench --bench proto_bench

# 三层基准
cargo run --release --example bench_proto
cargo run --release --example bench_l2_pump
cargo run --release --example bench_l3_loopback

# CI bench 编译步骤
cargo bench --bench proto_bench --no-run

# 换行与空白校验
git diff --check
```

## 10. 已知限制（引用数字时必须随附）

1. 所有基准均为**同机同环境**数据，只可用于本仓库自身的前后对比；跨机器、跨规则集、跨并发数的数字不可比。
2. L2 / L3 数字包含 harness（`DuplexStream` / socket / 任务调度）开销，不等于纯产品代码开销。
3. 公网 CF 链路数据**不计入验收**，仅作回归冒烟（`benchmark-spec.md` §0）。
4. Windows `Instant` 约 100 ns 粒度；亚微秒项的逐次分位数仅作上界，正式判定以 criterion 区间估计为准。
5. debug 构建数字不代表性能；所有数字来自 `--release` / `cargo bench`。
