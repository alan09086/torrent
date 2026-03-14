#!/bin/bash
set -euo pipefail

# Benchmark: torrent vs rqbit vs qbittorrent
# Usage: ./benchmarks/run_benchmark.sh <magnet_uri> [trials] [timeout_secs]

MAGNET="${1:?Usage: $0 <magnet_uri> [trials] [timeout_secs]}"
TRIALS="${2:-5}"
TIMEOUT="${3:-300}"  # 5 minute default timeout per trial
OUTPUT_DIR="/tmp/torrent-bench"
RESULTS="benchmarks/results.csv"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# --- Cleanup old processes and files ---
echo "Cleaning up old benchmark state..."

# Kill any leftover benchmark processes
for proc in "torrent download" "rqbit download" "qbittorrent-nox.*qbt-bench"; do
    pkill -f "$proc" 2>/dev/null || true
done
sleep 1
# Force-kill stragglers
for proc in "torrent download" "rqbit download" "qbittorrent-nox.*qbt-bench"; do
    pkill -9 -f "$proc" 2>/dev/null || true
done

# Remove old benchmark output
rm -rf "$OUTPUT_DIR"
rm -rf /tmp/qbt-bench-profile /tmp/qbt-bench-cookies.txt /tmp/qbt-bench-startup.log
mkdir -p "$OUTPUT_DIR"

# --- Check disk space (need ~2GB per trial per client) ---
REQUIRED_GB=$(( TRIALS * 3 * 2 ))
AVAIL_GB=$(df --output=avail /tmp 2>/dev/null | tail -1 | awk '{printf "%d", $1/1048576}')
if [ "$AVAIL_GB" -lt "$REQUIRED_GB" ]; then
    echo "ERROR: Need ~${REQUIRED_GB}GB free in /tmp, only ${AVAIL_GB}GB available" >&2
    echo "Clean up /tmp before running benchmarks." >&2
    exit 1
fi
echo "Disk space: ${AVAIL_GB}GB available (need ~${REQUIRED_GB}GB)"

echo "Building torrent-cli (release)..."
cargo build --release -p torrent-cli 2>/dev/null
TORRENT="target/release/torrent"

echo "Torrent binary: $TORRENT"
echo "Magnet: $MAGNET"
echo "Trials: $TRIALS"
echo "Timeout: ${TIMEOUT}s per trial"
echo ""

echo "client,trial,time_secs,cpu_secs,avg_speed_mbps,peak_rss_kb" > "$RESULTS"

parse_time_output() {
    local file="$1"
    local wall_time
    wall_time=$(grep "Elapsed (wall clock)" "$file" | sed 's/.*: //' | awk -F: '{
        if (NF == 3) print $1*3600 + $2*60 + $3;
        else if (NF == 2) print $1*60 + $2;
        else print $1
    }')
    local user_time
    user_time=$(grep "User time" "$file" | sed 's/.*: //')
    local sys_time
    sys_time=$(grep "System time" "$file" | sed 's/.*: //')
    local cpu_time
    cpu_time=$(echo "${user_time:-0} ${sys_time:-0}" | awk '{printf "%.2f", $1 + $2}')
    local rss
    rss=$(grep "Maximum resident" "$file" | sed 's/[^0-9]//g')
    echo "${wall_time:-0} ${cpu_time:-0} ${rss:-0}"
}

run_trial() {
    local client="$1"
    local trial="$2"
    local cmd="$3"
    local client_dir="$OUTPUT_DIR/$client"

    rm -rf "$client_dir"
    mkdir -p "$client_dir"

    echo "  Trial $trial: $client..."
    local time_file="$OUTPUT_DIR/${client}-time-${trial}.txt"
    local stats_file="$OUTPUT_DIR/${client}-stats-${trial}.txt"

    # Run with timeout to prevent hangs
    timeout --signal=TERM --kill-after=10 "$TIMEOUT" \
        /usr/bin/time -v bash -c "$cmd 2>\"$stats_file\"" 2>"$time_file"
    local exit_code=$?

    if [ "$exit_code" -eq 124 ]; then
        echo "    !! TIMED OUT after ${TIMEOUT}s"
    fi

    read -r wall_time cpu_time rss <<< "$(parse_time_output "$time_file")"
    local size
    size=$(du -sb "$client_dir" 2>/dev/null | awk '{print $1}')
    local avg_speed
    avg_speed=$(echo "${size:-0} $wall_time" | awk '{if ($2 > 0) printf "%.2f", $1/1048576/$2; else print 0}')

    echo "$client,$trial,$wall_time,$cpu_time,$avg_speed,$rss" >> "$RESULTS"
    echo "    -> ${wall_time}s, CPU ${cpu_time}s, ${avg_speed} MB/s, RSS ${rss} KB"

    # Kill any lingering processes from this client after each trial
    case "$client" in
        torrent) pkill -f "torrent download" 2>/dev/null || true ;;
        rqbit)   pkill -f "rqbit download" 2>/dev/null || true ;;
    esac
    sleep 1
}

for trial in $(seq 1 "$TRIALS"); do
    echo "=== Trial $trial/$TRIALS ==="

    run_trial "torrent" "$trial" \
        "$TORRENT download '$MAGNET' -o '$OUTPUT_DIR/torrent' -q"

    run_trial "rqbit" "$trial" \
        "rqbit download '$MAGNET' -o '$OUTPUT_DIR/rqbit' -e"

    run_trial "qbittorrent" "$trial" \
        "bash '$SCRIPT_DIR/qbt_wrapper.sh' '$MAGNET' '$OUTPUT_DIR/qbittorrent'"

    echo ""
done

echo "=== Torrent peer stats ===" && cat "$OUTPUT_DIR"/torrent-stats-*.txt 2>/dev/null || true
echo ""
echo "Results saved to $RESULTS"
echo ""
python benchmarks/summarize.py "$RESULTS"
