#!/usr/bin/env bash
# Cross-builds the UdeCx driver from Linux with clang-cl + lld-link against
# WDK/SDK headers unpacked from NuGet. No Visual Studio required.
set -euo pipefail

# Tool discovery: Debian ships clang-cl and lld-link under the versioned LLVM
# directory rather than on PATH, so look there before giving up. Override with
# CLANG_CL / LLD_LINK if you have them somewhere else.
find_llvm_tool() {
    local name="$1" version bindir
    if command -v "$name" >/dev/null 2>&1; then
        command -v "$name"
        return 0
    fi
    for version in 21 20 19 18 17 16 15 14; do
        if command -v "$name-$version" >/dev/null 2>&1; then
            command -v "$name-$version"
            return 0
        fi
    done
    if command -v llvm-config >/dev/null 2>&1; then
        bindir="$(llvm-config --bindir 2>/dev/null || true)"
        if [[ -n "$bindir" && -x "$bindir/$name" ]]; then
            printf '%s\n' "$bindir/$name"
            return 0
        fi
    fi
    for bindir in /usr/lib/llvm-*/bin; do
        if [[ -x "$bindir/$name" ]]; then
            printf '%s\n' "$bindir/$name"
            return 0
        fi
    done
    return 1
}

CLANG_CL="${CLANG_CL:-$(find_llvm_tool clang-cl || true)}"
LLD_LINK="${LLD_LINK:-$(find_llvm_tool lld-link || true)}"

if [[ -z "$CLANG_CL" ]]; then
    echo "clang-cl not found; install clang or set CLANG_CL" >&2
    exit 1
fi
if [[ -z "$LLD_LINK" ]]; then
    echo "lld-link not found; install lld or set LLD_LINK" >&2
    exit 1
fi

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
WDK="$ROOT/.wdk/microsoft.windows.wdk.x64.d/c"
SDK="$ROOT/.wdk/microsoft.windows.sdk.cpp.d/c"
SDKLIB="$ROOT/.wdk/microsoft.windows.sdk.cpp.x64.d/c"
SDKVER=10.0.28000.0
KMDF=1.33
OUT="$ROOT/driver/windows-ude/build"
mkdir -p "$OUT"

INC=(
  -I"$WDK/Include/$SDKVER/km"
  -I"$WDK/Include/$SDKVER/km/crt"
  -I"$WDK/Include/$SDKVER/km/ude"
  -I"$WDK/Include/$SDKVER/shared"
  -I"$WDK/Include/wdf/kmdf/$KMDF"
  -I"$WDK/Include/wdf/kmdf/1.23"
  -I"$SDK/Include/$SDKVER/shared"
  -I"$SDK/Include/$SDKVER/um"
  -I"$SDK/Include/$SDKVER/ucrt"
)

"$CLANG_CL" --target=x86_64-pc-windows-msvc \
  /c /GS- /W3 /Od /Zi \
  -D_AMD64_ -DAMD64 -D_WIN64 -D_KERNEL_MODE -DNTDDI_VERSION=0x0A000010 \
  -D_WIN32_WINNT=0x0A00 -DWINVER=0x0A00 \
  -DKMDF_VERSION_MAJOR=1 -DKMDF_VERSION_MINOR=33 \
  -Wno-unused-command-line-argument -Wno-microsoft-anon-tag \
  "${INC[@]}" \
  /Fo"$OUT/driver.obj" "$ROOT/driver/windows-ude/driver.c"

"$LLD_LINK" \
  /OUT:"$OUT/interdriver-ude.sys" \
  /DRIVER /SUBSYSTEM:NATIVE,10.00 /OSVERSION:10.0 /ENTRY:FxDriverEntry /NODEFAULTLIB \
  /MACHINE:X64 /DEBUG /MAP:"$OUT/interdriver-ude.map" \
  /BASE:0x1C0000000 /DYNAMICBASE /NXCOMPAT /TSAWARE:NO /RELEASE \
  /ALIGN:0x1000 /FILEALIGN:0x1000 \
  /SECTION:.text,ERP /SECTION:.rdata,RP /SECTION:.data,RWP /SECTION:.pdata,RP \
  /MERGE:_guard_check_icall_=.data \
  /MERGE:_guard_dispatch_icall_=.data \
  /MERGE:memset_=.data \
  /MERGE:.retplne=.data \
  /LIBPATH:"$WDK/Lib/$SDKVER/km/x64" \
  /LIBPATH:"$WDK/Lib/$SDKVER/km/x64/ude/1.0" \
  /LIBPATH:"$WDK/Lib/wdf/kmdf/x64/$KMDF" \
  /LIBPATH:"$SDKLIB/Lib/$SDKVER/um/x64" \
  "$OUT/driver.obj" \
  wdfdriverentry.lib wdfldr.lib udecxstub.lib \
  ntoskrnl.lib hal.lib bufferoverflowfastfailk.lib

echo "built: $OUT/interdriver-ude.sys"
