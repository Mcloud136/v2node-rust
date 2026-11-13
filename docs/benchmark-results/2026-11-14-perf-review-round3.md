# 性能复审报告（第三轮 · 实机取证版）

> 日期：2026-11-14
> 基线：`main` @ `8e70b0f`（含 ring AES-NI + 缓冲池两个性能提交）
> 方法：四台生产节点实机 SSH 取证 + srv85 系统调用级实测 + 本机 crate 源码核实
> 与前两轮关系：第一轮（纯代码分析）→ 第二轮（代码复审 + 联网检索）→ 本轮（实机验证 + 修正误判）

---

## 1. 四台节点 AES-NI 实测结果

逐台 SSH 读取 `/proc/cpuinfo` flags 行，**词边界精确匹配** `aes`（避免子串误报）：

| 节点 | NodeID | CPU | AES-NI | VAES/AVX | 内核 | io_uring |
|---|---|---|---|---|---|---|
| Node 1（<NODE1_IP>） | 1 | Intel Xeon Gold 5218 | **YES** | avx2 + avx512f | 7.0.0-38 | 待测 |
| Node 3（<NODE3_IP>） | 3 | Intel Xeon Gold 6152 | **YES** | avx2 + avx512f | 4.19.0-21 | 待测 |
| srv85（<NODE10_IP>） | 10 | **QEMU Virtual CPU 2.5+** | **NO** | 无（flags 被掩码） | 5.4.0-216 | **y（实测可用）** |
| Node 30（<NODE30_IP>，经代理） | 30 | AMD EPYC-Rome | **YES** | avx2（无 vaes/avx512） | 7.0.0-38 | y |

### 1.1 关键修正：srv85 无 AES-NI

**srv85 是 QEMU 虚拟机，宿主机未透传 AES-NI**，flags 行无 `aes`。这印证了项目记忆里的旧记录（"srv85 CPU 无 AES/AVX"），也修正了本轮第一次探测的误报。

> **测量陷阱记录**：首次用 `grep -o " aes " /proc/cpuinfo | head -1 && echo YES || echo NO` 误报 YES——`head -1` 无论是否匹配都返回成功，`&&` 分支恒真。改用 `tr " " "\n" | grep -cx aes` 精确计数后才得到正确结论。**凡"命令成功即判定"的写法都必须先验证能失败的正例。**

### 1.2 srv85 的实际加密路径

ring 0.17.14 源码（`src/aead/aes.rs:85-98`）证实：x86_64 上先查 CPU 特性，有 AES-NI 走 `Vp`（硬件），**无则走 `Fallback`（常量时间软件实现）**——不崩溃、不自动切 ChaCha。

因此 srv85 的 VLESS Encryption 实际走 **ring AES-GCM 软件回退**，吞吐受限于软件 AES。

### 1.2.1 修正：ChaCha 对 srv85 并无确定收益（原判断错误）

本报告初稿曾断言"ChaCha20 在无 AES-NI 的机器上通常比软件 AES 快，故 N-3 对 srv85 价值最大"。**该判断已被实测推翻。**

后续读取 srv85 完整 flags 行，实测结果：

```
flags: fpu de pse tsc msr pae mce cx8 apic sep mtrr pge mca cmov pat
       pse36 clflush mmx fxsr sse sse2 syscall nx lm constant_tsc nopl
       xtopology cpuid tsc_known_freq pni cx16 x2apic hypervisor
       lahf_lm cpuid_fault pti
```

srv85 **只有 SSE/SSE2/PNI(SSE3)，没有 SSSE3、没有 AVX2、没有 AES-NI**。

ring 的 ChaCha 后端选择（`src/aead/chacha.rs:126-151` 源码核实）在 x86_64 上按此顺序运行时检测：

| 条件 | 走的实现 |
|---|---|
| 长度 ≥129B 且有 AVX2 | `ChaCha20_ctr32_avx2` |
| 长度 ≥129B 且有 SSSE3 | `ChaCha20_ctr32_ssse3_4x` |
| 都没有 | `ChaCha20_ctr32_nohw`（纯 C 软件） |

因此 srv85 上两条路径**都是纯软件**：

| 算法 | srv85 实际路径 |
|---|---|
| AES-GCM | `Fallback` 纯软件（无 AES-NI） |
| ChaCha20-Poly1305 | `nohw` 纯 C 软件（无 SSSE3/AVX2） |

