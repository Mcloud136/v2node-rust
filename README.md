# v2node-rust

基于 Rust 的 V2Board v2node 节点端，独立于 Go 版 `v2node` 仓库。节点在 Rust 内直接实现所需的 VLESS 服务端协议子集，不依赖、不调用 Go xray-core。

> 这不是完整 xray-core 的 Rust 移植。当前 M1 只实现 v2node 节点侧需要的协议与传输；REALITY、routes 等后续能力按需增量实现。

## 已实现（M1）

- VLESS over WebSocket，TCP 与 UDP 转发
- TLS 终止：`cert_mode` 支持 `none` / `file` / `self` / `remote` / `dns` / `http`，证书热重载、SNI 校验
- ACME：Let's Encrypt + Cloudflare，HTTP-01 与 DNS-01 签发并热加载
- VLESS Encryption：`mlkem768x25519plus`，`native` / `xorpub` / `random`，1-RTT
- 面板接口：config、user（msgpack/JSON）、alivelist、push、alive
- 用户热增删、限速、设备数、在线 IP、流量统计与失败重试语义
- 配置/日志热更新、listener 原位切换、PID 不变
- 私网/回环目标默认拦截，WS 头部/消息大小上限，连接总数上限

## 性能对比

在同一节点、同一配置和同一入口下与 Go v0.4.6 实测：空载内存约 7.7–8.0 MiB（Go 版 55–64 MiB），16 并发 × 1 MiB 完成时间为 Go 版的约 78%–85%（快约 15%–22%），服务端 CPU 少约 30%–40%。单流吞吐和公网经 Cloudflare 的绝对值差异较小，受链路影响大。

完整测试方法、数据和边界见 [Rust 版与 Go 版性能对比](docs/performance-comparison.md)。

## 构建

需要 Rust 1.99（`Cargo.toml` 的 `rust-version`）。

```bash
cargo build --release
# 静态 musl 制品（生产推荐）
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

制品路径：

```text
target/release/v2node-rust
target/x86_64-unknown-linux-musl/release/v2node-rust
```

## 安装与管理

默认路径与 Go 版兼容，但服务/命令带 `rust` 后缀，避免覆盖 Go 版：

| 项目 | 默认值 |
|---|---|
| 二进制 | `/usr/local/v2node/v2node-rust` |
| 配置 | `/etc/v2node-rust/config.json` |
| 服务 | `v2node-rust.service`（systemd）/ `v2node-rust`（OpenRC） |
| 管理命令 | `/usr/bin/v2node-rust` |

### 从源码/本地二进制安装

```bash
sudo bash script/install.sh --local /path/to/v2node-rust \
  --api-host https://panel.example.com/ \
  --node-id 1 \
  --api-key 'CHANGE_ME'
```

`--local` 可以是二进制文件，也可以是包含 `v2node-rust`、`gen_x25519`、`vless_probe` 的目录。安装脚本会保留现有配置，备份旧二进制为 `.old-*`。

### 从一开始安装（GitHub Release）

发布资产命名约定：

```text
v2node-rust-linux-amd64.tar.gz
v2node-rust-linux-arm64.tar.gz
```

压缩包内至少包含 `v2node-rust`，可附带 `gen_x25519`、`vless_probe`、`config.example.json`。本地打包示例：

```bash
mkdir -p dist
tar -czf dist/v2node-rust-linux-amd64.tar.gz \
  -C target/x86_64-unknown-linux-musl/release \
  v2node-rust gen_x25519 vless_probe
```

```bash
# 默认仓库是 Mcloud136/v2node-rust；如你的仓库不同请设置
export V2NODE_RUST_REPO=owner/v2node-rust
sudo -E bash script/install.sh   # 最新版
sudo -E bash script/install.sh 0.1.0
```

若 GitHub 不可达，直接用 `--local` 安装已上传的构建产物。

### 管理命令

```bash
v2node-rust              # 管理菜单
v2node-rust install      # 安装（透传 install.sh 参数）
v2node-rust update       # 更新到最新版
v2node-rust update 0.1.0 # 更新到指定版本
v2node-rust start|stop|restart|status
v2node-rust enable|disable
v2node-rust log
v2node-rust config
v2node-rust generate
v2node-rust x25519
v2node-rust version
v2node-rust update_shell
v2node-rust uninstall
```

首次安装未提供 `--api-host/--node-id/--api-key` 时会交互提示生成配置；配置权限为 `600`。

> 同一面板节点不要同时运行 Go 版和 Rust 版（会重复上报流量/在线数据）。安装脚本检测到 Go 版 `v2node` 正在运行时会警告。

## 配置

字段以 `config.example.json` 为准，默认配置路径为 `/etc/v2node-rust/config.json`（目录 700、文件 600）；从 Go 版迁移时可先复制原配置到该路径，或通过 `V2NODE_RUST_CONFIG` / `--config` 指定其他位置。

关键字段：

| 字段 | 默认值 | 说明 |
|---|---|---|
| `Runtime.MaxConnections` | 4096 | 进程级连接上限，所有 listener 共享 |
| `Runtime.ListenBacklog` | 4096 | `listen(2)` 的 accept 队列长度，范围 128–65535，应不大于内核 `somaxconn` |
| `Runtime.AllowPrivateTargets` | false | 是否允许转发到私网/回环目标，默认拦截 |
| `Runtime.VlessHandshakeTimeoutSecs` | 10 | VLESS 握手超时 |
| `Runtime.PanelRetryBackoffMs` | 200 | 面板请求重试间隔 |
| `Runtime.PushIdempotency` | false | 流量推送批次幂等；需面板支持 `X-V2Node-Batch-Id` 去重 |
| `Runtime.TrafficSpoolPath` | 空 | 流量上报 spool 路径；设置后未送达批次会落盘并在重启后重放，建议 `/var/lib/v2node-rust/traffic.spool` |
| `Runtime.RelayBufferSize` | 32768 | 数据面 relay 与 WS 桥接缓冲。注意：VLESS Encryption 记录层 pump 使用独立的 16384 字节缓冲，不受此字段影响 |
| `Acme.DirectoryUrl` | LE staging | 生产必须显式改为 `https://acme-v02.api.letsencrypt.org/directory` |
| `Acme.Http01Bind` | `0.0.0.0:80` | HTTP-01 挑战端口，需公网可达 |
| `Acme.DnsEnv` | 空 | Cloudflare 凭据，形如 `{"CLOUDFLARE_DNS_API_TOKEN":"..."}`；面板下发的 `tls_settings.dns_env` 使用 `KEY=VALUE,KEY=VALUE`，会覆盖同名全局值 |

