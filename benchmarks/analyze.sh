#!/bin/bash
set -euo pipefail

# ─────────────────────────────────────────────────────────────────────
# analyze.sh — Comprehensive performance analysis for torrent engine
#
# Runs N download trials collecting per-trial metrics and deep profiling:
#   - Per-trial: wall time, speed, CPU, RSS, page faults, ctx switches
#   - Profile trial: flamegraph (CPU), heaptrack (heap), perf stat (HW counters)
#   - Summary: mean, median, stddev, min, max
#
# Usage:
#   ./benchmarks/analyze.sh <magnet_uri> [options]
#
# Options:
#   -n, --trials N         Number of trials (default: 10)
#   -t, --timeout SECS     Timeout per trial in seconds (default: 300)
#   -o, --output DIR       Output directory (default: /mnt/TempNVME/bench-results/TIMESTAMP)
#   -p, --port PORT        Listen port (default: 42020)
#   --no-profile           Skip flamegraph/heaptrack/perf profiling
#   --profile-trial N      Which trial to profile (default: 2, skips cold DHT)
#
# Requirements:
#   - torrent binary at target/release/torrent (run: cargo build --release)
#   - perf, heaptrack, flamegraph (optional, for profiling)
#
# Output structure:
#   <output>/
#     summary.txt           — Human-readable summary
#     results.csv           — Machine-readable per-trial data
#     flamegraph.svg        — CPU flamegraph (from profile trial)
#     heaptrack.txt         — Heap allocation report
#     heaptrack.zst         — Raw heaptrack data (for heaptrack_gui)
#     trial-N/              — Per-trial: time.txt, perf-stat.txt
# ─────────────────────────────────────────────────────────────────────

# ── Defaults ──────────────────────────────────────────────────────────

TRIALS=10
TIMEOUT=300
OUTPUT=""
PORT=42020
RUN_PROFILE=true
PROFILE_TRIAL=2

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TORRENT_BIN="$REPO_DIR/target/release/torrent"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# ── Parse arguments ───────────────────────────────────────────────────

MAGNET=""
while [[ $# -gt 0 ]]; do
    case $1 in
        -n|--trials)     TRIALS="$2"; shift 2 ;;
        -t|--timeout)    TIMEOUT="$2"; shift 2 ;;
        -o|--output)     OUTPUT="$2"; shift 2 ;;
        -p|--port)       PORT="$2"; shift 2 ;;
        --no-profile)    RUN_PROFILE=false; shift ;;
        --profile-trial) PROFILE_TRIAL="$2"; shift 2 ;;
        -h|--help)
            sed -n '/^# Usage:/,/^# ──/p' "$0" | head -n -1 | sed 's/^# //'
            exit 0
            ;;
        magnet:*)        MAGNET="$1"; shift ;;
        *)
            if [[ -z "$MAGNET" ]]; then
                MAGNET="$1"; shift
            else
                echo "Unknown option: $1" >&2; exit 1
            fi
            ;;
    esac
done

if [[ -z "$MAGNET" ]]; then
    echo "Usage: $0 <magnet_uri> [options]" >&2
    echo "Run '$0 --help' for details." >&2
    exit 1
fi

# ── Setup ─────────────────────────────────────────────────────────────

TIMESTAMP=$(date +%Y%m%d-%H%M%S)
if [[ -z "$OUTPUT" ]]; then
    OUTPUT="/mnt/TempNVME/bench-results/$TIMESTAMP"
fi
mkdir -p "$OUTPUT"

DOWNLOAD_DIR="/mnt/TempNVME/bench-dl/torrent"

# Extract torrent name from magnet for display
TORRENT_NAME=$(echo "$MAGNET" | grep -oP 'dn=\K[^&]+' | sed 's/+/ /g; s/%20/ /g' || echo "unknown")

# Check binary exists
if [[ ! -x "$TORRENT_BIN" ]]; then
    echo -e "${RED}Error: $TORRENT_BIN not found. Run: cargo build --release${NC}" >&2
    exit 1
fi

# Check profiling tools
HAS_PERF=$(command -v perf >/dev/null 2>&1 && echo true || echo false)
HAS_HEAPTRACK=$(command -v heaptrack >/dev/null 2>&1 && echo true || echo false)
HAS_FLAMEGRAPH=$(command -v flamegraph >/dev/null 2>&1 && echo true || echo false)

if $RUN_PROFILE; then
    if ! $HAS_PERF; then echo -e "${YELLOW}Warning: perf not found, skipping perf profiling.${NC}"; fi
    if ! $HAS_HEAPTRACK; then echo -e "${YELLOW}Warning: heaptrack not found, skipping heap profiling.${NC}"; fi
    if ! $HAS_FLAMEGRAPH; then echo -e "${YELLOW}Warning: flamegraph not found, skipping flamegraph.${NC}"; fi