**结论修正**：ChaCha 换 ring 对 srv85 **没有确定收益**——两者都无 SIMD 可用，快慢取决于具体软件实现，不能断言 ChaCha 更快。原判断隐含假设了"无 AES-NI 但有 SSSE3/AVX2"，实测证明该前提不成立。

**检测机制补充（回答"ChaCha 是否与 AES 一样自动检测"）**：是，机制一致且更精细——运行时检测 CPU 特性，支持则用硬件/SIMD，不支持自动回退软件，**不崩溃、无需编译开关**。AES 依赖专用加密指令（AES-NI/VAES），ChaCha 依赖通用 SIMD（AVX2/SSSE3/NEON），二者依赖的硬件不同。

### 1.3 对其余三台的意义与 N-3 适用场景判定

Node 1/3/30 有 AES-NI，ring 走硬件路径，VLESS Encryption 吞吐不受软件 AES 限制。Node 1/3 还有 avx512f，理论上可走 VAES-512（ring 是否启用需进一步确认，本轮未测）。

**N-3（ChaCha 换 ring）的真实受益场景是"无 AES-NI 但有 SSSE3/AVX2"的机器**。对照四台实测：

| 节点 | AES-NI | SSSE3/AVX2 | N-3 是否受益 |
|---|---|---|---|
| Node 1 | YES | avx2 | 否（已走硬件 AES） |
| Node 3 | YES | avx2 | 否（已走硬件 AES） |
| srv85 | NO | **都没有** | 否（两条路径都纯软件） |
| Node 30 | YES | avx2 | 否（已走硬件 AES） |

**四台节点无一落在 N-3 的受益区间。** 因此 N-3 的价值不在当前生产性能，而在：① 代码对称性（AES/ChaCha 统一用 ring）；② 未来部署到"无 AES-NI 但有 SIMD"机器时的保险。优先级见 §3.1。

---

## 2. io_uring / splice 实测：可用，但对加密代理无收益

### 2.1 系统调用级实测（srv85，真实运行非配置推断）

用 Python ctypes 直接调系统调用，在 srv85 实际执行：

| 测试 | 结果 | 证据 |
|---|---|---|
| `io_uring_setup(8, params)`（syscall 425） | **OK，fd=3** | 内核 5.4 `CONFIG_IO_URING=y`，真实创建成功 |
| `splice(pipe→socket)`（syscall 275） | **OK，22 字节** | 零拷贝原语可用 |
| `splice(socket→pipe→socket)` | **OK，19→19 字节** | 代理转发原语可用 |

**结论：io_uring 与 splice 在 srv85 内核层面完全可用。** 这推翻了第二轮"无法实现"的笼统判断——那个判断的依据是"加密路径不能零拷贝"，而非"内核不支持"。两者必须分开。

### 2.2 决定性测试：TLS socket 能否 splice

模拟加密代理核心约束，对 TLS socket 的底层 fd 做 splice：

```
splice(TLS_socket→pipe): ret=100 errno=0
spliced bytes = b'\x17\x03\x03\x00\xfa\xf6\xd5\xda...'
```

**拿到的是 `\x17\x03\x03` 开头的 TLS 密文记录**（0x17=23 = TLS application_data），不是明文。后续 `tls.recv()` 报 `WRONG_VERSION_NUMBER`——因为密文已被 splice 从内核缓冲搬走，SSL 层状态错乱。

### 2.3 为什么加密代理用不上 splice 零拷贝

splice 的本质是**内核态数据搬运，不经过用户态**。而 VLESS 代理的核心工作是：

```
客户端 TLS 密文 → [用户态: TLS解密 → VLESS解帧 → 路由/限速/计量 → VLESS封帧 → TLS加密] → 目标 TLS 密文
```

每一步都必须在用户态读写明文。splice 只能搬"已经是最终形态的字节"，无法参与加解密。实测证明它从 TLS socket 搬出的是密文——**对代理毫无用处，因为代理要的是明文**。

**io_uring 的推理依据**：它优化的是 syscall 提交/完成的批处理开销，而下方 §2.4 实测显示转发总成本的约 89% 在用户态加密，syscall/拷贝仅占约 11%。**注意：io_uring 本身未做吞吐基准，此判定为推理而非实测**，详见 §2.5 局限。

### 2.4 转发路径吞吐基准（srv85 实测，两轮）

在 srv85 用 C 程序实测三种转发模式，每模式 256 MiB，`AF_UNIX socketpair` 回环：

