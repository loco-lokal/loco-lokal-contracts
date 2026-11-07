#!/usr/bin/env bash
# Downloads the fonts the Dockerfile installs in /fonts into rust/vendor/fonts/.
# The `pdf` crate reads them from FONTS_DIR (defaults to /fonts, then this folder).
set -euo pipefail

DEST="$(cd "$(dirname "$0")/.." && pwd)/vendor/fonts"
mkdir -p "$DEST"

fetch() {
  local name=$1 url=$2 sha=$3
  if [[ -f "$DEST/$name" ]] && echo "$sha  $DEST/$name" | shasum -a 256 -c --status; then
    return
  fi
  echo "Downloading $name..."
  curl -fsSL -o "$DEST/$name.tmp" "$url"
  echo "$sha  $DEST/$name.tmp" | shasum -a 256 -c --status || { echo "checksum mismatch for $name" >&2; exit 1; }
  mv "$DEST/$name.tmp" "$DEST/$name"
}

BASE=https://github.com/satbyy/go-noto-universal/releases/download/v7.0
fetch GoNotoKurrent-Regular.ttf "$BASE/GoNotoKurrent-Regular.ttf" 2f2cee5fbb2403df352ca2005247f6c4faa70f3086ebd31b6c62308b5f2f9865
fetch GoNotoKurrent-Bold.ttf "$BASE/GoNotoKurrent-Bold.ttf" 6f5ab7b16acedeac76625f75159d9d303be85b291d47376760a1c9f87802853e
echo "fonts in $DEST"
