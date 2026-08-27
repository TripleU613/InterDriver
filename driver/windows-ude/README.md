# InterDriver UdeCx Virtual USB Host Controller (Windows import)

Presenting a virtual USB device on Windows needs a kernel driver: Microsoft's
UdeCx (USB Device Emulation class extension) owns the emulated controller
machinery and a client KMDF driver supplies the device. There is no user-space
alternative on Windows.

## Division of labour

This driver is a **thin shim**. It owns the emulated controller and root port
and exposes IOCTLs; all USB/IP protocol, session, policy and device logic stays
in the user-mode Rust service. Nothing device-specific is compiled into kernel
code, which keeps the kernel attack surface small and the interesting logic
testable in user space.

## Building — no Visual Studio, no WDK install

Cross-compiled from Linux with clang-cl and lld-link against WDK/SDK headers
unpacked from NuGet packages:

```sh
bash driver/windows-ude/build.sh      # -> build/interdriver-ude.sys
```

`scripts/fetch-wdk.sh` downloads and prepares `.wdk/` (about 1.2 GB, gitignored).

Two things make the cross-build work:

- The WDK ships kernel CRT headers under `Include/<ver>/km/crt`, which supply
  `excpt.h` and friends that MSVC would otherwise provide.
- WDK and SDK headers `#include` each other with inconsistent capitalisation,
  which Windows tolerates and Linux does not. `fetch-wdk.sh` scans every header
  for unresolved same-directory includes and adds case-variant symlinks.

KMDF version directories are incremental, so both the target version and an
older complete one are on the include path.

## Loading it

The driver is self-signed for development. Secure Boot is off on the test box,
so enabling test signing is enough:

```
bcdedit /set testsigning on      (reboot required)
```

A production driver needs an EV certificate and Microsoft attestation signing.
