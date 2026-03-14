#!/bin/bash
set -euo pipefail

# DHT reliability stress test: run N consecutive downloads
# Each trial has a 45-second timeout. If no data within 30s, it's a stall.
# Captures debug logs for failed trials.

MAGNET="${1:?Usage: $0 <magnet_uri> [trials]}"
TRIALS="${2:-10}"
TORRENT="target/release/torrent"
DL_DIR="/mnt/TempNVME/bench-dl/dht-test"
LOG_DIR="/mnt/TempNVME/bench-results/dht-stress"
PORT=42020

mkdir -p "$LOG_DIR"

pass=0
fail=0

cleanup() {
    pkill -f "torrent download" 2>/dev/null || true
    sleep 1
    pkill -9 -f "torrent download" 2>/dev/null || true
    sleep 1
}

for trial in $(seq 1 "$TRIALS"); do
    echo "=== Trial $trial/$TRIALS ==="
    cleanup
    rm -rf "$DL_DIR" && mkdir -p "$DL_DIR"

    # Verify port is free
    if sudo ss -tlnp 2>/dev/null | grep -q ":${PORT} "; then
        echo "  WARNING: port $PORT in use, force killing..."
        sudo kill -9 "$(sudo ss -tlnp | grep ":${PORT} " | grep -oP 'pid=\K[0-9]+')" 2>/dev/null || true
        sleep 2
    fi

    log_file="$LOG_DIR/trial-${trial}.log"
    start_time=$(date +%s)

    # Run with focused debug logging (V4 DHT only + session torrent state)
    RUST_LOG=torrent_dht::actor=debug,torrent_session::torrent=info \
        timeout --signal=TERM --kill-after=5 45 \
        "$TORRENT" download "$MAGNET" -o "$DL_DIR" -q -p "$PORT" \
        2>"$log_file" &
    pid=$!

    # Monitor progress: check for data every 5 seconds, stall after 30s
    stalled=false
    completed=false
    for check in $(seq 1 9); do
        sleep 5
        if ! kill -0 "$pid" 2>/dev/null; then
            wait "$pid" 2>/dev/null || true
            completed=true
            break
        fi
        size=$(du -sb "$DL_DIR" 2>/dev/null | awk '{print $1}')
        elapsed=$(($(date +%s) - start_time))
        echo "  ${elapsed}s: ${size:-0} bytes downloaded"

        if [ "$elapsed" -ge 30 ] && [ "${size:-0}" -lt 1048576 ]; then
            echo "  STALL DETECTED: <1MB after 30s"
            stalled=true
            kill "$pid" 2>/dev/null || true
            sleep 2
            kill -9 "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
            break
        fi
    done

    if ! $completed && ! $stalled; then
        wait "$pid" 2>/dev/null || true
        completed=true
    fi

    end_time=$(date +%s)
    elapsed=$((end_time - start_time))
    final_size=$(du -sb "$DL_DIR" 2>/dev/null | awk '{print $1}')
    final_mb=$(echo "${final_size:-0}" | awk '{printf "%.1f", $1/1048576}')

    if $stalled; then
        echo "  FAIL: stalled after ${elapsed}s (${final_mb} MB)"
        fail=$((fail + 1))
        echo "  Log saved: $log_file"
        # Extract key DHT events (V4 only)
        echo "  --- Key DHT events ---"
        grep -E "BEP 42|get_peers.*V4|lookup exhausted|regenerat|family=V4" "$log_file" 2>/dev/null | head -30
        echo "  ---"
    else
        echo "  PASS: completed in ${elapsed}s (${final_mb} MB)"
        pass=$((pass + 1))
        # Remove successful logs
        rm -f "$log_file"
    fi
    echo ""
done

echo "================================"
echo "Results: $pass/$TRIALS passed, $fail/$TRIALS failed"
echo "================================"

if [ "$fail" -gt 0 ]; then
    echo ""
    echo "Failed trial logs in: $LOG_DIR/"
    ls -la "$LOG_DIR"/trial-*.log 2>/dev/null || echo "(no failed logs)"
fi

cleanup
rm -rf "$DL_DIR"
