# Windows physical USB exporter

This KMDF function driver exclusively owns an Apple USB device at the composite
parent. It exists because `winusb.sys` rejects iPhones, which advertise several
USB configurations.

The driver does not implement USB/IP or networking. It exposes device metadata,
control and endpoint transfers, and endpoint cancellation through a bounded
IOCTL interface. `interdriver-usb windows export` provides the USB/IP server.

This is a test-signed development driver. Bind it to the composite parent, not
to `MI_00` or `MI_01`. Binding removes the local Apple functions until the stock
driver is restored.

Build both implementations, or select one explicitly:

```bash
cargo xtask windows-export build
cargo xtask windows-export package rust
cargo xtask windows-export package reference
```

The Cargo-driven `xtask` is the only build, packaging, signing, and install
entry point. The Rust KMDF package is staged under `package/rust`; the proven C
1.0.4 package remains under `package/reference` as a rollback during the Rust
hardware soak.

On a disposable Windows development machine, enable test signing once and
reboot:

```powershell
bcdedit /set testsigning on
Restart-Computer
```

Then copy one staged package directory to Windows and run this from an elevated
terminal:

```text
interdriver-driver-tool.exe windows-export sign-test .
interdriver-driver-tool.exe windows-export install .
interdriver-usb.exe windows export <bus-id> 3240
```

The signing command creates a local development certificate and trusts it on
that machine. It is not release signing. A distributed driver still needs
Microsoft's production driver-signing process.

The installer stops Apple Mobile Device Service and Bonjour, disables the
Apple composite child functions, binds the physical parent, and opens TCP 3240
in Windows Firewall. Those services intentionally remain stopped while the
phone is exported; they otherwise race the exclusive driver replacement.

The exporter remembers the published INF and re-applies it when an iPhone on
the selected physical port changes among normal (`12a8`), recovery (`1281`),
and DFU (`1227`) modes. `UpdateDriverForPlugAndPlayDevices` is hardware-ID
scoped rather than device-instance scoped, so use one exported iPhone per
Windows exporter host.

The raw URB path is deliberate. KMDF's high-level select-configuration helper
only selects the first USB configuration; iPhones use a later trusted
configuration after pairing. The driver sends select-configuration and
select-interface URBs itself and keeps control, bulk, and interrupt requests
concurrent.
