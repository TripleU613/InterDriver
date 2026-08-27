# Architecture

## Product Matrix

The product supports every exporter/importer pairing across the three desktop
platforms. This is implemented as six Rust platform adapters around one shared
Rust protocol and session core, not as nine divergent protocol implementations.

| Physical device host | Linux importer | macOS importer | Windows importer |
| --- | --- | --- | --- |
| Linux exporter | Supported | Supported | Supported |
| macOS exporter | Supported | Supported | Supported |
| Windows exporter | Supported | Supported | Supported |

All InterDriver protocol, policy, lifecycle, service, and driver code is Rust. The
project still uses the operating system's required kernel boundary: Linux
`usbip_host`/`vhci_hcd`, Apple IOUSBHost, and Microsoft KMDF/UdeCx. Platform
manifests, INF files, entitlements, and generated FFI bindings are not executable
implementation logic.

## Roles

The computer with the physical iPhone is the **device host/exporter**. The Mac running Apple Configurator is the **USB importer**. This naming matters even if the product UI calls the Mac a server.

```text
iPhone
  |
  | physical USB
  v
Linux/Windows edge agent
  |  USB/IP requests and responses
  |  mutually authenticated encrypted session
  v
InterDriver rendezvous/relay (optional after the direct proof)
  |
  v
macOS import daemon
  |
  | IOUSBHostControllerInterface virtual port
  v
IOKit -> Apple Mobile Device services -> Apple Configurator / cfgutil
```

## Data Plane

Use USB/IP 1.1 request semantics as the canonical wire model:

- device list and import negotiation;
- submit and unlink operations;
- control, bulk, interrupt, and isochronous transfers;
- status, actual length, and error propagation;
- device reset, disconnect, and reconnect events.

For the direct proof, the Linux kernel `usbip_host` driver and `usbipd` can export the device. The Mac importer translates between USB/IP operations and `IOUSBHostControllerInterface` controller, port, device, and endpoint state machines.

USB/IP has no adequate internet-facing authentication or confidentiality. Direct testing must be confined to Tailscale. Production transport should carry the USB/IP messages inside mutually authenticated TLS or QUIC with bounded message sizes, deadlines, flow control, and one active consumer per physical device.

## Control Plane

The control plane should not carry USB payloads. It should:

- authenticate the edge workstation and Mac host;
- enumerate allowed Apple devices by metadata;
- issue a short-lived, single-use session ticket;
- match one exporter to one importer;
- record state transitions and byte counts;
- revoke a session and force both ends to release the device.

Both peers should establish outbound connections so customer networks need no inbound port forwarding. A relay joins the two authenticated streams when direct peer-to-peer connectivity is unavailable.

## Device Lifecycle

Configurator operations reset and erase devices. The implementation must treat a USB attachment as a changing transport, not as a permanent bus ID.

1. The edge agent detects Apple vendor ID `0x05ac` and captures the physical port path, serial when available, product ID, speed, and descriptors.
2. The Mac importer raises a port-connect event and lets macOS enumerate the device normally.
3. Transfers are forwarded without interpreting Apple protocols.
4. On reset or erase, the edge agent reports disconnect immediately.
5. The Mac importer removes the virtual device cleanly while retaining the logical session.
6. The edge agent watches the same physical port for normal, recovery, or other Apple product IDs.
7. The importer raises a new connect event and macOS performs fresh enumeration.
8. A bounded preparation deadline is measured in tens of minutes, not the short enrollment UI timeout used by the installer.

Matching must prefer physical port identity during reset because a serial may be unavailable in recovery or early activation states.

## Platform Components

### Linux Exporter and Importer

- First proof: kernel `usbip_host` plus `usbipd`.
- Import through the in-tree `vhci_hcd` virtual controller.
- A Rust service owns discovery, authorization, binding, attachment, and lifecycle.
- Production option: a supervised service that binds only an approved device and wraps USB/IP in the InterDriver session transport.
- Stop or exclude `usbmuxd`, `ipheth`, and other local claimants while raw export owns the device.
- Restore the original driver binding when the session ends.

### Windows Exporter and Importer

Windows needs a signed capture/stub driver to claim the physical device and serialize USB requests. The existing company repository uses Electronic Team's proprietary stub and service. A clean implementation is a separate KMDF driver-signing project; it is not unlocked by the available C# source.

The importer is a Rust KMDF UDE client driver. Microsoft UdeCx owns the generic
emulated-controller machinery while the Rust driver supplies descriptors,
endpoints, reset handling, and USB/IP transfer completion. Windows Rust WDK
support is usable for development but is still described by Microsoft as early,
so the unsafe FFI boundary needs focused review and Windows HLK coverage.

### macOS Exporter and Importer

- The exporter is a privileged Rust daemon using `IOUSBHostDevice` device capture.
- A privileged daemon owns `IOUSBHostControllerInterface`.
- It creates a virtual controller and port, then translates endpoint commands to USB/IP.
- A small signed app installs/controls the daemon and displays approval or entitlement errors.
- Distribution requires `com.apple.developer.usb.host-controller-interface` in the signing profile.
- Configurator remains unmodified and consumes the resulting IOKit device.

### Relay

- Stateless byte/message relay where possible.
- Short-lived tickets and mutual authentication.
- Per-session bandwidth, memory, and time limits.
- No payload persistence.
- Explicit backpressure instead of unbounded buffering.

## Acceptance Tests

1. `cfgutil list` sees the remote device with the correct ECID/UDID.
2. Trust and pairing complete through the remote link.
3. A large backup/restore transfer remains byte-correct.
4. Configurator Prepare completes through erase and re-enumeration.
5. Pulling the cable produces a clean removal on macOS.
6. Reconnecting the same port resumes the logical session without attaching a different phone.
7. Network loss fails closed, releases the device, and reconnects only with a valid ticket.
8. A second Mac cannot attach an already claimed device.

## Delivery Phases

1. Shared Rust USB/IP codec and erase/re-enumeration session core.
2. Linux Rust service over the in-tree exporter and importer drivers.
3. Apple entitlement request and Rust macOS exporter/importer on a dedicated Mac.
4. Rust Windows capture and UdeCx drivers, signing, and HLK work.
5. Cross-platform direct-connect interoperability tests for all nine pairings.
6. Encrypted outbound rendezvous/relay.
7. Product UI, packaging, updates, telemetry, and fleet operations.
