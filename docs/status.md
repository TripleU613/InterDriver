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

## Not Yet Written

- macOS exporter. Codex declared `mod exporter` with `list_physical_devices`
  and `serve_physical` and never wrote the file. The two dead re-exports were
  removed so the importer compiles. Nothing referenced them.
- Windows exporter and importer. No crate exists.
- `usbip-server`'s `DeviceProvider`/`DeviceSession` have test mocks only, so the
  Rust server is in no data path. `platform-linux::serve` shells out to the
  system `usbipd` binary instead.

## Test Machines

- Linux exporter: this host, `192.0.2.30`, `usbip` + both kernel modules.
- macOS importer: `test-mac`, macOS 26.6.2 arm64, Xcode, SIP on.
- Leave `ci-macmini-levis` alone. It is the production release runner.

Restore any bound device with `usbip unbind -b <bus-id>`, then
`systemctl restart usbmuxd` for Apple devices.