fi

# Check disk space (need ~2GB per trial)
REQUIRED_MB=$(( TRIALS * 2048 ))
AVAIL_MB=$(df --output=avail /mnt/TempNVME 2>/dev/null | tail -1 | awk '{printf "%d", $1/1024}')
if [[ "$AVAIL_MB" -lt "$REQUIRED_MB" ]]; then
    echo -e "${RED}Error: Need ~$((REQUIRED_MB/1024))GB free on /mnt/TempNVME, only $((AVAIL_MB/1024))GB available.${NC}" >&2
    exit 1
fi

# Kill any leftover download processes
pkill -f "torrent download" 2>/dev/null || true
sleep 1

# ── Version info ──────────────────────────────────────────────────────

TORRENT_VERSION=$("$TORRENT_BIN" --version 2>&1 | head -1 || echo "unknown")

echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}║  Performance Analysis — ${TORRENT_VERSION}${NC}"
echo -e "${BOLD}╠══════════════════════════════════════════════════════════════╣${NC}"
echo -e "${BOLD}║${NC}  Torrent: ${TORRENT_NAME}"
echo -e "${BOLD}║${NC}  Trials:  ${TRIALS}"
echo -e "${BOLD}║${NC}  Timeout: ${TIMEOUT}s"
echo -e "${BOLD}║${NC}  Profile: $($RUN_PROFILE && echo "trial $PROFILE_TRIAL" || echo "disabled")"
echo -e "${BOLD}║${NC}  Output:  ${OUTPUT}"
echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${NC}"
echo ""

# ── CSV header ────────────────────────────────────────────────────────

CSV="$OUTPUT/results.csv"
echo "trial,wall_secs,speed_mbps,cpu_user_secs,cpu_sys_secs,cpu_total_secs,rss_mib,major_faults,minor_faults,voluntary_ctx,involuntary_ctx,total_ctx" > "$CSV"

# ── Trial runner ──────────────────────────────────────────────────────