| 模式 | 轮1 吞吐 | 轮2 吞吐 | 轮1 CPU/MiB | 实现 |
|---|---|---|---|---|
| A splice 零拷贝 | 604.2 MiB/s | 550.7 MiB/s | 0.0016s | socket→pipe→socket |
| B 用户态明文拷贝 | 480.4 MiB/s | 436.4 MiB/s | 0.0021s | read→write |
| C 用户态加密转发 | **53.6 MiB/s** | **48.0 MiB/s** | **0.0185s** | read→加密→write |

**关键比值（两轮一致）**：

- splice 比用户态拷贝快 **1.26×**（省一次用户态往返）
- 加密转发比明文拷贝慢 **约 9×**

**成本分解**：C 的 0.0185s CPU/MiB 中，拷贝占 0.0021s（**11%**），加密占 0.0164s（**89%**）。即使 splice 能消除全部拷贝成本，加密路径吞吐上限也仅提升约 **13%**；而 §2.2 已证明 splice 只能搬密文，连这 13% 都拿不到。

**敏感性检验**：把加密成本降到 1/10（1 轮 S-box 代替 10 轮），吞吐 181–232 MiB/s，仍比 splice 慢 2.4–3.3×。**结论对加密模型精度不敏感**。

复现命令（程序已按纪律从 srv85 清理，需要时重建）：

```bash
gcc -O2 -o bench_forward bench_forward.c -lpthread
./bench_forward 256   # 参数为每模式 MiB
```

### 2.5 最终判定与证据强度

| 方案 | 内核可用 | 对本服务有用 | 判定 | 证据强度 |
|---|---|---|---|---|
| splice 零拷贝 | ✅ 实测 | ❌ 只能搬密文；天花板仅 13% | **不采用** | **实测**（syscall + 密文字节 + 吞吐基准） |
| io_uring | ✅ 实测 | ❌ 89% 成本在用户态加密 | **不采用** | **推理**（未做 io_uring 吞吐基准） |

**与第二轮的区别**：第二轮说"无法实现"是错的——内核完全支持。正确表述是"**可用但不适用**"。

**必须声明的四点局限**（关系到结论可信度）：

1. **io_uring 未做吞吐基准**。只验证了 `io_uring_setup` 可成功创建，从未测 io_uring vs read/write 的吞吐对比。"io_uring 不适用"是**基于成本分解的推理**，不是实测结论。
2. **加密是合成 S-box 循环，非真实 AES-GCM**。它只作 CPU 成本代理。srv85 无 Rust 工具链，未在生产机安装，故未能用真实 ring 软件 AES 校准。
3. **用 `AF_UNIX socketpair`，不是真实 TCP 回环**。程序内打印的"TCP 回环"措辞不准确，以本表为准。
4. **仅 2 轮，未达 [benchmark-spec](../benchmark-spec.md) 要求的 ≥5 轮中位数 + P50/P99**。10 轮敏感性组因 fork/shutdown 竞态产出 0.0，属**无效数据已丢弃**。

**用户裁决（2026-11-14）**：认可上述数据与"不采用"结论，io_uring 吞吐基准不再补做。

---

## 3. 修正后的优化路线（按实机证据重排）

### 3.1 优先级变化

| 项 | 第二轮优先级 | 本轮调整 | 理由 |
|---|---|---|---|
| N-3 ChaCha 换 ring | P2 | **维持 P2**（曾误升 P1，已撤回） | 实测 srv85 无 SSSE3/AVX2，两条路径都纯软件，无确定收益；四台节点无一落在受益区间（§1.3）。价值仅为代码对称性 + 未来保险 |
| N-1 入站 TCP_NODELAY | P0 | 维持 P0 | 与硬件无关，确定收益 |
| N-5 UDP 上行缓冲复用 | P0 | 维持 P0 | 与硬件无关，确定收益 |
| N-2 TLS 会话恢复 | P1 | 维持 P1 | 重连场景收益，与硬件无关 |
| N-4 AEAD 原地解密 | P1 | 维持 P1 | 需 L2 打点确认占比 |
| io_uring / splice | 排除 | **确认排除（附实测证据）** | 见 §2 |

### 3.2 srv85 专项建议（已按实测修正）

srv85 无 AES-NI **且无 SSSE3/AVX2**，是虚拟化 CPU 模型过于保守所致（`QEMU Virtual CPU version 2.5+`）。

**唯一确定的大杠杆是运维侧透传 CPU 特性**：

