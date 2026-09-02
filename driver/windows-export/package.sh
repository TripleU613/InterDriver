#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SRC="$ROOT/driver/windows-export"
OUT="$SRC/package"
BT="$ROOT/.wdk/buildtools.d/bin"
WDKBIN="$ROOT/.wdk/microsoft.windows.wdk.x64.d/c/bin"

mkdir -p "$OUT"
cargo build --release --target x86_64-pc-windows-gnu -p interdriver-usb
cp "$SRC/build/interdriver-export.sys" "$OUT/"
cp "$SRC/interdriver-export.inf" "$OUT/"
cp "$ROOT/target/x86_64-pc-windows-gnu/release/interdriver-usb.exe" "$OUT/"
cp "$SRC/install.ps1" "$OUT/"
cp "$SRC/sign-test.ps1" "$OUT/"
BTVER="$(ls "$BT" | head -1)"
cp "$BT/$BTVER/x64/signtool.exe" "$OUT/"
WDKVER="$(ls "$WDKBIN" | head -1)"
cp "$WDKBIN/$WDKVER/x86/Inf2Cat.exe" "$OUT/" 2>/dev/null || true
echo "staged: $OUT"
