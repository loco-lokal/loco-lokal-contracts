#!/usr/bin/env bash
# Downloads the field detection model and ONNX Runtime into rust/vendor/onnx/:
#
# - model.onnx: the model the Dockerfile puts in /app/tmp/model.onnx
#   (docusealco/fields-detection 2.0.0, model_704_int8).
# - lib/libonnxruntime.{so,dylib}: ONNX Runtime 1.24.4, the version Alpine packages for the
#   Rails image, from Microsoft's NuGet package (the release with the native libraries
#   for Linux and macOS).
#
# The `detect` crate loads them at runtime: ONNX_MODEL_PATH and ORT_DYLIB_PATH override
# these paths.
set -euo pipefail

MODEL_URL="https://github.com/docusealco/fields-detection/releases/download/2.0.0/model_704_int8.onnx"
MODEL_SHA=a0a92ef7f50b6e5c707b7da9c59977bc7c9e9d8f85542bd75169c693f5efa4da
ORT_VERSION=1.24.4
ORT_URL="https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime/${ORT_VERSION}/microsoft.ml.onnxruntime.${ORT_VERSION}.nupkg"
ORT_SHA=4b978d5065b85e7004b6c6f60ca494bd978fbe6836cbf0a0b52d82b61ab99638

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)  RUNTIME=osx-arm64;   LIB=libonnxruntime.dylib ;;
  Linux-x86_64)  RUNTIME=linux-x64;   LIB=libonnxruntime.so ;;
  Linux-aarch64) RUNTIME=linux-arm64; LIB=libonnxruntime.so ;;
  *) echo "Unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

DEST="$(cd "$(dirname "$0")/.." && pwd)/vendor/onnx"
mkdir -p "$DEST/lib"

sha_ok() {
  if command -v sha256sum >/dev/null; then
    echo "$1  $2" | sha256sum -c --status
  else
    echo "$1  $2" | shasum -a 256 -c --status
  fi
}

if [[ -f "$DEST/model.onnx" ]] && sha_ok "$MODEL_SHA" "$DEST/model.onnx"; then
  echo "model already present in $DEST"
else
  echo "Downloading model_704_int8.onnx..."
  curl -fsSL -o "$DEST/model.onnx.tmp" "$MODEL_URL"
  sha_ok "$MODEL_SHA" "$DEST/model.onnx.tmp" || { echo "checksum mismatch for model.onnx" >&2; exit 1; }
  mv "$DEST/model.onnx.tmp" "$DEST/model.onnx"
fi

STAMP="$DEST/lib/.release"
if [[ -f "$STAMP" && "$(cat "$STAMP")" == "$ORT_VERSION/$RUNTIME" && -f "$DEST/lib/$LIB" ]]; then
  echo "onnxruntime $ORT_VERSION already present in $DEST/lib"
else
  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT
  echo "Downloading onnxruntime $ORT_VERSION ($RUNTIME)..."
  curl -fsSL -o "$TMP/ort.nupkg" "$ORT_URL"
  sha_ok "$ORT_SHA" "$TMP/ort.nupkg" || { echo "checksum mismatch for the onnxruntime package" >&2; exit 1; }
  if command -v unzip >/dev/null; then
    unzip -q -o "$TMP/ort.nupkg" "runtimes/$RUNTIME/native/*" -d "$TMP/ort"
  else
    python3 -I -c 'import sys, zipfile
z = zipfile.ZipFile(sys.argv[1])
z.extractall(sys.argv[2], [n for n in z.namelist() if n.startswith(sys.argv[3])])' \
      "$TMP/ort.nupkg" "$TMP/ort" "runtimes/$RUNTIME/native/"
  fi
  rm -f "$DEST/lib/"libonnxruntime*
  cp "$TMP/ort/runtimes/$RUNTIME/native/"* "$DEST/lib/"
  echo "$ORT_VERSION/$RUNTIME" > "$STAMP"
fi
echo "model and onnxruntime in $DEST"
