#!/bin/bash
# Requires the official Xray binary (>= v26.11.9) at /opt/xray-test/xray, or set XRAY=/path/to/xray.
set -euo pipefail

DIR=/tmp/v2node-interop
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
BIN=${BIN:-/opt/v2node-rust-target/debug}
NODE=$BIN/v2node-rust
PROBE=$BIN/vless_probe
GEN=$BIN/gen_x25519
MOCK=$SCRIPT_DIR/mock_panel.py
HTTP=$SCRIPT_DIR/http_echo.py
XRAY=${XRAY:-/opt/xray-test/xray}
UUID1=77777777-7777-7777-7777-777777777777
HOST=hk2.wxmuma.cc.cd
PADDING=100-111-1111.75-0-111.50-0-3333

cleanup_test_procs() {
  pkill -x v2node-rust 2>/dev/null || true
  pkill -x xray 2>/dev/null || true
	for pid in $(ss -lntp 2>/dev/null | grep -E '127\.0\.0\.1:(181|182|201)[0-9][0-9]' | awk -F'pid=' '{print $2}' | cut -d',' -f1 | sort -u); do
    kill "$pid" 2>/dev/null || true
  done
}

fail() {
  echo "INTEROP_FAIL: $*"
  cleanup_test_procs
  for log in "$DIR"/*/node.log "$DIR"/*/xray.out; do
    echo "--- $log ---"
    tail -n 60 "$log" 2>/dev/null || true
  done
  exit 1
}

cleanup_test_procs
rm -rf "$DIR"
mkdir -p "$DIR"
[ -x "$XRAY" ] || fail "xray binary missing at $XRAY"
"$XRAY" version | head -1

run_rust_server_xray_client() {
  local mode=$1 padding=$2 idx=$3
  local port=$((20100 + idx * 10)) panel=$((18100 + idx * 10))
  local socks=$((10900 + idx * 10)) http_port=$((18200 + idx * 10))
  local case_dir="$DIR/rs-$mode"
  mkdir -p "$case_dir"
  local keys private public
  keys=$("$GEN")
  private=$(printf '%s\n' "$keys" | sed -n 's/^PRIVATE=//p')
  public=$(printf '%s\n' "$keys" | sed -n 's/^PUBLIC=//p')
  [ -n "$private" ] || fail "gen_x25519 private"
  [ -n "$public" ] || fail "gen_x25519 public"

  cat > "$case_dir/config.json" <<EOF
{
  "Log": {"Level": "debug", "Output": "$case_dir/node.log", "Access": "none", "Ansi": false},
  "Nodes": [
    {"ApiHost": "http://127.0.0.1:$panel", "NodeID": 1, "ApiKey": "test", "Timeout": 5, "RetryCount": 1}
  ],
  "PprofPort": 0,
  "Runtime": {
    "ConfigWatchIntervalSecs": 1,
    "PanelConnectTimeoutSecs": 3,
    "PanelRequestTimeoutSecs": 5,
    "PanelRetryCount": 1,
    "PanelRetryBackoffMs": 100,
    "DefaultPullIntervalSecs": 60,
    "DefaultPushIntervalSecs": 60,
    "WsHandshakeTimeoutSecs": 4,
    "WsMaxHeaderBytes": 8192,
    "WsEarlyDataMaxBytes": 2048,
    "ConnectionIdleTimeoutSecs": 120,
    "TcpConnectTimeoutSecs": 5,
    "UdpIdleTimeoutSecs": 5,
    "RelayBufferSize": 32768,
    "AllowPrivateTargets": true
  },
  "Acme": {"DirectoryUrl": "https://acme-staging-v02.api.letsencrypt.org/directory", "AccountDir": "$case_dir/acme", "Http01Bind": "127.0.0.1:0", "RenewCheckIntervalSecs": 86400}
}
EOF

  cat > "$case_dir/state.json" <<EOF
{
  "etag": "$idx",
  "node": {
    "listen_ip": "127.0.0.1",
    "server_port": $port,
    "network": "ws",
    "network_settings": {"path": "/cdn-api", "headers": {"Host": "$HOST"}},
    "protocol": "vless",
    "tls": 0,
    "tls_settings": null,
    "flow": "",
    "encryption": "mlkem768x25519plus",
    "encryption_settings": {"mode": "$mode", "rtt": "1rtt", "ticket": "0s", "server_padding": "$padding", "client_padding": "$padding", "private_key": "$private", "password": "$public"},
    "trusted_x_forwarded_for": ["X-Forwarded-For"],
    "base_config": {"push_interval": 60, "pull_interval": 60, "node_report_min_traffic": 0, "device_online_min_traffic": 0},
    "routes": []
  },
  "users": {"users": [{"id": 1, "uuid": "$UUID1", "speed_limit": 0, "device_limit": 0}]},
  "alive": {"alive": {}}
}
EOF

  python3 "$MOCK" --state "$case_dir/state.json" --port "$panel" --dir "$case_dir" > "$case_dir/mock.out" 2>&1 &
  local mock_pid=$!
  for _ in $(seq 1 50); do
    curl -s --max-time 1 "http://127.0.0.1:$panel/api/v2/server/config?node_type=v2node&node_id=1&token=test" >/dev/null 2>&1 && break
    sleep 0.1
  done
  python3 "$HTTP" "$http_port" > "$case_dir/http.out" 2>&1 &
  local http_pid=$!
  "$NODE" --config "$case_dir/config.json" > "$case_dir/node.out" 2>&1 &
  local node_pid=$!
  for _ in $(seq 1 60); do
    grep -q "listener started" "$case_dir/node.log" 2>/dev/null && break
    sleep 0.2
  done
  grep -q "VLESS encryption enabled" "$case_dir/node.log" || fail "$mode: node encryption not enabled"
  grep -q "listener started" "$case_dir/node.log" || fail "$mode: node listener not started"

  local enc="mlkem768x25519plus.$mode.1rtt.$padding.$public"
  cat > "$case_dir/xray-client.json" <<EOF
{
  "log": {"loglevel": "warning"},
  "inbounds": [{"tag": "socks", "listen": "127.0.0.1", "port": $socks, "protocol": "socks", "settings": {"udp": false}}],
  "outbounds": [{
    "tag": "vless-out",
    "protocol": "vless",
    "settings": {"vnext": [{"address": "127.0.0.1", "port": $port, "users": [{"id": "$UUID1", "encryption": "$enc", "level": 0}]}]},
    "streamSettings": {"network": "ws", "security": "none", "wsSettings": {"path": "/cdn-api", "headers": {"Host": "$HOST"}}}
  }]
}
EOF
  "$XRAY" run -c "$case_dir/xray-client.json" > "$case_dir/xray.out" 2>&1 &
  local xray_pid=$!
  for _ in $(seq 1 50); do
    (exec 3<>"/dev/tcp/127.0.0.1/$socks") 2>/dev/null && { exec 3>&- 2>/dev/null || true; break; }
    sleep 0.1
  done

  python3 -c "open('$case_dir/expect_get','wb').write(b'x'*200000+b'END')"
  curl -s --max-time 30 --socks5-hostname "127.0.0.1:$socks" "http://127.0.0.1:$http_port/" -o "$case_dir/get.bin" || fail "$mode: GET through xray failed"
  cmp -s "$case_dir/expect_get" "$case_dir/get.bin" || fail "$mode: GET body mismatch ($(stat -c%s "$case_dir/get.bin" 2>/dev/null || echo 0) bytes)"
  head -c 100000 /dev/urandom > "$case_dir/post.bin"
  local digest expected
  digest=$(curl -s --max-time 30 --socks5-hostname "127.0.0.1:$socks" --data-binary "@$case_dir/post.bin" "http://127.0.0.1:$http_port/upload" || true)
  expected=$(sha256sum "$case_dir/post.bin" | awk '{print $1}')
  [ "$digest" = "$expected" ] || fail "$mode: POST digest mismatch got=$digest want=$expected"

  kill "$xray_pid" "$node_pid" "$mock_pid" "$http_pid" 2>/dev/null || true
  wait "$xray_pid" 2>/dev/null || true
  wait "$node_pid" 2>/dev/null || true
  wait "$mock_pid" 2>/dev/null || true
  wait "$http_pid" 2>/dev/null || true
  echo "INTEROP_RS_OK mode=$mode get=200003 post=100000"
}

