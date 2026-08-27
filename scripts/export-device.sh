#!/usr/bin/env bash
# Export the attached Apple device from this host and serve it over USB/IP.
# Restores the device to its normal drivers on exit.
set -euo pipefail

PORT="${PORT:-3240}"
BIN="${BIN:-$(dirname "$0")/../target/release/interdriver-usb}"
[[ -x "$BIN" ]] || BIN="$(dirname "$0")/../target/debug/interdriver-usb"

if [[ ! -x "$BIN" ]]; then
  echo "build first: cargo build --release -p interdriver-usb" >&2
  exit 1
fi

BUS_ID="${1:-}"
if [[ -z "$BUS_ID" ]]; then
  for device in /sys/bus/usb/devices/*; do
    [[ -r "$device/idVendor" ]] || continue
    [[ "$(<"$device/idVendor")" == "05ac" ]] || continue
    BUS_ID="$(basename "$device")"
    break
  done
fi

if [[ -z "$BUS_ID" ]]; then
  echo "no Apple device attached" >&2
  exit 1
fi

restore() {
  echo
  echo "restoring $BUS_ID"
  echo "$BUS_ID" > /sys/bus/usb/drivers/usb/unbind 2>/dev/null || true
  sleep 1
  echo "$BUS_ID" > /sys/bus/usb/drivers/usb/bind 2>/dev/null || true
  systemctl restart usbmuxd 2>/dev/null || true
}
trap restore EXIT

ADDRESS="$(tailscale ip -4 2>/dev/null | head -1 || true)"
[[ -n "$ADDRESS" ]] || ADDRESS="$(hostname -I | awk '{print $1}')"

echo "exporting $BUS_ID on ${ADDRESS}:${PORT}"
echo "import it with:  interdriver-usb probe ${ADDRESS} ${BUS_ID}"
echo
exec "$BIN" linux export "$BUS_ID" "$PORT"
