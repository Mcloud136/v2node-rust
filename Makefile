# 常用质量入口。quality 是提交/发布前必须全绿的一条命令。
# C0b 起仓库为 workspace，命令统一加 --workspace / --all 覆盖 v2node-proto。
.PHONY: fmt fmt-check lint lint-strict test check quality clean

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	cargo clippy --workspace --all-targets -- -D warnings

# 观测用途：pedantic / nursery 全量报告，逐条收敛用。
# 基线随工具链变化：clippy 0.1.99 本机 140 / clippy 1.100 CI 147（2026-11-14）。
lint-strict:
	cargo clippy --workspace --all-targets -- -W clippy::pedantic -W clippy::nursery

test:
	cargo test --workspace

check:
	cargo check --workspace --all-targets

quality: fmt-check lint test

clean:
	cargo clean
