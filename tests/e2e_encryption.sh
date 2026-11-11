#!/bin/bash
set -euo pipefail

DIR=/tmp/v2node-e2e-enc
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
BIN=${BIN:-/opt/v2node-rust-target/debug}
NODE=$BIN/v2node-rust
PROBE=$BIN/vless_probe
GEN=$BIN/gen_x25519
MOCK=$SCRIPT_DIR/mock_panel.py
UUID1=66666666-6666-6666-6666-666666666666
PORT=20006
PANEL=18086
HOST=hk2.wxmuma.cc.cd

fail() {
  echo "ENC_E2E_FAIL: $*"
  echo "--- node.log ---"
  tail -n 160 "$DIR/node.log" 2>/dev/null || true
  echo "--- reports.log ---"
  cat "$DIR/reports.log" 2>/dev/null || true
  exit 1
}

rm -rf "$DIR"
mkdir -p "$DIR"
KEYS=$("$GEN")
PRIVATE=$(printf '%s\n' "$KEYS" | sed -n 's/^PRIVATE=//p')
PUBLIC=$(printf '%s\n' "$KEYS" | sed -n 's/^PUBLIC=//p')
[ -n "$PRIVATE" ] || fail "gen_x25519 did not print PRIVATE"
[ -n "$PUBLIC" ] || fail "gen_x25519 did not print PUBLIC"

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
    "DnsProvider": "cloudflare",
    "DnsEnv": {},
    "DnsPropagationDelaySecs": 0,
    "CloudflareApiBase": "http://127.0.0.1:18084/client/v4",
    "RootCert": ""
  }
}
EOF

make_state() {
  local file=$1 etag=$2 encryption=$3 enc_settings=$4
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
    "encryption": "$encryption",
    "encryption_settings": $enc_settings,
    "trusted_x_forwarded_for": ["X-Forwarded-For"],
    "base_config": {"push_interval": 2, "pull_interval": 2, "node_report_min_traffic": 0, "device_online_min_traffic": 0},
    "routes": []
  },
  "users": {"users": [{"id": 1, "uuid": "$UUID1", "speed_limit": 0, "device_limit": 0}]},
  "alive": {"alive": {}}
}
EOF
}

SETTINGS=$(printf '{"mode":"native","rtt":"1rtt","ticket":"0s","server_padding":null,"client_padding":null,"private_key":"%s","password":"%s"}' "$PRIVATE" "$PUBLIC")
SETTINGS_XOR=$(printf '{"mode":"xorpub","rtt":"1rtt","ticket":"0s","server_padding":"100-111-1111.75-0-111.50-0-3333","client_padding":"100-111-1111.75-0-111.50-0-3333","private_key":"%s","password":"%s"}' "$PRIVATE" "$PUBLIC")
SETTINGS_RANDOM=$(printf '{"mode":"random","rtt":"1rtt","ticket":"0s","server_padding":"100-111-1111.75-0-111.50-0-3333","client_padding":"100-111-1111.75-0-111.50-0-3333","private_key":"%s","password":"%s"}' "$PRIVATE" "$PUBLIC")
make_state "$DIR/state1.json" 1 "mlkem768x25519plus" "$SETTINGS"
make_state "$DIR/state2.json" 2 "mlkem768x25519plus" "$SETTINGS_XOR"
make_state "$DIR/state3.json" 3 "mlkem768x25519plus" "$SETTINGS_RANDOM"
make_state "$DIR/state4.json" 4 "" "null"
cp "$DIR/state1.json" "$DIR/state.json"
python3 "$MOCK" --state "$DIR/state.json" --port "$PANEL" --dir "$DIR" > "$DIR/mock.out" 2>&1 &
MOCK_PID=$!
for _ in $(seq 1 50); do
  curl -s --max-time 1 "http://127.0.0.1:$PANEL/api/v2/server/config?node_type=v2node&node_id=1&token=test" >/dev/null 2>&1 && break
  sleep 0.1
done
"$NODE" --config "$DIR/config.json" > "$DIR/node.out" 2>&1 &
NODE_PID=$!
cleanup() {
  kill "$NODE_PID" "$MOCK_PID" 2>/dev/null || true
  wait "$NODE_PID" 2>/dev/null || true
  wait "$MOCK_PID" 2>/dev/null || true
}
trap cleanup EXIT

for _ in $(seq 1 60); do
  grep -q "VLESS encryption enabled" "$DIR/node.log" 2>/dev/null && break
  sleep 0.2
done
grep -q "VLESS encryption enabled" "$DIR/node.log" || fail "node did not enable VLESS encryption"
grep -q "listener started" "$DIR/node.log" || fail "listener did not start"

ENC="mlkem768x25519plus.native.1rtt.$PUBLIC"
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload encrypted --encryption "$ENC"
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode udp --payload encrypted-udp --encryption "$ENC"

sleep 3
grep -q "/api/v1/server/UniProxy/push" "$DIR/reports.log" || fail "encrypted user traffic was not reported"
grep -q "/api/v1/server/UniProxy/alive" "$DIR/reports.log" || fail "encrypted online user was not reported"

PID_BEFORE=$(cat /proc/"$NODE_PID"/stat | awk '{print $1}')
APPLIED=$(grep -c "node config applied" "$DIR/node.log" || true)
cp "$DIR/state2.json" "$DIR/state.json"
for _ in $(seq 1 50); do
  NOW=$(grep -c "node config applied" "$DIR/node.log" || true)
  [ "$NOW" -gt "$APPLIED" ] && break
  sleep 0.3
done
ENC_XOR="mlkem768x25519plus.xorpub.1rtt.100-111-1111.75-0-111.50-0-3333.$PUBLIC"
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload xorpub --encryption "$ENC_XOR"
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode udp --payload xorpub-udp --encryption "$ENC_XOR"

APPLIED=$(grep -c "node config applied" "$DIR/node.log" || true)
cp "$DIR/state3.json" "$DIR/state.json"
for _ in $(seq 1 50); do
  NOW=$(grep -c "node config applied" "$DIR/node.log" || true)
  [ "$NOW" -gt "$APPLIED" ] && break
  sleep 0.3
done
ENC_RANDOM="mlkem768x25519plus.random.1rtt.100-111-1111.75-0-111.50-0-3333.$PUBLIC"
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload random --encryption "$ENC_RANDOM"
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload-size 100000 --encryption "$ENC_RANDOM"
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode udp --payload random-udp --encryption "$ENC_RANDOM"

APPLIED=$(grep -c "node config applied" "$DIR/node.log" || true)
cp "$DIR/state4.json" "$DIR/state.json"
for _ in $(seq 1 50); do
  NOW=$(grep -c "node config applied" "$DIR/node.log" || true)
  [ "$NOW" -gt "$APPLIED" ] && break
  sleep 0.3
done
"$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload plain
if "$PROBE" --url "ws://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload stale --encryption "$ENC" >/dev/null 2>&1; then
  fail "encrypted client still accepted after encryption was disabled"
fi
PID_AFTER=$(cat /proc/"$NODE_PID"/stat | awk '{print $1}')
[ "$PID_BEFORE" = "$PID_AFTER" ] || fail "node process restarted during encryption config changes"

echo "ENC_E2E_PASS pid=$PID_AFTER"
