#!/usr/bin/env bash
# Cross-builds the UdeCx driver from Linux with clang-cl + lld-link against
# WDK/SDK headers unpacked from NuGet. No Visual Studio required.
set -euo pipefail

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

clang-cl --target=x86_64-pc-windows-msvc \
  /c /GS- /W3 /Od /Zi \
  -D_AMD64_ -DAMD64 -D_WIN64 -D_KERNEL_MODE -DNTDDI_VERSION=0x0A000010 \
  -D_WIN32_WINNT=0x0A00 -DWINVER=0x0A00 \
  -DKMDF_VERSION_MAJOR=1 -DKMDF_VERSION_MINOR=33 \
  -Wno-unused-command-line-argument -Wno-microsoft-anon-tag \
  "${INC[@]}" \
  /Fo"$OUT/driver.obj" "$ROOT/driver/windows-ude/driver.c"

lld-link \
  /OUT:"$OUT/interdriver-ude.sys" \
  /DRIVER /SUBSYSTEM:NATIVE /ENTRY:FxDriverEntry /NODEFAULTLIB \
  /MACHINE:X64 /DEBUG /MAP:"$OUT/interdriver-ude.map" \
  /BASE:0x10000 /DYNAMICBASE /NXCOMPAT /RELEASE \
  /ALIGN:0x1000 /FILEALIGN:0x1000 \
  /SECTION:.text,ERP /SECTION:.rdata,RP /SECTION:.data,RWP /SECTION:.pdata,RP \
  /MERGE:_guard_check_icall_=.rdata \
  /MERGE:_guard_dispatch_icall_=.rdata \
  /MERGE:memset_=.rdata \
  /LIBPATH:"$WDK/Lib/$SDKVER/km/x64" \
  /LIBPATH:"$WDK/Lib/$SDKVER/km/x64/ude/1.0" \
  /LIBPATH:"$WDK/Lib/wdf/kmdf/x64/$KMDF" \
  /LIBPATH:"$SDKLIB/Lib/$SDKVER/um/x64" \
  "$OUT/driver.obj" \
  wdfdriverentry.lib wdfldr.lib udecxstub.lib \
  ntoskrnl.lib hal.lib bufferoverflowfastfailk.lib

echo "built: $OUT/interdriver-ude.sys"
