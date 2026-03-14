#!/bin/bash
set -uo pipefail

# Torrent-only profiling: N trials with perf stat (all), flamegraph (trial 1),
# heaptrack (trial 2), network connection logging (all).
# Output: /mnt/TempNVME/bench-results/<timestamp>/

MAGNET="${1:?Usage: $0 <magnet_uri> [trials] [timeout]}"
TRIALS="${2:-10}"
TIMEOUT="${3:-120}"
# Resolve to absolute path so sudo/timeout don't lose it
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
TORRENT="$REPO_ROOT/target/release/torrent"
if [ ! -x "$TORRENT" ]; then
    echo "ERROR: Binary not found at $TORRENT — run 'cargo build --release' first" >&2
    exit 1
fi
BASE_DIR="/mnt/TempNVME/bench-results"
TIMESTAMP=$(date +%Y%m%d-%H%M%S)
RESULTS_DIR="$BASE_DIR/$TIMESTAMP"
DL_BASE="/mnt/TempNVME/bench-dl/torrent"
PORT=42020

mkdir -p "$RESULTS_DIR"

# Pre-flight: ensure benchmark port is free (stale process from killed run)
if ss -tlnp 2>/dev/null | grep -q ":${PORT} "; then
    echo "Port $PORT already in use — killing stale process..."
    stale_pid=$(sudo ss -tlnp 2>/dev/null | grep ":${PORT} " | grep -oP 'pid=\K[0-9]+' | head -1)
    if [ -n "$stale_pid" ]; then
        sudo kill "$stale_pid" 2>/dev/null || true
        sleep 2
        if ss -tlnp 2>/dev/null | grep -q ":${PORT} "; then
            sudo kill -9 "$stale_pid" 2>/dev/null || true
            sleep 1
        fi
    fi
    if ss -tlnp 2>/dev/null | grep -q ":${PORT} "; then
        echo "ERROR: Cannot free port $PORT — aborting" >&2
        exit 1
    fi
    echo "Port $PORT freed."
fi

# Check disk space
AVAIL_GB=$(df --output=avail /mnt/TempNVME 2>/dev/null | tail -1 | awk '{printf "%d", $1/1048576}')
REQUIRED_GB=$(( TRIALS * 2 + 5 ))
if [ "$AVAIL_GB" -lt "$REQUIRED_GB" ]; then
    echo "ERROR: Need ~${REQUIRED_GB}GB free on /mnt/TempNVME, only ${AVAIL_GB}GB available" >&2
    exit 1
fi

CSV="$RESULTS_DIR/results.csv"
echo "trial,wall_secs,user_secs,sys_secs,cpu_total_secs,avg_speed_mbps,peak_rss_kb,voluntary_ctx,involuntary_ctx,page_faults" > "$CSV"

parse_time() {
    local f="$1"
    local wall user sys rss vctx ictx faults
    wall=$(grep "Elapsed (wall clock)" "$f" | sed 's/.*: //' | awk -F: '{
        if (NF == 3) print $1*3600 + $2*60 + $3;
        else if (NF == 2) print $1*60 + $2;
        else print $1}')
    user=$(grep "User time" "$f" | sed 's/.*: //')
    sys=$(grep "System time" "$f" | sed 's/.*: //')
    rss=$(grep "Maximum resident" "$f" | sed 's/[^0-9]//g')
    vctx=$(grep "Voluntary context" "$f" | sed 's/[^0-9]//g')
    ictx=$(grep "Involuntary context" "$f" | sed 's/[^0-9]//g')
    faults=$(grep "Minor.*page faults" "$f" | sed 's/[^0-9]//g')
    echo "${wall:-0} ${user:-0} ${sys:-0} ${rss:-0} ${vctx:-0} ${ictx:-0} ${faults:-0}"
}

