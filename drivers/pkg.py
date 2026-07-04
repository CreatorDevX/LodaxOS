#!/usr/bin/env python3
"""Package individual driver ELFs into drivers.elf with manifest.

Usage:
    python pkg.py <output> <name:class:path> [<name:class:path> ...]

Example:
    python pkg.py drivers.elf framebuffer:0:target/target/debug/framebuffer

class: 0 = Hardware, 1 = Abstraction
"""

import struct
import sys

MAGIC = b"LODAXPKG"
ENTRY_SIZE = 44  # 32 name + 4 class + 4 offset + 4 size


def main():
    out_path = sys.argv[1]
    drivers = []
    for arg in sys.argv[2:]:
        parts = arg.split(":", 2)
        if len(parts) != 3:
            print(f"Invalid entry: {arg} (expected name:class:path)")
            sys.exit(1)
        name, cls, path = parts
        with open(path, "rb") as f:
            data = f.read()
        drivers.append((name.encode("ascii", "replace"), int(cls), data))

    count = len(drivers)
    entries_offset = 12  # magic(8) + count(4)
    data_offset = entries_offset + count * ENTRY_SIZE
    # Pad to 8-byte alignment so ELF structs in driver binaries
    # are not misaligned when the kernel casts their pointers.
    if data_offset % 8 != 0:
        data_offset += 8 - (data_offset % 8)

    with open(out_path, "wb") as f:
        # Header
        f.write(MAGIC)
        f.write(struct.pack("<I", count))

        # Entries — offset is absolute byte offset from file start
        cur = 0
        for name_bytes, cls, data in drivers:
            entry_name = name_bytes.ljust(32, b"\0")[:32]
            f.write(entry_name)
            f.write(struct.pack("<I", cls))
            f.write(struct.pack("<I", data_offset + cur))
            f.write(struct.pack("<I", len(data)))
            cur += len(data)

        # Pad to data_offset
        pad = data_offset - f.tell()
        if pad > 0:
            f.write(b"\0" * pad)

        # Driver data
        for _, _, data in drivers:
            f.write(data)

    total = len(open(out_path, "rb").read())
    print(f"Packaged {count} driver(s) into {out_path} ({total} bytes)")


if __name__ == "__main__":
    main()
