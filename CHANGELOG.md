# 更新日志

## v0.2.0 - 2026-11-11

功能版本。新增可配置 listen backlog、配置原位热更新、流量上报可靠性改造，并把质量门禁固化到 CI。

### 新增

- `Runtime.ListenBacklog`（默认 4096，范围 128–65535）：监听改用 `TcpSocket` + `listen(backlog)`，突发连接不再受限固定的 128 全连接队列。
- `Runtime.PushIdempotency` 与 `X-V2Node-Batch-Id`：流量推送重试复用同一批次 ID，配合面板侧 300 秒去重，避免「响应丢失 + 重试」造成的重复计费。
- `Runtime.TrafficSpoolPath`：待上报流量以 tmp + fsync + rename 原子落盘，重启后可重放；`NotSent` 永久重放，`Unknown` 仅在幂等开启且 240 秒内重放。
- 证书到期告警：`cert_mode` 为 `file` / `self` / `dns` / `http` 时周期检查本地证书，剩余 ≤14 天 WARN、≤7 天 ERROR、已过期 ERROR；同一自然日只报一次，避免每轮检查刷屏。
- ACME 连续失败升级：连续失败 3 次后转为 ERROR 并每 3 轮提示一次，成功一次即清零。

### 变更

- 配置热更新改为按节点差异原位应用：`Runtime` 与 `Acme` 变更不再重建 `NodeManager`，进程与现有连接保持；只有 bind / path / TLS / backlog 变化才重绑该节点的 listener，重绑失败自动恢复旧监听。
- 流量批次按 500 用户分片；HTTP 4xx（429 除外）判定为永久失败并丢弃，429 安全重试，5xx 与超时判定为结果未知。
- 流量推送失败日志不再输出带 `token=` 的完整 URL。
- 停机时按 `DrainTimeoutSecs` 做有限排空，残余 pending 记录字节数并保留在 spool。
- 版本号由 0.1.1 升至 0.2.0。

### 工程质量

- 新增 `rustfmt.toml`、`clippy.toml`、`Makefile`、`.pre-commit-config.yaml`，`Cargo.toml` 增加 `[lints]` 段（`unsafe_code`、`unused_must_use`、`dbg_macro`、`todo`、`unimplemented`、`mem_forget`、`ignored_unit_patterns`、`manual_let_else` 为 deny）。
- CI 改为 `quality` 硬门禁（`fmt --check`、`clippy -D warnings`、`build`、`test`）加非阻断的 `strict-report`（pedantic + nursery）。
- pedantic/nursery 严格基线由 351 降至 248；`ignored_unit_patterns`（78 处）与 `manual_let_else`（9 处）已清零并锁定。

### 兼容性

- 面板 API 与 VLESS 数据面未变。未配置 `ListenBacklog` / `PushIdempotency` / `TrafficSpoolPath` 时行为与 0.1.1 一致（默认 4096 / false / 空）。
- 面板需支持 `X-V2Node-Batch-Id` 才会真正去重；面板未部署对应补丁时请保持 `PushIdempotency=false`。
- 启用 spool 需保证目录可写（建议 `/var/lib/v2node-rust/`，属主与运行用户一致）。

### 升级

```bash
sudo bash script/install.sh 0.2.0
# 或
sudo v2node-rust update 0.2.0
```

### 资产

- `v2node-rust-linux-amd64.tar.gz`：static-pie musl x86_64，包含 `v2node-rust`、`gen_x25519`、`vless_probe`、`config.example.json`。
- 压缩包 SHA256：`524c08c37072ab667ba00053335eac73babcc75e0e6e0df615ad1c5e20ae6cc7`
- `v2node-rust`（musl）SHA256：`5d719c0700c68eb0c5e68be6d578cbd56424daf26406495856a7f910b0e061fe`
- `v2node-rust`（gnu）SHA256：`edaf172e201f898472aae4343936b82c2c63dfb06a1657a87150a9f07af23ac6`

### 验证

