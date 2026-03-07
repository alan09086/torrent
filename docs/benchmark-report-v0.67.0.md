---
title: Benchmark Report
version: 0.67.0
date: 2026-03-07
---

# Benchmark Report v0.67.0

## Summary

| Client | Time (s) | Speed (MB/s) | CPU (s) | RSS (MiB) |
|--------|----------|--------------|---------|-----------|
| qbittorrent | 62.0 +/- 16.1 | 24.9 +/- 7.0 | 18.8 +/- 4.5 | 1521.7 +/- 7.5 |
| rqbit | 33.2 +/- 24.0 | 57.9 +/- 26.0 | 10.3 +/- 2.4 | 37.3 +/- 2.0 |
| torrent | 54.2 +/- 10.1 | 27.6 +/- 5.5 | 27.5 +/- 3.1 | 82.1 +/- 6.9 |

## Relative Performance

- **torrent vs qbittorrent**: 1.1x faster
- **torrent vs rqbit**: 1.6x slower

## Per-Trial Data

| Client | Trial | Time (s) | CPU (s) | Speed (MB/s) | RSS (MiB) |
|--------|-------|----------|---------|--------------|-----------|
| torrent | 1 | 59.86 | 30.68 | 24.25 | 85.0 |
| rqbit | 1 | 34.56 | 13.47 | 42.00 | 38.3 |
| qbittorrent | 1 | 67.11 | 20.11 | 21.63 | 1525.1 |
| torrent | 2 | 45.73 | 22.93 | 31.74 | 76.2 |
| rqbit | 2 | 18.87 | 9.32 | 76.93 | 38.6 |
| qbittorrent | 2 | 43.89 | 13.62 | 33.07 | 1533.0 |
| torrent | 3 | 59.29 | 27.48 | 24.48 | 88.6 |
| rqbit | 3 | 19.34 | 8.21 | 75.06 | 33.8 |
| qbittorrent | 3 | 76.81 | 22.07 | 18.90 | 1515.6 |
| torrent | 4 | 41.23 | 26.70 | 35.21 | 73.4 |
| rqbit | 4 | 19.08 | 8.24 | 76.08 | 37.3 |
| qbittorrent | 4 | 76.12 | 23.58 | 19.07 | 1515.3 |
| torrent | 5 | 64.84 | 29.89 | 22.39 | 87.3 |
| rqbit | 5 | 74.38 | 12.23 | 19.52 | 38.3 |
| qbittorrent | 5 | 45.84 | 14.44 | 31.67 | 1519.5 |

## Per-Client Details

### qbittorrent

- **Trials**: 5
- **Time**: 62.0 +/- 16.1 s
- **Speed**: 24.9 +/- 7.0 MB/s
- **CPU**: 18.8 +/- 4.5 s
- **RSS**: 1521.7 +/- 7.5 MiB

### rqbit

- **Trials**: 5
- **Time**: 33.2 +/- 24.0 s
- **Speed**: 57.9 +/- 26.0 MB/s
- **CPU**: 10.3 +/- 2.4 s
- **RSS**: 37.3 +/- 2.0 MiB

### torrent

- **Trials**: 5
- **Time**: 54.2 +/- 10.1 s
- **Speed**: 27.6 +/- 5.5 MB/s
- **CPU**: 27.5 +/- 3.1 s
- **RSS**: 82.1 +/- 6.9 MiB

## Changes Since Previous Version

(to be filled manually)
