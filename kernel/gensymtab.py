#!/usr/bin/env python3
"""
Extract symbols + DWARF line info from kernel.elf and generate symtab.rs.

Dependencies:
  pip install pyelftools

Optional:
  rustfilt on PATH for Rust symbol demangling
"""

from __future__ import annotations

import bisect
import os
import shutil
import subprocess
import sys
from pathlib import Path

from elftools.elf.elffile import ELFFile
from elftools.elf.sections import SymbolTableSection


def rust_string(s: str) -> str:
    """Escape a Python string as a Rust string literal body."""
    out = []
    for ch in s:
        o = ord(ch)
        if ch == "\\":
            out.append(r"\\")
        elif ch == '"':
            out.append(r"\"")
        elif ch == "\n":
            out.append(r"\n")
        elif ch == "\r":
            out.append(r"\r")
        elif ch == "\t":
            out.append(r"\t")
        elif o < 0x20 or o == 0x7F:
            out.append(f"\\x{o:02x}")
        else:
            out.append(ch)
    return "".join(out)


def demangle(names: list[str]) -> list[str]:
    if not names:
        return []
    if shutil.which("rustfilt") is None:
        return names

    try:
        p = subprocess.run(
            ["rustfilt"],
            input="\n".join(names) + "\n",
            text=True,
            capture_output=True,
            check=False,
        )
        if p.returncode != 0:
            return names
        out = p.stdout.splitlines()
        if len(out) != len(names):
            return names
        return [line if line else raw for raw, line in zip(names, out)]
    except Exception:
        return names


def load_symbols(elffile: ELFFile) -> list[tuple[int, str]]:
    sec = elffile.get_section_by_name(".symtab")
    if not isinstance(sec, SymbolTableSection):
        raise SystemExit("No .symtab found")

    symbols: list[tuple[int, str]] = []
    for sym in sec.iter_symbols():
        name = sym.name
        if not name or name.startswith("."):
            continue

        st_value = int(sym.entry["st_value"])
        st_type = sym.entry["st_info"]["type"]
        st_shndx = sym.entry["st_shndx"]

        if st_value == 0:
            continue
        if st_shndx in (0, "SHN_UNDEF"):
            continue
        if st_type not in ("STT_FUNC", "STT_OBJECT", "STT_NOTYPE"):
            continue

        symbols.append((st_value, name))

    symbols.sort(key=lambda x: x[0])

    demangled = demangle([name for _, name in symbols])
    symbols = [(addr, demangled[i]) for i, (addr, _) in enumerate(symbols)]
    return symbols


def load_line_rows(elffile: ELFFile) -> list[tuple[int, str, int]]:
    dwarf = elffile.get_dwarf_info()
    rows: list[tuple[int, str, int]] = []

    if dwarf is None:
        return rows

    for cu in dwarf.iter_CUs():
        lp = dwarf.line_program_for_CU(cu)
        if lp is None:
            continue

        dirs = [""] + [
            d.decode("utf-8", errors="replace") if isinstance(d, bytes) else str(d)
            for d in lp.header.include_directory
        ]
        files = lp.header.file_entry

        for entry in lp.get_entries():
            st = entry.state
            if st is None or st.end_sequence:
                continue
            if st.file <= 0 or st.file > len(files):
                continue

            fe = files[st.file - 1]
            fname = fe.name.decode("utf-8", errors="replace") if isinstance(fe.name, bytes) else str(fe.name)
            dname = ""
            if fe.dir_index and fe.dir_index < len(dirs):
                dname = dirs[fe.dir_index]

            path = os.path.join(dname, fname) if dname else fname
            rows.append((int(st.address), path, int(st.line or 0)))

    rows.sort(key=lambda x: x[0])
    return rows


def lookup_line(rows: list[tuple[int, str, int]], addr: int) -> tuple[str, int]:
    if not rows:
        return ("", 0)

    addrs = [r[0] for r in rows]
    i = bisect.bisect_right(addrs, addr) - 1
    if i < 0:
        return ("", 0)
    return rows[i][1], rows[i][2]


def generate_rs(out_path: str, symbols: list[tuple[int, str]], rows: list[tuple[int, str, int]]) -> None:
    with open(out_path, "w", encoding="utf-8") as f:
        f.write("// Auto-generated; do not edit.\n")
        f.write("// Regenerate with this script.\n\n")
        f.write("#[derive(Debug, Clone, Copy)]\n")
        f.write("pub struct Symbol {\n")
        f.write("    pub addr: u64,\n")
        f.write("    pub name: &'static str,\n")
        f.write("    pub file: &'static str,\n")
        f.write("    pub line: u32,\n")
        f.write("}\n\n")
        f.write("pub static SYMBOLS: &[Symbol] = &[\n")

        for addr, name in symbols:
            file, line = lookup_line(rows, addr)
            f.write(
                f'    Symbol {{ addr: 0x{addr:016x}, name: "{rust_string(name)}", '
                f'file: "{rust_string(file)}", line: {line} }},\n'
            )

        f.write("];\n")


def main() -> None:
    if len(sys.argv) != 3:
        print("usage: gensymtab.py <kernel.elf> <output.rs>", file=sys.stderr)
        raise SystemExit(2)

    elf_path, out_path = sys.argv[1], sys.argv[2]

    with open(elf_path, "rb") as f:
        elffile = ELFFile(f)
        symbols = load_symbols(elffile)
        rows = load_line_rows(elffile)

    generate_rs(out_path, symbols, rows)
    print(f"wrote {len(symbols)} symbols to {out_path}")


if __name__ == "__main__":
    main()