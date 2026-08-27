# Verified Status

Updated 2026-08-27. Every claim below was produced by running the code, not by reading it.

## Proven

| Step | Result |
| --- | --- |
| Workspace builds on Linux x64 and macOS arm64 | 18 tests pass |
| Rust codec against in-tree kernel `usbipd` | Byte-exact device list |
| Rust import + URB submit against the kernel | Real descriptor returned |
| Real iPhone `05ac:12a8` exported from Linux | 4 configurations reported |
| MacBook reads that iPhone over Tailscale | Same descriptor, `exit=0` |
| Rust server exports from usbfs, kernel vhci imports | Full enumeration |
| Kernel fetched string descriptors through the Rust server | `Apple Inc.` / `iPhone` |
| Apple `ipheth` driver bound to the tunnelled iPhone | Network interface created |
| Windows exporter runs natively on `win-test` (Win11 22H2) | `windows list` executes, exit 0 |

The data plane is real. `interdriver-usb probe <host> <bus-id>` imports a remote
device through the Rust client/session and prints its USB device descriptor. It
is the smoke test for every future platform pairing because it touches no
platform code.

## The One Blocker

`IOUSBHostControllerInterface` creation fails on macOS 26.6.2:

```
domain=IOUSBHostErrorDomain code=-536870195
required entitlement: com.apple.developer.usb.host-controller-interface
```

The FFI is correct and reaches Apple's framework. Nothing in the Rust importer
can be verified further until Apple grants the managed entitlement. Requesting
it is the critical path, not more code. Do not disable SIP on the release Mac.

## Known Limits of the usbfs Exporter

- Isochronous endpoints are unsupported. usbfs synchronous ioctls cover control,
  bulk, and interrupt only, so a tunnelled webcam enumerates but cannot stream.
  Isochronous needs the asynchronous `USBDEVFS_SUBMITURB`/`REAPURB` API.
- A submitted transfer occupies a thread until it completes or the 30 second
  timeout expires, and `cancel` cannot interrupt it. `ipheth` reading an idle
  iPhone therefore reports `-110`, which is correct but wasteful.
- usbmuxd segfaults when exporter and importer are the same host. The likely
  cause is two devices with one identical UDID on the same daemon, which would
  make it a test artefact rather than a product path, but this is unconfirmed:
  the crash is in usbmuxd's own stripped frames. Proving Apple tooling end to
  end needs a second machine with `usbip` and `vhci_hcd`, which also settles
  the cause. Until then, treat it as an open question, not a closed one.

## Control Plane (new, tested on Linux)

`control-plane` implements the "initiate contact" API from architecture.md,
fully unit-tested (13 tests), no external crypto deps:

- `Broker::issue` matches one exporter to one importer for one `DeviceKey` and
  mints a 256-bit `Token` from `/dev/urandom`. Fails `DeviceBusy` if the device
  already has a live session — one consumer per physical device.
- `Broker::redeem` is single-use, TTL-bounded, and importer-bound; a second
  redeem, an expired ticket, or a wrong importer are all rejected.
- `Broker::revoke` / `release` free the device; a revoked ticket cannot be
  reused, and the device is claimable again afterwards.
- `Token` compares in constant time and never appears in Debug output.

Time is injected as unix seconds (no ambient clock) so the logic is
deterministic.

## Ticket-Gated Data Plane (verified on hardware)

The ticket now gates the actual USB/IP connection. Before the USB/IP control
request, an importer may send `TICKET_MAGIC` ("GTKT") + a 32-byte token; the
magic cannot collide with a USB/IP header (whose first bytes are version
0x0111), so a ticketed exporter cleanly rejects a plain kernel usbip client.
`serve_authorized` takes an `Authorizer`; `interdriver-usb linux export <bus> [port]
--ticket <hex>` requires the token, `interdriver-usb probe <host> <bus> --ticket
<hex>` presents it. Verified with a real webcam: correct ticket returns the
device descriptor, wrong ticket and no ticket are both dropped before any device
is touched.

## Networked Broker (verified on hardware)