cleanup() {
    pgrep -x torrent | xargs -r kill -9 2>/dev/null
    pgrep -f "heaptrack.*torrent" | xargs -r kill -9 2>/dev/null
    pgrep -f "perf.*torrent" | xargs -r kill -9 2>/dev/null
    sleep 2
    # Verify port is free
    if sudo ss -tlnp 2>/dev/null | grep -q ":${PORT} "; then
        local pid
        pid=$(sudo ss -tlnp | grep ":${PORT} " | grep -oP 'pid=\K[0-9]+' | head -1)
        if [ -n "$pid" ]; then
            sudo kill -9 "$pid" 2>/dev/null || true
            sleep 2
        fi
    fi
}

# Ensure cleanup runs on script exit/interrupt (prevents stale port on Ctrl-C)
trap cleanup EXIT INT TERM

echo "=== Torrent Profiling Benchmark ==="
echo "Results: $RESULTS_DIR"
echo "Trials: $TRIALS"
echo "Disk: ${AVAIL_GB}GB available (need ~${REQUIRED_GB}GB)"
echo ""

# Delete DHT state for cold-start consistency on trial 1
rm -f ~/.local/share/torrent/session.dat

pass=0
fail=0

for trial in $(seq 1 "$TRIALS"); do
    out_dir="$RESULTS_DIR/trial-$trial"
    mkdir -p "$out_dir"

    cleanup
    rm -rf "$DL_BASE" && mkdir -p "$DL_BASE"

    time_file="$out_dir/time.txt"
    net_file="$out_dir/network.log"

    echo "=== Trial $trial/$TRIALS ==="

    # Network connection logger (background)
    (while true; do
        echo "$(date +%s.%N) $(ss -tnp 2>/dev/null | grep -c 'torrent' || echo 0) connections"
        sleep 0.5
    done) > "$net_file" 2>/dev/null &
    net_pid=$!

    start_time=$(date +%s)

    if [ "$trial" -eq 1 ]; then
        echo "  [flamegraph + perf stat] (timeout ${TIMEOUT}s)"
        sudo timeout --signal=TERM --kill-after=10 "$TIMEOUT" \
            perf record -g -F 999 -o "$out_dir/perf.data" -- \
            /usr/bin/time -v "$TORRENT" download "$MAGNET" -o "$DL_BASE" -q -p "$PORT" \
            2>"$time_file" || true
        # Generate flamegraph
        if [ -f "$out_dir/perf.data" ]; then
            sudo perf script -i "$out_dir/perf.data" 2>/dev/null | \
                flamegraph --title "torrent trial 1" > "$out_dir/flamegraph.svg" 2>/dev/null || true
            sudo chown alan:alan "$out_dir/perf.data" "$out_dir/flamegraph.svg" 2>/dev/null || true
            # Also generate perf stat from the recording
            sudo perf stat report -i "$out_dir/perf.data" 2>"$out_dir/perf-stat.txt" || true
            sudo chown alan:alan "$out_dir/perf-stat.txt" 2>/dev/null || true
        fi
    elif [ "$trial" -eq 2 ]; then
        echo "  [heaptrack] (timeout ${TIMEOUT}s)"
        DISPLAY= timeout --signal=TERM --kill-after=10 "$TIMEOUT" \
            /usr/bin/time -v heaptrack -o "$out_dir/heaptrack" \
            "$TORRENT" download "$MAGNET" -o "$DL_BASE" -q -p "$PORT" \
            2>"$time_file" || true
    else
        echo "  [perf stat] (timeout ${TIMEOUT}s)"
        # Use software counters (hardware PMU may be unavailable on some kernels)
        sudo timeout --signal=TERM --kill-after=10 "$TIMEOUT" \
            perf stat -e cpu-clock,task-clock,context-switches,cpu-migrations,page-faults \
            -o "$out_dir/perf-stat.txt" -- \
            /usr/bin/time -v "$TORRENT" download "$MAGNET" -o "$DL_BASE" -q -p "$PORT" \
            2>"$time_file" || true
        sudo chown alan:alan "$out_dir/perf-stat.txt" 2>/dev/null || true
    fi

    kill $net_pid 2>/dev/null || true
    wait $net_pid 2>/dev/null || true

    end_time=$(date +%s)
    elapsed=$((end_time - start_time))

    # Parse results
    if [ -f "$time_file" ] && grep -q "Elapsed" "$time_file" 2>/dev/null; then
        read -r wall user sys rss vctx ictx faults <<< "$(parse_time "$time_file")"
        cpu=$(echo "$user $sys" | awk '{printf "%.2f", $1+$2}')
        size=$(du -sb "$DL_BASE" 2>/dev/null | awk '{print $1}')
        speed=$(echo "${size:-0} $wall" | awk '{if($2>0) printf "%.2f", $1/1048576/$2; else print 0}')

        echo "$trial,$wall,$user,$sys,$cpu,$speed,$rss,$vctx,$ictx,$faults" >> "$CSV"

        if [ "${size:-0}" -gt 1048576 ]; then
            echo "  -> ${wall}s, ${speed} MB/s, CPU ${cpu}s, RSS ${rss}KB"
            pass=$((pass + 1))
        else
            echo "  -> STALL: ${elapsed}s elapsed, 0 bytes downloaded"
            fail=$((fail + 1))
        fi
    else
        echo "  -> ERROR: no time output (process may have crashed)"
        echo "$trial,0,0,0,0,0,0,0,0,0" >> "$CSV"
        fail=$((fail + 1))
    fi

    rm -rf "$DL_BASE"
    sleep 3
