#!/usr/bin/env python3
"""Generate a markdown benchmark report from JSON stats."""
import argparse
import csv
import json
import sys
from datetime import date


def load_json(path):
    with open(path) as f:
        return json.load(f)


def load_csv(path):
    """Load raw per-trial data from CSV."""
    rows = []
    with open(path) as f:
        for row in csv.DictReader(f):
            rows.append(row)
    return rows


def fmt_val(mean, sd):
    """Format a value as 'mean +/- sd'."""
    return f"{mean:.1f} +/- {sd:.1f}"


def relative_perf(base_time, other_time):
    """Return relative performance string (e.g., '2.5x slower')."""
    if other_time == 0:
        return "N/A"
    ratio = base_time / other_time
    if ratio > 1.0:
        return f"{ratio:.1f}x slower"
    elif ratio < 1.0:
        return f"{1/ratio:.1f}x faster"
    else:
        return "equal"


def build_report(stats, version, csv_rows=None):
    today = date.today().isoformat()
    clients = stats["clients"]

    lines = []

    # YAML front matter
    lines.append("---")
    lines.append(f"title: Benchmark Report")
    lines.append(f"version: {version}")
    lines.append(f"date: {today}")
    lines.append("---")
    lines.append("")

    # Title
    lines.append(f"# Benchmark Report v{version}")
    lines.append("")

    # Summary table
    lines.append("## Summary")
    lines.append("")
    lines.append("| Client | Time (s) | Speed (MB/s) | CPU (s) | RSS (MiB) |")
    lines.append("|--------|----------|--------------|---------|-----------|")
    for name in sorted(clients):
        c = clients[name]
        lines.append(
            f"| {name} "
            f"| {fmt_val(c['time_mean'], c['time_sd'])} "
            f"| {fmt_val(c['speed_mean'], c['speed_sd'])} "
            f"| {fmt_val(c['cpu_mean'], c['cpu_sd'])} "
            f"| {fmt_val(c['rss_mean'], c['rss_sd'])} |"
        )
    lines.append("")

    # Relative performance
    if "torrent" in clients:
        lines.append("## Relative Performance")
        lines.append("")
        torrent_time = clients["torrent"]["time_mean"]
        for name in sorted(clients):
            if name == "torrent":
                continue
            other_time = clients[name]["time_mean"]
            perf = relative_perf(torrent_time, other_time)
            lines.append(f"- **torrent vs {name}**: {perf}")
        lines.append("")

    # Per-trial data
    if csv_rows:
        lines.append("## Per-Trial Data")
        lines.append("")
        lines.append("| Client | Trial | Time (s) | CPU (s) | Speed (MB/s) | RSS (MiB) |")
        lines.append("|--------|-------|----------|---------|--------------|-----------|")
        for row in csv_rows:
            rss_mib = float(row["peak_rss_kb"]) / 1024
            lines.append(
                f"| {row['client']} "
                f"| {row['trial']} "
                f"| {row['time_secs']} "
                f"| {row['cpu_secs']} "
                f"| {row['avg_speed_mbps']} "
                f"| {rss_mib:.1f} |"
            )
        lines.append("")

    # Per-client details
    lines.append("## Per-Client Details")
    lines.append("")
    for name in sorted(clients):
        c = clients[name]
        lines.append(f"### {name}")
        lines.append("")
        lines.append(f"- **Trials**: {c['trials']}")
        lines.append(f"- **Time**: {fmt_val(c['time_mean'], c['time_sd'])} s")
        lines.append(f"- **Speed**: {fmt_val(c['speed_mean'], c['speed_sd'])} MB/s")
        lines.append(f"- **CPU**: {fmt_val(c['cpu_mean'], c['cpu_sd'])} s")
        lines.append(f"- **RSS**: {fmt_val(c['rss_mean'], c['rss_sd'])} MiB")
        lines.append("")

    # Changes placeholder
    lines.append("## Changes Since Previous Version")
    lines.append("")
    lines.append("(to be filled manually)")
    lines.append("")

    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description="Generate markdown benchmark report.")
    parser.add_argument("--json-file", required=True,
                        help="Path to JSON stats file (from summarize.py --json)")
    parser.add_argument("--version", required=True,
                        help="Version string (e.g., 0.67.0)")
    parser.add_argument("--csv", default=None,
                        help="Path to raw CSV for per-trial data")
    args = parser.parse_args()

    stats = load_json(args.json_file)

    csv_rows = None
    if args.csv:
        csv_rows = load_csv(args.csv)

    report = build_report(stats, args.version, csv_rows)
    print(report, end="")


if __name__ == "__main__":
    main()
