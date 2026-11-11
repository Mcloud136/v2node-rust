#!/bin/bash
set -euo pipefail

DIR=/tmp/v2node-e2e-config
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
BIN=${BIN:-/opt/v2node-rust-target/debug}
NODE=$BIN/v2node-rust
PROBE=$BIN/vless_probe
MOCK=$SCRIPT_DIR/mock_panel.py
UUID1=77777777-7777-7777-7777-777777777777
PORT=20007
PANEL=18087
HOST=hk2.wxmuma.cc.cd
LOG1=$DIR/node1.log
LOG2=$DIR/node2.log
BUSY_PORT=20008
BLOCK_PID=""

fail() {
  echo "CONFIG_E2E_FAIL: $*"
  echo "--- node1.log ---"; tail -n 80 "$LOG1" 2>/dev/null || true
  echo "--- node2.log ---"; tail -n 80 "$LOG2" 2>/dev/null || true
  exit 1
}

rm -rf "$DIR"; mkdir -p "$DIR"
cat > "$DIR/config.json" <<EOF
{
  "Log": {"Level": "info", "Output": "$LOG1", "Access": "none", "Ansi": false},
  "Nodes": [{"ApiHost": "http://127.0.0.1:$PANEL", "NodeID": 1, "ApiKey": "test", "Timeout": 5, "RetryCount": 1}],
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
    "ListenBacklog": 2048,
    "DrainTimeoutSecs": 10,
    "TcpConnectTimeoutSecs": 5,
    "AllowPrivateTargets": true,
    "UdpIdleTimeoutSecs": 5,
    "RelayBufferSize": 32768,
    "TlsHandshakeTimeoutSecs": 4,
    "TlsMinVersion": "1.2",
    "TlsMaxVersion": "1.3",
    "TlsAlpn": ["http/1.1"],
    "CertReloadIntervalSecs": 5
  },
  "Acme": {
    "DirectoryUrl": "https://acme-staging-v02.api.letsencrypt.org/directory",
    "Email": "", "AccountDir": "$DIR/acme", "Http01Bind": "127.0.0.1:8080",
    "RenewBeforeDays": 30, "RenewCheckIntervalSecs": 86400,
    "DnsProvider": "cloudflare", "DnsEnv": {}, "DnsPropagationDelaySecs": 0,
    "CloudflareApiBase": "http://127.0.0.1:18084/client/v4", "RootCert": ""
  }
}
EOF

cat > "$DIR/state.json" <<EOF
{
  "etag": "1",
  "node": {
    "listen_ip": "127.0.0.1", "server_port": $PORT, "network": "ws",
    "network_settings": {"path": "/cdn-api", "headers": {"Host": "$HOST"}},
    "protocol": "vless", "tls": 1,
    "tls_settings": {"server_name": "$HOST", "cert_mode": "none"},
    "flow": "", "encryption": "",
    "trusted_x_forwarded_for": ["X-Forwarded-For"],
    "base_config": {"push_interval": 2, "pull_interval": 2, "node_report_min_traffic": 0, "device_online_min_traffic": 0},
    "routes": []
  },
  "users": {"users": [{"id": 1, "uuid": "$UUID1", "speed_limit": 0, "device_limit": 0}]},
  "alive": {"alive": {}}
}
EOF

python3 "$MOCK" --state "$DIR/state.json" --port "$PANEL" --dir "$DIR" > "$DIR/mock.out" 2>&1 &
MOCK_PID=$!
"$NODE" --config "$DIR/config.json" > "$DIR/node.out" 2>&1 &
NODE_PID=$!
cleanup() {
  kill "$NODE_PID" "$MOCK_PID" ${BLOCK_PID:+"$BLOCK_PID"} 2>/dev/null || true
  wait "$NODE_PID" 2>/dev/null || true
  wait "$MOCK_PID" 2>/dev/null || true
}
trap cleanup EXIT

for _ in $(seq 1 50); do
  grep -q "listener started" "$LOG1" 2>/dev/null && break
  sleep 0.2
done
grep -q "listener started" "$LOG1" || fail "initial listener did not start"
BACKLOG=$(ss -lnt 2>/dev/null | awk -v port=":$PORT" '$4 ~ port {print $3; exit}')
[ "$BACKLOG" = "2048" ] || fail "ListenBacklog not applied: send-q=$BACKLOG"
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload before
PID_BEFORE=$(cat /proc/"$NODE_PID"/stat | awk '{print $1}')

