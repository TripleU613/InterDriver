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

## macOS Export — WORKING end to end (proven on hardware)

`interdriver-usb macos list` / `macos export <bus-id> [port]` share a locally
attached Apple device over USB/IP. `MacProvider` implements the same
`DeviceProvider` contract as the Linux and Windows exporters.

```
mac:   2-1  05ac:12a8  interfaces=1  config=1
mac:   exporting 2-1 from IOUSBHost on port 3240
linux: interdriver-usb probe 127.0.0.1 2-1
       id 05ac:12a8   configurations 5   usb 2.00
```

Linux imported the iPhone attached to the Mac and read its real device
descriptor; the configuration count matches `mac-device-groundtruth.md`. Device
capture needs no entitlement and no root, unlike the importer.

Two things had to be right: `IOUSBDeviceRequest` is exactly eight bytes with the
setup-packet layout, so it is packed into one register and passed by value; and
objc2's encoding check requires `bytes`/`mutableBytes` to be typed `*c_void`,
not `*u8`, which otherwise panics the URB thread after a successful transfer.

Remaining: endpoint traffic through `IOUSBHostPipe`; non-zero endpoints
currently report a stall.

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

### Windows import — WORKING (verified on hardware 2026-08-31)

A device physically attached to the Linux box now appears on Windows as a fully
working device, through the UdeCx driver, over a reverse SSH tunnel:

```
USB Composite Device   status=OK problem=0 inf=usb.inf
SD webcam 5160         status=OK problem=0 inf=usbvideo.inf
USB Microphone         status=OK problem=0 inf=wdma_usb.inf
InterDriver Virtual USB Host Controller  status=OK
```

usbccgp split the composite device into functions and Windows bound its own
`usbvideo` and `wdma_usb` class drivers to them. No hardware, no purchased
toolchain, no bugcheck.

**The recurring bugcheck was the CFG function pointers.**
`__guard_check_icall_fptr` and `__guard_dispatch_icall_fptr` are written by the
kernel at load time. MSVC emits `.00cfg` read-write for that reason; lld folded
it into read-only `.rdata`, so the kernel's own write bugchecked
ATTEMPTED_WRITE_TO_READONLY_MEMORY every time, regardless of what else was
fixed. `.00cfg` is now merged into `.data` and CI fails the build if either
pointer lands in a section without the write flag.

Three other real defects were fixed along the way and are worth keeping in mind
because none of them was the recurring crash: completing URBs while holding the
spin lock (both the teardown path and the fetch path), the endpoint-0 queue
being parented to the emulated device so teardown freed it under a live
pointer, and declaring only endpoint 0 so usbccgp could not start a composite
device.

### Windows import — earlier: a remote device enumerates but fails to start

The UdeCx driver plugs in a device built from descriptors captured off the real
remote device. Verified on `win-test` with the Linux webcam exported over a reverse
SSH tunnel:

```
win-test:   attached 3-11 from 127.0.0.1: 18 byte device descriptor, 1771 byte configuration
      USB\VID_0C45&PID_6340\2&1B8366BC&0&1     <- the Linux webcam, on Windows
      InterDriver Virtual USB Host Controller  OK
```

Windows enumerated a device that is physically attached to another machine, with
no hardware and no purchased toolchain. The device still reports an error
because endpoint 0 URBs are answered `STATUS_NOT_IMPLEMENTED`; forwarding those
to the user-mode service (which already holds the USB/IP session) is the
remaining work.

The earlier bugcheck is fixed and did not recur: hotpatch metadata
(`_guard_*_$fo_*$`, `memset_$fo_*$`) is merged into `.data` rather than
read-only `.rdata`.

### Windows import — the gadget board remains a hardware alternative

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

## macOS Import — Runs and Presents the Device (2026-08-31)

With SIP off + `amfi_get_out_of_my_way=1` + ad-hoc signing with
`entitlements.plist`, the importer runs (the managed entitlement is no longer
enforced). Fixed three latent u32/i32 signedness bugs in the controller FFI that
had never surfaced because AMFI blocked the path (message status and link state
are signed int32; the doorbell is unsigned uint32) — commit 298a3ee.

**Result:** the Mac presents a virtual iPhone with the real remote serial
(`4a6fbe05…`, the iPhone attached to the Linux exporter), matched in IOKit under
`AppleUSBUserHCIPort`, `bNumConfigurations=4`. Enumeration runs deeply — dozens
of control-endpoint transfers complete successfully (device descriptor, every
configuration descriptor, string descriptors, status stages).