run_xray_server_rust_client() {
  local mode=$1 padding=$2 idx=$3
  local port=$((20200 + idx * 10))
  local case_dir="$DIR/xs-$mode"
  mkdir -p "$case_dir"
  local keys private public
  keys=$("$GEN")
  private=$(printf '%s\n' "$keys" | sed -n 's/^PRIVATE=//p')
  public=$(printf '%s\n' "$keys" | sed -n 's/^PUBLIC=//p')
  [ -n "$private" ] || fail "gen_x25519 private"
  [ -n "$public" ] || fail "gen_x25519 public"

  local dec="mlkem768x25519plus.$mode.0s.$padding.$private"
  cat > "$case_dir/xray-server.json" <<EOF
{
  "log": {"loglevel": "warning"},
  "inbounds": [{
    "tag": "vless-in",
    "listen": "127.0.0.1",
    "port": $port,
    "protocol": "vless",
    "settings": {"clients": [{"id": "$UUID1", "flow": ""}], "decryption": "$dec"},
    "streamSettings": {"network": "ws", "security": "none", "wsSettings": {"path": "/cdn-api"}}
  }],
  "outbounds": [{"protocol": "freedom", "settings": {"finalRules": [{"action": "allow", "ip": ["127.0.0.1"]}]}}]
}
EOF
  "$XRAY" run -c "$case_dir/xray-server.json" > "$case_dir/xray.out" 2>&1 &
  local xray_pid=$!
  for _ in $(seq 1 50); do
    (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && { exec 3>&- 2>/dev/null || true; break; }
    sleep 0.1
  done

  local enc="mlkem768x25519plus.$mode.1rtt.$padding.$public"
  "$PROBE" --url "ws://127.0.0.1:$port/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload-size 100000 --encryption "$enc" > "$case_dir/probe-tcp.out" 2>&1 || fail "$mode: probe tcp against xray server failed"
  "$PROBE" --url "ws://127.0.0.1:$port/cdn-api" --host "$HOST" --uuid "$UUID1" --mode udp --payload hello-udp --encryption "$enc" > "$case_dir/probe-udp.out" 2>&1 || fail "$mode: probe udp against xray server failed"

  kill "$xray_pid" 2>/dev/null || true
  wait "$xray_pid" 2>/dev/null || true
  echo "INTEROP_XS_OK mode=$mode tcp=100000 udp=hello-udp"
}

run_rust_server_xray_client native "" 1
run_rust_server_xray_client xorpub "$PADDING" 2
run_rust_server_xray_client random "$PADDING" 3
run_xray_server_rust_client native "" 1
run_xray_server_rust_client xorpub "$PADDING" 2
run_xray_server_rust_client random "$PADDING" 3

echo "INTEROP_ALL_PASS"
