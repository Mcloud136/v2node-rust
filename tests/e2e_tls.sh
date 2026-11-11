#!/bin/bash
set -euo pipefail

DIR=/tmp/v2node-e2e-tls
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
BIN=${BIN:-/opt/v2node-rust-target/debug}
NODE=$BIN/v2node-rust
PROBE=$BIN/vless_probe
MOCK=$SCRIPT_DIR/mock_panel.py
UUID1=33333333-3333-3333-3333-333333333333
PORT=20002
PANEL=18081
HOST=localhost
CERT=$DIR/node.crt
KEY=$DIR/node.key

fail() {
  echo "TLS_E2E_FAIL: $*"
  echo "--- node.log ---"
  tail -n 100 "$DIR/node.log" 2>/dev/null || true
  echo "--- requests.log ---"
  cat "$DIR/requests.log" 2>/dev/null || true
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
    "CertReloadIntervalSecs": 2
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
  local file=$1 etag=$2 cert_mode=$3 reject=$4 path=${5:-/cdn-api}
  cat > "$file" <<EOF
{
  "etag": "$etag",
  "node": {
    "listen_ip": "127.0.0.1",
    "server_port": $PORT,
    "network": "ws",
    "network_settings": {"path": "$path", "headers": {"Host": "$HOST"}},
    "protocol": "vless",
    "tls": 1,
    "tls_settings": {
      "server_name": "$HOST",
      "cert_mode": "$cert_mode",
      "cert_file": "$CERT",
      "key_file": "$KEY",
      "reject_unknown_sni": "$reject"
    },
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
}

make_state "$DIR/state1.json" 1 self 0
make_state "$DIR/state2.json" 2 file 0
make_state "$DIR/state3.json" 3 file 1
make_state "$DIR/state4.json" 4 file 0
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
  grep -q "listener started.*tls=true" "$DIR/node.log" 2>/dev/null && break
  sleep 0.2
done
grep -q "listener started.*tls=true" "$DIR/node.log" || fail "TLS listener did not start"
[ -f "$CERT" ] || fail "self-signed certificate was not generated"
[ -f "$KEY" ] || fail "self-signed key was not generated"

"$PROBE" --url "wss://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload self --insecure
"$PROBE" --url "wss://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode udp --payload selfudp --insecure

PID_BEFORE=$(cat /proc/"$NODE_PID"/stat | awk '{print $1}')
cp "$DIR/state2.json" "$DIR/state.json"
sleep 3
"$PROBE" --url "wss://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload file --insecure
grep -q "node config applied" "$DIR/node.log" || fail "file mode hot update not applied"

cp "$DIR/state3.json" "$DIR/state.json"
sleep 3
if "$PROBE" --url "wss://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload reject --insecure >/dev/null 2>&1; then
  fail "TLS handshake without SNI was accepted while reject_unknown_sni=1"
fi

cp "$DIR/state4.json" "$DIR/state.json"
sleep 3
"$PROBE" --url "wss://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload recovered --insecure

if command -v openssl >/dev/null 2>&1; then
  openssl req -x509 -newkey rsa:2048 -nodes -keyout "$KEY" -out "$CERT" -days 1 -subj "/CN=$HOST" >/dev/null 2>&1
  sleep 4
  grep -q "TLS certificate reloaded" "$DIR/node.log" || fail "certificate hot reload not logged"
  "$PROBE" --url "wss://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload reloaded --insecure
fi

openssl req -x509 -newkey rsa:2048 -nodes -keyout "$DIR/remote.key" -out "$DIR/remote.crt" -days 1 -subj "/CN=$HOST" >/dev/null 2>&1
python3 - "$DIR/state4.json" "$DIR/state5.json" "$DIR/remote.crt" "$DIR/remote.key" "$DIR/loaded.crt" "$DIR/loaded.key" <<'PY'
import json, sys
src, dst, cert, key, loaded_cert, loaded_key = sys.argv[1:7]
state = json.load(open(src, encoding="utf-8"))
state["etag"] = "5"
state["node"]["tls_settings"]["cert_mode"] = "remote"
state["node"]["tls_settings"]["cert_file"] = loaded_cert
state["node"]["tls_settings"]["key_file"] = loaded_key
state["node"]["tls_settings"]["tls_cert"] = open(cert, encoding="utf-8").read()
state["node"]["tls_settings"]["tls_key"] = open(key, encoding="utf-8").read()
state["node"]["tls_settings"]["reject_unknown_sni"] = "0"
json.dump(state, open(dst, "w", encoding="utf-8"), separators=(",", ":"))
PY
cp "$DIR/state5.json" "$DIR/state.json"
sleep 3
"$PROBE" --url "wss://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload remote --insecure
[ -s "$DIR/loaded.crt" ] || fail "remote certificate was not written"
[ -s "$DIR/loaded.key" ] || fail "remote private key was not written"

PID_AFTER=$(cat /proc/"$NODE_PID"/stat | awk '{print $1}')
[ "$PID_BEFORE" = "$PID_AFTER" ] || fail "node process restarted during TLS config changes"

echo "TLS_E2E_PASS pid=$PID_AFTER requests=$(wc -l < "$DIR/requests.log")"
