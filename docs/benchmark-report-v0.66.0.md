---
title: Benchmark Report
version: 0.66.0
date: 2026-03-07
---

# Benchmark Report v0.66.0

## Summary

| Client | Time (s) | Speed (MB/s) | CPU (s) | RSS (MiB) |
|--------|----------|--------------|---------|-----------|
| qbittorrent | 51.5 +/- 10.7 | 29.1 +/- 5.3 | 18.1 +/- 2.5 | 1525.5 +/- 5.5 |
| rqbit | 34.4 +/- 24.1 | 56.7 +/- 27.0 | 10.4 +/- 0.6 | 37.5 +/- 3.9 |
| torrent | 100.3 +/- 19.3 | 15.0 +/- 3.5 | 37.1 +/- 4.7 | 170.2 +/- 42.9 |

## Relative Performance

- **torrent vs qbittorrent**: 1.9x slower
- **torrent vs rqbit**: 2.9x slower

## Per-Trial Data

| Client | Trial | Time (s) | CPU (s) | Speed (MB/s) | RSS (MiB) |
|--------|-------|----------|---------|--------------|-----------|
| torrent | 1 | 117.67 | 39.72 | 12.34 | 154.8 |
| rqbit | 1 | 18.55 | 10.74 | 78.25 | 39.9 |
| qbittorrent | 1 | 43.99 | 18.90 | 33.00 | 1534.6 |
| torrent | 2 | 100.11 | 39.35 | 14.50 | 184.1 |
| rqbit | 2 | 19.73 | 9.48 | 73.57 | 32.5 |
| qbittorrent | 2 | 49.83 | 17.30 | 29.13 | 1524.9 |
| torrent | 3 | 115.63 | 35.85 | 12.55 | 135.4 |
| rqbit | 3 | 19.14 | 10.06 | 75.84 | 34.1 |
| qbittorrent | 3 | 41.84 | 14.69 | 34.69 | 1525.7 |
| torrent | 4 | 98.61 | 41.26 | 14.72 | 238.6 |
| rqbit | 4 | 40.1 | 10.80 | 36.20 | 39.9 |
| qbittorrent | 4 | 68.9 | 21.58 | 21.07 | 1521.1 |
| torrent | 5 | 69.37 | 29.51 | 20.93 | 138.2 |
| rqbit | 5 | 74.36 | 10.76 | 19.52 | 41.1 |
| qbittorrent | 5 | 52.81 | 18.11 | 27.49 | 1521.1 |

## Per-Client Details

### qbittorrent

- **Trials**: 5
- **Time**: 51.5 +/- 10.7 s
- **Speed**: 29.1 +/- 5.3 MB/s
- **CPU**: 18.1 +/- 2.5 s
- **RSS**: 1525.5 +/- 5.5 MiB

### rqbit

- **Trials**: 5
- **Time**: 34.4 +/- 24.1 s
- **Speed**: 56.7 +/- 27.0 MB/s
- **CPU**: 10.4 +/- 0.6 s
- **RSS**: 37.5 +/- 3.9 MiB

### torrent

- **Trials**: 5
- **Time**: 100.3 +/- 19.3 s
- **Speed**: 15.0 +/- 3.5 MB/s
- **CPU**: 37.1 +/- 4.7 s
- **RSS**: 170.2 +/- 42.9 MiB

## Changes Since Previous Version

(to be filled manually)
