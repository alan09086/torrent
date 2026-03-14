#!/bin/bash
set -euo pipefail

# qbittorrent benchmark wrapper
# Usage: qbt_wrapper.sh <magnet_uri> <output_dir>
# Uses the running desktop qBittorrent instance (WebUI on port 8080).
# Adds magnet, polls until complete, removes torrent, then exits.

MAGNET="${1:?Usage: $0 <magnet_uri> <output_dir>}"
OUTPUT_DIR="${2:?Usage: $0 <magnet_uri> <output_dir>}"
QBT_PORT=8080
BASE="http://localhost:$QBT_PORT"

mkdir -p "$OUTPUT_DIR"

# Verify WebUI is reachable
if ! curl -s -o /dev/null -w "" "$BASE/api/v2/app/version" 2>/dev/null; then
    echo "ERROR: qBittorrent WebUI not reachable at $BASE" >&2
    exit 1
fi

# Add torrent with save path
curl -s \
    -d "urls=$MAGNET" \
    -d "savepath=$OUTPUT_DIR" \
    "$BASE/api/v2/torrents/add" > /dev/null

# Wait briefly for torrent to appear
sleep 1

# Get the info_hash of the torrent we just added (most recently added)
HASH=$(curl -s "$BASE/api/v2/torrents/info?sort=added_on&reverse=true&limit=1" \
    | python3 -c "import sys,json; t=json.load(sys.stdin); print(t[0]['hash'] if t else '')" 2>/dev/null)

if [ -z "$HASH" ]; then
    echo "ERROR: Could not find added torrent" >&2
    exit 1
fi

cleanup() {
    # Remove the torrent (but keep files for size measurement)
    curl -s -d "hashes=$HASH&deleteFiles=false" "$BASE/api/v2/torrents/delete" > /dev/null 2>&1 || true
}
trap cleanup EXIT

# Poll until download completes
while true; do
    PROGRESS=$(curl -s "$BASE/api/v2/torrents/info?hashes=$HASH" 2>/dev/null \
        | python3 -c "import sys,json; t=json.load(sys.stdin); print(t[0]['progress'] if t else 0)" 2>/dev/null || echo "0")

    if python3 -c "exit(0 if float('${PROGRESS}') >= 1.0 else 1)" 2>/dev/null; then
        break
    fi
    sleep 1
done

# Clean exit — trap handles torrent removal