run_trial() {
    local trial="$1"
    local do_profile="$2"  # "true" or "false"

    rm -rf "$DOWNLOAD_DIR"
    mkdir -p "$DOWNLOAD_DIR"

    local trial_dir="$OUTPUT/trial-$trial"
    mkdir -p "$trial_dir"

    local time_file="$trial_dir/time.txt"
    local perf_file="$trial_dir/perf-stat.txt"
    local cmd="$TORRENT_BIN download '$MAGNET' -o '$DOWNLOAD_DIR' -q -p $PORT"

    # For the profile trial, run flamegraph + heaptrack first (separate runs)
    # IMPORTANT: invoke the binary directly (not via bash -c) so profilers
    # attach to the torrent process, not a bash wrapper.
    if [[ "$do_profile" == "true" ]]; then
        if $HAS_HEAPTRACK; then
            echo -e "  ${CYAN}[heaptrack]${NC} Running heap profiling..."
            rm -rf "$DOWNLOAD_DIR" && mkdir -p "$DOWNLOAD_DIR"
            local ht_prefix="$trial_dir/heaptrack"
            timeout "$TIMEOUT" heaptrack --record-only -o "$ht_prefix" \
                "$TORRENT_BIN" download "$MAGNET" -o "$DOWNLOAD_DIR" -q -p "$PORT" \
                >/dev/null 2>&1 || true
            local ht_data
            ht_data=$(ls "$ht_prefix"*.zst 2>/dev/null | head -1 || true)
            if [[ -n "$ht_data" ]]; then
                heaptrack_print "$ht_data" > "$OUTPUT/heaptrack.txt" 2>/dev/null || true
                cp "$ht_data" "$OUTPUT/heaptrack.zst" 2>/dev/null || true
                echo -e "  ${GREEN}[heaptrack]${NC} Done → heaptrack.txt"
            fi
        fi

        if $HAS_FLAMEGRAPH; then
            echo -e "  ${CYAN}[flamegraph]${NC} Running CPU profiling..."
            rm -rf "$DOWNLOAD_DIR" && mkdir -p "$DOWNLOAD_DIR"
            timeout "$TIMEOUT" flamegraph -o "$OUTPUT/flamegraph.svg" --freq 997 -- \
                "$TORRENT_BIN" download "$MAGNET" -o "$DOWNLOAD_DIR" -q -p "$PORT" \
                >/dev/null 2>&1 || true
            if [[ -f "$OUTPUT/flamegraph.svg" ]]; then
                echo -e "  ${GREEN}[flamegraph]${NC} Done → flamegraph.svg"
            fi
        fi

        # Clean for the actual timed run
        rm -rf "$DOWNLOAD_DIR" && mkdir -p "$DOWNLOAD_DIR"
    fi

    # Build the timed command: perf stat wrapping the download
    local full_cmd
    if $HAS_PERF; then
        full_cmd="perf stat -e task-clock,context-switches,cpu-migrations,page-faults,cycles,instructions,cache-references,cache-misses,branch-misses -o '$perf_file' timeout $TIMEOUT bash -c \"$cmd\""
    else
        full_cmd="timeout $TIMEOUT bash -c \"$cmd\""
    fi

    /usr/bin/time -v bash -c "$full_cmd" >"$trial_dir/stdout.txt" 2>"$time_file" || true

    # ── Parse /usr/bin/time output ──
    local wall_raw cpu_user cpu_sys rss_kb major_faults minor_faults vol_ctx invol_ctx

    wall_raw=$(grep "wall clock" "$time_file" 2>/dev/null | awk '{print $NF}' || echo "0:00.00")
    cpu_user=$(grep "User time" "$time_file" 2>/dev/null | awk '{print $NF}' || echo "0")
    cpu_sys=$(grep "System time" "$time_file" 2>/dev/null | awk '{print $NF}' || echo "0")
    rss_kb=$(grep "Maximum resident" "$time_file" 2>/dev/null | awk '{print $NF}' || echo "0")
    major_faults=$(grep "Major.*page faults" "$time_file" 2>/dev/null | awk '{print $NF}' || echo "0")
    minor_faults=$(grep "Minor.*page faults" "$time_file" 2>/dev/null | awk '{print $NF}' || echo "0")
    vol_ctx=$(grep "Voluntary context" "$time_file" 2>/dev/null | awk '{print $NF}' || echo "0")
    invol_ctx=$(grep "Involuntary context" "$time_file" 2>/dev/null | awk '{print $NF}' || echo "0")

    # Parse wall time (h:mm:ss.cc or m:ss.cc or ss.cc)
    local wall_secs
    wall_secs=$(echo "$wall_raw" | awk -F: '{
        if (NF==3) printf "%.2f", $1*3600+$2*60+$3
        else if (NF==2) printf "%.2f", $1*60+$2
        else printf "%.2f", $1
    }')

    local cpu_total rss_mib total_ctx
    cpu_total=$(awk "BEGIN {printf \"%.1f\", $cpu_user + $cpu_sys}")
    rss_mib=$(awk "BEGIN {printf \"%.0f\", $rss_kb / 1024}")
    total_ctx=$(( vol_ctx + invol_ctx ))

    # Compute speed from actual download size
    local size_bytes
    size_bytes=$(du -sb "$DOWNLOAD_DIR" 2>/dev/null | awk '{print $1}')
    size_bytes=${size_bytes:-0}

    local speed_mbps
    speed_mbps=$(awk "BEGIN {
        if ($wall_secs > 0 && $size_bytes > 0)
            printf \"%.1f\", $size_bytes / 1048576 / $wall_secs
        else
            printf \"0.0\"
    }")

    # Write CSV row
    echo "$trial,$wall_secs,$speed_mbps,$cpu_user,$cpu_sys,$cpu_total,$rss_mib,$major_faults,$minor_faults,$vol_ctx,$invol_ctx,$total_ctx" >> "$CSV"

    # Display with colour coding
    local color=$NC
    if (( $(awk "BEGIN {print ($speed_mbps >= 50)}") )); then color=$GREEN
    elif (( $(awk "BEGIN {print ($speed_mbps >= 30)}") )); then color=$YELLOW
    else color=$RED; fi

    printf "  Trial %2d: ${color}%6.1f MB/s${NC} | %6.1fs | CPU %5.1fs | RSS %4s MiB | faults %s+%s | ctx %s+%s\n" \
        "$trial" "$speed_mbps" "$wall_secs" "$cpu_total" "$rss_mib" \
        "$major_faults" "$minor_faults" "$vol_ctx" "$invol_ctx"

    # Clean download data between trials
    rm -rf "$DOWNLOAD_DIR"
}

# ── Run trials ────────────────────────────────────────────────────────

for trial in $(seq 1 "$TRIALS"); do
    do_profile="false"
    if $RUN_PROFILE && [[ "$trial" -eq "$PROFILE_TRIAL" ]]; then
        do_profile="true"
    fi
    run_trial "$trial" "$do_profile"
done

echo ""

# ── Generate summary ──────────────────────────────────────────────────

