#!/usr/bin/env python3
"""Reads the bugcheck code, parameters and loaded-module list out of a Windows
kernel crash dump, so a driver crash can be diagnosed without WinDbg.

Layout is DUMP_HEADER64: 'PAGEDU64' magic, then fixed offsets for the bugcheck
fields, then a PsLoadedModuleList the small-dump writer copies inline.
"""
import struct
import sys

BUGCHECKS = {
    0x0A: "IRQL_NOT_LESS_OR_EQUAL",
    0x1E: "KMODE_EXCEPTION_NOT_HANDLED",
    0x3B: "SYSTEM_SERVICE_EXCEPTION",
    0x50: "PAGE_FAULT_IN_NONPAGED_AREA",
    0x7E: "SYSTEM_THREAD_EXCEPTION_NOT_HANDLED",
    0xBE: "ATTEMPTED_WRITE_TO_READONLY_MEMORY",
    0xC1: "SPECIAL_POOL_DETECTED_MEMORY_CORRUPTION",
    0xC4: "DRIVER_VERIFIER_DETECTED_VIOLATION",
    0xC9: "DRIVER_VERIFIER_IOMANAGER_VIOLATION",
    0xD1: "DRIVER_IRQL_NOT_LESS_OR_EQUAL",
    0x133: "DPC_WATCHDOG_VIOLATION",
    0x139: "KERNEL_SECURITY_CHECK_FAILURE",
    0x1E7: "INVALID_FLOATING_POINT_STATE",
}


def main(path):
    data = open(path, "rb").read()
    if data[:8] not in (b"PAGEDU64", b"PAGEDUMP"):
        print(f"not a kernel dump (magic {data[:8]!r})")
        return 1
    wide = data[:8] == b"PAGEDU64"
    if wide:
        code = struct.unpack_from("<I", data, 0x80)[0]
        params = struct.unpack_from("<4Q", data, 0x88)
    else:
        code = struct.unpack_from("<I", data, 0x38)[0]
        params = struct.unpack_from("<4I", data, 0x3C)

    name = BUGCHECKS.get(code, "unknown")
    print(f"bugcheck 0x{code:X}  {name}")
    for index, value in enumerate(params, start=1):
        print(f"  arg{index} = 0x{value:016x}")

    if code == 0xBE:
        print("\narg1 = virtual address written, arg3 = faulting instruction pointer")
    elif code in (0x0A, 0xD1):
        print("\narg1 = referenced address, arg2 = IRQL, arg4 = faulting instruction")
    elif code == 0x50:
        print("\narg1 = referenced address, arg3 = faulting instruction")

    # Driver names appear as UTF-16 paths inside the dump; surface ours plus any
    # module adjacent to the fault.
    text = data[:0x40000].decode("utf-16-le", errors="ignore")
    hits = sorted({w for w in text.split("\x00") if w.lower().endswith(".sys") and len(w) < 200})
    if hits:
        print("\nmodules named in the dump header:")
        for hit in hits[:25]:
            mark = "   <-- ours" if "interdriver" in hit.lower() else ""
            print(f"  {hit}{mark}")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print("usage: read-crashdump.py <MEMORY.DMP|minidump.dmp>")
        raise SystemExit(2)
    raise SystemExit(main(sys.argv[1]))
