#!/usr/bin/env bash
set -u

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This probe must run on macOS" >&2
  exit 2
fi

sw_vers
echo "arch=$(uname -m)"
echo "sip=$(csrutil status 2>/dev/null || echo unknown)"

if xcodebuild -version >/dev/null 2>&1; then
  xcodebuild -version
else
  echo "xcode=missing"
fi

configurator="/Applications/Apple Configurator.app"
cfgutil="$configurator/Contents/MacOS/cfgutil"
if [[ -d "$configurator" ]]; then
  version="$(mdls -raw -name kMDItemVersion "$configurator" 2>/dev/null || true)"
  echo "configurator.version=$version"
else
  echo "configurator=missing"
fi

if [[ -x "$cfgutil" ]]; then
  "$cfgutil" version 2>&1 | head -1
  echo "cfgutil.devices.begin"
  "$cfgutil" list 2>&1 || true
  echo "cfgutil.devices.end"
else
  echo "cfgutil=missing"
fi

if [[ -d /System/Library/Frameworks/IOUSBHost.framework ]]; then
  echo "iousbhost.framework=present"
else
  echo "iousbhost.framework=missing"
fi

echo "apple.usb.begin"
system_profiler SPUSBDataType 2>/dev/null | grep -E -i -A8 'iPhone|iPad|Apple Mobile' || true
echo "apple.usb.end"

if [[ -n "${CLIENT_BIN:-}" && -f "$CLIENT_BIN" ]]; then
  echo "client.entitlements.begin"
  codesign -d --entitlements :- "$CLIENT_BIN" 2>&1 || true
  echo "client.entitlements.end"
fi