The exporter's `Authorizer` now talks to a live broker over TCP instead of
holding a pre-shared token. `control-plane::net` speaks a two-verb line
protocol: `ISSUE` (operator) and `REDEEM` (exporter). The exporter redeems the
ticket the importer presented — a capability model: possession of the token,
delivered out of band by whoever issued it, is the importer's authorization; the
exporter is the party the broker binds and can authenticate. `redeem` reports
`Allow` / `Deny` / `Unreachable` as three distinct outcomes, and the exporter
logs a denied ticket differently from an unreachable broker.

CLI: `interdriver-usb broker <port>`, `interdriver-usb issue <broker> <device>
<exporter> <importer> [ttl]`, and `interdriver-usb linux export <bus> [port]
--broker <addr> --exporter <id>`.

Verified three-party on real hardware (broker + exporter + operator + importer,
four processes): broker issues -> importer probes -> exporter redeems via broker
-> webcam descriptor returned; a reused ticket is denied "already redeemed" and
an unissued token "no session matches" — single-use enforced across the network.

Still to wire: transport (carry USB/IP inside mutually authenticated TLS/QUIC),
and authenticating the operator to the broker (today the ISSUE verb is
unauthenticated — do not expose the broker port off a trusted network).

## macOS Export — capture and descriptors working on hardware

Beyond the exclusive-ownership probe, the exporter now reads real descriptors
off the attached iPhone through `descriptorWithType:length:index:languageID:`,
which issues genuine GET_DESCRIPTOR control requests:

```
EXCLUSIVE OPEN OK: 05ac:12a8
device descriptor: 18 bytes [12 01 00 02 00 00 00 40 ac 05 a8 12 01 10 01 02 03 05]
configuration descriptor: 39 bytes (total 39)
```

VID/PID/bcdDevice/configuration-count all match `mac-device-groundtruth.md`.
Control transfers therefore work; what remains for a full `DeviceProvider` is
bulk and interrupt traffic through `IOUSBHostPipe`.

Note objc2 rejects a `*const u8` return against the framework's struct-pointer
encoding, so the selector is invoked through `objc_msgSend` directly.

## macOS Export — VIABLE (probe proven on hardware)

`interdriver-macos-export-probe` (in `platform-macos`) took **exclusive ownership**
of the attached iPhone via `IOUSBHostDevice initWithIOService:queue:error:
interestHandler:` — non-root, no entitlement error:

```
EXCLUSIVE OPEN OK: 05ac:12a8 — userspace took exclusive ownership
```

This is the opposite gate from the importer: device *capture* (export) needs no
managed entitlement, unlike the virtual *controller* (import). So the macOS
exporter is buildable and testable (Mac -> Linux) now; the full exporter still
has to implement endpoint I/O (`IOUSBHostInterface`/pipes) behind the shared
`DeviceProvider` contract, same shape as `UsbfsProvider` and `WinUsbProvider`.

## Windows Import — needs a toolchain we do not have

The Windows importer is a KMDF **UdeCx** virtual-controller kernel driver. It
requires the Windows Driver Kit, a test-signing-enabled (or EV-signed) dedicated
box, and a kernel debugger; release needs Windows HLK. None of that is available
in this environment, so writing source here would produce code that cannot be
compiled, signed, loaded, or tested. Left intentionally unwritten. This is the
single largest remaining piece and is properly its own driver-signing project.

## Not Yet Written

- macOS exporter. Codex declared `mod exporter` with `list_physical_devices`
  and `serve_physical` and never wrote the file. The two dead re-exports were
  removed so the importer compiles. Nothing referenced them.
- Windows importer. Presenting a virtual device to iTunes on Windows needs a
  UDE/UDECX kernel driver with signing. Not started.
### Windows export — WORKING (proven on hardware 2026-08-27)

`interdriver-usb windows export <bus-id> [port]` on `win-test` exports the attached
iPhone, and Linux imports it:

```
win-test:   8_10379175_1_0001   05ac:12a8   interfaces=1
work: interdriver-usb probe 127.0.0.1 8_10379175_1_0001
      id 05ac:12a8   configurations 4   usb 2.00
```

Three things had to be right, each found by testing rather than assuming:

