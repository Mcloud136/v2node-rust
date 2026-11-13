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

因此 srv85 的 VLESS Encryption 实际走 **ring AES-GCM 软件回退**，吞吐受限于软件 AES。这正是 N-3（ChaCha 也换 ring）对 srv85 价值最大的原因：ChaCha20 在无 AES-NI 的机器上通常比软件 AES 快。

### 1.3 对其余三台的意义

Node 1/3/30 有 AES-NI，ring 走硬件路径，VLESS Encryption 吞吐不受软件 AES 限制。Node 1/3 还有 avx512f，理论上可走 VAES-512（ring 是否启用需进一步确认，本轮未测）。

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

**io_uring 同理**：它优化的是 syscall 提交/完成的批处理开销，但本服务的瓶颈在用户态加密管线（L2 实测 pump 吞吐仅为裸 AEAD 的 1/3），不在 syscall。L3 已证明 TCP 回环 vs 内存流的差异主要是系统边界，io_uring 改不了用户态加密这个大头。

### 2.4 最终判定

| 方案 | 内核可用 | 对本服务有用 | 判定 |
|---|---|---|---|
| splice 零拷贝 | ✅ 实测可用 | ❌ 只能搬密文，代理需明文 | **不采用**（实测证据，非推断） |
| io_uring | ✅ 实测可用 | ❌ 瓶颈在用户态加密非 syscall | **不采用**（收益不抵复杂度） |

**与第二轮的区别**：第二轮说"无法实现"是错的——内核完全支持。正确表述是"**可用但不适用**"，依据是 TLS splice 实测拿到密文这一决定性证据。

---

## 3. 修正后的优化路线（按实机证据重排）

### 3.1 优先级变化

| 项 | 第二轮优先级 | 本轮调整 | 理由 |
|---|---|---|---|
| N-3 ChaCha 换 ring | P2 | **升 P1** | srv85 无 AES-NI 走软件 AES，ChaCha 软件实现通常更快；srv85 是真实生产节点 |
| N-1 入站 TCP_NODELAY | P0 | 维持 P0 | 与硬件无关，确定收益 |
| N-5 UDP 上行缓冲复用 | P0 | 维持 P0 | 与硬件无关，确定收益 |
| N-2 TLS 会话恢复 | P1 | 维持 P1 | 重连场景收益，与硬件无关 |
| N-4 AEAD 原地解密 | P1 | 维持 P1 | 需 L2 打点确认占比 |
| io_uring / splice | 排除 | **确认排除（附实测证据）** | 见 §2 |

### 3.2 srv85 专项建议

srv85 无 AES-NI 是**虚拟化配置问题**，有两条路：

1. **短期（代码侧）**：N-3 把 ChaCha 也换 ring。无 AES-NI 时 ChaCha20-Poly1305 软件实现通常优于软件 AES-GCM，srv85 直接受益。
2. **长期（运维侧）**：若宿主机支持，给 srv85 的 QEMU 透传 AES-NI（`-cpu host` 或显式 `+aes`）。这是**宿主机配置变更**，需用户授权，不在本计划代码范围内。透传后 srv85 走硬件 AES，吞吐大幅提升。

> 注意：透传 AES-NI 属生产虚拟化变更，须先确认宿主机 CPU 支持且不影响同宿主其他 VM，本文档只记录方向，不擅自执行。

### 3.3 不建议做的（附证据）

- **io_uring / splice**：§2 实测排除。
- **编译期强制 AES-NI**（`RUSTFLAGS=+aes`）：srv85 无 AES-NI，强制会非法指令崩溃。运行时检测（现状）是正确选择。
- **VAES-512 优化**：Node 1/3 有 avx512f 但 srv85/Node30 无，且 ring 是否启用 VAES-512 未确认；收益不确定，暂不投入。

---

## 4. 自我审查

| 检查项 | 结果 |
|---|---|
| AES-NI 结论经精确重测 | 首次误报已纠正，四台均用词边界匹配复核 |
| io_uring/splice 用真实 syscall 测试 | 非配置推断，srv85 实际运行 |
| TLS splice 拿到密文有原始字节证据 | `\x17\x03\x03` TLS record 头 |
| 修正了第二轮的错误判断 | "无法实现"→"可用但不适用"，附证据 |
| ring 软件回退有源码依据 | aes.rs:85-98 Fallback 分支 |
| 测试文件已清理 | srv85 与本机 /tmp 均已删 |
| 未改生产配置/代码 | 全程只读探测 + 临时测试程序（已删） |
| Node 30 经代理访问 | 按用户指示用 Clash 7897 隧道，端口 22 |

### 4.1 未决 / 待核实

1. **Node 1/3 的 io_uring 未实测**（内核 7.0/4.19，4.19 早于 io_uring 引入的 5.1，Node 3 大概率不支持；但本轮结论"不适用"与内核支持无关，不影响判定）。
2. **ring 是否对 Node 1/3 启用 VAES-512** 未确认，需读 ring 的 vp 模块或实测。
3. **srv85 宿主机是否支持 AES-NI 透传** 未查（需宿主机访问权限，超出本节点 SSH 范围）。

### 4.2 测量纪律备忘（本轮新增）

- `grep ... && echo YES || echo NO` 中若 grep 后接 `head`，`head` 恒成功导致 YES 恒真——**判定命令必须先跑"能失败的正例"验证**。
- QEMU 虚拟 CPU 的 flags 可能被宿主机掩码，`model name` 显示 "QEMU Virtual CPU" 时不能假设有 AES-NI，必须读 flags 实证。