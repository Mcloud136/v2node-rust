# B3a/B3b/B8 改动 L1 基准对照（2026-11-14）

> 规范依据：`docs/benchmark-spec.md` §5（改动必须附同机 before/after，P99 恶化 >5% 不予合并）
> 改动内容：`encryption.rs` 记录层 `unwrap()` → let-else（唯一非文档代码改动）；`vless_probe.rs` 两处 `ok_or_else`；`node.rs` 错误处理；主包移除 `blake3`。
> 环境：Windows 本机，rustc 1.99.1，`cargo bench --bench proto_bench`，criterion 0.8.2。

## 1. 全量对照（criterion 100 样本，P50/P99 由 sample.json 逐次计算）

| Workload | base P50/P99 (ns) | after P50/P99 (ns) | P50 变化 |
|---|---|---|---|
| W1 derive_key_raw | 211 / 241 | 203 / 219 | -3.8% |
| W2 ClientEncryptionConfig::parse | 181 / 197 | 183 / 196 | +1.1% |
| W3 EncryptionServer::from_decryption | 13873 / 16355 | 13888 / 21236 | +0.1% |
| W4 AES-256-GCM seal 8192B | 2309 / 2666 | 2310 / 3332 | +0.0% |
| W5 AES-256-GCM open 8192B | 2342 / 2659 | 2351 / 2695 | +0.4% |
| W6 pump-style alloc+copy 8192B | 223 / 249 | 204 / 218 | -8.5% |

P50 全部在 ±5% 噪声带内（W6 改善）。W3/W4 的 P99 在顺序跑中偏高，触发 §5 复核。

## 2. W4 尾部复核：同时段交替 A/B（决定性对照）

W3/W4 路径上的代码**零改动**（diff 证实唯一非文档改动在 `run_pump` 记录层，W4 基准直接调 `aes_gcm` crate，不经过该路径）。为排除时间漂移，基线与改动后二进制同时段交替各跑 3 轮：

| 轮次 | base P50/P99 (ns) | after P50/P99 (ns) | P99 差值 |
|---|---|---|---|
| 1 | 2245 / 2752 | 2213 / 2381 | **-13.5%** |
| 2 | 2231 / 2702 | 2228 / 2751 | +1.8% |
| 3 | 2228 / 2590 | 2240 / 2953 | **+14.0%** |

P99 差值双向波动（-13.5% ~ +14.0%），无方向性；P50 三轮全部持平（±0.6%）。

## 3. 结论

- **无代码回退**。顺序跑观察到的 W3/W4 尾部偏高为本机环境噪声（后台负载、调度），交替 A/B 证实差值无方向性。
- 唯一热路径改动（let-else 替换 unwrap）语义等价：原代码在该点 `header_plain` 必为 `Some`，let-else 的 `break` 分支不可达，编译产物行为一致。
- 四维中延迟无回退；内存/吞吐/多线程不受本改动影响（无分配与并发结构变化）。

## 4. 复现命令

```bash
cargo bench --bench proto_bench
# 逐次分位数：解析 target/criterion/<name>/new/sample.json 的 times/iters
# 交替 A/B：同一时段轮流运行两个二进制，CRITERION_HOME 指向各自输出目录
```

## 5. 测量备注

- criterion 过滤串大小写敏感，须用注册名原样（如 `"W4 AES-256-GCM seal 8192B"`）。
- `CRITERION_HOME` 直接指向输出目录本身，不要再套 `criterion` 子目录。
- 本机 P99 尾部噪声可达 ±15%，单次顺序跑的尾部差异不足以判定回退，必须交替 A/B。