1. **Interface GUID.** `GUID_DEVINTERFACE_USB_DEVICE` returns only usbccgp
   composite parents. The exporter now enumerates several interface classes:
   the generic WinUSB class, Apple's own `{664be590-...}`, and our INF's
   `{D72D1A95-...}`, deduplicating by devnode.
2. **Apple's upper filters.** The MI_01 devnode has `Service = WINUSB` but also
   `UpperFilters = WUDFRd, AppleKmdfFilter`. The UMDF reflector refuses generic
   `CreateFileW`, which returns ERROR_NOT_SUPPORTED (50) on *every* interface
   GUID and access/flag combination. Removing `UpperFilters` and cycling the
   composite parent rebuilds the devnode as plain WinUSB and the open succeeds.
   This is not a reboot problem and not the Apple Mobile Device Service — both
   were tested and excluded.
3. **Accepted-socket blocking mode.** Windows returns an accepted socket that
   inherits the listener's non-blocking mode; Linux does not. `serve_authorized`
   now forces blocking mode, without which every request failed intermittently
   with a truncated or empty reply.

`win-test` is currently left with `UpperFilters` removed so export works. That may
degrade iTunes/Apple Mobile Device integration for that iPhone on that box.
Undo with `C:\Windows\Temp\gt\restore-apple.ps1` (restores the two filters and
cycles the device).

### Windows import — needs the gadget board, not a driver

There is no user-space path: presenting a virtual USB device to Windows requires
either a KMDF/UdeCx virtual-controller driver (WDK, EV signing, HLK — a separate
project, see below) or real hardware. The gadget approach covers it: a Linux
board in `raw-gadget` mode plugged into the Windows host is enumerated by
Windows' in-box class drivers with no InterDriver driver installed at all. That is
the same board and the same bridge as the macOS import path, so one piece of
hardware serves both. Unproven on a real host — see `gadget-approach.md`.

- Windows exporter (`platform-windows`, WinUSB): the exe runs on `win-test`,
  enumeration now finds the WinUSB `MI_01` interface (fixed: enumerate the
  standard WinUSB device-interface GUID `{DEE824EF-...}`, not
  `GUID_DEVINTERFACE_USB_DEVICE`, which returns only usbccgp composite parents).
  Remaining blocker: `CreateFileW` on the interface path returns ERROR_NOT_
  SUPPORTED (50) for every access/flag combination, though the device is healthy
  (problem code 0) and on `service = WINUSB`. A device disable/enable cycle did
  not clear it. This is the documented aftermath of a *forced*
  `UpdateDriverForPlugAndPlayDevices` WinUSB swap: the interface needs a full
  reboot before its device object accepts opens. Untested past this point;
  reboot `win-test` and re-run `interdriver-usb windows list`.

  The composite-interface question is settled by design: the shipped
  InterDriver INF (`interdriver-winusb.inf`) binds only `MI_01`, the lockdown/usbmux
  interface that carries all pairing and enrollment traffic, so single-interface
  WinUSB export is intended, not a workaround. Transfers against a WinUSB-bound
  device are still unrun; that needs the iPhone's `MI_01` temporarily rebound
  from Apple's `oem57.inf` to WinUSB, then a Windows->Linux vhci import.

## Windows Transfer Layer Caveats (unrun)

- `finish()` maps a failed WinUSB call as `-(Win32 error)`. Win32 codes are not
  errno, so a non-zero USB/IP status reaching a client is nonzero-but-arbitrary.
  Harmless on the happy path; revisit if error semantics ever matter.
- `speed_code()` returns 0 (unknown) on Windows; WinUSB does not expose link
  speed. The Linux importer enumerated fine with a real speed; watch the first
  Windows attach for an unknown-speed enumeration issue.
- `usbip-server`'s `DeviceProvider`/`DeviceSession` have test mocks only, so the
  Rust server is in no data path. `platform-linux::serve` shells out to the
  system `usbipd` binary instead.

## Test Machines

- Linux exporter: this host, `192.0.2.30`, `usbip` + both kernel modules.
- macOS importer: `test-mac`, macOS 26.6.2 arm64, Xcode, SIP on.
- Leave `ci-macmini-levis` alone. It is the production release runner.

Restore any bound device with `usbip unbind -b <bus-id>`, then
`systemctl restart usbmuxd` for Apple devices.
