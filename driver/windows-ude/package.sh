#!/usr/bin/env bash
# Stages the driver package for deployment: .sys + .inf + the Windows tools
# needed to catalog and sign it on the target.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SRC="$ROOT/driver/windows-ude"
OUT="$SRC/package"
BT="$ROOT/.wdk/buildtools.d/bin"
WDKBIN="$ROOT/.wdk/microsoft.windows.wdk.x64.d/c/bin"

rm -rf "$OUT" && mkdir -p "$OUT"
cp "$SRC/build/interdriver-ude.sys" "$OUT/"
cp "$SRC/interdriver-ude.inf" "$OUT/"
BTVER="$(ls "$BT" | head -1)"
cp "$BT/$BTVER/x64/signtool.exe" "$OUT/"
cp "$BT/$BTVER/x64/MakeCert.exe" "$OUT/"
WDKVER="$(ls "$WDKBIN" | head -1)"
cp "$WDKBIN/$WDKVER/x86/Inf2Cat.exe" "$OUT/" 2>/dev/null || true
echo "staged:"; ls -la "$OUT"