**Fully diagnosed (2026-08-31, deeper pass):** the endpoint is in state
Paused (endpointState == 2) at the moment the 0x45 completion arrives — the
kernel pauses the control endpoint as part of its own sequence and treats it as
Paused the instant it sends the EndpointPause command, before the client can
respond. So the successful 4-byte answer cannot be delivered
(enqueueTransferCompletionForMessage rejects a completion on a Paused endpoint),
and the transfer must be re-issued after the kernel resumes the endpoint.
Network latency is NOT the cause (Mac<->exporter is direct, ~5 ms round trip).
Codex's controller defers the pause and unlinks the in-flight transfer, then
crashes on the rejected completion. Tolerating the rejection stops the crash but
the device tears down because the re-issued 0x45 is not completed while the
endpoint is Active. **The fix is a pause/resume-ordering rework in the endpoint
state machine:** hold or re-drive the transfer so the re-issued 0x45 completes
during an Active window, and report ENDPOINT_STOPPED for the superseded attempt.
This is controller-internals work best done by the controller author.

**Root cause pinned to one USB request (earlier pass):** the failing
control transfer is Apple vendor request `bmRequestType=0xc0 bRequest=0x45`
(device->host, 4 bytes), sent right after macOS reads the configuration
descriptors and before any SET_CONFIGURATION. The remote answers it correctly
(4 bytes, status SUCCESS), but `enqueueTransferCompletionForMessage` rejects the
completion with `-536870174 "message was not sent"`, even though the message
matches `currentTransferMessage`. Making the completion non-fatal (dropping the
superseded transfer) is NOT a valid fix: the host needs that answer, so with the
drop the device stops presenting entirely. The completion must be made to
succeed — a subtle IOUSBHostCI control-transfer discipline detail specific to a
vendor request. This needs the controller author or focused Apple-doc study.

**Earlier characterization:** `AppleUSBHostiOSDevice` (the bridge
to AMDevice/usbmuxd that `cfgutil` reads) stalls at `!registered`. The daemon
fails one control-endpoint (`ep 0x00`) transfer completion with
`IOUSBHostErrorDomain code=-536870174 "Transfer completion message was not
sent."`. Diagnostics ruled out the obvious cause: the completed message DID
match `currentTransferMessage` (pointer and content), and the failing transfer
is identical in shape (`type=0x39` NORMAL, 4 bytes, `status=SUCCESS`) to dozens
that just succeeded. This is a subtle control-transfer sequencing issue in the
endpoint state machine (`drive_endpoint_inner` / `handle_transfer_finished`),
not a signedness or pointer-staleness bug. Needs a focused pass on the
IOUSBHostCI control-transfer discipline (SETUP→DATA→STATUS ordering).

**Hedge:** `gadget-linux` presents a remote device over real USB hardware and
sidesteps this cell entirely.

## Delivered: Mutual-TLS transport (2026-08-31)

`interdriver-tls` — rustls mTLS with in-process CA + peer cert generation
(rcgen/ring), PEM persistence, cross-compiles to Windows and macOS.

- **Control-plane API over mTLS**: broker ISSUE/REDEEM carried over mutually
  authenticated TLS; a peer from a different CA is rejected at handshake;
  single-use redemption holds over the encrypted channel.
- **Data-plane mTLS tunnel**: `tunnel::serve`/`forward` wrap the USB/IP data
  plane in mutual TLS with no change to the USB/IP engine. A single-threaded
  non-blocking pump keeps one rustls session on one thread.
- **CLI**: `tls init` (generate CA + exporter/importer identities), `tls serve`,
  `tls forward`. Verified on hardware — a USB device descriptor flows through
  the full tunnel (`probe -> forward -> mutual TLS -> serve -> exporter ->
  webcam`) byte-correct.

## Delivered: Windows import verified live (2026-08-31)

The `InterDriver Virtual USB Host Controller` (UdeCx) driver is loaded on the
Windows test box. A webcam attached to the Linux box was imported over the
tunnel: `windows attach` pulled the 18-byte device descriptor and 1771-byte
configuration, and Windows enumerated it as a real composite USB device
(`SD webcam 5160` + `USB Microphone` + `USB Composite Device`, all OK). The
remote Linux device works as a real device on Windows.

## macOS import — exact failure loop (2026-09-01, on branch codex/macos-0x45-pause-fix)

Codex's pause/resume fix removed the crash: the Apple vendor request 0xc0/0x45
now completes with the endpoint Active. But the device still doesn't reach
cfgutil. Traced the precise post-0x45 sequence on hardware:

```
CTRL [c0 45 00 00 00 00 04 00] len=4   kernel issues vendor 0x45 (data stage) on ep0
CMD  0x2b (EndpointPause) d0=1          kernel pauses ep0 (device 1, ep 0)
CTRL [c0 45 ...]                        kernel RE-ISSUES 0x45
CMD  0x2b x3                            pauses again
CTRL [c0 45 ...]                        re-issues 0x45 (3rd time)
"Failed to send interrupts to kernel"   -> controller torn down
```

Mechanism: the kernel pauses ep0 within ~milliseconds of issuing 0x45, well
before our ~110 ms remote round trip returns. The Active-guard in
complete_endpoint_transfer therefore DROPS the 0x45 completion (endpoint is
Paused when the answer arrives), so the kernel never receives the 4-byte answer,
re-issues 0x45, and the loop repeats 3x before the kernel gives up (the
"Failed to send interrupts" messages are the teardown side effect, not the
cause). Latency is ~5 ms (direct), so this is NOT a timeout.

