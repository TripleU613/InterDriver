#!/usr/bin/env bash
# Signs the importer with the host-controller entitlement and reports whether
# this Mac will let it run.
#
# The entitlement is enforced by AMFI at launch. On a Mac with Full Security a
# self-signed binary claiming it is killed outright; with SIP disabled the check
# is relaxed and the controller can be created. See docs/macos-entitlement-bypass.md.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${1:-$ROOT/target/debug/interdriver-macos-controller-probe}"
ENT="$ROOT/crates/platform-macos/entitlements.plist"

[[ -x "$BIN" ]] || { echo "build first: cargo build -p interdriver-platform-macos" >&2; exit 1; }

echo "== machine security =="
csrutil status 2>&1 || true
if command -v bputil >/dev/null 2>&1; then
  sudo bputil -d 2>/dev/null | grep -E "Security Mode|Boot Args Filtering|SIP Status" || true
fi

echo
echo "== signing with the entitlement =="
codesign -s - --entitlements "$ENT" --force "$BIN"
codesign -d --entitlements - "$BIN" 2>&1 | grep -A2 host-controller || true

echo
echo "== running =="
set +e
"$BIN"
rc=$?
set -e
case $rc in
  0)   echo "RESULT: controller created — macOS import is unblocked on this machine" ;;
  137) echo "RESULT: killed by AMFI (SIGKILL). SIP is still enforcing; see below." ;;
  *)   echo "RESULT: exited $rc (see message above)" ;;
esac

if [[ $rc -ne 0 ]]; then
  cat <<'MSG'

To unblock on THIS Mac (development only, never the release runner):
  1. Shut down. Hold the power button until "Loading startup options".
     Click Options -> Continue to enter Recovery.
  2. Utilities -> Terminal:   csrutil disable
  3. Reboot, then re-run this script.

Shipping to customers instead requires Apple granting the entitlement:
  docs/apple-entitlement-request.md  (needs the Team ID)
MSG
fi
