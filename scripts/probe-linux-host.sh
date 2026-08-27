#!/usr/bin/env bash
set -u

echo "os=$(uname -srmo)"

if command -v usbip >/dev/null; then
  echo "usbip=$(command -v usbip)"
else
  echo "usbip=missing"
fi

if command -v usbipd >/dev/null; then
  echo "usbipd=$(command -v usbipd)"
else
  echo "usbipd=missing"
fi

for module in usbip_core usbip_host vhci_hcd; do
  if [[ -d "/sys/module/$module" ]]; then
    echo "module.$module=loaded"
  elif command -v modinfo >/dev/null && modinfo "$module" >/dev/null 2>&1; then
    echo "module.$module=available"
  else
    echo "module.$module=missing"
  fi
done

if command -v systemctl >/dev/null; then
  echo "service.usbmuxd=$(systemctl is-active usbmuxd 2>/dev/null || true)"
fi

found=0
for device in /sys/bus/usb/devices/*; do
  [[ -r "$device/idVendor" ]] || continue
  [[ "$(<"$device/idVendor")" == "05ac" ]] || continue
  found=1
  busid="$(basename "$device")"
  product="$(<"$device/idProduct")"
  serial=""
  [[ -r "$device/serial" ]] && serial="$(<"$device/serial")"
  driver="none"
  [[ -L "$device/driver" ]] && driver="$(basename "$(readlink "$device/driver")")"
  echo "apple.$busid=product:$product serial:${serial:-unavailable} driver:$driver"

  for interface in "$device":*; do
    [[ -e "$interface" ]] || continue
    interface_driver="none"
    [[ -L "$interface/driver" ]] && interface_driver="$(basename "$(readlink "$interface/driver")")"
    echo "apple.$busid.interface.$(basename "$interface")=$interface_driver"
  done
done

[[ "$found" == "1" ]] || echo "apple.devices=none"

if command -v lsusb >/dev/null; then
  lsusb -d 05ac: 2>/dev/null || true
fi

