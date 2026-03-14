#!/bin/bash
# Quick reliability test: N downloads with timeout, counts passes/fails
MAGNET="${1:?Usage: $0 <magnet_uri> [trials] [timeout]}"
TRIALS="${2:-10}"
TIMEOUT="${3:-35}"
DL_DIR="/mnt/TempNVME/bench-dl/dht-test"
PORT=42020

pass=0
fail=0

for i in $(seq 1 "$TRIALS"); do
    rm -rf "$DL_DIR" 2>/dev/null
    mkdir -p "$DL_DIR"

    # Ensure port is clear
    for attempt in 1 2 3; do
        if ! ss -tlnp 2>/dev/null | grep -q ":${PORT} "; then
            break
        fi
        pid=$(sudo ss -tlnp 2>/dev/null | grep ":${PORT} " | grep -oP 'pid=\K[0-9]+' | head -1)
        if [ -n "$pid" ]; then
            sudo kill -9 "$pid" 2>/dev/null
        fi
        sleep 1
    done

    timeout --signal=TERM --kill-after=5 "$TIMEOUT" \
        target/release/torrent download "$MAGNET" -o "$DL_DIR" -q -p "$PORT" \
        >/dev/null 2>&1
    exit_code=$?

    size=$(du -sb "$DL_DIR" 2>/dev/null | awk '{print $1}')
    size_mb=$(echo "${size:-0}" | awk '{printf "%.0f", $1/1048576}')

    if [ "${size:-0}" -gt 1048576 ]; then
        echo "Trial $i: PASS (${size_mb} MB, exit $exit_code)"
        pass=$((pass + 1))
    else
        echo "Trial $i: FAIL (${size_mb} MB, exit $exit_code)"
        fail=$((fail + 1))
    fi

    # Cleanup between trials
    pkill -f "torrent download" 2>/dev/null || true
    sleep 2
done

echo "---"
echo "Result: $pass/$TRIALS passed, $fail/$TRIALS failed"

rm -rf "$DL_DIR" 2>/dev/null
