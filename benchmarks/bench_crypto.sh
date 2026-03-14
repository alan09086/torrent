#!/bin/bash
set -euo pipefail

# Benchmark: ring vs aws-lc-rs vs openssl crypto backends
# Usage: ./benchmarks/bench_crypto.sh [trials] [timeout_secs] [backends]
# backends: comma-separated list, default "ring,aws-lc"

MAGNET="magnet:?xt=urn:btih:a4373c326657898d0c588c3ff892a0fac97ffa20&dn=archlinux-2026.03.01-x86_64.iso"
TRIALS="${1:-3}"
TIMEOUT="${2:-300}"
BACKENDS="${3:-ring,aws-lc}"
OUTPUT_DIR="/tmp/torrent-bench-crypto"
RESULTS="benchmarks/crypto_bench_results.csv"
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"

cd "$PROJECT_DIR"

# --- Cleanup ---
echo "Cleaning up old benchmark state..."
pkill -f "torrent download" 2>/dev/null || true
sleep 1
pkill -9 -f "torrent download" 2>/dev/null || true
rm -rf "$OUTPUT_DIR"
mkdir -p "$OUTPUT_DIR"

echo "client,trial,time_secs,cpu_user_secs,cpu_sys_secs,cpu_total_secs,avg_speed_mbps,peak_rss_kb,voluntary_ctx_switches,involuntary_ctx_switches,page_faults,wall_clock" > "$RESULTS"

parse_time_output() {
    local file="$1"
    local wall_time
    wall_time=$(grep "Elapsed (wall clock)" "$file" | sed 's/.*: //' | awk -F: '{
        if (NF == 3) print $1*3600 + $2*60 + $3;
        else if (NF == 2) print $1*60 + $2;
        else print $1
    }')
    local wall_clock
    wall_clock=$(grep "Elapsed (wall clock)" "$file" | sed 's/.*: //')
    local user_time
    user_time=$(grep "User time" "$file" | sed 's/.*: //')
    local sys_time
    sys_time=$(grep "System time" "$file" | sed 's/.*: //')
    local cpu_time
    cpu_time=$(echo "${user_time:-0} ${sys_time:-0}" | awk '{printf "%.2f", $1 + $2}')
    local rss
    rss=$(grep "Maximum resident" "$file" | sed 's/[^0-9]//g')
    local vol_ctx
    vol_ctx=$(grep "Voluntary context" "$file" | sed 's/[^0-9]//g')
    local invol_ctx
    invol_ctx=$(grep "Involuntary context" "$file" | sed 's/[^0-9]//g')
    local page_faults
    page_faults=$(grep "Minor.*page faults" "$file" | sed 's/[^0-9]//g')
    echo "${wall_time:-0} ${user_time:-0} ${sys_time:-0} ${cpu_time:-0} ${rss:-0} ${vol_ctx:-0} ${invol_ctx:-0} ${page_faults:-0} ${wall_clock:-0}"
}

run_trial() {
    local client="$1"
    local trial="$2"
    local binary="$3"
    local client_dir="$OUTPUT_DIR/$client"

    rm -rf "$client_dir"
    mkdir -p "$client_dir"

    echo "  Trial $trial: $client..."
    local time_file="$OUTPUT_DIR/${client}-time-${trial}.txt"
    local stats_file="$OUTPUT_DIR/${client}-stats-${trial}.txt"

    timeout --signal=TERM --kill-after=10 "$TIMEOUT" \
        /usr/bin/time -v bash -c "'$binary' download '$MAGNET' -o '$client_dir' -q 2>'$stats_file'" 2>"$time_file"
    local exit_code=$?

    if [ "$exit_code" -eq 124 ]; then
        echo "    !! TIMED OUT after ${TIMEOUT}s"
    fi

    read -r wall_time user_time sys_time cpu_time rss vol_ctx invol_ctx page_faults wall_clock <<< "$(parse_time_output "$time_file")"
    local size
    size=$(du -sb "$client_dir" 2>/dev/null | awk '{print $1}')
    local avg_speed
    avg_speed=$(echo "${size:-0} $wall_time" | awk '{if ($2 > 0) printf "%.2f", $1/1048576/$2; else print 0}')

    echo "$client,$trial,$wall_time,$user_time,$sys_time,$cpu_time,$avg_speed,$rss,$vol_ctx,$invol_ctx,$page_faults,$wall_clock" >> "$RESULTS"
    echo "    -> ${wall_time}s wall, CPU ${cpu_time}s (user ${user_time} + sys ${sys_time}), ${avg_speed} MB/s, RSS ${rss} KB"
    echo "    -> ctx switches: ${vol_ctx} voluntary, ${invol_ctx} involuntary, page faults: ${page_faults}"

    pkill -f "torrent download" 2>/dev/null || true
    sleep 2
}

