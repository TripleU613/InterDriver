# interdriver

Private InterDriver project for exporting a USB device from Windows, Linux, or macOS and presenting it as a locally attached USB device on Windows, Linux, or macOS. The Apple Configurator use case is Linux/Windows to macOS, but the transport supports the complete 3x3 platform matrix.

## Feasibility

The design is technically possible. USB/IP already defines how a physical USB device is exported as USB requests and imported through a virtual host controller. The difficult component is the macOS importer: Apple Configurator only documents iPhone and iPad preparation over a USB connection, so the imported device must exist in macOS IOKit as a complete USB device. A TCP proxy or remote `usbmuxd` socket alone is insufficient for unmodified Configurator.

Apple exposes `IOUSBHostControllerInterface` for a user-space virtual host controller. Shipping software that uses it requires the managed `com.apple.developer.usb.host-controller-interface` entitlement. Apple accepts entitlement requests for products that represent a network-attached USB device locally. Development without that entitlement requires disabling SIP, which must not be done on the production Drone Mac.

## First Milestone

Prove one direct connection before building a relay or UI:

1. Attach an iPhone to a dedicated Linux test computer.
2. Export the entire Apple USB device using USB/IP.
3. Import it on a dedicated Mac through `IOUSBHostControllerInterface`.
4. Confirm the device appears in `ioreg`, `system_profiler`, and `cfgutil list`.
5. Confirm trust, pairing, erase, reboot, and USB re-enumeration.
6. Run one Apple Configurator prepare operation and verify the resulting ABM assignment.

The first milestone is not complete merely because Lockdown or `usbmuxd` works. Apple Configurator must see and operate on the virtual IOKit device.

## Project Rules

- Keep this project private.
- Do not copy Electronic Team binaries or protocol implementation into this directory.
- Do not copy code from repositories without a usable license.
- Use the documented USB/IP protocol for the clean-room data plane.
- Never expose unencrypted USB/IP port 3240 to the public internet.
- Do not log USB payloads, pairing records, activation records, or Configurator credentials.
- Do not disable SIP on the production release runner.

## Layout

- `apps/interdriver-usb`: Rust development CLI for platform adapters.
- `crates/usbip-protocol`: bounded, byte-accurate USB/IP 1.1.1 codec.
- `crates/usbip-client`: Rust device-list/import negotiation and persistent data connection.
- `crates/usbip-session`: concurrent Rust URB submit, completion, and cancellation engine.
- `crates/driver-core`: cross-platform device and re-enumeration lifecycle.
- `crates/platform-linux`: Rust control of Linux `usbip_host` and `vhci_hcd`.
- `crates/platform-macos`: Rust ABI and state machines for Apple IOUSBHost.
- `docs/architecture.md`: proposed cross-platform architecture and delivery phases.
- `docs/windows-reference-audit.md`: audit of the existing paid Windows tunnel.
- `scripts/probe-linux-host.sh`: read-only Linux USB/exporter diagnostics.
- `scripts/probe-macos-host.sh`: read-only Mac importer and Configurator diagnostics.

Run the current Rust checks with:

```sh
cargo test --workspace
cargo run -p interdriver-usb -- matrix
cargo run -p interdriver-usb -- linux list
```

## Required Apple Request

Submit an Apple Feedback Assistant request for:

- Platform: macOS
- Title: `Request for Entitlement - com.apple.developer.usb.host-controller-interface`
- Problem Area: USB
- Type: Other Bug
- Include the Apple Developer Team ID, company/product overview, and company/product links.

Describe the product as a managed enterprise utility that creates a local virtual USB controller representing an organization-owned iPhone physically attached to an authenticated remote workstation, so Apple Configurator can prepare that device.

## References

- [Apple Configurator device connection](https://support.apple.com/guide/apple-configurator-mac/connect-devices-cad9d4b2211e/mac)
- [Apple IOUSBHostControllerInterface](https://developer.apple.com/documentation/iousbhost/iousbhostcontrollerinterface)
- [Apple entitlement request guidance](https://developer.apple.com/forums/thread/802495)
- [Linux USB/IP README](https://github.com/torvalds/linux/blob/master/tools/usb/usbip/README)
- [Linux USB/IP protocol](https://docs.kernel.org/usb/usbip_protocol.html)
