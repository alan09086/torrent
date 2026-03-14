#!/bin/bash
set -euo pipefail

# Comprehensive profiling benchmark: torrent vs rqbit
# Runs N trials with perf stat, flamegraph (trial 1), heaptrack (trial 2),
# and network connection logging on every trial.
# Output: /mnt/TempNVME/bench-results/

MAGNET="${1:?Usage: $0 <magnet_uri> [trials]}"
TRIALS="${2:-10}"
BASE_DIR="/mnt/TempNVME/bench-results"
TORRENT="target/release/torrent"
TIMESTAMP=$(date +%Y%m%d-%H%M%S)
RESULTS_DIR="$BASE_DIR/$TIMESTAMP"

mkdir -p "$RESULTS_DIR"

# Check disk space (need ~2GB per trial)
AVAIL_GB=$(df --output=avail /mnt/TempNVME 2>/dev/null | tail -1 | awk '{printf "%d", $1/1048576}')
REQUIRED_GB=$(( TRIALS * 2 * 2 + 5 ))
if [ "$AVAIL_GB" -lt "$REQUIRED_GB" ]; then
    echo "ERROR: Need ~${REQUIRED_GB}GB free on /mnt/TempNVME, only ${AVAIL_GB}GB available" >&2
    exit 1
fi
echo "Disk: ${AVAIL_GB}GB available (need ~${REQUIRED_GB}GB)"

TORRENT_PORT=42020

# Kill any stale processes and verify port is free before each trial
kill_stale() {
    local client="$1"
    pkill -9 -f "torrent download" 2>/dev/null || true
    pkill -9 -f "rqbit download" 2>/dev/null || true
    pkill -9 -f "heaptrack.*torrent" 2>/dev/null || true
    pkill -9 -f "heaptrack.*rqbit" 2>/dev/null || true
    sleep 2
    # Verify port is free
    local port="$TORRENT_PORT"
    [ "$client" = "rqbit" ] && port=4240
    if ss -tlnp | grep -q ":${port} "; then
        echo "    WARNING: port $port still in use, waiting..."
        sleep 5
        pkill -9 -f "$client" 2>/dev/null || true
        sleep 2
    fi
}

CSV="$RESULTS_DIR/results.csv"
echo "client,trial,wall_secs,user_secs,sys_secs,cpu_total_secs,avg_speed_mbps,peak_rss_kb,voluntary_ctx,involuntary_ctx,page_faults" > "$CSV"

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

run_torrent_trial() {
    local trial=$1
    local out_dir="$RESULTS_DIR/torrent-trial-$trial"
    rm -rf "$out_dir" && mkdir -p "$out_dir"

    kill_stale "torrent"

    local dl_dir="/mnt/TempNVME/bench-dl/torrent"
    rm -rf "$dl_dir" && mkdir -p "$dl_dir"

    local time_file="$out_dir/time.txt"
    local net_file="$out_dir/network.log"

    echo "  torrent trial $trial..."

    # Start network connection logger in background
    (while true; do
        echo "$(date +%s.%N) $(ss -tnp 2>/dev/null | grep -c 'torrent' || echo 0) connections"
        sleep 0.5
    done) > "$net_file" 2>/dev/null &
    local net_pid=$!

    # Trial 1: also capture flamegraph
    if [ "$trial" -eq 1 ]; then
        echo "    (+ flamegraph capture)"
        sudo perf record -g -F 999 -o "$out_dir/perf.data" -- \
            /usr/bin/time -v "$TORRENT" download "$MAGNET" -o "$dl_dir" -q -p 42020 \
            2>"$time_file"
        # Generate flamegraph
        sudo perf script -i "$out_dir/perf.data" | flamegraph --title "torrent v0.91.0 trial 1" > "$out_dir/flamegraph.svg" 2>/dev/null || true
        sudo chown alan:alan "$out_dir/perf.data" "$out_dir/flamegraph.svg" 2>/dev/null || true
    # Trial 2: heaptrack
    elif [ "$trial" -eq 2 ]; then
        echo "    (+ heaptrack capture)"
        /usr/bin/time -v heaptrack -o "$out_dir/heaptrack" \
            "$TORRENT" download "$MAGNET" -o "$dl_dir" -q -p 42020 \
            2>"$time_file"
    # Trial 3+: perf stat only
    else
        sudo perf stat -d -o "$out_dir/perf-stat.txt" -- \
            /usr/bin/time -v "$TORRENT" download "$MAGNET" -o "$dl_dir" -q -p 42020 \
            2>"$time_file"
        sudo chown alan:alan "$out_dir/perf-stat.txt" 2>/dev/null || true
    fi

    kill $net_pid 2>/dev/null || true
    wait $net_pid 2>/dev/null || true

    read -r wall user sys rss vctx ictx faults <<< "$(parse_time "$time_file")"
    local cpu=$(echo "$user $sys" | awk '{printf "%.2f", $1+$2}')
    local size=$(du -sb "$dl_dir" 2>/dev/null | awk '{print $1}')
    local speed=$(echo "${size:-0} $wall" | awk '{if($2>0) printf "%.2f", $1/1048576/$2; else print 0}')

    echo "torrent,$trial,$wall,$user,$sys,$cpu,$speed,$rss,$vctx,$ictx,$faults" >> "$CSV"
    echo "    -> ${wall}s, CPU ${cpu}s, ${speed} MB/s, RSS ${rss}KB, ctx=${vctx}v/${ictx}i"

    rm -rf "$dl_dir"
    sleep 3
}

