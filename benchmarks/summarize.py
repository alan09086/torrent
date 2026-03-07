#!/usr/bin/env python3
"""Summarize benchmark results from CSV."""
import argparse
import csv
import json
import sys
from collections import defaultdict
from math import sqrt


def parse_csv(path):
    data = defaultdict(lambda: {"time": [], "cpu": [], "speed": [], "rss": []})
    with open(path) as f:
        for row in csv.DictReader(f):
            c = row["client"]
            data[c]["time"].append(float(row["time_secs"]))
            data[c]["cpu"].append(float(row.get("cpu_secs", 0)))
            data[c]["speed"].append(float(row["avg_speed_mbps"]))
            data[c]["rss"].append(float(row["peak_rss_kb"]) / 1024)  # to MiB
    return data


def stats(vals):
    n = len(vals)
    if n == 0:
        return 0, 0
    mean = sum(vals) / n
    if n < 2:
        return mean, 0
    sd = sqrt(sum((x - mean) ** 2 for x in vals) / (n - 1))
    return mean, sd


def print_table(data):
    print(f"{'Client':<14} {'Time (s)':>12} {'CPU (s)':>12} {'Speed (MB/s)':>14} {'RSS (MiB)':>12}")
    print("-" * 66)
    for client in sorted(data):
        t_mean, t_sd = stats(data[client]["time"])
        c_mean, c_sd = stats(data[client]["cpu"])
        s_mean, s_sd = stats(data[client]["speed"])
        r_mean, r_sd = stats(data[client]["rss"])
        print(
            f"{client:<14} "
            f"{t_mean:>7.1f} +/-{t_sd:<4.1f} "
            f"{c_mean:>7.1f} +/-{c_sd:<4.1f} "
            f"{s_mean:>9.1f} +/-{s_sd:<4.1f} "
            f"{r_mean:>7.1f} +/-{r_sd:<4.1f}"
        )


def print_json(data):
    result = {"clients": {}}
    for client in sorted(data):
        t_mean, t_sd = stats(data[client]["time"])
        c_mean, c_sd = stats(data[client]["cpu"])
        s_mean, s_sd = stats(data[client]["speed"])
        r_mean, r_sd = stats(data[client]["rss"])
        result["clients"][client] = {
            "time_mean": round(t_mean, 1),
            "time_sd": round(t_sd, 1),
            "cpu_mean": round(c_mean, 1),
            "cpu_sd": round(c_sd, 1),
            "speed_mean": round(s_mean, 1),
            "speed_sd": round(s_sd, 1),
            "rss_mean": round(r_mean, 1),
            "rss_sd": round(r_sd, 1),
            "trials": len(data[client]["time"]),
        }
    json.dump(result, sys.stdout, indent=2)
    print()


def main():
    parser = argparse.ArgumentParser(description="Summarize benchmark results from CSV.")
    parser.add_argument("csv_path", nargs="?", default="benchmarks/results.csv",
                        help="Path to results CSV (default: benchmarks/results.csv)")
    parser.add_argument("--json", action="store_true",
                        help="Output JSON instead of table")
    args = parser.parse_args()

    data = parse_csv(args.csv_path)

    if args.json:
        print_json(data)
    else:
        print_table(data)


if __name__ == "__main__":
    main()
