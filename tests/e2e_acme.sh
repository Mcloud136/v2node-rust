#!/bin/bash
set -euo pipefail

DIR=/tmp/v2node-e2e-acme
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
BIN=${BIN:-/opt/v2node-rust-target/debug}
NODE=$BIN/v2node-rust
PROBE=$BIN/vless_probe
MOCK=$SCRIPT_DIR/mock_panel.py
CFMOCK=$SCRIPT_DIR/mock_cloudflare.py
UUID1=55555555-5555-5555-5555-555555555555
PORT=20004
PANEL=18085
CFPORT=18084
HOST=hk2.wxmuma.cc.cd
PEBBLE=/opt/pebble/pebble
PEBBLE_CFG=/opt/pebble/config.json
PEBBLE_LOG=/opt/pebble/pebble.log

fail() {
  echo "ACME_E2E_FAIL: $*"
  echo "--- node.log ---"
  tail -n 160 "$DIR/node.log" 2>/dev/null || true
  echo "--- pebble.log ---"
  tail -n 80 "$PEBBLE_LOG" 2>/dev/null || true
  echo "--- cloudflare.log ---"
  cat "$DIR/cloudflare.log" 2>/dev/null || true
  exit 1
}

start_pebble() {
  local always_valid=$1
  pkill -f "/opt/pebble/pebble -config" 2>/dev/null || true
  sleep 1
  if [ "$always_valid" = "1" ]; then
    PEBBLE_VA_ALWAYS_VALID=1 PEBBLE_VA_NOSLEEP=1 nohup "$PEBBLE" -config "$PEBBLE_CFG" -strict=false > "$PEBBLE_LOG" 2>&1 &
  else
    env -u PEBBLE_VA_ALWAYS_VALID PEBBLE_VA_NOSLEEP=1 nohup "$PEBBLE" -config "$PEBBLE_CFG" -strict=false > "$PEBBLE_LOG" 2>&1 &
  fi
  for _ in $(seq 1 30); do
    if curl -sk --max-time 1 https://localhost:14000/dir >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.2
  done
  fail "pebble did not start"
}

rm -rf "$DIR"
mkdir -p "$DIR"
cp /etc/hosts "$DIR/hosts.bak"
if ! grep -q "v2node-acme-test" /etc/hosts; then
  echo "127.0.0.1 http01.example.com dns.acme.example.com # v2node-acme-test" >> /etc/hosts
fi

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
    "DirectoryUrl": "https://localhost:14000/dir",
    "Email": "",
    "AccountDir": "$DIR/acme",
    "Http01Bind": "127.0.0.1:5002",
    "RenewBeforeDays": 30,
    "RenewCheckIntervalSecs": 5,
    "DnsProvider": "cloudflare",
    "DnsEnv": {},
    "DnsPropagationDelaySecs": 0,
    "CloudflareApiBase": "http://127.0.0.1:$CFPORT/client/v4",
    "RootCert": "/opt/pebble/ca.pem"
  }
}
EOF

make_state() {
  local file=$1 etag=$2 mode=$3 domain=$4 cert=$5 key=$6 provider=$7 dns_env=$8
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
    "tls_settings": {
      "server_name": "$domain",
      "cert_mode": "$mode",
      "cert_file": "$cert",
      "key_file": "$key",
      "provider": "$provider",
      "dns_env": "$dns_env",
      "reject_unknown_sni": "0"
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

make_state "$DIR/state1.json" 1 http http01.example.com "$DIR/http.crt" "$DIR/http.key" "" ""
make_state "$DIR/state2.json" 2 dns dns.acme.example.com "$DIR/dns.crt" "$DIR/dns.key" "cloudflare" "CLOUDFLARE_DNS_API_TOKEN=test"
cp "$DIR/state1.json" "$DIR/state.json"

start_pebble 0
python3 "$MOCK" --state "$DIR/state.json" --port "$PANEL" --dir "$DIR" > "$DIR/mock.out" 2>&1 &
MOCK_PID=$!
python3 "$CFMOCK" --port "$CFPORT" --dir "$DIR" > "$DIR/cfmock.out" 2>&1 &
CF_PID=$!
"$NODE" --config "$DIR/config.json" > "$DIR/node.out" 2>&1 &
NODE_PID=$!
cleanup() {
  kill "$NODE_PID" "$MOCK_PID" "$CF_PID" 2>/dev/null || true
  wait "$NODE_PID" 2>/dev/null || true
  wait "$MOCK_PID" 2>/dev/null || true
  wait "$CF_PID" 2>/dev/null || true
  cp "$DIR/hosts.bak" /etc/hosts 2>/dev/null || true
  pkill -f "/opt/pebble/pebble -config" 2>/dev/null || true
}
trap cleanup EXIT

for _ in $(seq 1 100); do
  grep -q "ACME certificate issued and applied" "$DIR/node.log" 2>/dev/null && break
  sleep 0.3
done
grep -q "ACME certificate issued and applied" "$DIR/node.log" || fail "HTTP-01 ACME issuance did not complete"
grep -q "listener started.*tls=true" "$DIR/node.log" || fail "TLS listener did not start after HTTP-01 issuance"
[ -s "$DIR/http.crt" ] || fail "HTTP-01 certificate file missing"
[ -s "$DIR/http.key" ] || fail "HTTP-01 private key missing"
[ -s "$DIR/acme/account.json" ] || fail "ACME account credentials were not persisted"
grep -q "Attempting to validate w/ HTTP" "$PEBBLE_LOG" || fail "Pebble did not log HTTP-01 validation"
"$PROBE" --url "wss://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload http01 --insecure
"$PROBE" --url "wss://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode udp --payload http01udp --insecure

PID_BEFORE=$(cat /proc/"$NODE_PID"/stat | awk '{print $1}')
start_pebble 1
rm -f "$DIR/acme/account.json"
cp "$DIR/state2.json" "$DIR/state.json"
for _ in $(seq 1 100); do
  [ -s "$DIR/dns.crt" ] && break
  sleep 0.3
done
[ -s "$DIR/dns.crt" ] || fail "DNS-01 certificate file missing"
[ -s "$DIR/dns.key" ] || fail "DNS-01 private key missing"
grep -q "GET zones" "$DIR/cloudflare.log" || fail "Cloudflare zones API was not called"
grep -q "POST /client/v4/zones/zone-123/dns_records" "$DIR/cloudflare.log" || fail "Cloudflare create TXT was not called"
grep -q "_acme-challenge.dns.acme.example.com" "$DIR/cloudflare.log" || fail "Cloudflare TXT name missing"
grep -q "DELETE /client/v4/zones/zone-123/dns_records/record-456" "$DIR/cloudflare.log" || fail "Cloudflare delete TXT was not called"
for _ in $(seq 1 50); do
  grep -q "node config applied" "$DIR/node.log" 2>/dev/null && break
  sleep 0.3
done
"$PROBE" --url "wss://127.0.0.1:$PORT/cdn-api" --host "$HOST" --uuid "$UUID1" --mode tcp --payload dns01 --insecure
PID_AFTER=$(cat /proc/"$NODE_PID"/stat | awk '{print $1}')
[ "$PID_BEFORE" = "$PID_AFTER" ] || fail "node process restarted during ACME config changes"

echo "ACME_E2E_PASS pid=$PID_AFTER http_cert=$(stat -c%s "$DIR/http.crt") dns_cert=$(stat -c%s "$DIR/dns.crt") cf_calls=$(wc -l < "$DIR/cloudflare.log")"
