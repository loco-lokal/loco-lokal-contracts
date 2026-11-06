#!/usr/bin/env bash
# Downloads a pinned, checksum-verified libpdfium build into rust/vendor/pdfium/.
# The Rust `pdf` crate loads it at runtime via PDFIUM_LIB_DIR (defaults to this folder).
set -euo pipefail

RELEASE="chromium/8138"
BASE_URL="https://github.com/bblanchon/pdfium-binaries/releases/download/${RELEASE}"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)  ASSET=pdfium-mac-arm64.tgz;   SHA=21a1567b397b571f4f77f21ca91eac7b892774219c3c715a5127a20950ccb18b ;;
  Darwin-x86_64) ASSET=pdfium-mac-x64.tgz;     SHA=72662ac8eff7bf5381ef82ceebd0a0ad5bddb051b303981115114e6972fd201c ;;
  Linux-x86_64)  ASSET=pdfium-linux-x64.tgz;   SHA=d6b1e28950a0392590d79bbdbb7eb19686872b688f8a36f170acb19e41473113 ;;
  Linux-aarch64) ASSET=pdfium-linux-arm64.tgz; SHA=c70b1ce4d13257f741af19e0aeee575890875995eb81370a65be942e601a1862 ;;
  *) echo "Unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

DEST="$(cd "$(dirname "$0")/.." && pwd)/vendor/pdfium"
STAMP="$DEST/.release"

if [[ -f "$STAMP" && "$(cat "$STAMP")" == "$RELEASE/$ASSET" ]]; then
  echo "pdfium $RELEASE already present in $DEST"
  exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Downloading $ASSET ($RELEASE)..."
curl -fsSL -o "$TMP/$ASSET" "$BASE_URL/$ASSET"

if command -v sha256sum >/dev/null; then
  echo "$SHA  $TMP/$ASSET" | sha256sum -c -
else
  echo "$SHA  $TMP/$ASSET" | shasum -a 256 -c -
fi

rm -rf "$DEST"
mkdir -p "$DEST"
tar -xzf "$TMP/$ASSET" -C "$DEST"
echo "$RELEASE/$ASSET" > "$STAMP"
echo "pdfium installed to $DEST/lib"