done

echo ""
echo "=== Summary ==="
echo "Passed: $pass/$TRIALS, Failed: $fail/$TRIALS"
echo ""
cat "$CSV"
echo ""

# Compute averages (skip failed trials)
python3 -c "
import csv, statistics
speeds, cpus, rss_vals, walls, vctx_vals = [], [], [], [], []
with open('$CSV') as f:
    reader = csv.DictReader(f)
    for row in reader:
        s = float(row['avg_speed_mbps'])
        if s > 0:  # skip failed trials
            speeds.append(s)
            cpus.append(float(row['cpu_total_secs']))
            rss_vals.append(float(row['peak_rss_kb'])/1024)
            walls.append(float(row['wall_secs']))
            vctx_vals.append(float(row['voluntary_ctx']))

if len(speeds) >= 2:
    print(f'Speed: {statistics.mean(speeds):.1f} ± {statistics.stdev(speeds):.1f} MB/s  (min={min(speeds):.1f}, max={max(speeds):.1f})')
    print(f'Wall:  {statistics.mean(walls):.1f} ± {statistics.stdev(walls):.1f}s')
    print(f'CPU:   {statistics.mean(cpus):.1f} ± {statistics.stdev(cpus):.1f}s')
    print(f'RSS:   {statistics.mean(rss_vals):.0f} ± {statistics.stdev(rss_vals):.0f} MB')
    cv = statistics.stdev(speeds)/statistics.mean(speeds)*100
    print(f'CV:    {cv:.1f}% (coefficient of variation)')
    print(f'Successful trials: {len(speeds)}')
elif len(speeds) == 1:
    print(f'Speed: {speeds[0]:.1f} MB/s (only 1 successful trial)')
else:
    print('No successful trials!')
"

echo ""
echo "Results saved to: $RESULTS_DIR"
[ -f "$RESULTS_DIR/trial-1/flamegraph.svg" ] && echo "Flamegraph: $RESULTS_DIR/trial-1/flamegraph.svg"
[ -d "$RESULTS_DIR/trial-2" ] && ls "$RESULTS_DIR/trial-2"/heaptrack* 2>/dev/null | head -1 | xargs -I{} echo "Heaptrack: {}"
echo "Perf stat: $RESULTS_DIR/trial-*/perf-stat.txt"
