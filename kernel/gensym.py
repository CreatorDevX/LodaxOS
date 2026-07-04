import sys
import struct
from elftools.elf.elffile import ELFFile
from elftools.elf.sections import SymbolTableSection

def main():
    if len(sys.argv) != 3:
        print("usage: gensym.py <elf> <output.sym>")
        sys.exit(1)

    elf_path, out_path = sys.argv[1], sys.argv[2]
    
    with open(elf_path, "rb") as f:
        elf = ELFFile(f)
        sec = elf.get_section_by_name(".symtab")
        if not isinstance(sec, SymbolTableSection):
            print("No .symtab")
            sys.exit(1)

        with open(out_path, "wb") as out:
            for sym in sec.iter_symbols():
                if sym.name and sym.entry["st_value"] != 0:
                    name_bytes = sym.name.encode("utf-8")
                    # Format: addr(8) + len(4) + name(N)
                    out.write(struct.pack("<QI", sym.entry["st_value"], len(name_bytes)))
                    out.write(name_bytes)

if __name__ == "__main__":
    main()
