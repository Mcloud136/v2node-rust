#!/bin/bash
set -euo pipefail

DIR=/tmp/v2node-e2e
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
BIN=${BIN:-/opt/v2node-rust-target/debug}
NODE=$BIN/v2node-rust
PROBE=$BIN/vless_probe
MOCK=$SCRIPT_DIR/mock_panel.py
UUID1=11111111-1111-1111-1111-111111111111
UUID2=22222222-2222-2222-2222-222222222222
PORT=20001
PANEL=18080
HOST=hk2.wxmuma.cc.cd

fail() {
  echo "E2E_FAIL: $*"
  echo "--- node.log ---"
  tail -n 80 "$DIR/node.log" 2>/dev/null || true
  echo "--- requests.log ---"
  cat "$DIR/requests.log" 2>/dev/null || true
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

cat > "$DIR/state1.json" <<EOF
{
  "etag": "1",
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
  "users": {"users": [{"id": 1, "uuid": "$UUID1", "speed_limit": 0, "device_limit": 0}]},
  "alive": {"alive": {}}
}
EOF

cat > "$DIR/state2.json" <<EOF
{
  "etag": "2",
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
  "users": {"users": [{"id": 2, "uuid": "$UUID2", "speed_limit": 0, "device_limit": 0}]},
  "alive": {"alive": {}}
}
EOF

python3 - "$DIR/state2.json" "$DIR/state3.json" "$PORT" "$HOST" "$UUID2" <<'PY'
import json, sys
src, dst, port, host, uuid = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4], sys.argv[5]
state = json.load(open(src))
state["etag"] = "3"
state["node"]["network_settings"]["path"] = "/cdn-api2"
json.dump(state, open(dst, "w"), separators=(",", ":"))
PY

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

"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload hello
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload early --early
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode udp --payload ping

sleep 3
grep -q "GET /api/v2/server/config 200" "$DIR/requests.log" || fail "config endpoint not called"
grep -q "GET /api/v1/server/UniProxy/user 200" "$DIR/requests.log" || fail "user endpoint not called"
grep -q "GET /api/v1/server/UniProxy/alivelist 200" "$DIR/requests.log" || fail "alivelist endpoint not called"
grep -q "/api/v1/server/UniProxy/push" "$DIR/reports.log" || fail "push report not received"
grep -q "/api/v1/server/UniProxy/alive" "$DIR/reports.log" || fail "alive report not received"

PID_BEFORE=$(cat /proc/"$NODE_PID"/stat | awk '{print $1}')
cp "$DIR/state2.json" "$DIR/state.json"
sleep 3
if "$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload old >/dev/null 2>&1; then
  fail "old user still accepted after user update"
fi
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID2" --mode tcp --payload new

cp "$DIR/state3.json" "$DIR/state.json"
sleep 3
if "$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID2" --mode tcp --payload oldpath >/dev/null 2>&1; then
  fail "old path still accepted after node config update"
fi
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api2" --host "$HOST" --uuid "$UUID2" --mode tcp --payload newpath

PID_AFTER=$(cat /proc/"$NODE_PID"/stat | awk '{print $1}')
[ "$PID_BEFORE" = "$PID_AFTER" ] || fail "node process restarted"
grep -q "node config applied" "$DIR/node.log" || fail "node config hot update not logged"

echo "E2E_PASS pid=$PID_AFTER requests=$(wc -l < "$DIR/requests.log") reports=$(wc -l < "$DIR/reports.log")"