# --- Build backends ---
IFS=',' read -ra BACKEND_LIST <<< "$BACKENDS"
declare -A BINARIES

for backend in "${BACKEND_LIST[@]}"; do
    case "$backend" in
        ring)
            echo ""
            echo "=== Building ring backend (default) ==="
            cargo build --release -p torrent-cli 2>/dev/null
            cp target/release/torrent "$OUTPUT_DIR/torrent-ring"
            BINARIES[ring]="$OUTPUT_DIR/torrent-ring"
            ;;
        aws-lc)
            echo ""
            echo "=== Building aws-lc-rs backend ==="
            cargo build --release -p torrent-cli --features crypto-aws-lc 2>/dev/null
            cp target/release/torrent "$OUTPUT_DIR/torrent-aws-lc"
            BINARIES[aws-lc]="$OUTPUT_DIR/torrent-aws-lc"
            ;;
        openssl)
            echo ""
            echo "=== Building openssl backend ==="
            cargo build --release -p torrent-cli --features crypto-openssl 2>/dev/null
            cp target/release/torrent "$OUTPUT_DIR/torrent-openssl"
            BINARIES[openssl]="$OUTPUT_DIR/torrent-openssl"
            ;;
        *)
            echo "Unknown backend: $backend (valid: ring, aws-lc, openssl)"
            exit 1
            ;;
    esac
done

# Verify linking
echo ""
for backend in "${BACKEND_LIST[@]}"; do
    echo "$backend binary:"
    ldd "${BINARIES[$backend]}" | grep -iE 'ssl|crypto|aws' || echo "  (no shared crypto libs — statically linked)"
    echo ""
done

echo "Magnet: $MAGNET"
echo "Trials: $TRIALS"
echo "Timeout: ${TIMEOUT}s per trial"
echo ""

# --- Run benchmarks ---
for trial in $(seq 1 "$TRIALS"); do
    echo "=== Trial $trial/$TRIALS ==="

    for backend in "${BACKEND_LIST[@]}"; do
        run_trial "$backend" "$trial" "${BINARIES[$backend]}"
    done

    echo ""
done

# --- Summary ---
echo "=== Results ==="
echo ""
column -t -s, "$RESULTS"
echo ""

echo "=== Averages ==="
awk -F, 'NR>1 {
    client=$1; speed[client]+=$7; cpu[client]+=$6; rss[client]+=$8;
    user[client]+=$4; sys[client]+=$5;
    vol[client]+=$9; invol[client]+=$10; faults[client]+=$11;
    n[client]++
}
END {
    for (c in n) {
        printf "%s: %.1f MB/s, CPU %.2fs (user %.2f + sys %.2f), RSS %.0f KB, ctx %d/%d, faults %d (%d trials)\n",
            c, speed[c]/n[c], cpu[c]/n[c], user[c]/n[c], sys[c]/n[c],
            rss[c]/n[c], vol[c]/n[c], invol[c]/n[c], faults[c]/n[c], n[c]
    }
}' "$RESULTS"

echo ""
echo "Results saved to $RESULTS"
