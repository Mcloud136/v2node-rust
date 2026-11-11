#!/bin/bash
set -euo pipefail

DIR=/tmp/v2node-e2e-report
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
BIN=${BIN:-/opt/v2node-rust-target/debug}
NODE=$BIN/v2node-rust
PROBE=$BIN/vless_probe
MOCK=$SCRIPT_DIR/mock_panel.py
UUID1=88888888-8888-8888-8888-888888888888
PORT=20009
PANEL=18088
HOST=hk2.wxmuma.cc.cd

fail() {
  echo "REPORT_E2E_FAIL: $*"
  echo "--- node.log ---"; tail -n 120 "$DIR/node.log" 2>/dev/null || true
  echo "--- reports.log ---"; cat "$DIR/reports.log" 2>/dev/null || true
  echo "--- requests.log ---"; tail -n 60 "$DIR/requests.log" 2>/dev/null || true
  exit 1
}

stats() {
  python3 - "$DIR/reports.log" <<'PY'
import json, sys
total = 0
pushes = 0
alive = 0
for line in open(sys.argv[1], encoding="utf-8", errors="replace"):
    path, _, body = line.partition(" ")
    if path.endswith("/push"):
        try:
            data = json.loads(body)
        except Exception:
            continue
        total += sum(int(v[0]) + int(v[1]) for v in data.values())
        pushes += 1
    elif path.endswith("/alive"):
        alive += 1
print(f"{total} {pushes} {alive}")
PY
}

rm -rf "$DIR"; mkdir -p "$DIR"

cat > "$DIR/config.json" <<EOF
{
  "Log": {"Level": "debug", "Output": "$DIR/node.log", "Access": "none", "Ansi": false},
  "Nodes": [{"ApiHost": "http://127.0.0.1:$PANEL", "NodeID": 1, "ApiKey": "test", "Timeout": 5, "RetryCount": 0}],
  "PprofPort": 0,
  "Runtime": {
    "ConfigWatchIntervalSecs": 1,
    "PanelConnectTimeoutSecs": 1,
    "PanelRequestTimeoutSecs": 2,
    "PanelRetryCount": 0,
    "PanelRetryBackoffMs": 100,
    "DefaultPullIntervalSecs": 2,
    "DefaultPushIntervalSecs": 2,
    "WsHandshakeTimeoutSecs": 4,
    "WsMaxHeaderBytes": 8192,
    "WsEarlyDataMaxBytes": 2048,
    "WsHeartbeatSecs": 0,
    "ConnectionIdleTimeoutSecs": 0,
    "DrainTimeoutSecs": 5,
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

start_mock() {
  python3 "$MOCK" --state "$DIR/state.json" --port "$PANEL" --dir "$DIR" >> "$DIR/mock.out" 2>&1 &
  MOCK_PID=$!
  for _ in $(seq 1 50); do
    if curl -s --max-time 1 "http://127.0.0.1:$PANEL/api/v2/server/config?node_type=v2node&node_id=1&token=test" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  fail "mock panel did not start"
}

start_mock
"$NODE" --config "$DIR/config.json" > "$DIR/node.out" 2>&1 &
NODE_PID=$!
cleanup() {
  kill "$NODE_PID" 2>/dev/null || true
  kill "${MOCK_PID:-0}" 2>/dev/null || true
  wait "$NODE_PID" 2>/dev/null || true
  wait "${MOCK_PID:-0}" 2>/dev/null || true
}
trap cleanup EXIT

for _ in $(seq 1 50); do
  grep -q "listener started" "$DIR/node.log" 2>/dev/null && break
  sleep 0.2
done
grep -q "listener started" "$DIR/node.log" || fail "node listener did not start"

# A held connection must be reported as online in every push window,
# and a failed alive report must be retried while the link stays open.
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" \
  --mode tcp --payload-size 10000 --hold-seconds 12 > "$DIR/probe1.out" 2>&1 &
PROBE1_PID=$!
for _ in $(seq 1 50); do
  read -r _ _ ALIVE <<< "$(stats)"
  [ "${ALIVE:-0}" -ge 1 ] && break
  sleep 0.2
done
read -r TOTAL PUSHES ALIVE <<< "$(stats)"
[ "$ALIVE" -ge 1 ] || fail "held connection was never reported online"
touch "$DIR/fail_alive"
sleep 3
rm -f "$DIR/fail_alive"
for _ in $(seq 1 30); do
  read -r TOTAL PUSHES ALIVE <<< "$(stats)"
  [ "$ALIVE" -ge 2 ] && break
  sleep 0.25
done
[ "$ALIVE" -ge 2 ] || fail "alive report was not retried after a failed window (alive=$ALIVE)"
for _ in $(seq 1 60); do
  read -r TOTAL PUSHES ALIVE <<< "$(stats)"
  [ "$TOTAL" -ge 20000 ] && break
  sleep 0.25
done
[ "$TOTAL" -eq 20000 ] || fail "held connection traffic not reported exactly once (total=$TOTAL)"
wait "$PROBE1_PID"
grep -q "PROBE_OK" "$DIR/probe1.out" || fail "held probe did not finish"

# Stop the panel, generate traffic, then bring it back: traffic that was
# definitely not delivered must be retried, not silently dropped or doubled.
kill "$MOCK_PID" 2>/dev/null || true
wait "$MOCK_PID" 2>/dev/null || true
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" \
  --mode tcp --payload-size 5000 > "$DIR/probe2.out" 2>&1
grep -q "PROBE_OK" "$DIR/probe2.out" || fail "outage probe did not finish"
sleep 4
grep -q "keeping it for retry" "$DIR/node.log" || fail "client did not classify the outage as definitely-not-sent"
start_mock
for _ in $(seq 1 60); do
  read -r TOTAL PUSHES ALIVE <<< "$(stats)"
  [ "$TOTAL" -ge 30000 ] && break
  sleep 0.25
done
read -r TOTAL PUSHES ALIVE <<< "$(stats)"
[ "$TOTAL" -eq 30000 ] || fail "traffic after panel outage was lost or duplicated (total=$TOTAL expected=30000)"
if grep -q "dropping the batch" "$DIR/node.log"; then
  fail "a delivered-unknown batch was retried"
fi

# A 500 response is ambiguous: the batch is dropped (not retried) to avoid
# double billing. The mock logs the received body, so a retry would double
# the reported total.
touch "$DIR/fail_push"
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" \
  --mode tcp --payload-size 3000 > "$DIR/probe3.out" 2>&1
grep -q "PROBE_OK" "$DIR/probe3.out" || fail "ambiguous-failure probe did not finish"
sleep 4
rm -f "$DIR/fail_push"
sleep 4
read -r TOTAL PUSHES ALIVE <<< "$(stats)"
[ "$TOTAL" -eq 36000 ] || fail "ambiguous push outcome was retried or lost (total=$TOTAL expected=36000)"
grep -q "dropping the batch" "$DIR/node.log" || fail "ambiguous push failure was not logged as dropped"

echo "REPORT_E2E_PASS total=$TOTAL pushes=$PUSHES alive=$ALIVE"