- Linux 单元测试：65 个通过，0 失败；探针测试：9 个通过，0 失败。
- `cargo fmt -- --check`、`cargo clippy --all-targets -- -D warnings` 通过。
- GitHub Actions `Rust / quality` 与 `strict-report` 通过（`a866bfd`）。
- 制品类型：ELF 64-bit static-pie，已 strip。

## v0.1.1 - 2026-11-11

安装与部署修复版本。节点协议、面板接口和数据面行为与 v0.1.0 一致，二进制功能无变化，仅版本号递增。

### 变更

- 安装器和管理命令的默认配置路径改为 `/etc/v2node-rust/config.json`，与 Go 版 `/etc/v2node/config.json` 分离，避免迁移期间互相覆盖。
- 安装时对已存在的配置文件补 `chmod 600`；`v2node-rust config` 编辑后再次收紧权限。
- systemd 单元新增 `TasksMax=65536`、`LimitNPROC=65536`，避免连接上限被 systemd 默认任务数压住。
- README 同步新的配置路径、权限和迁移说明。

### 兼容性

- 节点协议、面板 API、VLESS 数据面和热更新逻辑与 v0.1.0 相同，本版本不包含协议或性能改动。
- 升级会保留新路径下的现有配置；仍使用 `/etc/v2node/config.json` 的节点，需要先复制配置到新路径，或安装时通过 `--config` 指定原路径。
- 同一面板节点不要同时运行 Go 版和 Rust 版，否则会重复上报流量和在线数据。

### 升级

```bash
sudo bash script/install.sh 0.1.1
# 或
sudo v2node-rust update 0.1.1
```

### 资产

- `v2node-rust-linux-amd64.tar.gz`：static-pie musl x86_64，包含 `v2node-rust`、`gen_x25519`、`vless_probe`、`config.example.json`。
- 压缩包 SHA256：`f7af077a536ab104d4197a61cb60080ae9a448454df08a82c66b42fd69a61bd4`
- `v2node-rust` 二进制 SHA256：`a28a5a22078960a5bbb28648425f4d08f938b36c24d77e40549bf53ec3d4ff5f`

### 验证

- Linux 单元测试：60 个通过，0 失败。
- 探针测试：9 个通过，0 失败。
- 制品类型：ELF 64-bit static-pie，已 strip。

## v0.1.0 - 2026-11-11

Rust 版 v2node 节点端首版，不依赖 Go / xray-core。

### 功能

- VLESS + WebSocket，支持 TCP、UDP、early data 和 X-Forwarded-For。
- TLS 终止：`none`、`file`、`self`、`remote`。
- ACME：Let's Encrypt + Cloudflare，支持 HTTP-01 和 DNS-01。
- VLESS Encryption：`mlkem768x25519plus`，支持 native、xorpub、random。
- 面板接口：config、user、alivelist、push、alive。
- 用户热增删、限速、设备数、在线 IP、全局连接上限、配置与证书热更新。
- 安装器和管理脚本支持 systemd / OpenRC、Release 下载、本地安装和原子更新。

### 质量与修复

- 面板流量上报区分“确定未送达”和“结果未知”；在线数据按活跃链路持续上报。
- WebSocket 和加密层支持排水，避免关闭时截断响应尾部。
- `MaxConnections` 改为进程级共享信号量，热更新不重绑监听、不重复计数。
- `ws_heartbeat_secs`、`ws_max_header_bytes` 等配置真正生效。
- ACME 账户创建和 HTTP-01 端口绑定进程级互斥。
- 通过 60 个单元测试、9 个探针测试、8 套端到端测试，以及与官方 Xray 26.11.9 的双向互通验证。

### 资产

- `v2node-rust-linux-amd64.tar.gz`：static-pie musl x86_64。
- 压缩包 SHA256：`8c6362e7168d491e8512f6260e8f41e8e26589ab33403f6e3c16281204d8668f`

### 已知边界

- 不包含 REALITY、routes 和 0-RTT。
- WebSocket 无 TCP 半关闭语义。
- `PprofPort`、`Log.Access` 仅做兼容解析并在启动时告警。