- 建议用 **`-cpu host`** 而非仅 `+aes`——一次把 AES-NI + SSSE3 + AVX2 全部透传，AES 与 ChaCha 两条路径同时受益。仅加 `+aes` 只能救 AES，ChaCha 仍是纯软件。
- 前提：**宿主机物理 CPU 必须支持这些特性**。本轮无宿主机访问权限，**未确认**，属待核实项。
- 代价：CPU 模型不能热改，**需重启 VM**（短暂停机）；`-cpu host` 后 live migration 受限（只能迁到特性相同/更新的宿主机）；对同宿主其他 VM 无影响。
- 收益量级：AES-NI 硬件指令相比软件实现典型快 5–10×，是 srv85 加密吞吐的最大单一杠杆。

**代码侧无救急方案**：原设想的 N-3（ChaCha 换 ring）经实测否定（§1.2.1），不能作为 srv85 的替代方案。

> 透传属生产虚拟化变更，须先确认宿主机 CPU 支持、评估重启窗口，本文档只记录方向，不擅自执行。用户已明确"AES-NI 先放一边"，本项暂不推进。

### 3.3 不建议做的（附证据）

- **io_uring / splice**：§2 排除。splice 有完整实测证据；io_uring 为成本分解推理（未做吞吐基准，用户已认可不再补做）。
- **编译期强制 AES-NI**（`RUSTFLAGS=+aes`）：srv85 无 AES-NI，强制会非法指令崩溃。运行时检测（现状）是正确选择。
- **VAES-512 优化**：Node 1/3 有 avx512f 但 srv85/Node30 无，且 ring 是否启用 VAES-512 未确认；收益不确定，暂不投入。
- **把 N-3 当 srv85 救急方案**：已实测否定，见 §1.2.1。

---

## 4. 自我审查

| 检查项 | 结果 |
|---|---|
| AES-NI 结论经精确重测 | 首次误报已纠正，四台均用词边界匹配复核 |
| io_uring/splice 用真实 syscall 测试 | 非配置推断，srv85 实际运行 |
| TLS splice 拿到密文有原始字节证据 | `\x17\x03\x03` TLS record 头 |
| 转发吞吐有实测数据 | 三模式 × 2 轮 + 敏感性检验（§2.4） |
| 修正了第二轮的错误判断 | "无法实现"→"可用但不适用"，附证据 |
| **修正了本轮自己的错误判断** | ChaCha 对 srv85 收益（§1.2.1）：原断言基于"有 SIMD"的错误假设，实测 flags 后撤回，N-3 从 P1 降回 P2 |
| ring 软件回退有源码依据 | aes.rs:85-98 Fallback；chacha.rs:126-151 三级检测 |
| **io_uring 判定证据强度已标注** | 明确标为推理而非实测，四点局限见 §2.5 |
| 测试文件已清理 | srv85 与本机 /tmp 均已删 |
| 未改生产配置/代码 | 全程只读探测 + 临时测试程序（已删） |
| Node 30 经代理访问 | 按用户指示用 Clash 7897 隧道，端口 22 |

### 4.0 本轮两次自我误判记录

| 误判 | 原因 | 纠正方式 |
|---|---|---|
| srv85 有 AES-NI | `grep ... \| head -1 && echo YES` 中 head 恒成功，`&&` 分支恒真 | 改 `tr " " "\n" \| grep -cx aes` 精确计数 |
| ChaCha 对 srv85 收益大 | 隐含假设"无 AES-NI 但有 SSSE3/AVX2"，未读完整 flags | 读完整 flags 发现只有 SSE/SSE2/PNI，两条路径都纯软件 |

**共同教训**：两次都是**推断先于取证**。CPU 能力必须读完整 flags 实证，不能凭"通常""大概率"假设；判定命令必须先验证"能失败的正例"。

### 4.1 未决 / 待核实

1. **Node 1/3 的 io_uring 未实测**（内核 7.0/4.19，4.19 早于 io_uring 引入的 5.1，Node 3 大概率不支持；但本轮结论"不适用"与内核支持无关，不影响判定）。
2. **ring 是否对 Node 1/3 启用 VAES-512** 未确认，需读 ring 的 vp 模块或实测。
3. **srv85 宿主机是否支持 AES-NI 透传** 未查（需宿主机访问权限，超出本节点 SSH 范围）。

### 4.2 测量纪律备忘（本轮新增）

- `grep ... && echo YES || echo NO` 中若 grep 后接 `head`，`head` 恒成功导致 YES 恒真——**判定命令必须先跑"能失败的正例"验证**。
- QEMU 虚拟 CPU 的 flags 可能被宿主机掩码，`model name` 显示 "QEMU Virtual CPU" 时不能假设有 AES-NI，必须读 flags 实证。