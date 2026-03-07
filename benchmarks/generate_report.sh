#!/bin/bash
set -euo pipefail

VERSION="${1:?Usage: $0 <version> [csv_path]}"
CSV="${2:-benchmarks/results.csv}"
JSON_TMP=$(mktemp)

python3 benchmarks/summarize.py "$CSV" --json > "$JSON_TMP"
python3 benchmarks/build_report.py --json-file "$JSON_TMP" --version "$VERSION" --csv "$CSV" > "docs/benchmark-report-v${VERSION}.md"
rm -f "$JSON_TMP"

echo "Report generated: docs/benchmark-report-v${VERSION}.md"
