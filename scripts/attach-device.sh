#!/usr/bin/env bash
# Import a device exported by export-device.sh and hand it to the local kernel.
# Detaches cleanly on exit.
set -euo pipefail

HOST="${1:?usage: attach-device.sh <host> [bus-id]}"
BUS_ID="${2:-}"
BIN="${BIN:-$(dirname "$0")/../target/release/interdriver-usb}"
[[ -x "$BIN" ]] || BIN="$(dirname "$0")/../target/debug/interdriver-usb"

if [[ -z "$BUS_ID" ]]; then
  BUS_ID="$("$BIN" linux remote-list "$HOST" | awk 'NR==1 {print $1}')"
  [[ -n "$BUS_ID" ]] || { echo "no devices exported by $HOST" >&2; exit 1; }
fi

modprobe vhci_hcd
PORT_NUMBER=""
detach() {
  [[ -n "$PORT_NUMBER" ]] && usbip detach -p "$PORT_NUMBER" 2>/dev/null || true
  systemctl restart usbmuxd 2>/dev/null || true
}
trap detach EXIT

usbip attach -r "$HOST" -b "$BUS_ID"
PORT_NUMBER="$(usbip port | awk '/Port [0-9]+:/ {gsub(":","",$2); last=$2} END {print last}')"
echo "attached $BUS_ID from $HOST on vhci port $PORT_NUMBER"
echo "press Ctrl-C to detach"
while sleep 3600; do :; done
