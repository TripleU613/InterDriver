# interdriver-usb

Plug a USB device into one machine and use it on another as if it were plugged in
locally. Not a file share, not a socket proxy — the importing operating system
enumerates a real USB device, so software that refuses to talk to anything else
works unmodified.

The case this was built for: **an iPhone plugged into a Windows or Linux box in
one office, driven by Apple Configurator on a Mac somewhere else.** Configurator
only talks to a USB device in IOKit, so a `usbmuxd` socket forward is not enough.
This creates the real thing.

Clean-room USB/IP 1.1.1. No Electronic Team or usbipd-win code.

---

## What works

Every direction is implemented and has been run on real hardware.

| Device plugged into ↓ / Software runs on → | Linux | macOS | Windows |
| --- | --- | --- | --- |
| **Linux** | yes | yes | yes |
| **macOS** | yes | yes | yes |
| **Windows** | yes | yes | yes |

Proven end to end: Apple Configurator on a MacBook driving an iPhone physically
attached to either Linux or Windows. On Windows, the custom KMDF exporter owns
the Apple USB parent, supports all five configurations, and carries the trust
handshake and usbmux traffic. `cfgutil list` returns the real ECID and UDID and
`cfgutil get name` returns the phone's name. A full iOS restore has run over the
Linux path; the Windows path has been exercised through Configurator but not a
destructive restore.

---

## The 60-second version

Two commands. One machine exports, the other imports.

```bash
# On the machine with the device plugged in:
interdriver-usb linux export 3-12 3240

# On the machine that should see it:
interdriver-usb macos attach 192.0.2.10 3-12
```

`3-12` is the *port*, not the device. Whatever is plugged into that port is what
gets carried — if the device reboots or re-enumerates, the link follows it.

---

## Guide: Windows device → Apple Configurator on a Mac

This is the hardest path in the project. Follow it in order; the two steps people
lose a day to are marked **← this one**.

> **Read this first:** exporting an iPhone takes exclusive ownership of its USB
> parent. iTunes and Apple Devices on the Windows host cannot use the phone at
> the same time.

### 1. Windows: find the device

```powershell
interdriver-usb windows list
```

Nothing listed? That is expected before the next step. An iPhone ships bound to
Apple's driver.

### 2. Windows: build, sign, and bind the exporter driver ← this one

Build and stage the driver package on Linux:

```bash
./driver/windows-export/build.sh
./driver/windows-export/package.sh
```

Sign the staged INF, SYS, and catalog with a trusted development certificate.
The test machine must have test signing enabled. Copy the package to Windows,
then use an elevated PowerShell window:

```powershell
Set-ExecutionPolicy -Scope Process Bypass
.\install.ps1
```

The script stops the Apple services, disables their open composite functions,
forces the KMDF driver onto the physical parent, and creates the inbound
firewall rule. `interdriver-usb windows list` then prints a stable `win-...` bus id.
Keep the Apple services stopped while exporting. To restore local Apple access,
uninstall the InterDriver parent driver in Device Manager, scan for hardware
changes, re-enable the Apple functions, and start the two services.

### 3. Windows: export it

```powershell
interdriver-usb windows export <bus-id> 3240
```

Leave it running. It holds the port, not the device.

### 4. Mac: get the entitlement problem out of the way ← this one

A user-space virtual USB host controller needs Apple's managed entitlement
`com.apple.developer.usb.host-controller-interface`. Request it for anything you
ship (draft in `docs/apple-entitlement-request.md` — it needs a Team ID).

For development, bypass it on a **scratch Mac only**:

```bash
# 1. Boot to recovery, disable SIP
csrutil disable

# 2. Back in macOS, allow the unentitled binary to run
sudo nvram boot-args="amfi_get_out_of_my_way=0x1"
sudo reboot

# 3. After the reboot, sign the binary ad-hoc every time you rebuild
codesign --force --sign - \
  --entitlements crates/platform-macos/entitlements.plist \
  target/debug/interdriver-usb
```

Never do this on a machine that ships builds or holds signing keys. SIP off is
the whole security model off.

Skip the codesign step and the process dies instantly with exit 137 and no
useful message — that is AMFI killing it, not a crash.

### 5. Mac: attach

```bash
interdriver-usb macos attach <windows-host> <bus-id>
```

You should see:

```
<bus-id>: attached 05ac:12a8
```

### 6. Mac: confirm Configurator can actually see it

```bash
ioreg -p IOUSB -w0 | grep -A2 AppleUSBUserHCI
cfgutil list
```

`ioreg` should show your `AppleUSBUserHCI` with the device as a child, and
`cfgutil list` should print the real ECID and UDID. If `ioreg` shows the device
but `cfgutil` shows nothing, the device enumerated but was never configured —
see Troubleshooting.

---

## Per-platform requirements

**Linux (export)** — nothing to install. Uses usbfs directly; no `usbip_host`,
no `usbipd`. Needs root for `/dev/bus/usb`. Find the port with
`interdriver-usb linux list`; bus ids look like `3-12` and match `/sys/bus/usb/devices/`.

**Linux (import)** — needs the in-tree `vhci_hcd` module (`modprobe vhci-hcd`).