CONFIG NOTE (grounded): the iPhone advertises bNumConfigurations=4 while
untrusted (config 4 = PTP + Apple Mobile Device + Ethernet, which already
contains the mux interface), and exposes a 5th "trusted" config only after
pairing (paired-Mac ground truth). Config 5 is therefore NOT required for the
mux interface or for cfgutil to bind — the blocker is the 0x45 completion loop
above, not the config count.

FIX DIRECTION (controller author): the 0x45 control completion must reach the
kernel. Either complete it during the Active window before the kernel pauses
(don't drop it on pause), deliver a held completion on resume that the kernel
accepts, or handle the DATA->pause->STATUS control-transfer sequencing so the
kernel advances past 0x45 instead of re-issuing. This is intricate IOUSBHostCI
control-transfer state-machine work.

## macOS import — three theories eliminated on hardware (2026-09-01)

Continued directly (full latitude). Instrumented the completion path and tested
three independent fixes end-to-end:

1. Tolerate/drop the paused completion — device tears down (tried earlier).
2. Stop unlinking the in-flight transfer when a pause is deferred — KEPT: this is
   a real improvement. The 0x45 completion now arrives Ok with the endpoint
   Active (epstate==3) instead of Paused-and-dropped. Confirmed via RESULT trace:
   `RESULT ep0 ok len=4 status=0 epstate=3`.
3. interruptRateHz 1000 -> 0 (deliver interrupts to the kernel immediately) —
   no change.

Despite the completion now landing Ok on an Active endpoint, the kernel/framework
still logs "Failed to send interrupts to kernel" (x3) and tears the controller
down; cfgutil never lists the device. The wall is now in the
IOUSBHostControllerInterface interrupt-DELIVERY path to the kernel, not in our
completion logic, the unlink, or the interrupt rate. This needs kernel/IOKit-side
visibility (or Apple) to go further; it is not solvable by more edits to the
user-space controller's completion handling from here.

State on this branch: 0x45 no longer crashes and its completion reaches our code
Ok+Active. Remaining blocker: framework interrupt delivery. macOS import is the
only non-working cell; the gadget-mode importer remains the hardware hedge.

## macOS import — delivered (2026-09-01)

Apple Configurator on `test-mac` drives an iPhone that is
physically plugged into the Linux box, over `interdriver-usb`:

```
$ cfgutil list
Type: iPhone10,1  ECID: 0xC60CC183B883A  UDID: 4a6fbe059b44f8f206f8ca96b9319078cf2c36a9
                  Location: 0x80100000   Name: iPhone
$ cfgutil get name
iPhone
```

The device appears under our own `AppleUSBUserHCI` as a matched
`IOUSBHostDevice`, is configured into the trusted configuration, and carries
live usbmux traffic on the bulk pipes.

Four defects stood between the controller and this, all found against
`IOUSBHostControllerInterfaceDefinitions.h` rather than by guesswork:

1. A completion that arrived while an EndpointPause was deferred was reported
   as `EndpointStopped` with length 0, discarding real data. The kernel
   re-issued the request forever. `EndpointStopped` is only for a transfer that
   generated an event *instead of* completing.
2. The Apple vendor handshake makes the iPhone re-enumerate with a fifth
   (trusted) configuration. The exporter followed that silently under the live
   session, so the host never learned its device changed. A re-enumeration is
   now a disconnect and the importer rebuilds.
3. Every URB was serialized behind one mutex held across the blocking ioctl,
   so a bulk IN parked on the mux endpoint blocked the bulk OUT that would
   have made the device answer.
4. `IOUSBHostCIMessageTypeLink` (0x3c), the entry that chains one transfer
   array to the next, was treated as unsupported, killing the session at the
   first ring wrap (~480 transfers in).

### Caveats

A reset or re-enumeration *during* an operation (`cfgutil erase`, a restore) now
surfaces as a session teardown and controller rebuild rather than the transparent
reopen the exporter used to do. That is what a physical host sees, but it has not
yet been exercised against a real erase -- the user deferred the wipe.

An idle IN pipe used to be reported as a Timeout, which halts the endpoint on the
importing host and killed usbmux mid-session. usbfs transfers are synchronous, so
we give up where a real controller would leave the URB queued; a quiet IN read is
now retired empty instead. The only halt left in a long run is ep0 going Offline
during the trust re-enumeration, which is the device genuinely going away.

### Throughput

Correct but latency-bound: one transfer in flight per endpoint, and this pair
of machines has no direct route — tailscale falls back to `DERP(nyc)` at
28-230ms RTT, so every USB round trip crosses the relay. That caps us near 30
transfers/s and makes a lockdown read take ~110s. A direct route, or
pipelining multiple URBs per endpoint (which needs per-endpoint completion
ordering to stay stream-safe), is where the speed is.
