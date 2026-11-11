#!/bin/bash
# 真实 Let's Encrypt + 真实 Cloudflare ACME 验证（TODO P2-2）
# 与 e2e_acme.sh 的区别：不启动 Pebble、不使用 mock_cloudflare.py，直接对接
# 真实 ACME 目录与 https://api.cloudflare.com/client/v4。
#
# 在 Linux 测试机上运行（本脚本需与 mock_panel.py 同目录）：
#   export CLOUDFLARE_DNS_API_TOKEN='...'          # Zone:DNS Edit，勿写入仓库
#   export ACME_DNS_DOMAIN='acme-dns.example.com'  # 托管在 Cloudflare 的测试专用子域
#   export ACME_HTTP_DOMAIN='acme-http.example.com' # DNS-only A 记录指向本机公网 IP，80 端口公网可达
#   export BIN_DIR=/usr/local/v2node
#   bash e2e_acme_real.sh --preflight-only        # 只做环境预检，不签发证书
#   bash e2e_acme_real.sh                        # 完整验证
#
# 可选环境变量：
#   ACME_DIRECTORY_URL 默认 LE staging，切生产用 https://acme-v02.api.letsencrypt.org/directory
#   ACME_EMAIL、HTTP01_BIND(默认 0.0.0.0:80)、DNS_PROPAGATION_DELAY_SECS(默认 30)
#   NODE_PORT(默认 20104)、PANEL_PORT(默认 18085)、WORKDIR、NODE_BIN
#   SKIP_HTTP01=1 或 SKIP_DNS01=1 可只跑其中一种挑战
#
# 安全约束：只使用独立临时目录/端口，不读写 /etc/v2node-rust/config.json，
# 不操作 systemd 服务；含 token 的临时文件权限 600 且退出时删除。

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
MOCK="$SCRIPT_DIR/mock_panel.py"
WORKDIR=${WORKDIR:-/tmp/v2node-real-acme}
BIN_DIR=${BIN_DIR:-/usr/local/v2node}
NODE_BIN=${NODE_BIN:-$BIN_DIR/v2node-rust}
NODE_PORT=${NODE_PORT:-20104}
PANEL_PORT=${PANEL_PORT:-18085}
ACME_DIRECTORY_URL=${ACME_DIRECTORY_URL:-https://acme-staging-v02.api.letsencrypt.org/directory}
ACME_EMAIL=${ACME_EMAIL:-}
HTTP01_BIND=${HTTP01_BIND:-0.0.0.0:80}
DNS_PROPAGATION_DELAY_SECS=${DNS_PROPAGATION_DELAY_SECS:-30}
CF_API_BASE=https://api.cloudflare.com/client/v4
ACME_DNS_DOMAIN=${ACME_DNS_DOMAIN:-}
ACME_HTTP_DOMAIN=${ACME_HTTP_DOMAIN:-}
CLOUDFLARE_DNS_API_TOKEN=${CLOUDFLARE_DNS_API_TOKEN:-}
UUID1=55555555-5555-5555-5555-555555555555
PREFLIGHT_ONLY=0
[ "${1:-}" = "--preflight-only" ] && PREFLIGHT_ONLY=1

NODE_PID=""
MOCK_PID=""

log() { echo "[real-acme] $*"; }
fail() {
  echo "REAL_ACME_FAIL: $*" >&2
  echo "--- node.log (tail) ---" >&2
  tail -n 80 "$WORKDIR/node.log" 2>/dev/null >&2 || true
  exit 1
}
warn_missing() {
  if [ "$PREFLIGHT_ONLY" = "1" ]; then
    log "PREFLIGHT_MISSING: $*"
    return 0
  fi
  fail "缺少前置条件: $*"
}
cleanup() {
  [ -n "$NODE_PID" ] && kill "$NODE_PID" 2>/dev/null || true
  [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
  [ -n "$NODE_PID" ] && wait "$NODE_PID" 2>/dev/null || true
  [ -n "$MOCK_PID" ] && wait "$MOCK_PID" 2>/dev/null || true
  rm -f "$WORKDIR/config.json" "$WORKDIR/config_fail.json" "$WORKDIR/state.json"
  log "含凭据的临时文件已清理，日志保留在 $WORKDIR"
}
trap cleanup EXIT

preflight() {
  log "目标 ACME 目录: $ACME_DIRECTORY_URL"
  for tool in python3 openssl curl ss; do
    command -v "$tool" >/dev/null 2>&1 || fail "缺少工具 $tool"
  done
  [ -f "$MOCK" ] || fail "缺少 $MOCK（请把 tests/ 同步到测试机）"
  [ -x "$NODE_BIN" ] || fail "二进制不可执行: $NODE_BIN"
  log "二进制: $NODE_BIN ($("$NODE_BIN" --version 2>/dev/null || echo unknown))"

  local le_code
  le_code=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 15 "$ACME_DIRECTORY_URL" || echo 000)
  [ "$le_code" = "200" ] || fail "ACME 目录不可达: HTTP $le_code"
  log "ACME 目录可达 (HTTP 200)"

  local cf_code
  cf_code=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 15 "$CF_API_BASE/user/tokens/verify" || echo 000)
  [ "$cf_code" = "000" ] && fail "Cloudflare API 不可达"
  log "Cloudflare API 可达 (HTTP $cf_code，未带凭据时的预期为 400/401)"

  if [ "${SKIP_DNS01:-0}" != "1" ]; then
    [ -n "$ACME_DNS_DOMAIN" ] || warn_missing "ACME_DNS_DOMAIN（Cloudflare 托管的测试子域）"
    [ -n "$CLOUDFLARE_DNS_API_TOKEN" ] || warn_missing "CLOUDFLARE_DNS_API_TOKEN（Zone:DNS Edit）"
    if [ -n "$CLOUDFLARE_DNS_API_TOKEN" ]; then
      local verify
      verify=$(curl -sS --max-time 15 -H "Authorization: Bearer $CLOUDFLARE_DNS_API_TOKEN" \
        "$CF_API_BASE/user/tokens/verify" || echo '{}')
      echo "$verify" | python3 -c \
        'import json,sys; sys.exit(0 if json.load(sys.stdin).get("success") else 1)' \
        || fail "Cloudflare token 校验失败（需要 Zone:DNS Edit 权限）"
      log "Cloudflare token 校验通过"
    fi
  fi

  if [ "${SKIP_HTTP01:-0}" != "1" ]; then
    [ -n "$ACME_HTTP_DOMAIN" ] || warn_missing "ACME_HTTP_DOMAIN（DNS-only 且指向本机公网 IP）"
    if [ -n "$ACME_HTTP_DOMAIN" ]; then
      local public_ip resolved
      public_ip=$(curl -sS --max-time 10 https://api.ipify.org || true)
      resolved=$(getent ahostsv4 "$ACME_HTTP_DOMAIN" | head -n 1 | awk '{print $1}' || true)
      log "HTTP-01 域名 $ACME_HTTP_DOMAIN -> $resolved；本机公网 IP $public_ip"
      [ -n "$public_ip" ] && [ "$resolved" = "$public_ip" ] \
        || warn_missing "HTTP-01 域名未直连本机（当前为 Cloudflare 代理或 A 记录不符），需改为 DNS-only"
      ss -lntp 2>/dev/null | grep -q ":80 " && warn_missing "80 端口已被占用" || log "80 端口空闲"
    fi
  fi
  log "PREFLIGHT_OK（缺项以 PREFLIGHT_MISSING 标出）"
}

write_config() {
  local path=$1 directory=$2 renew_days=$3 renew_interval=$4 log_file=$5
  WORKDIR="$WORKDIR" OUT="$path" DIRECTORY="$directory" RENEW_DAYS="$renew_days" \
  RENEW_INTERVAL="$renew_interval" LOG_FILE="$log_file" PANEL_PORT="$PANEL_PORT" \
  HTTP01_BIND="$HTTP01_BIND" ACME_EMAIL="$ACME_EMAIL" \
  DNS_DELAY="$DNS_PROPAGATION_DELAY_SECS" CF_API_BASE="$CF_API_BASE" \
  python3 - <<'PY'
import json, os
cfg = {
  "Log": {"Level": "debug", "Output": os.environ["LOG_FILE"], "Access": "none", "Ansi": False},
  "Nodes": [{"ApiHost": "http://127.0.0.1:" + os.environ["PANEL_PORT"],
             "NodeID": 1, "ApiKey": "real-acme-test", "Timeout": 10, "RetryCount": 1}],
  "PprofPort": 0,
  "Runtime": {
    "ConfigWatchIntervalSecs": 2,
    "PanelConnectTimeoutSecs": 5,
    "PanelRequestTimeoutSecs": 10,
    "PanelRetryCount": 1,
    "PanelRetryBackoffMs": 200,
    "DefaultPullIntervalSecs": 2,
    "DefaultPushIntervalSecs": 2,
    "WsHandshakeTimeoutSecs": 4,
    "VlessHandshakeTimeoutSecs": 10,
    "WsMaxHeaderBytes": 8192,
    "WsMaxMessageBytes": 1048576,
    "WsEarlyDataMaxBytes": 2048,
    "WsHeartbeatSecs": 0,
    "ConnectionIdleTimeoutSecs": 120,
    "MaxConnections": 64,
    "DrainTimeoutSecs": 10,
    "TcpConnectTimeoutSecs": 5,
    "AllowPrivateTargets": True,
    "UdpIdleTimeoutSecs": 5,
    "RelayBufferSize": 32768,
    "TlsHandshakeTimeoutSecs": 4,
    "TlsMinVersion": "1.2",
    "TlsMaxVersion": "1.3",
    "TlsAlpn": ["http/1.1"],
    "CertReloadIntervalSecs": 300,
  },
  "Acme": {
    "DirectoryUrl": os.environ["DIRECTORY"],
    "Email": os.environ["ACME_EMAIL"],
    "AccountDir": os.environ["WORKDIR"] + "/acme",
    "Http01Bind": os.environ["HTTP01_BIND"],
    "RenewBeforeDays": int(os.environ["RENEW_DAYS"]),
    "RenewCheckIntervalSecs": int(os.environ["RENEW_INTERVAL"]),
    "DnsProvider": "cloudflare",
    "DnsEnv": {"CLOUDFLARE_DNS_API_TOKEN": os.environ.get("CLOUDFLARE_DNS_API_TOKEN", "")},
    "DnsPropagationDelaySecs": int(os.environ["DNS_DELAY"]),
    "CloudflareApiBase": os.environ["CF_API_BASE"],
    "RootCert": "",
  },
}
with open(os.environ["OUT"], "w", encoding="utf-8") as fh:
    json.dump(cfg, fh, indent=2)
os.chmod(os.environ["OUT"], 0o600)
PY
}

write_state() {
  local etag=$1 mode=$2 domain=$3 cert=$4 key=$5
  WORKDIR="$WORKDIR" ETAG="$etag" MODE="$mode" DOMAIN="$domain" CERT="$cert" KEY="$key" \
  NODE_PORT="$NODE_PORT" UUID1="$UUID1" python3 - <<'PY'
import json, os
mode = os.environ["MODE"]
state = {
  "etag": os.environ["ETAG"],
  "node": {
    "listen_ip": "127.0.0.1",
    "server_port": int(os.environ["NODE_PORT"]),
    "network": "ws",
    "network_settings": {"path": "/cdn-api", "headers": {"Host": os.environ["DOMAIN"]}},
    "protocol": "vless",
    "tls": 1,
    "tls_settings": {
      "server_name": os.environ["DOMAIN"],
      "cert_mode": mode,
      "cert_file": os.environ["CERT"],
      "key_file": os.environ["KEY"],
      "provider": "cloudflare" if mode == "dns" else "",
      "dns_env": "CLOUDFLARE_DNS_API_TOKEN=" + os.environ.get("CLOUDFLARE_DNS_API_TOKEN", "") if mode == "dns" else "",
      "reject_unknown_sni": "0",
    },
    "flow": "",
    "encryption": "",
    "routes": [],
    "trusted_x_forwarded_for": [],
    "base_config": {"push_interval": 2, "pull_interval": 2,
                    "node_report_min_traffic": 0, "device_online_min_traffic": 0},
  },
  "users": {"users": [{"id": 1, "uuid": os.environ["UUID1"],
                       "speed_limit": 0, "device_limit": 0}]},
  "alive": {"alive": {}},
}
path = os.environ["WORKDIR"] + "/state.json"
with open(path, "w", encoding="utf-8") as fh:
    json.dump(state, fh, indent=2)
os.chmod(path, 0o600)
PY
}

issued_count() {
  grep -c "ACME certificate issued and applied" "$WORKDIR/node.log" 2>/dev/null || true
}

wait_issued() {
  local label=$1 target=$2 limit=${3:-300}
  local elapsed=0
  while [ "$(issued_count)" -lt "$target" ]; do
    [ "$elapsed" -ge "$limit" ] && fail "$label 未在 ${limit}s 内完成签发"
    sleep 3
    elapsed=$((elapsed + 3))
  done
  log "$label 签发完成（累计 $(issued_count) 次）"
}

verify_cert() {
  local cert=$1 key=$2 domain=$3 label=$4
  [ -s "$cert" ] || fail "$label 证书文件缺失"
  [ -s "$key" ] || fail "$label 私钥文件缺失"
  local cert_pub key_pub
  cert_pub=$(openssl x509 -in "$cert" -pubkey -noout | openssl sha256)
  key_pub=$(openssl pkey -in "$key" -pubout | openssl sha256)
  [ "$cert_pub" = "$key_pub" ] || fail "$label cert/key 不配对"
  openssl x509 -in "$cert" -noout -subject -issuer -dates | sed 's/^/  /'
  openssl x509 -in "$cert" -noout -subject | grep -q "$domain" \
    || fail "$label 证书 subject 不含 $domain"
  openssl x509 -in "$cert" -noout -issuer | grep -qi "Let's Encrypt" \
    || fail "$label 证书不是 Let's Encrypt 签发（可能仍在使用 mock/Pebble）"
  log "$label 证书校验通过（真实 LE 签发、cert/key 配对、域名匹配）"
}

verify_tls_handshake() {
  local domain=$1 label=$2
  local subject
  subject=$( (echo | timeout 20 openssl s_client -connect "127.0.0.1:$NODE_PORT" \
    -servername "$domain" 2>/dev/null) | openssl x509 -noout -subject 2>/dev/null || true)
  [ -n "$subject" ] || fail "$label TLS 握手未返回证书"
  echo "$subject" | grep -q "$domain" || fail "$label 服务端加载的不是 $domain 的证书"
  log "$label TLS 握手热加载验证通过：$subject"
}

start_services() {
  local config=$1 log_file=$2
  python3 "$MOCK" --state "$WORKDIR/state.json" --port "$PANEL_PORT" --dir "$WORKDIR" \
    > "$WORKDIR/mock.out" 2>&1 &
  MOCK_PID=$!
  "$NODE_BIN" --config "$config" > "$WORKDIR/node.out" 2>&1 &
  NODE_PID=$!
}

stop_node() {
  [ -n "$NODE_PID" ] || return 0
  kill "$NODE_PID" 2>/dev/null || true
  wait "$NODE_PID" 2>/dev/null || true
  NODE_PID=""
}

phase_challenge() {
  local mode=$1 domain=$2 label=$3 etag=$4
  write_state "$etag" "$mode" "$domain" "$WORKDIR/$mode.crt" "$WORKDIR/$mode.key"
  local before
  before=$(issued_count)
  wait_issued "$label" "$((before + 1))"
  verify_cert "$WORKDIR/$mode.crt" "$WORKDIR/$mode.key" "$domain" "$label"
  verify_tls_handshake "$domain" "$label"
  grep -q "listener started.*tls=true" "$WORKDIR/node.log" || fail "$label TLS 监听未启动"
}

phase_renewal_pairing() {
  local mode=$1 domain=$2
  log "续期检查：故意破坏 cert/key 配对，验证自动识别并重新签发"
  cp "$WORKDIR/$mode.crt" "$WORKDIR/$mode.crt.bak"
  openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 \
    -out "$WORKDIR/$mode.key" 2>/dev/null
  local before
  before=$(issued_count)
  wait_issued "cert/key 不配对后的自动续期" "$((before + 1))" 240
  verify_cert "$WORKDIR/$mode.crt" "$WORKDIR/$mode.key" "$domain" "续期后"
  log "续期检查能识别 cert/key 配对并恢复，验收通过"
}

phase_acme_failure() {
  local domain=$1
  log "ACME 失败容错：停用 ACME 目录但保留磁盘证书，重启节点"
  stop_node
  # 强制续期 + 删除已缓存账户，确保 account() 一定走不可达目录而不是命中缓存
  rm -f "$WORKDIR/acme/account.json"
  write_config "$WORKDIR/config_fail.json" "https://127.0.0.1:9/directory" 200 10 \
    "$WORKDIR/node_fail.log"
  "$NODE_BIN" --config "$WORKDIR/config_fail.json" > "$WORKDIR/node_fail.out" 2>&1 &
  NODE_PID=$!
  local ok=0
  for _ in $(seq 1 40); do
    grep -q "listener started.*tls=true" "$WORKDIR/node_fail.log" 2>/dev/null && { ok=1; break; }
    sleep 1
  done
  [ "$ok" = "1" ] || fail "ACME 失败后节点未能以已有证书启动 TLS 监听"
  verify_tls_handshake "$domain" "ACME 失败容错"
  grep -q "ACME certificate check failed\|ACME renewal check failed" "$WORKDIR/node_fail.log" \
    || fail "未观察到 ACME 失败日志（测试未真正覆盖失败路径）"
  log "ACME 失败不影响已有证书启动，验收通过"
}

main() {
  mkdir -p "$WORKDIR"
  chmod 700 "$WORKDIR"
  : > "$WORKDIR/node.log"
  preflight
  [ "$PREFLIGHT_ONLY" = "1" ] && { log "仅预检，未签发证书"; exit 0; }

  write_config "$WORKDIR/config.json" "$ACME_DIRECTORY_URL" 30 10 "$WORKDIR/node.log"
  local last_mode="" last_domain=""
  if [ "${SKIP_HTTP01:-0}" != "1" ]; then
    write_state 1 http "$ACME_HTTP_DOMAIN" "$WORKDIR/http.crt" "$WORKDIR/http.key"
    start_services "$WORKDIR/config.json" "$WORKDIR/node.log"
    local before
    before=$(issued_count)
    wait_issued "HTTP-01" "$((before + 1))"
    verify_cert "$WORKDIR/http.crt" "$WORKDIR/http.key" "$ACME_HTTP_DOMAIN" "HTTP-01"
    verify_tls_handshake "$ACME_HTTP_DOMAIN" "HTTP-01"
    last_mode=http
    last_domain="$ACME_HTTP_DOMAIN"
  else
    log "跳过 HTTP-01（SKIP_HTTP01=1）"
  fi

  if [ "${SKIP_DNS01:-0}" != "1" ]; then
    local pid_before=""
    if [ -n "$NODE_PID" ]; then
      pid_before=$NODE_PID
      phase_challenge dns "$ACME_DNS_DOMAIN" "DNS-01" 2
      [ "$NODE_PID" = "$pid_before" ] || fail "切换挑战模式时节点进程重启"
      log "切换 HTTP-01 -> DNS-01 后 PID 不变"
    else
      write_state 1 dns "$ACME_DNS_DOMAIN" "$WORKDIR/dns.crt" "$WORKDIR/dns.key"
      start_services "$WORKDIR/config.json" "$WORKDIR/node.log"
      phase_challenge dns "$ACME_DNS_DOMAIN" "DNS-01" 2
    fi
    last_mode=dns
    last_domain="$ACME_DNS_DOMAIN"
  else
    log "跳过 DNS-01（SKIP_DNS01=1）"
  fi

  [ -n "$last_mode" ] || fail "两种挑战都被跳过，无可验证内容"
  phase_renewal_pairing "$last_mode" "$last_domain"
  phase_acme_failure "$last_domain"

  echo "REAL_ACME_PASS directory=$ACME_DIRECTORY_URL http=$([ "${SKIP_HTTP01:-0}" = 1 ] && echo skipped || echo verified) dns=$([ "${SKIP_DNS01:-0}" = 1 ] && echo skipped || echo verified)"
}

main "$@"
