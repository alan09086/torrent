#!/bin/bash
set -euo pipefail

# qbittorrent-nox benchmark wrapper
# Usage: qbt_wrapper.sh <magnet_uri> <output_dir>
# Starts qbittorrent-nox, adds magnet, polls until complete, then exits.

MAGNET="${1:?Usage: $0 <magnet_uri> <output_dir>}"
OUTPUT_DIR="${2:?Usage: $0 <magnet_uri> <output_dir>}"
QBT_PORT=18080
QBT_PROFILE="/tmp/qbt-bench-profile"

rm -rf "$QBT_PROFILE"
mkdir -p "$QBT_PROFILE" "$OUTPUT_DIR"

# Start qbittorrent-nox in background, capture output for password
QBT_LOG="/tmp/qbt-bench-startup.log"
qbittorrent-nox \
    --profile="$QBT_PROFILE" \
    --webui-port="$QBT_PORT" \
    --confirm-legal-notice \
    > "$QBT_LOG" 2>&1 &
QBT_PID=$!

cleanup() {
    kill "$QBT_PID" 2>/dev/null || true
    wait "$QBT_PID" 2>/dev/null || true
    rm -rf "$QBT_PROFILE"
}
trap cleanup EXIT

# Wait for WebUI to come up and extract password
QBT_PASS=""
for i in $(seq 1 30); do
    if grep -q "temporary password" "$QBT_LOG" 2>/dev/null; then
        QBT_PASS=$(grep "temporary password" "$QBT_LOG" | sed 's/.*: //')
        break
    fi
    sleep 0.2
done

if [ -z "$QBT_PASS" ]; then
    echo "ERROR: Could not extract qbittorrent password" >&2
    exit 1
fi

BASE="http://localhost:$QBT_PORT"
COOKIE_JAR="/tmp/qbt-bench-cookies.txt"

# Wait for WebUI to accept connections
for i in $(seq 1 30); do
    if curl -s -o /dev/null -w "%{http_code}" "$BASE/api/v2/app/version" 2>/dev/null | grep -q "200\|403"; then
        break
    fi
    sleep 0.2
done

# Authenticate
curl -s -c "$COOKIE_JAR" -d "username=admin&password=$QBT_PASS" "$BASE/api/v2/auth/login" > /dev/null

# Set save path and add torrent
curl -s -b "$COOKIE_JAR" \
    -d "urls=$MAGNET" \
    -d "savepath=$OUTPUT_DIR" \
    "$BASE/api/v2/torrents/add" > /dev/null

# Poll until download completes
while true; do
    PROGRESS=$(curl -s -b "$COOKIE_JAR" "$BASE/api/v2/torrents/info" 2>/dev/null \
        | python3 -c "import sys,json; t=json.load(sys.stdin); print(t[0]['progress'] if t else 0)" 2>/dev/null || echo "0")

    if python3 -c "exit(0 if float('${PROGRESS}') >= 1.0 else 1)" 2>/dev/null; then
        break
    fi
    sleep 2
done

# Clean exit — trap handles kill
