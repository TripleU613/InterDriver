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
cargo xtask windows-ude build         # -> driver/windows-ude/build/interdriver-ude.sys
cargo xtask windows-ude package       # -> driver/windows-ude/package/
```

`cargo xtask wdk fetch` downloads and prepares `.wdk/` (about 1.2 GB,
gitignored).

Two things make the cross-build work:

- The WDK ships kernel CRT headers under `Include/<ver>/km/crt`, which supply
  `excpt.h` and friends that MSVC would otherwise provide.
- WDK and SDK headers `#include` each other with inconsistent capitalisation,
  which Windows tolerates and Linux does not. `cargo xtask wdk fetch` scans
  every header for unresolved same-directory includes and adds case-variant
  symlinks.

KMDF version directories are incremental, so both the target version and an
older complete one are on the include path.

## PE requirements the kernel enforces

Getting a cross-built image to load took matching several fields MSVC sets and
lld-link does not by default. The Rust xtask supplies all of them; the decisive
one is last:

| flag | why |
| --- | --- |
| `/ALIGN:0x1000 /FILEALIGN:0x1000` | kernel images are page-aligned in the file |
| `/RELEASE` | writes the PE checksum, which the loader validates |
| `/SUBSYSTEM:NATIVE,10.00 /OSVERSION:10.0` | default 6.00 is rejected |
| `/SECTION:...,P` | sections must be non-pageable, as MSVC marks them |
| `/MERGE:_guard_*=.rdata` | folds hotpatch metadata lld leaves unreadable |
| **`/BASE:0x1C0000000`** | **a kernel image cannot prefer a user-space base** |

The last one is the whole ballgame. With `/BASE:0x10000` every load fails with
`STATUS_CONFLICTING_ADDRESSES` (ERROR_INVALID_ADDRESS, 487) no matter what else
is correct, because the preferred base lies in user address space. It was found
by bisecting with a minimal ntoskrnl-only driver and diffing every optional
header field against Microsoft's `udecx.sys`.

Note `pnputil` deduplicates by `DriverVer`, so replacing the binary requires
`/delete-driver <oemNN.inf> /uninstall /force` first or the DriverStore keeps
serving the old image.

## Loading it

The driver is self-signed for development. Secure Boot is off on the test box,
so enabling test signing is enough:

```
bcdedit /set testsigning on      (reboot required)
```

A production driver needs an EV certificate and Microsoft attestation signing.