run_rqbit_trial() {
    local trial=$1
    local out_dir="$RESULTS_DIR/rqbit-trial-$trial"
    rm -rf "$out_dir" && mkdir -p "$out_dir"

    kill_stale "rqbit"

    local dl_dir="/mnt/TempNVME/bench-dl/rqbit"
    rm -rf "$dl_dir" && mkdir -p "$dl_dir"

    local time_file="$out_dir/time.txt"
    local net_file="$out_dir/network.log"

    echo "  rqbit trial $trial..."

    # Network logger
    (while true; do
        echo "$(date +%s.%N) $(ss -tnp 2>/dev/null | grep -c 'rqbit' || echo 0) connections"
        sleep 0.5
    done) > "$net_file" 2>/dev/null &
    local net_pid=$!

    if [ "$trial" -eq 1 ]; then
        echo "    (+ flamegraph capture)"
        sudo perf record -g -F 999 -o "$out_dir/perf.data" -- \
            /usr/bin/time -v rqbit download "$MAGNET" -o "$dl_dir" -e \
            2>"$time_file"
        sudo perf script -i "$out_dir/perf.data" | flamegraph --title "rqbit trial 1" > "$out_dir/flamegraph.svg" 2>/dev/null || true
        sudo chown alan:alan "$out_dir/perf.data" "$out_dir/flamegraph.svg" 2>/dev/null || true
    elif [ "$trial" -eq 2 ]; then
        echo "    (+ heaptrack capture)"
        /usr/bin/time -v heaptrack -o "$out_dir/heaptrack" \
            rqbit download "$MAGNET" -o "$dl_dir" -e \
            2>"$time_file"
    else
        sudo perf stat -d -o "$out_dir/perf-stat.txt" -- \
            /usr/bin/time -v rqbit download "$MAGNET" -o "$dl_dir" -e \
            2>"$time_file"
        sudo chown alan:alan "$out_dir/perf-stat.txt" 2>/dev/null || true
    fi

    kill $net_pid 2>/dev/null || true
    wait $net_pid 2>/dev/null || true

    read -r wall user sys rss vctx ictx faults <<< "$(parse_time "$time_file")"
    local cpu=$(echo "$user $sys" | awk '{printf "%.2f", $1+$2}')
    local size=$(du -sb "$dl_dir" 2>/dev/null | awk '{print $1}')
    local speed=$(echo "${size:-0} $wall" | awk '{if($2>0) printf "%.2f", $1/1048576/$2; else print 0}')

    echo "rqbit,$trial,$wall,$user,$sys,$cpu,$speed,$rss,$vctx,$ictx,$faults" >> "$CSV"
    echo "    -> ${wall}s, CPU ${cpu}s, ${speed} MB/s, RSS ${rss}KB, ctx=${vctx}v/${ictx}i"

    rm -rf "$dl_dir"
    sleep 3
}

echo "=== Profiling Benchmark v0.91.0 ==="
echo "Results: $RESULTS_DIR"
echo "Trials: $TRIALS per client"
echo "Output: /mnt/TempNVME/bench-dl/"
echo ""

echo "=== TORRENT (${TRIALS} trials) ==="
for trial in $(seq 1 "$TRIALS"); do
    run_torrent_trial "$trial"
done

echo ""
echo "=== RQBIT (${TRIALS} trials) ==="
for trial in $(seq 1 "$TRIALS"); do
    run_rqbit_trial "$trial"
done

echo ""
echo "=== Summary ==="
cat "$CSV"
echo ""

# Compute averages
python3 -c "
import csv, statistics
data = {}
with open('$CSV') as f:
    reader = csv.DictReader(f)
    for row in reader:
        client = row['client']
        if client not in data:
            data[client] = {'speeds': [], 'cpus': [], 'rss': [], 'walls': [], 'vctx': [], 'ictx': []}
        data[client]['speeds'].append(float(row['avg_speed_mbps']))
        data[client]['cpus'].append(float(row['cpu_total_secs']))
        data[client]['rss'].append(float(row['peak_rss_kb'])/1024)
        data[client]['walls'].append(float(row['wall_secs']))
        data[client]['vctx'].append(float(row['voluntary_ctx']))
        data[client]['ictx'].append(float(row['involuntary_ctx']))

for client, d in sorted(data.items()):
    speeds = d['speeds']
    cpus = d['cpus']
    rss = d['rss']
    walls = d['walls']
    vctx = d['vctx']
    print(f'')
    print(f'{client}:')
    print(f'  Speed: {statistics.mean(speeds):.1f} ± {statistics.stdev(speeds):.1f} MB/s  (min={min(speeds):.1f}, max={max(speeds):.1f})')
    print(f'  Wall:  {statistics.mean(walls):.1f} ± {statistics.stdev(walls):.1f}s')
    print(f'  CPU:   {statistics.mean(cpus):.1f} ± {statistics.stdev(cpus):.1f}s')
    print(f'  RSS:   {statistics.mean(rss):.0f} ± {statistics.stdev(rss):.0f} MB')
    print(f'  Ctx:   {statistics.mean(vctx):.0f} voluntary, {statistics.mean(d[\"ictx\"]):.0f} involuntary')
    cv = statistics.stdev(speeds)/statistics.mean(speeds)*100 if statistics.mean(speeds) > 0 else 0
    print(f'  CV:    {cv:.1f}% (coefficient of variation)')
"

echo ""
echo "Results saved to: $RESULTS_DIR"
echo "Flamegraphs: $RESULTS_DIR/*/flamegraph.svg"
echo "Heaptrack: $RESULTS_DIR/*/heaptrack*"
