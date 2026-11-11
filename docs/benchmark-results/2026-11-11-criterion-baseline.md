# 正式基准基线：criterion L1（2026-11-11）

> 规范依据：`docs/benchmark-spec.md` v1.0 §2 / §3 / §5
> 状态：**正式基线**。作为 C0b 抽 crate 前后代码执行效率对比的基准；**仍不可用于跨语言结论**（缺 Go 对照组）。

## 1. 元数据与复现

| 项 | 值 |
|---|---|
| 提交 | `0d5a063`（含本轮未提交改动） |
| 编译器 / 架构 | rustc 1.99.1 / x86_64 / Windows 本地开发机 |
| 工具 | `criterion 0.8.2`，离线接入（本地 cargo 缓存已有完整依赖链） |
| harness | `benches/proto_bench.rs`，`[dev-dependencies]` + `[[bench]] harness = false` |
| profile | `cargo bench`（bench profile 继承 `--release` 优化） |
| 样本 | criterion 默认 100 次采样 / 组；逐次探针 5,000 样本 + 500 预热 |
| 复现命令 | `cargo bench --bench proto_bench` |
| 网络 IO | **无**（纯函数 / 内存流） |

## 2. criterion 区间估计（lower / 估计值 / upper）

| Workload | lower | estimate | upper |
|---|---:|---:|---:|
| W1 `derive_key_raw` | 164.47 ns | **165.63 ns** | 167.09 ns |
| W2 `ClientEncryptionConfig::parse` | 165.07 ns | **165.69 ns** | 166.35 ns |
| W3 `EncryptionServer::from_decryption` | 13.060 µs | **13.128 µs** | 13.203 µs |
| W4 AES-256-GCM seal 8192B | 2.1706 µs | **2.1862 µs** | 2.2041 µs |
| W5 AES-256-GCM open 8192B | 2.1887 µs | **2.2060 µs** | 2.2272 µs |
| W6 pump 式分配+拷贝 8192B | 190.34 ns | **193.19 ns** | 197.30 ns |

## 3. 逐次分位数探针（补齐规范 §2 的 P50 / P99）

| Workload | P50 | P99 | P99.9 |
|---|---:|---:|---:|
| W1 `derive_key_raw` | 200 ns | 200 ns | 200 ns |
| W2 `ClientEncryptionConfig::parse` | 200 ns | 200 ns | 300 ns |
| W3 `EncryptionServer::from_decryption` | 12,500 ns | 17,400 ns | 34,200 ns |
| W4 AES-256-GCM seal 8192B | 2,200 ns | 2,700 ns | 4,800 ns |
| W5 AES-256-GCM open 8192B | 2,200 ns | 2,800 ns | 8,900 ns |
| W6 pump 式分配+拷贝 8192B | 200 ns | 400 ns | 1,700 ns |

**精度限制（必须随数字一起引用）**：

1. Windows 本机 `Instant` 分辨率约 **100 ns**，因此亚微秒项（W1/W2/W6）呈 100 ns 台阶，P50/P99 只能作上界参考；正式判定以 criterion 区间估计为准。
2. 逐次计时含约 20–30 ns 的 `Instant` 开销，W1 的 200 ns 中计时器占比不可忽略。
3. 长尾显著：W3 的 P99.9（34.2 µs）约为 P50（12.5 µs）的 2.7 倍；W5 的 P99.9 达 8.9 µs。若按规范 §5「P99 恶化 >5% 视为回退」，应以 **P99** 而非均值判定。

## 4. 与零依赖 harness 的交叉验证

| Workload | L1 零依赖 5 轮中位数 | criterion 估计值 | 偏差 |
|---|---:|---:|---:|
| W1 | 176.5 ns | 165.63 ns | -6% |
| W2 | 186.8 ns | 165.69 ns | -11% |
| W3 | 14,521 ns | 13,128 ns | -10% |
| W4 | 2,228.9 ns | 2,186.2 ns | -2% |
| W5 | 2,288.8 ns | 2,206.0 ns | -4% |
| W6 | 205.2 ns | 193.19 ns | -6% |

两者在 2–11% 内一致，方向相同；差异来自零依赖 harness 的外层循环与 `black_box` 开销。**criterion 数值更稳定（区间窄）且提供长尾信息，后续一律以 criterion 为正式基线**，零依赖 example 保留为无依赖环境下的快速校验。

## 5. 结论与用途

1. 正式基准工具链已建立：`cargo bench` + CI 非阻断观测（`bench-observe` job）。
2. 本批六项数值即 **C0b 抽 `v2node-proto` crate 前的回归基准**；重构后须重跑同一命令，按规范 §5 判定四维是否回退、P99 是否恶化 >5%。
3. 加密算术仍非瓶颈（W4/W5 约 2.2 µs / 3.5 GiB/s），与 L1 结论一致。
4. **剩余缺口**：Go 对照组（本机无 Go 工具链）、RSS / 每连接内存采样（需平台 API）、L3 mock 面板 / 订阅链路。三者均列入正式开发前测试报告的阻塞项。
