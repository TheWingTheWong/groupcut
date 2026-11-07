#!/usr/bin/env bash
# Download Wikimedia hourly pageview dumps into data/.
#
#   scripts/fetch-wiki.sh [YYYYMMDD] [first_hour] [hours]
#
# Defaults reproduce the README: 2026-11-01, 12:00-17:00 UTC (6 files,
# ~45 MB each compressed). Files are kept gzipped; groupcut decompresses them
# on load. Data: https://dumps.wikimedia.org/other/pageviews/ (CC0).
set -euo pipefail

day=${1:-20261101}
first=${2:-12}
hours=${3:-6}
year=${day:0:4}
month=${day:4:2}
dir="$(cd "$(dirname "$0")/.." && pwd)/data"
mkdir -p "$dir"

for ((h = first; h < first + hours; h++)); do
  name=$(printf "pageviews-%s-%02d0000.gz" "$day" "$h")
  out="$dir/$name"
  if [[ -s "$out" ]] && gzip -t "$out" 2>/dev/null; then
    echo "have  $name"
    continue
  fi
  echo "fetch $name"
  curl -fsSL --retry 3 -o "$out.part" \
    "https://dumps.wikimedia.org/other/pageviews/$year/$year-$month/$name"
  gzip -t "$out.part"
  mv "$out.part" "$out"
done
ls -1 "$dir"/pageviews-"$day"-*.gz
