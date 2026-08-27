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

## Not Yet Written

- macOS exporter. Codex declared `mod exporter` with `list_physical_devices`
  and `serve_physical` and never wrote the file. The two dead re-exports were
  removed so the importer compiles. Nothing referenced them.
- Windows importer. Presenting a virtual device to iTunes on Windows needs a
  UDE/UDECX kernel driver with signing. Not started.
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
