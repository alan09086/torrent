#!/usr/bin/env python3
"""Summarize benchmark results from CSV."""
import csv
import sys
from collections import defaultdict
from math import sqrt

def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "benchmarks/results.csv"

    data = defaultdict(lambda: {"time": [], "speed": [], "rss": []})
    with open(path) as f:
        for row in csv.DictReader(f):
            c = row["client"]
            data[c]["time"].append(float(row["time_secs"]))
            data[c]["speed"].append(float(row["avg_speed_mbps"]))
            data[c]["rss"].append(float(row["peak_rss_kb"]) / 1024)  # to MiB

    def stats(vals):
        n = len(vals)
        if n == 0:
            return 0, 0
        mean = sum(vals) / n
        if n < 2:
            return mean, 0
        sd = sqrt(sum((x - mean) ** 2 for x in vals) / (n - 1))
        return mean, sd

    print(f"{'Client':<12} {'Time (s)':>12} {'Speed (MB/s)':>14} {'RSS (MiB)':>12}")
    print("-" * 52)
    for client in sorted(data):
        t_mean, t_sd = stats(data[client]["time"])
        s_mean, s_sd = stats(data[client]["speed"])
        r_mean, r_sd = stats(data[client]["rss"])
        print(
            f"{client:<12} "
            f"{t_mean:>7.1f} +/-{t_sd:<4.1f} "
            f"{s_mean:>9.1f} +/-{s_sd:<4.1f} "
            f"{r_mean:>7.1f} +/-{r_sd:<4.1f}"
        )

if __name__ == "__main__":
    main()
