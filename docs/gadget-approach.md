# Gadget-Mode Import — Avoiding the macOS Entitlement Entirely

## Why

The macOS importer (`IOUSBHostControllerInterface`) is gated by a restricted
entitlement that AMFI enforces at launch; a self-signed binary claiming it is
SIGKILLed (see `macos-entitlement-bypass.md`). Disabling SIP is not shippable to
customers, and a usbmuxd TCP proxy is not a driver — it defeats the point of
presenting a real device.

Third option: do not synthesise the device in the host's software at all.
Present it as **genuine hardware**. A small Linux board runs in USB **gadget**
mode (`/dev/raw-gadget`), plugged into the Mac. The Mac enumerates a real USB
device over a real cable, so there is no driver, no kext, no entitlement, and no
SIP change on the Mac. The board forwards URBs over TCP to the edge exporter.

The same board works unchanged against a Windows host, which would otherwise
need a KMDF/UdeCx virtual-controller driver plus signing and HLK. One hardware
path covers both blocked import cells.

## Endpoint budget (the constraint that decides the hardware)

`raw-gadget` can only expose endpoints the UDC physically has, so the device's
endpoint needs must fit the board.

Decoded from the Mac's IORegistry `UsbDeviceSignature` for the attached iPhone
(config 5, the configuration macOS selects once trusted) — eight interfaces:

| class/subclass/protocol | interface |
| --- | --- |
| `06 01 01` | PTP (still image) |
| `01 01 00` | Audio control |
| `01 02 00` | Audio streaming (isochronous) |
| `03 00 00` | HID |
| `ff fe 02` | **usbmux / lockdown** |
| `ff fd 01` | ipheth (personal hotspot) |
| `02 0d 00` | CDC-NCM control |
| `0a 00 01` | CDC-NCM data |

Proxying all eight needs roughly 12-14 endpoints including isochronous, which
exceeds a dwc2 UDC (Pi Zero / Pi 4 USB-C) and which raw-gadget handles poorly
for iso.

**But Apple Configurator drives only the usbmux interface** (`ff fe 02`) — that
is the lockdown channel behind AMDevice/usbmuxd. PTP is photos, ipheth and NCM
are tethering, audio/HID are accessory functions. A gadget that presents the
correct VID/PID/serial plus a single `ff fe 02` interface needs
**1 bulk IN + 1 bulk OUT + EP0**, which every UDC supports comfortably.

So the design is a *reduced* proxy, not a byte-faithful clone of all eight
interfaces. Endpoint budget therefore passes.

Open question for later: Configurator "Prepare" with erase drops the device into
recovery/DFU, which enumerates as a different PID with its own interface layout.
The bridge must re-present the new identity when that happens, which the
`driver-core` re-enumeration state machine already models.

## Testing without hardware

`dummy_hcd` provides a virtual UDC and a virtual HCD on the same Linux host, so
the bridge can be developed and proven locally: raw-gadget binds `dummy_udc.0`
and the local USB stack enumerates the proxied device. Both modules are present
in this kernel.

**This proves the bridge speaks raw-gadget correctly and that a Linux host
enumerates the result. It does NOT prove a Mac accepts it** — the Mac never sees
`dummy_hcd`. That claim needs a real board on a real cable.

## Hardware

No board on the tailnet has a UDC (all x86_64 hosts). A real test needs an SBC
with USB device/OTG support — a Pi Zero 2 W or Pi 4/5 (USB-C port, dwc2). Do not
purchase until the dummy_hcd bridge works, then the requirement is confirmed at
1 bulk IN + 1 bulk OUT.