`tls_settings.cert_mode` 的取值与证书来源：

| cert_mode | 证书来源 | 备注 |
|---|---|---|
| `none` | 不启用 TLS | 通常由 Cloudflare / cloudflared 终止 |
| `file` | `cert_file` / `key_file` | 本地文件，支持热重载 |
| `self` | 自动生成自签证书 | 测试用 |
| `remote` | 面板下发的 `tls_cert` / `tls_key` | 写入本地文件后加载 |
| `dns` | ACME DNS-01（Cloudflare） | 需 `provider` 与 DNS 凭据 |
| `http` | ACME HTTP-01 | 需 `Http01Bind` 公网可达 |

> 凭据只放在配置文件或环境变量中，不要提交到仓库或粘贴到聊天。

## 质量检查

```bash
make quality            # = fmt-check + lint + test，提交前必须全绿
```

等价于手动执行：

```bash
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

其他入口：

| 命令 | 用途 |
|---|---|
| `make fmt` | 就地格式化 |
| `make lint-strict` | pedantic + nursery 全量报告（**不阻断**，用于逐条收敛） |
| `make check` | 仅类型检查，最快反馈 |
| `make test` | 单元测试 |

配置来源：`rustfmt.toml`（强制 LF，避免 Windows 写回 CRLF）、`clippy.toml`（复杂度/行数/参数阈值）、`Cargo.toml` 的 `[lints]`（`unsafe_code`、`unused_must_use`、`dbg_macro`、`todo`、`unimplemented`、`mem_forget` 为 deny）。
`GitHub Actions` 的 `.github/workflows/rust.yml` 跑同一套硬门禁，另有 `strict-report` 作业输出严格模式报告但不阻断。

严格模式基线随工具链版本变化，**引用时必须带工具链版本**：clippy 0.1.99（Windows 本机）140 条、clippy 1.100（Linux CI）147 条（2026-11-14 实测；本机当日早先为 143，补文档后降 3）。已收敛并锁定为 `deny` 的两批：`ignored_unit_patterns`（78 处）、`manual_let_else`（9 处）；剩余集中在 `cast_possible_truncation`、`cast_precision_loss`、`cast_possible_wrap`、`missing_const_for_fn`、`unreadable_literal`。收敛计划见 [完善推进计划](docs/improvement-plan.md) 的 B5c。

实机 E2E 在 Linux 构建机运行，需要 `vless_probe` 与 Python mock 面板：

```bash
export BIN=/path/to/build/debug
bash tests/e2e.sh
bash tests/e2e_tls.sh
bash tests/e2e_limits.sh
bash tests/e2e_config.sh
bash tests/e2e_report.sh
bash tests/e2e_encryption.sh
bash tests/e2e_acme.sh
bash tests/interop_xray.sh
```

真实 Let's Encrypt + Cloudflare 验证（不使用 Pebble / mock，需测试专用域名与 API Token）：

```bash
export CLOUDFLARE_DNS_API_TOKEN=...      # Zone:DNS Edit，勿写入仓库
export ACME_DNS_DOMAIN=acme-dns.example.com
export ACME_HTTP_DOMAIN=acme-http.example.com  # DNS-only A 记录指向本机公网 IP
export BIN_DIR=/usr/local/v2node
bash tests/e2e_acme_real.sh --preflight-only
bash tests/e2e_acme_real.sh
```

## 文档

- [完善推进计划](docs/improvement-plan.md)（当前进度载体）
- [Rust 版与 Go 版性能对比](docs/performance-comparison.md)
- [VLESS Encryption 生产配置](docs/vless-encryption-prod.md)
- [M1 阶段完成情况（历史归档）](docs/m1-completion-plan.md)
- [跨语言性能基准规范](docs/benchmark-spec.md)

自研客户端已独立为单独仓库（McloudClient），其设计与交接文档随客户端仓库维护，不在本仓库。

## 说明

- 本仓库是独立的 Rust 版；Go 版和它的 git 历史保留在原 `v2node` 仓库。
- 完整 xray-core 的 Rust 重写不在本项目范围内；当前实现只覆盖 v2node 节点所需的服务端子集。
- 许可证见 `LICENSE`。
