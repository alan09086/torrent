#!/bin/bash
set -euo pipefail

# Benchmark: ferrite vs rqbit vs libtorrent
# Usage: ./benchmarks/run_benchmark.sh <magnet_uri> [trials]

MAGNET="${1:?Usage: $0 <magnet_uri> [trials]}"
TRIALS="${2:-3}"
OUTPUT_DIR="/tmp/ferrite-bench"
RESULTS="benchmarks/results.csv"

echo "Building ferrite-cli (release)..."
cargo build --release -p ferrite-cli 2>/dev/null
FERRITE="target/release/ferrite"

echo "Ferrite binary: $FERRITE"
echo "Magnet: $MAGNET"
echo "Trials: $TRIALS"
echo ""

echo "client,trial,time_secs,avg_speed_mbps,peak_rss_kb" > "$RESULTS"

parse_time_output() {
    local file="$1"
    local wall_time
    wall_time=$(grep "Elapsed (wall clock)" "$file" | sed 's/.*: //' | awk -F: '{
        if (NF == 3) print $1*3600 + $2*60 + $3;
        else if (NF == 2) print $1*60 + $2;
        else print $1
    }')
    local rss
    rss=$(grep "Maximum resident" "$file" | sed 's/[^0-9]//g')
    echo "${wall_time:-0} ${rss:-0}"
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

    /usr/bin/time -v bash -c "$cmd" 2>"$time_file" || true

    read -r wall_time rss <<< "$(parse_time_output "$time_file")"
    local size
    size=$(du -sb "$client_dir" 2>/dev/null | awk '{print $1}')
    local avg_speed
    avg_speed=$(echo "${size:-0} $wall_time" | awk '{if ($2 > 0) printf "%.2f", $1/1048576/$2; else print 0}')

    echo "$client,$trial,$wall_time,$avg_speed,$rss" >> "$RESULTS"
    echo "    -> ${wall_time}s, ${avg_speed} MB/s, RSS ${rss} KB"
}

for trial in $(seq 1 "$TRIALS"); do
    echo "=== Trial $trial/$TRIALS ==="

    run_trial "ferrite" "$trial" \
        "$FERRITE download '$MAGNET' -o '$OUTPUT_DIR/ferrite' -q"

    run_trial "rqbit" "$trial" \
        "rqbit download '$MAGNET' -o '$OUTPUT_DIR/rqbit' -e"

    run_trial "libtorrent" "$trial" \
        "python benchmarks/lt_download.py '$MAGNET' '$OUTPUT_DIR/libtorrent'"

    echo ""
done

echo "Results saved to $RESULTS"
echo ""
python benchmarks/summarize.py "$RESULTS"