python3 - "$DIR/config.json" "$LOG2" <<'PATCH_LOG'
import json, sys
path, out = sys.argv[1], sys.argv[2]
config = json.load(open(path, encoding="utf-8"))
config["Log"]["Level"] = "debug"
config["Log"]["Output"] = out
with open(path, "w", encoding="utf-8") as handle:
    json.dump(config, handle, separators=(",", ":"))
PATCH_LOG

for _ in $(seq 1 80); do
  grep -q "logging config applied" "$LOG2" 2>/dev/null && break
  sleep 0.25
done
grep -q "logging config applied" "$LOG2" || fail "logging-only hot reload was not applied"
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload after-log
grep -q "DEBUG" "$LOG2" || fail "log level hot reload did not take effect"
if grep -q "node runtime started" "$LOG2"; then
  fail "logging-only hot reload restarted the node runtime"
fi

RESTARTS=$(grep -c "node runtime started" "$LOG2" || true)
APPLIED=$(grep -c "node config applied in place" "$LOG2" || true)
python3 - "$DIR/config.json" <<'PATCH_RUNTIME'
import json, sys
path = sys.argv[1]
config = json.load(open(path, encoding="utf-8"))
config["Runtime"]["DefaultPullIntervalSecs"] = 3
with open(path, "w", encoding="utf-8") as handle:
    json.dump(config, handle, separators=(",", ":"))
PATCH_RUNTIME
for _ in $(seq 1 80); do
  NOW=$(grep -c "node config applied in place" "$LOG2" || true)
  [ "$NOW" -gt "$APPLIED" ] && break
  sleep 0.25
done
NOW=$(grep -c "node config applied in place" "$LOG2" || true)
[ "$NOW" -gt "$APPLIED" ] || fail "runtime config change was not applied in place"
RESTARTS_AFTER=$(grep -c "node runtime started" "$LOG2" || true)
[ "$RESTARTS_AFTER" = "$RESTARTS" ] || fail "runtime config change restarted the node runtime"
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload after-runtime

python3 - "$DIR/config.json" <<'PATCH_NODE'
import json, sys
path = sys.argv[1]
config = json.load(open(path, encoding="utf-8"))
config["Nodes"][0]["ApiHost"] = "http://127.0.0.1:1"
config["Nodes"][0]["Timeout"] = 1
config["Nodes"][0]["RetryCount"] = 0
with open(path, "w", encoding="utf-8") as handle:
    json.dump(config, handle, separators=(",", ":"))
PATCH_NODE
for _ in $(seq 1 80); do
  grep -q "previous config restored" "$LOG2" 2>/dev/null && break
  sleep 0.25
done
grep -q "previous config restored" "$LOG2" || fail "failed hot reload did not restore the previous config"
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload after-restore

python3 -m http.server "$BUSY_PORT" --bind 127.0.0.1 >/dev/null 2>&1 &
BLOCK_PID=$!
sleep 0.5
python3 - "$DIR/state.json" "$BUSY_PORT" <<'PATCH_PORT'
import json, sys
path, port = sys.argv[1], int(sys.argv[2])
state = json.load(open(path, encoding="utf-8"))
state["etag"] = "2"
state["node"]["server_port"] = port
with open(path, "w", encoding="utf-8") as handle:
    json.dump(state, handle, separators=(",", ":"))
PATCH_PORT
for _ in $(seq 1 80); do
  grep -q "previous listener restored" "$LOG2" 2>/dev/null && break
  sleep 0.25
done
grep -q "previous listener restored" "$LOG2" || fail "listener bind failure did not restore the previous listener"
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload after-bind-failure
kill "$BLOCK_PID" 2>/dev/null || true
BLOCK_PID=""
APPLIED=$(grep -c "node config applied" "$LOG2" || true)
for _ in $(seq 1 80); do
  NOW=$(grep -c "node config applied" "$LOG2" || true)
  [ "$NOW" -gt "$APPLIED" ] && break
  sleep 0.25
done
NOW=$(grep -c "node config applied" "$LOG2" || true)
[ "$NOW" -gt "$APPLIED" ] || fail "failed node config was not retried after the port became available"
"$PROBE" --url "ws://127.0.0.1:$BUSY_PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload after-retry

PID_AFTER=$(cat /proc/"$NODE_PID"/stat | awk '{print $1}')
[ "$PID_BEFORE" = "$PID_AFTER" ] || fail "node process restarted on config file change"
echo "CONFIG_E2E_PASS pid=$PID_AFTER log2=$(wc -l < "$LOG2")"
