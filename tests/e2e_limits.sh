#!/bin/bash
set -euo pipefail

DIR=/tmp/v2node-e2e-limits
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
BIN=${BIN:-/opt/v2node-rust-target/debug}
NODE=$BIN/v2node-rust
PROBE=$BIN/vless_probe
MOCK=$SCRIPT_DIR/mock_panel.py
UUID1=44444444-4444-4444-4444-444444444444
PORT=20003
PANEL=18083
HOST=hk2.wxmuma.cc.cd

fail() {
  echo "LIMITS_E2E_FAIL: $*"
  echo "--- node.log ---"
  tail -n 120 "$DIR/node.log" 2>/dev/null || true
  echo "--- reports.log ---"
  cat "$DIR/reports.log" 2>/dev/null || true
  exit 1
}

rm -rf "$DIR"
mkdir -p "$DIR"

cat > "$DIR/config.json" <<EOF
{
  "Log": {"Level": "debug", "Output": "$DIR/node.log", "Access": "none", "Ansi": false},
  "Nodes": [
    {"ApiHost": "http://127.0.0.1:$PANEL", "NodeID": 1, "ApiKey": "test", "Timeout": 5, "RetryCount": 1}
  ],
  "PprofPort": 0,
  "Runtime": {
    "ConfigWatchIntervalSecs": 1,
    "PanelConnectTimeoutSecs": 3,
    "PanelRequestTimeoutSecs": 5,
    "PanelRetryCount": 1,
    "PanelRetryBackoffMs": 100,
    "DefaultPullIntervalSecs": 2,
    "DefaultPushIntervalSecs": 2,
    "WsHandshakeTimeoutSecs": 4,
    "WsMaxHeaderBytes": 8192,
    "WsEarlyDataMaxBytes": 2048,
    "WsHeartbeatSecs": 0,
    "ConnectionIdleTimeoutSecs": 120,
    "DrainTimeoutSecs": 10,
    "TcpConnectTimeoutSecs": 5,
    "UdpIdleTimeoutSecs": 5,
    "RelayBufferSize": 32768,
    "AllowPrivateTargets": true,
    "TlsHandshakeTimeoutSecs": 4,
    "TlsMinVersion": "1.2",
    "TlsMaxVersion": "1.3",
    "TlsAlpn": ["http/1.1"],
    "CertReloadIntervalSecs": 5
  },
  "Acme": {
    "DirectoryUrl": "https://acme-staging-v02.api.letsencrypt.org/directory",
    "Email": "",
    "AccountDir": "$DIR/acme",
    "Http01Bind": "127.0.0.1:8080",
    "RenewBeforeDays": 30,
    "RenewCheckIntervalSecs": 86400,
    "DnsProvider": "",
    "DnsEnv": {}
  }
}
EOF

make_state() {
  local file=$1 etag=$2 speed=$3 device=$4 alive=$5
  cat > "$file" <<EOF
{
  "etag": "$etag",
  "node": {
    "listen_ip": "127.0.0.1",
    "server_port": $PORT,
    "network": "ws",
    "network_settings": {"path": "/cdn-api", "headers": {"Host": "$HOST"}},
    "protocol": "vless",
    "tls": 1,
    "tls_settings": {"server_name": "$HOST", "cert_mode": "none"},
    "flow": "",
    "encryption": "",
    "trusted_x_forwarded_for": ["X-Forwarded-For"],
    "base_config": {"push_interval": 2, "pull_interval": 2, "node_report_min_traffic": 0, "device_online_min_traffic": 0},
    "routes": []
  },
  "users": {"users": [{"id": 1, "uuid": "$UUID1", "speed_limit": $speed, "device_limit": $device}]},
  "alive": $alive
}
EOF
}

make_state "$DIR/state1.json" 1 0 0 '{"alive":{}}'
make_state "$DIR/state2.json" 2 1 0 '{"alive":{}}'
make_state "$DIR/state3.json" 3 0 1 '{"alive":{"1":1}}'
make_state "$DIR/state4.json" 4 0 1 '{"alive":{}}'
cp "$DIR/state1.json" "$DIR/state.json"
python3 "$MOCK" --state "$DIR/state.json" --port "$PANEL" --dir "$DIR" > "$DIR/mock.out" 2>&1 &
MOCK_PID=$!
"$NODE" --config "$DIR/config.json" > "$DIR/node.out" 2>&1 &
NODE_PID=$!
cleanup() {
  kill "$NODE_PID" "$MOCK_PID" 2>/dev/null || true
  wait "$NODE_PID" 2>/dev/null || true
  wait "$MOCK_PID" 2>/dev/null || true
}
trap cleanup EXIT

for _ in $(seq 1 50); do
  grep -q "listener started" "$DIR/node.log" 2>/dev/null && break
  sleep 0.2
done
grep -q "listener started" "$DIR/node.log" || fail "node listener did not start"

PID_BEFORE=$(cat /proc/"$NODE_PID"/stat | awk '{print $1}')

"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload xff --xff 203.0.113.7
sleep 3
grep -q "203.0.113.7" "$DIR/reports.log" || fail "X-Forwarded-For client IP was not reported"

cp "$DIR/state2.json" "$DIR/state.json"
sleep 3
OUTPUT=$("$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload-size 131072)
echo "$OUTPUT"
ELAPSED=$(printf '%s\n' "$OUTPUT" | sed -n 's/.*elapsed_ms=\([0-9][0-9]*\).*/\1/p')
[ -n "$ELAPSED" ] || fail "could not parse probe elapsed time"
[ "$ELAPSED" -ge 1000 ] || fail "rate limit not effective: ${ELAPSED}ms for 128KB at 1Mbps"

cp "$DIR/state3.json" "$DIR/state.json"
sleep 3
if "$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload blocked --xff 203.0.113.8 >/dev/null 2>&1; then
  fail "device limit did not reject a new IP"
fi

cp "$DIR/state4.json" "$DIR/state.json"
sleep 3
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload allowed --xff 203.0.113.8

PID_AFTER=$(cat /proc/"$NODE_PID"/stat | awk '{print $1}')
[ "$PID_BEFORE" = "$PID_AFTER" ] || fail "node process restarted"

echo "LIMITS_E2E_PASS pid=$PID_AFTER elapsed_ms=$ELAPSED reports=$(wc -l < "$DIR/reports.log")"