**macOS (export)** — no entitlement and no root. Takes exclusive ownership of the
`IOUSBHostDevice`, so unplug it from Configurator first.

**macOS (import)** — entitlement, or the SIP-off bypass above. This is the only
platform with a hard gate.

**Windows (export)** — custom KMDF parent driver in
`driver/windows-export/`. Development builds require test signing. Ordinary
single-configuration devices can still use the WinUSB exporter.

**Windows (import)** — the UdeCx virtual host controller driver in
`driver/windows-ude/`. Test-signed builds need `bcdedit /set testsigning on`.

---

## Locking it down

The data plane is plain USB/IP on port 3240. **Never expose 3240 to the
internet.** Two mechanisms are built in.

**Single-use tickets** — a broker issues a token scoped to one device, one
exporter, one importer, with a TTL. One consumer per device; revocation takes
effect mid-session.

```bash
interdriver-usb broker 9000                                    # run the broker
interdriver-usb issue 10.0.0.5:9000 3-12 edge-1 mac-1 300      # mint a ticket
interdriver-usb linux export 3-12 3240 --broker 10.0.0.5:9000 --exporter edge-1
interdriver-usb macos attach 10.0.0.5 3-12 --ticket <hex>
interdriver-usb sessions 10.0.0.5:9000                         # who holds what
interdriver-usb revoke  10.0.0.5:9000 <token>                  # cut a session off
```

**mTLS** — both planes can run inside a mutually authenticated tunnel.

```bash
interdriver-usb tls init ./certs
interdriver-usb tls serve   3241 127.0.0.1:3240 ./certs        # exporter side
interdriver-usb tls forward 3240 exporter-host:3241 exporter ./certs   # importer side
```

Do not log USB payloads, pairing records, activation records, or Configurator
credentials.

---

## Troubleshooting

**Mac process exits immediately, code 137** — AMFI killed it. You skipped the
ad-hoc `codesign`, or `amfi_get_out_of_my_way` is not set.

**Windows phone disappears during driver binding** — stop Apple Mobile Device
Service and Bonjour and disable the phone's `MI_00`/`MI_01` child devnodes, or
use `driver/windows-export/install.ps1`. An open Apple function vetoes the
exclusive parent replacement.

**Device shows in `ioreg` but `cfgutil list` is empty** — enumerated but not
configured. Run the Mac side with `INTERDRIVER_TRACE=1` and look for
`req=[00, 09, ...]` (SET_CONFIGURATION). No such line means the host is still
probing and gave up.

**Everything is slow** — see Known limits. Check whether your network is actually
direct: `tailscale ping <host>`. A relayed path multiplies every USB round trip.

**iPhone keeps dropping and coming back** — that is usually correct. Trust,
reboots, recovery mode and restores all re-enumerate the device, and the link is
built to follow that. `attached 05ac:1281` is recovery mode, `05ac:12a8` is
normal or RestoreOS.

**Reading the trace** — `INTERDRIVER_TRACE=1` on either side prints the protocol.

```
FIN ep0x04 req=[...] st=0 len=32768/32768   transfer done, full length
  <-- SHORT                                  fewer bytes than asked for
st=-32                                       device sent a STALL
CMD 0x1c / 0x20 / 0x28                       port reset / device create / endpoint create
```

---

## Known limits

**Throughput is round-trip bound.** One transfer is in flight per endpoint, so
throughput is roughly *transfer size ÷ RTT*. On a LAN this is fine. Over a
relayed link (~60 ms RTT) it lands near 400 KiB/s, which makes a multi-gigabyte
iOS restore take hours.

The fix is URB pipelining — several transfers in flight per endpoint. It must
preserve per-endpoint completion ordering, because a reordered bulk stream is a
corrupted firmware write.

**Check your path before blaming the code.** Tailscale silently falls back to a
DERP relay; `tailscale ping` tells you whether you have a direct route.

---

## Layout

```
apps/interdriver-usb/       CLI
crates/usbip-protocol/  wire codec
crates/usbip-client/    importer side of USB/IP
crates/usbip-server/    exporter side (DeviceProvider / DeviceSession)
crates/usbfs-linux/     Linux exporter, usbfs ioctls
crates/platform-linux/  Linux importer, kernel vhci
crates/platform-macos/  macOS importer, IOUSBHostControllerInterface
crates/platform-windows/ Windows exporters (KMDF/WinUSB) and importer (UdeCx)
crates/control-plane/   broker, tickets, revocation
crates/tls/             mTLS for both planes
driver/windows-ude/     the Windows virtual host controller driver
driver/windows-export/  the Windows physical USB exporter driver
docs/                   architecture, status, platform notes
```

`cargo test` runs the suite (28 green).

Working on the macOS importer? Read `docs/` and Apple's own headers first — they
are in the Xcode SDK at
`.../IOUSBHost.framework/Versions/A/Headers/IOUSBHostControllerInterfaceDefinitions.h`
and they answer questions that are otherwise days of guessing.

---

## Project rules

- Keep this project private.
- Clean-room only. Do not copy Electronic Team, usbipd-win, or any unlicensed code.
- Never expose unencrypted port 3240 to the public internet.
- Do not log USB payloads, pairing records, activation records, or credentials.
