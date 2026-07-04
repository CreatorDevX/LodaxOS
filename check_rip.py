import struct

with open('kernel.elf','rb') as f:
    magic = f.read(16)
    if magic[:4] != b'\x7fELF':
        print('Not ELF')
        exit()
    f.seek(0)
    ehdr = f.read(64)
    e_phoff = struct.unpack_from('<Q', ehdr, 32)[0]
    e_phentsize = struct.unpack_from('<H', ehdr, 54)[0]
    e_phnum = struct.unpack_from('<H', ehdr, 56)[0]
    
    print(f'Program headers at offset {e_phoff}, {e_phnum} entries of {e_phentsize} bytes')
    
    for i in range(e_phnum):
        f.seek(e_phoff + i * e_phentsize)
        phdr = f.read(56)
        p_type = struct.unpack_from('<I', phdr, 0)[0]
        p_flags = struct.unpack_from('<I', phdr, 4)[0]
        p_offset = struct.unpack_from('<Q', phdr, 8)[0]
        p_vaddr = struct.unpack_from('<Q', phdr, 16)[0]
        p_paddr = struct.unpack_from('<Q', phdr, 24)[0]
        p_filesz = struct.unpack_from('<Q', phdr, 32)[0]
        p_memsz = struct.unpack_from('<Q', phdr, 40)[0]
        
        if p_type == 1:
            print(f'  LOAD: offset={p_offset:#x} vaddr={p_vaddr:#x} paddr={p_paddr:#x} filesz={p_filesz:#x} memsz={p_memsz:#x} flags={p_flags:#x}')
            if p_vaddr <= 0x104929 < p_vaddr + p_memsz:
                file_off = p_offset + (0x104929 - p_vaddr)
                print(f'    -> VA 0x104929 is at file offset {file_off:#x}')
                f.seek(file_off)
                data = f.read(32)
                print(f'    Bytes: {" ".join(f"{b:02x}" for b in data)}')
                print(f'    Hex (as u64): {struct.unpack_from("<Q", data)[0]:#x}')