{
    echo "╔══════════════════════════════════════════════════════════════════════════════════════╗"
    echo "║  Performance Analysis Summary — $TORRENT_VERSION"
    echo "║  $(date '+%Y-%m-%d %H:%M')  •  $TORRENT_NAME"
    echo "╠══════════════════════════════════════════════════════════════════════════════════════╣"
    echo ""

    # Compute statistics from CSV
    speeds=$(awk -F, 'NR>1 {print $3}' "$CSV" | sort -n)
    count=$(echo "$speeds" | wc -l)

    mean=$(echo "$speeds" | awk '{s+=$1; n++} END {printf "%.1f", s/n}')
    median=$(echo "$speeds" | awk -v n="$count" 'NR==int((n+1)/2) {printf "%.1f", $1}')
    stddev=$(echo "$speeds" | awk -v m="$mean" '{d=$1-m; s+=d*d; n++} END {printf "%.1f", sqrt(s/n)}')
    min_speed=$(echo "$speeds" | head -1)
    max_speed=$(echo "$speeds" | tail -1)

    avg_cpu=$(awk -F, 'NR>1 {s+=$6; n++} END {printf "%.1f", s/n}' "$CSV")
    avg_rss=$(awk -F, 'NR>1 {s+=$7; n++} END {printf "%.0f", s/n}' "$CSV")
    avg_major=$(awk -F, 'NR>1 {s+=$8; n++} END {printf "%.0f", s/n}' "$CSV")
    avg_minor=$(awk -F, 'NR>1 {s+=$9; n++} END {printf "%.0f", s/n}' "$CSV")
    avg_vol_ctx=$(awk -F, 'NR>1 {s+=$10; n++} END {printf "%.0f", s/n}' "$CSV")
    avg_invol_ctx=$(awk -F, 'NR>1 {s+=$11; n++} END {printf "%.0f", s/n}' "$CSV")
    avg_total_ctx=$(awk -F, 'NR>1 {s+=$12; n++} END {printf "%.0f", s/n}' "$CSV")

    echo "  Speed:"
    echo "    Mean:   ${mean} MB/s"
    echo "    Median: ${median} MB/s"
    echo "    StdDev: ${stddev} MB/s"
    echo "    Min:    ${min_speed} MB/s"
    echo "    Max:    ${max_speed} MB/s"
    echo ""
    echo "  Resources (averages across $count trials):"
    echo "    CPU:             ${avg_cpu}s"
    echo "    RSS:             ${avg_rss} MiB"
    echo "    Major faults:    ${avg_major}"
    echo "    Minor faults:    ${avg_minor}"
    echo "    Ctx switches:    ${avg_total_ctx} (${avg_vol_ctx} voluntary + ${avg_invol_ctx} involuntary)"
    echo ""

    echo "── Per-Trial Data ──"
    echo ""
    printf "  %5s  %8s  %8s  %8s  %8s  %8s  %10s\n" \
        "Trial" "Speed" "Wall" "CPU" "RSS" "Faults" "CtxSw"
    printf "  %5s  %8s  %8s  %8s  %8s  %8s  %10s\n" \
        "-----" "------" "------" "------" "------" "------" "--------"
    awk -F, 'NR>1 {
        printf "  %5d  %6.1f MB  %6.1fs  %6.1fs  %4d MiB  %4d+%d  %d+%d\n",
            $1, $3, $2, $6, $7, $8, $9, $10, $11
    }' "$CSV"

    if $RUN_PROFILE; then
        echo ""
        echo "── Profiling Artifacts ──"
        [[ -f "$OUTPUT/flamegraph.svg" ]] && echo "  Flamegraph:      $OUTPUT/flamegraph.svg"
        [[ -f "$OUTPUT/heaptrack.txt" ]] && echo "  Heaptrack:       $OUTPUT/heaptrack.txt"
        [[ -f "$OUTPUT/heaptrack.zst" ]] && echo "  Heaptrack (raw): $OUTPUT/heaptrack.zst"
        for f in "$OUTPUT"/trial-*/perf-stat.txt; do
            [[ -f "$f" ]] && echo "  Perf stat:       $f"
        done
    fi

    echo ""
    echo "╚══════════════════════════════════════════════════════════════════════════════════════╝"
} | tee "$OUTPUT/summary.txt"

echo ""
echo -e "${GREEN}Results saved to: $OUTPUT/${NC}"
echo -e "  summary.txt   — Human-readable report"
echo -e "  results.csv   — Machine-readable data"
[[ -f "$OUTPUT/flamegraph.svg" ]] && echo -e "  flamegraph.svg — CPU flamegraph"
[[ -f "$OUTPUT/heaptrack.txt" ]] && echo -e "  heaptrack.txt  — Heap allocation report"

# ── Cleanup ───────────────────────────────────────────────────────────

rm -rf /mnt/TempNVME/bench-dl
