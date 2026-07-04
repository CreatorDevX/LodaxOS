#![allow(dead_code)]

use alloc::vec::Vec;
use uefi::boot;
use uefi::boot::ScopedProtocol;
use uefi::proto::media::block::BlockIO;

const SECTOR_SIZE: usize = 512;

/// ext4 partition type GUID (Linux filesystem: 0FC63DAF-8483-4772-8E79-3D69D8477DE4).
const EXT4_GUID: [u16; 8] = [
    0x3DAF, 0x0FC6, 0x8483, 0x4772,
    0x798E, 0x693D, 0x47D8, 0xE47D,
];

// ---- ELF64 loader ----

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const EI_CLASS: usize = 4;
const ELFCLASS64: u8 = 2;
const ET_EXEC: u16 = 2;
const PT_LOAD: u32 = 1;

/// Maximum physical address the bootloader is willing to place a
/// `PT_LOAD` segment at. The bootloader runs in UEFI with the
/// identity map active, so segments must land in the first 4 GiB of
/// physical memory. 1 GiB leaves room for kernel growth while still
/// staying well within the identity-mapped 4 GiB range.
const KERNEL_LOAD_PHYS_LIMIT: u64 = 0x4000_0000; // 1 GiB

const E_IDENT: usize = 0;
const E_TYPE: usize = 16;
const E_ENTRY: usize = 24;
const E_PHOFF: usize = 32;
const E_PHENTSIZE: usize = 54;
const E_PHNUM: usize = 56;

const P_TYPE: usize = 0;
const P_FLAGS: usize = 4;
const P_OFFSET: usize = 8;
const P_VADDR: usize = 16;
const P_PADDR: usize = 24;
const P_FILESZ: usize = 32;
const P_MEMSZ: usize = 40;

fn addr_in_any_region(paddr: u64, size: u64, memory_regions: &[super::MemoryRegion], region_count: usize) -> bool {
    let end = paddr.saturating_add(size);
    for i in 0..region_count.min(memory_regions.len()) {
        let r = &memory_regions[i];
        let r_end = r.phys_start.saturating_add(r.size);
        if paddr >= r.phys_start && end <= r_end {
            return true;
        }
    }
    false
}

pub fn load_elf(data: &[u8], memory_regions: &[super::MemoryRegion], region_count: usize) -> Option<u64> {
    if data.len() < 64 {
        log::error!("ELF: file too small ({} bytes)", data.len());
        return None;
    }
    if data[E_IDENT..E_IDENT + 4] != ELF_MAGIC {
        log::error!("ELF: bad magic");
        return None;
    }
    if data[EI_CLASS] != ELFCLASS64 {
        log::error!("ELF: not 64-bit (class={})", data[EI_CLASS]);
        return None;
    }

    let ei_data = data[5];
    if ei_data != 1 {
        log::error!("ELF: not little-endian");
        return None;
    }

    let et = read_u16_le(data, E_TYPE);
    if et != ET_EXEC {
        log::error!("ELF: not executable (type={})", et);
        return None;
    }

    let entry = read_u64_le(data, E_ENTRY);
    let phoff = read_u64_le(data, E_PHOFF) as usize;
    let phentsize = read_u16_le(data, E_PHENTSIZE) as usize;
    let phnum = read_u16_le(data, E_PHNUM) as usize;

    log::info!(
        "ELF: entry={:#x} phoff={} phentsize={} phnum={}",
        entry, phoff, phentsize, phnum
    );

    if phoff + phnum * phentsize > data.len() {
        log::error!("ELF: program header table out of bounds");
        return None;
    }

    for i in 0..phnum {
        let off = phoff + i * phentsize;
        let p_type_val = read_u32_le(data, off + P_TYPE);

        if p_type_val != PT_LOAD {
            continue;
        }

        let seg_offset = read_u64_le(data, off + P_OFFSET) as usize;
        let vaddr = read_u64_le(data, off + P_VADDR);
        let paddr = read_u64_le(data, off + P_PADDR);
        let filesz = read_u64_le(data, off + P_FILESZ) as usize;
        let memsz = read_u64_le(data, off + P_MEMSZ) as usize;
        let flags = read_u32_le(data, off + P_FLAGS);

        log::info!(
            "  LOAD[{}]: paddr={:#x} vaddr={:#x} filesz={:#x} memsz={:#x} flags={:#x}",
            i, paddr, vaddr, filesz, memsz, flags
        );

        let seg_end = paddr.checked_add(memsz as u64).unwrap_or(u64::MAX);
        if seg_end > KERNEL_LOAD_PHYS_LIMIT {
            log::error!(
                "ELF: segment at {:#x} (end {:#x}) exceeds physical load limit {:#x} ({} MiB)",
                paddr, seg_end, KERNEL_LOAD_PHYS_LIMIT,
                KERNEL_LOAD_PHYS_LIMIT / (1024 * 1024)
            );
            return None;
        }

        if !addr_in_any_region(paddr, memsz as u64, memory_regions, region_count) {
            log::error!(
                "ELF: segment at {:#x}..{:#x} does not fall within any available memory region",
                paddr, seg_end
            );
            return None;
        }

        if seg_offset + filesz > data.len() {
            log::error!("ELF: segment data out of bounds");
            return None;
        }

        let dst = paddr as *mut u8;
        unsafe {
            core::ptr::copy_nonoverlapping(data[seg_offset..].as_ptr(), dst, filesz);
            if memsz > filesz {
                core::ptr::write_bytes(dst.add(filesz), 0, memsz - filesz);
            }
        }
    }

    log::info!("ELF: loaded, entry={:#x}", entry);
    Some(entry)
}

fn read_u16_le(data: &[u8], offset: usize) -> u16 {
    if offset + 1 >= data.len() {
        panic!("read_u16_le: offset {} out of bounds (len {})", offset, data.len());
    }
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    if offset + 3 >= data.len() {
        panic!("read_u32_le: offset {} out of bounds (len {})", offset, data.len());
    }
    u32::from_le_bytes([data[offset], data[offset + 1], data[offset + 2], data[offset + 3]])
}

fn read_u64_le(data: &[u8], offset: usize) -> u64 {
    if offset + 7 >= data.len() {
        panic!("read_u64_le: offset {} out of bounds (len {})", offset, data.len());
    }
    u64::from_le_bytes([
        data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
        data[offset + 4], data[offset + 5], data[offset + 6], data[offset + 7],
    ])
}

// ---- ext4 on-disk structures ----

#[derive(Clone, Copy)]
#[repr(C, packed)]
struct Superblock {
    s_inodes_count: u32,
    s_blocks_count_lo: u32,
    s_r_blocks_count_lo: u32,
    s_free_blocks_count_lo: u32,
    s_free_inodes_count: u32,
    s_first_data_block: u32,
    s_log_block_size: u32,
    s_log_cluster_size: u32,
    s_blocks_per_group: u32,
    s_clusters_per_group: u32,
    s_inodes_per_group: u32,
    s_mtime: u32,
    s_wtime: u32,
    s_mnt_count: u16,
    s_max_mnt_count: u16,
    s_magic: u16,
    s_state: u16,
    s_errors: u16,
    s_minor_rev_level: u16,
    s_lastcheck: u32,
    s_checkinterval: u32,
    s_creator_os: u32,
    s_rev_level: u32,
    s_def_resuid: u16,
    s_def_resgid: u16,
    // Extended superblock fields (rev_level >= 1)
    s_first_ino: u32,
    s_inode_size: u16,
    s_block_group_nr: u16,
    s_feature_compat: u32,
    s_feature_incompat: u32,
    s_feature_ro_compat: u32,
}

const EXT4_FEATURE_RO_COMPAT_64BIT: u32 = 0x80;

const EXT4_64BIT_BGDT_SIZE: usize = 64;
const EXT4_32BIT_BGDT_SIZE: usize = 32;

const SB_MAGIC: u16 = 0xEF53;
const SB_OFFSET: u64 = 1024;

#[derive(Clone, Copy, Default)]
#[repr(C, packed)]
struct BlockGroupDesc {
    bg_block_bitmap_lo: u32,
    bg_inode_bitmap_lo: u32,
    bg_inode_table_lo: u32,
    bg_free_blocks_count_lo: u16,
    bg_free_inodes_count_lo: u16,
    bg_used_dirs_count_lo: u16,
    bg_flags: u16,
    bg_exclude_bitmap_lo: u32,
    bg_block_bitmap_csum_lo: u16,
    bg_inode_bitmap_csum_lo: u16,
    bg_itable_unused_lo: u16,
    bg_checksum: u16,
}

#[derive(Clone, Copy, Default)]
#[repr(C, packed)]
struct Inode {
    i_mode: u16,
    i_uid: u16,
    i_size_lo: u32,
    i_atime: u32,
    i_ctime: u32,
    i_mtime: u32,
    i_dtime: u32,
    i_gid: u16,
    i_links_count: u16,
    i_blocks_lo: u32,
    i_flags: u32,
    i_osd1: u32,
    i_block: [u32; 15],
    i_generation: u32,
    i_file_acl_lo: u32,
    i_size_high: u32,
}

#[derive(Clone, Copy, Default)]
#[repr(C, packed)]
struct DirEntry {
    inode: u32,
    rec_len: u16,
    name_len: u8,
    file_type: u8,
}

struct ExtentEntry {
    ee_block: u32,
    ee_len: u32,
    ee_start: u64,
}

// ---- UEFI Block I/O sector reader with cache ----

/// Number of cached sectors for the block cache. 8 entries covers the
/// ext4 superblock, block group descriptor table, and inode table blocks
/// in a typical workload — eliminating repeated UEFI BlockIO calls.
const CACHE_SIZE: usize = 8;

struct SectorReader {
    proto: ScopedProtocol<BlockIO>,
    block_size: u32,
    media_id: u32,
    partition_offset: u64,
    /// Direct-mapped cache: `cache[idx]` where `idx = sector % CACHE_SIZE`.
    /// Each entry stores the sector number and its 512-byte data.
    cache_sectors: [u64; CACHE_SIZE],
    cache_data: [[u8; SECTOR_SIZE]; CACHE_SIZE],
}

impl SectorReader {
    fn new(handle: uefi::Handle, partition_offset: u64) -> Option<Self> {
        let proto = boot::open_protocol_exclusive::<BlockIO>(handle).ok()?;
        let block_size = proto.media().block_size();
        let media_id = proto.media().media_id();
        Some(Self {
            proto,
            block_size,
            media_id,
            partition_offset,
            cache_sectors: [u64::MAX; CACHE_SIZE],
            cache_data: [[0u8; SECTOR_SIZE]; CACHE_SIZE],
        })
    }

    fn read_sector(&mut self, sector: u64) -> Option<()> {
        let idx = (sector as usize) % CACHE_SIZE;
        if self.cache_sectors[idx] == sector {
            return Some(());
        }
        let absolute_sector = self.partition_offset + sector;
        let block_size = self.block_size as u64;
        let byte_offset = absolute_sector * SECTOR_SIZE as u64;
        let start_block = byte_offset / block_size;
        let block_offset = (byte_offset % block_size) as usize;

        if block_size >= SECTOR_SIZE as u64 {
            let mut block_buf = alloc::vec![0u8; block_size as usize];
            self.proto.read_blocks(self.media_id, start_block, &mut block_buf).ok()?;
            let copy_len = SECTOR_SIZE.min(block_size as usize - block_offset);
            self.cache_data[idx][..copy_len].copy_from_slice(&block_buf[block_offset..block_offset + copy_len]);
        } else {
            let blocks_needed = (SECTOR_SIZE as u64 + block_size - 1) / block_size;
            let mut block_buf = alloc::vec![0u8; (blocks_needed * block_size) as usize];
            self.proto.read_blocks(self.media_id, start_block, &mut block_buf).ok()?;
            let copy_len = SECTOR_SIZE.min(block_buf.len() - block_offset);
            self.cache_data[idx][..copy_len].copy_from_slice(&block_buf[block_offset..block_offset + copy_len]);
        }
        self.cache_sectors[idx] = sector;
        Some(())
    }
}

// ---- GPT scanning ----

fn read_sector_raw(handle: uefi::Handle, sector: u64, buf: &mut [u8; SECTOR_SIZE]) -> Option<()> {
    let proto = boot::open_protocol_exclusive::<BlockIO>(handle).ok()?;
    let media = proto.media();
    let block_size = media.block_size() as u64;
    let media_id = media.media_id();
    let byte_offset = sector * SECTOR_SIZE as u64;
    let start_block = byte_offset / block_size;
    let block_offset = (byte_offset % block_size) as usize;

    if block_size >= SECTOR_SIZE as u64 {
        let mut block_buf = alloc::vec![0u8; block_size as usize];
        proto.read_blocks(media_id, start_block, &mut block_buf).ok()?;
        let copy_len = SECTOR_SIZE.min(block_size as usize - block_offset);
        buf[..copy_len].copy_from_slice(&block_buf[block_offset..block_offset + copy_len]);
    } else {
        let blocks_needed = (SECTOR_SIZE as u64 + block_size - 1) / block_size;
        let mut block_buf = alloc::vec![0u8; (blocks_needed * block_size) as usize];
        proto.read_blocks(media_id, start_block, &mut block_buf).ok()?;
        let copy_len = SECTOR_SIZE.min(block_buf.len() - block_offset);
        buf[..copy_len].copy_from_slice(&block_buf[block_offset..block_offset + copy_len]);
    }
    Some(())
}

fn try_handle_for_ext4(handle: uefi::Handle, buf: &mut [u8; SECTOR_SIZE]) -> Option<u64> {
    // Read protective MBR (LBA 0)
    read_sector_raw(handle, 0, buf)?;

    // Read GPT header (LBA 1)
    read_sector_raw(handle, 1, buf)?;

    if &buf[0..8] != b"EFI PART" {
        log::info!("Not a GPT disk on this handle, trying next...");
        return None;
    }

    let entries_lba = u64::from_le_bytes(buf[72..80].try_into().unwrap());
    let num_entries = u32::from_le_bytes(buf[80..84].try_into().unwrap());
    let entry_size = u32::from_le_bytes(buf[84..88].try_into().unwrap()) as usize;

    let entries_per_sector = SECTOR_SIZE / entry_size;
    let sectors_needed = (num_entries as usize + entries_per_sector - 1) / entries_per_sector;

    for s in 0..sectors_needed {
        read_sector_raw(handle, entries_lba + s as u64, buf)?;
        for i in 0..entries_per_sector {
            let off = i * entry_size;
            if off + entry_size > SECTOR_SIZE {
                break;
            }
            let type_guid = &buf[off..off + 16];
            let guid_words: [u16; 8] = core::array::from_fn(|j| {
                u16::from_le_bytes([type_guid[j * 2], type_guid[j * 2 + 1]])
            });
            if guid_words == EXT4_GUID {
                let first_lba = u64::from_le_bytes(buf[off + 32..off + 40].try_into().unwrap());
                let last_lba = u64::from_le_bytes(buf[off + 40..off + 48].try_into().unwrap());
                log::info!("ext4 partition: handle={:?} LBA {}..{}", handle, first_lba, last_lba);
                return Some(first_lba);
            }
        }
    }
    None
}

fn find_ext4_partition() -> Option<(uefi::Handle, u64)> {
    let handles = boot::find_handles::<BlockIO>().ok()?;
    let mut buf = [0u8; SECTOR_SIZE];

    for &handle in handles.iter() {
        if let Some(lba) = try_handle_for_ext4(handle, &mut buf) {
            return Some((handle, lba));
        }
    }

    log::error!("ext4 partition not found on any block device");
    None
}

// ---- ext4 reading ----

fn read_ext4_sectors(
    reader: &mut SectorReader,
    sector: u64,
    buf: &mut [u8],
) -> Option<()> {
    let bytes_needed = buf.len();
    if bytes_needed == 0 {
        return Some(());
    }
    let absolute_sector = reader.partition_offset + sector;
    let block_size = reader.block_size as u64;
    let byte_offset = absolute_sector * SECTOR_SIZE as u64;
    let start_block = byte_offset / block_size;

    if block_size == SECTOR_SIZE as u64 {
        let aligned = (bytes_needed + SECTOR_SIZE - 1) / SECTOR_SIZE * SECTOR_SIZE;
        let mut aligned_buf = alloc::vec![0u8; aligned];
        // Do NOT pre-fill aligned_buf from `buf` — the buffer is a destination,
        // not a source. The UEFI Block I/O read populates aligned_buf directly;
        // copying from the uninitialised destination would only seed it with
        // zeros at best, and would let stale call-site data leak into the read.
        if reader.proto.read_blocks(reader.media_id, start_block, &mut aligned_buf).is_err() {
            log::error!("read_ext4_sectors: read_blocks failed at LBA {} size {}", start_block, aligned);
            return None;
        }
        buf[..bytes_needed].copy_from_slice(&aligned_buf[..bytes_needed]);
        return Some(());
    }

    let full_sectors = bytes_needed / SECTOR_SIZE;
    let remainder = bytes_needed % SECTOR_SIZE;
    let total_sectors = full_sectors + if remainder > 0 { 1 } else { 0 };
    for i in 0..total_sectors as u64 {
        let bs = byte_offset + i * SECTOR_SIZE as u64;
        let start = bs / block_size;
        let boff_inner = (bs % block_size) as usize;
        let mut block_buf = alloc::vec![0u8; block_size as usize];
        reader.proto.read_blocks(reader.media_id, start, &mut block_buf).ok()?;
        let dst = i as usize * SECTOR_SIZE;
        let copy_len = SECTOR_SIZE.min(bytes_needed - dst);
        buf[dst..dst + copy_len].copy_from_slice(&block_buf[boff_inner..boff_inner + copy_len]);
    }
    Some(())
}

fn read_inode(
    reader: &mut SectorReader,
    sb: &Superblock,
    block_group_descs: &[BlockGroupDesc],
    inode_num: u32,
) -> Option<Inode> {
    let inodes_per_group = sb.s_inodes_per_group;
    if inode_num == 0 || inodes_per_group == 0 {
        log::error!("read_inode: invalid inode_num={} inodes_per_group={}", inode_num, inodes_per_group);
        return None;
    }

    let inode_size = if sb.s_rev_level >= 1 && sb.s_inode_size != 0 {
        sb.s_inode_size as u32
    } else {
        128u32
    };

    let group = (inode_num - 1) / inodes_per_group;
    let index = (inode_num - 1) % inodes_per_group;
    if group as usize >= block_group_descs.len() {
        log::error!(
            "read_inode: group {} >= block_group_descs.len() {}",
            group, block_group_descs.len()
        );
        return None;
    }
    let desc = block_group_descs[group as usize];

    let block_size = 1024u64 << sb.s_log_block_size;
    let inode_table_sector = desc.bg_inode_table_lo as u64 * block_size / SECTOR_SIZE as u64;
    let inode_offset = index as u64 * inode_size as u64;
    let sector = inode_table_sector + (inode_offset / SECTOR_SIZE as u64);
    let byte_in_sector = (inode_offset % SECTOR_SIZE as u64) as usize;

    let read_size = inode_size as usize;
    let mut raw = alloc::vec![0u8; read_size];
    let mut offset = 0usize;
    let mut current_sector = sector;
    while offset < read_size {
        let mut sector_buf = [0u8; SECTOR_SIZE];
        read_ext4_sectors(reader, current_sector, &mut sector_buf)?;
        let src_start = if current_sector == sector { byte_in_sector } else { 0 };
        let copy_len = (SECTOR_SIZE - src_start).min(read_size - offset);
        raw[offset..offset + copy_len].copy_from_slice(&sector_buf[src_start..src_start + copy_len]);
        offset += copy_len;
        current_sector += 1;
    }

    let i_block = core::array::from_fn(|i| {
        let off = 40 + i * 4;
        u32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]])
    });

    Some(Inode {
        i_mode: u16::from_le_bytes([raw[0], raw[1]]),
        i_uid: u16::from_le_bytes([raw[2], raw[3]]),
        i_size_lo: u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]),
        i_atime: u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]),
        i_ctime: u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]),
        i_mtime: u32::from_le_bytes([raw[16], raw[17], raw[18], raw[19]]),
        i_dtime: u32::from_le_bytes([raw[20], raw[21], raw[22], raw[23]]),
        i_gid: u16::from_le_bytes([raw[24], raw[25]]),
        i_links_count: u16::from_le_bytes([raw[26], raw[27]]),
        i_blocks_lo: u32::from_le_bytes([raw[28], raw[29], raw[30], raw[31]]),
        i_flags: u32::from_le_bytes([raw[32], raw[33], raw[34], raw[35]]),
        i_osd1: u32::from_le_bytes([raw[36], raw[37], raw[38], raw[39]]),
        i_block,
        i_generation: u32::from_le_bytes([raw[100], raw[101], raw[102], raw[103]]),
        i_file_acl_lo: u32::from_le_bytes([raw[104], raw[105], raw[106], raw[107]]),
        i_size_high: u32::from_le_bytes([raw[108], raw[109], raw[110], raw[111]]),
    })
}

const EXT4_EXTENTS_FL: u32 = 0x80000;

const EXTENT_HEADER_SIZE: usize = 12;
const EXTENT_ENTRY_SIZE: usize = 12;

fn read_extent_block(
    reader: &mut SectorReader,
    sb: &Superblock,
    block: u64,
    buf: &mut [u8; 4096],
) -> Option<()> {
    let block_size = 1024u64 << sb.s_log_block_size;
    let sectors_per_block = block_size / SECTOR_SIZE as u64;
    let sector = block * sectors_per_block;
    let byte_count = block_size.min(buf.len() as u64) as usize;
    read_ext4_sectors(reader, sector, &mut buf[..byte_count as usize])
}

fn collect_leaf_extents(
    reader: &mut SectorReader,
    sb: &Superblock,
    block: u64,
    extents: &mut Vec<ExtentEntry>,
) -> Option<()> {
    let mut raw = [0u8; 4096];
    read_extent_block(reader, sb, block, &mut raw)?;

    let magic = u16::from_le_bytes([raw[0], raw[1]]);
    let entries = u16::from_le_bytes([raw[2], raw[3]]);
    let depth = u16::from_le_bytes([raw[6], raw[7]]);

    if magic != 0xF30A {
        return None;
    }

    if depth == 0 {
        for i in 0..entries as usize {
            let off = EXTENT_HEADER_SIZE + i * EXTENT_ENTRY_SIZE;
            if off + EXTENT_ENTRY_SIZE > 4096 {
                break;
            }
            let ee_block = u32::from_le_bytes(raw[off..off + 4].try_into().unwrap());
            let raw_len = u16::from_le_bytes(raw[off + 4..off + 6].try_into().unwrap());
            let ee_start_hi = u16::from_le_bytes(raw[off + 6..off + 8].try_into().unwrap());
            let ee_start_lo = u32::from_le_bytes(raw[off + 8..off + 12].try_into().unwrap());
            let ee_len = (raw_len & 0x7FFF) as u32;
            let ee_start = (ee_start_hi as u64) << 32 | ee_start_lo as u64;
            extents.push(ExtentEntry { ee_block, ee_len, ee_start });
        }
        Some(())
    } else {
        for i in 0..entries as usize {
            let off = EXTENT_HEADER_SIZE + i * EXTENT_ENTRY_SIZE;
            if off + EXTENT_ENTRY_SIZE > 4096 {
                break;
            }
            let ei_leaf_lo = u32::from_le_bytes(raw[off + 4..off + 8].try_into().unwrap());
            let ei_leaf_hi = u16::from_le_bytes(raw[off + 8..off + 10].try_into().unwrap());
            let child_block = (ei_leaf_hi as u64) << 32 | ei_leaf_lo as u64;
            collect_leaf_extents(reader, sb, child_block, extents)?;
        }
        Some(())
    }
}

fn parse_extents(
    reader: &mut SectorReader,
    sb: &Superblock,
    inode: &Inode,
) -> Option<Vec<ExtentEntry>> {
    let mut raw = [0u8; 60];
    unsafe {
        core::ptr::copy_nonoverlapping(
            core::ptr::addr_of!(inode.i_block) as *const u8,
            raw.as_mut_ptr(),
            60,
        );
    }
    let magic = u16::from_le_bytes([raw[0], raw[1]]);
    let depth = u16::from_le_bytes([raw[6], raw[7]]);
    if magic != 0xF30A {
        log::error!("parse_extents: bad magic {:#x}", magic);
        return None;
    }

    if depth == 0 {
        let entries = u16::from_le_bytes([raw[2], raw[3]]);
        let mut extents = Vec::new();
        for i in 0..entries as usize {
            let off = EXTENT_HEADER_SIZE + i * EXTENT_ENTRY_SIZE;
            if off + EXTENT_ENTRY_SIZE > 60 {
                break;
            }
            let ee_block = u32::from_le_bytes(raw[off..off + 4].try_into().unwrap());
            let raw_len = u16::from_le_bytes(raw[off + 4..off + 6].try_into().unwrap());
            let ee_start_hi = u16::from_le_bytes(raw[off + 6..off + 8].try_into().unwrap());
            let ee_start_lo = u32::from_le_bytes(raw[off + 8..off + 12].try_into().unwrap());
            let ee_len = (raw_len & 0x7FFF) as u32;
            let ee_start = (ee_start_hi as u64) << 32 | ee_start_lo as u64;
            extents.push(ExtentEntry { ee_block, ee_len, ee_start });
        }
        log::info!("parse_extents: {} entries", extents.len());
        Some(extents)
    } else {
        let entries = u16::from_le_bytes([raw[2], raw[3]]);
        let mut extents = Vec::new();
        for i in 0..entries as usize {
            let off = EXTENT_HEADER_SIZE + i * EXTENT_ENTRY_SIZE;
            if off + EXTENT_ENTRY_SIZE > 60 {
                break;
            }
            let ei_leaf_lo = u32::from_le_bytes(raw[off + 4..off + 8].try_into().unwrap());
            let ei_leaf_hi = u16::from_le_bytes(raw[off + 8..off + 10].try_into().unwrap());
            let child_block = (ei_leaf_hi as u64) << 32 | ei_leaf_lo as u64;
            collect_leaf_extents(reader, sb, child_block, &mut extents)?;
        }
        log::info!("parse_extents: {} entries (depth {})", extents.len(), depth);
        Some(extents)
    }
}

fn resolve_block_extents(
    reader: &mut SectorReader,
    sb: &Superblock,
    inode: &Inode,
    logical_block: u32,
) -> Option<u32> {
    let mut raw_i_block = [0u8; 60];
    unsafe {
        core::ptr::copy_nonoverlapping(
            core::ptr::addr_of!(inode.i_block) as *const u8,
            raw_i_block.as_mut_ptr(),
            60,
        );
    }

    let eh_magic = u16::from_le_bytes([raw_i_block[0], raw_i_block[1]]);
    if eh_magic != 0xF30A {
        log::error!("Extent header bad magic: {:#x}", eh_magic);
        return None;
    }

    resolve_extent_in_block(reader, sb, &raw_i_block, logical_block)
}

fn resolve_extent_in_block(
    reader: &mut SectorReader,
    sb: &Superblock,
    block_data: &[u8],
    logical_block: u32,
) -> Option<u32> {
    let eh_entries = u16::from_le_bytes([block_data[2], block_data[3]]);
    let eh_depth = u16::from_le_bytes([block_data[6], block_data[7]]);

    if eh_depth == 0 {
        for i in 0..eh_entries as usize {
            let off = EXTENT_HEADER_SIZE + i * EXTENT_ENTRY_SIZE;
            if off + EXTENT_ENTRY_SIZE > block_data.len() {
                break;
            }
            let ee_block = u32::from_le_bytes(block_data[off..off + 4].try_into().unwrap());
            let raw_len = u16::from_le_bytes(block_data[off + 4..off + 6].try_into().unwrap());
            let ee_start_hi = u16::from_le_bytes(block_data[off + 6..off + 8].try_into().unwrap());
            let ee_start_lo = u32::from_le_bytes(block_data[off + 8..off + 12].try_into().unwrap());
            let len = (raw_len & 0x7FFF) as u32;
            if logical_block >= ee_block && logical_block < ee_block + len {
                let physical = (ee_start_hi as u64) << 32 | ee_start_lo as u64;
                return Some((physical + (logical_block - ee_block) as u64) as u32);
            }
        }
        log::error!("Extent leaf: block {} not found in {} entries", logical_block, eh_entries);
        None
    } else {
        for i in 0..eh_entries as usize {
            let off = EXTENT_HEADER_SIZE + i * EXTENT_ENTRY_SIZE;
            if off + EXTENT_ENTRY_SIZE > block_data.len() {
                break;
            }
            let ei_block = u32::from_le_bytes(block_data[off..off + 4].try_into().unwrap());
            let ei_leaf_lo = u32::from_le_bytes(block_data[off + 4..off + 8].try_into().unwrap());
            let ei_leaf_hi = u16::from_le_bytes(block_data[off + 8..off + 10].try_into().unwrap());
            let child_block = (ei_leaf_hi as u64) << 32 | ei_leaf_lo as u64;

            let next_block = if i + 1 < eh_entries as usize {
                let next_off = EXTENT_HEADER_SIZE + (i + 1) * EXTENT_ENTRY_SIZE;
                if next_off + 4 <= block_data.len() {
                    Some(u32::from_le_bytes(block_data[next_off..next_off + 4].try_into().unwrap()))
                } else {
                    None
                }
            } else {
                None
            };

            let in_range = logical_block >= ei_block
                && (next_block.map_or(true, |nb| logical_block < nb));

            if in_range {
                let block_size = 1024u64 << sb.s_log_block_size;
                let mut child_raw = alloc::vec![0u8; block_size as usize];
                let sector = child_block * (block_size / SECTOR_SIZE as u64);
                read_ext4_sectors(reader, sector, &mut child_raw)?;
                return resolve_extent_in_block(reader, sb, &child_raw, logical_block);
            }
        }
        log::error!("Extent index: block {} not found in {} entries", logical_block, eh_entries);
        None
    }
}

fn resolve_block(
    reader: &mut SectorReader,
    sb: &Superblock,
    inode: &Inode,
    logical_block: u32,
) -> Option<u32> {
    let flags = inode.i_flags;
    if flags & EXT4_EXTENTS_FL != 0 {
        let r = resolve_block_extents(reader, sb, inode, logical_block);
        if r.is_none() {
            log::error!("resolve_block_extents returned None for logical_block {}", logical_block);
        }
        r
    } else if logical_block < 12 {
        let b = {
            let mut ib = [0u32; 15];
            unsafe { core::ptr::copy_nonoverlapping(core::ptr::addr_of!(inode.i_block) as *const u32, ib.as_mut_ptr(), 15); }
            ib[logical_block as usize]
        };
        if b == 0 {
            log::warn!("direct block {} is zero (logical_block={})", logical_block, logical_block);
        }
        Some(b)
    } else {
        // Traditional indirect block map (just single indirect for now)
        let block_size = 1024u64 << sb.s_log_block_size;
        let entries_per_block = (block_size / 4) as usize;
        let indirect_idx = logical_block as usize - 12;
        if indirect_idx < entries_per_block && inode.i_block[12] != 0 {
            let mut indirect_buf = alloc::vec![0u8; block_size as usize];
            let sector = inode.i_block[12] as u64 * (block_size / SECTOR_SIZE as u64);
            read_ext4_sectors(reader, sector, &mut indirect_buf)?;
            Some(u32::from_le_bytes(indirect_buf[indirect_idx * 4..indirect_idx * 4 + 4].try_into().unwrap()))
        } else {
            None
        }
    }
}

fn read_data_block(
    reader: &mut SectorReader,
    sb: &Superblock,
    block: u32,
    buf: &mut [u8],
) -> Option<()> {
    if block == 0 {
        return Some(());
    }
    let block_size = 1024u64 << sb.s_log_block_size;
    let sectors_per_block = block_size / SECTOR_SIZE as u64;
    let sector = block as u64 * sectors_per_block;
    read_ext4_sectors(reader, sector, buf)
}

fn read_file_into(
    reader: &mut SectorReader,
    sb: &Superblock,
    inode: &Inode,
    data: &mut [u8],
) -> Option<()> {
    let block_size = 1024u64 << sb.s_log_block_size;
    let file_size = data.len();

    if inode.i_flags & EXT4_EXTENTS_FL != 0 {
        if let Some(extents) = parse_extents(reader, sb, inode) {
            let sectors_per_block = block_size / SECTOR_SIZE as u64;
            for ext in &extents {
                let file_off = ext.ee_block as u64 * block_size;
                if file_off >= file_size as u64 {
                    break;
                }
                let extent_bytes = ext.ee_len as u64 * block_size;
                let copy_len = extent_bytes.min(file_size as u64 - file_off) as usize;
                if ext.ee_start == 0 {
                    continue;
                }
                let sector = ext.ee_start as u64 * sectors_per_block;
                read_ext4_sectors(reader, sector, &mut data[file_off as usize..][..copy_len])?;
            }
            return Some(());
        }
        log::warn!("extent parse failed, falling back to block-by-block");
    }

    let blocks_needed = (file_size as u64 + block_size - 1) / block_size;
    let mut offset = 0usize;
    for logical_block in 0..blocks_needed as u32 {
        let block = resolve_block(reader, sb, inode, logical_block)?;
        if block == 0 {
            offset = (offset + block_size as usize).min(file_size);
            continue;
        }
        let chunk = block_size as usize;
        let end = (offset + chunk).min(file_size);
        read_data_block(reader, sb, block, &mut data[offset..end])?;
        offset = end;
    }
    Some(())
}

fn read_file(
    reader: &mut SectorReader,
    sb: &Superblock,
    inode: &Inode,
) -> Option<Vec<u8>> {
    let file_size = (inode.i_size_high as u64) << 32 | (inode.i_size_lo as u64);
    log::info!("read_file: file_size={}", file_size);

    let mut data = alloc::vec![0u8; file_size as usize];
    read_file_into(reader, sb, inode, &mut data)?;
    log::info!("read_file: complete, {} bytes read", data.len());
    Some(data)
}

/// Load a file from the ext4 root directory by name via UEFI Block I/O.
pub fn load_file_from_ext4(filename: &[u8]) -> Option<Vec<u8>> {
    log::info!("Finding ext4 partition...");
    let (ext4_handle, partition_lba) = find_ext4_partition()?;

    let mut reader = SectorReader::new(ext4_handle, partition_lba)?;

    // Read superblock (at byte 1024 of the partition)
    let mut sb_raw = [0u8; SECTOR_SIZE];
    read_ext4_sectors(&mut reader, SB_OFFSET / SECTOR_SIZE as u64, &mut sb_raw)?;

    let s_rev_level = u32::from_le_bytes(sb_raw[76..80].try_into().unwrap());
    let (s_inode_size, s_feature_ro_compat) = if s_rev_level >= 1 {
        (
            u16::from_le_bytes(sb_raw[88..90].try_into().unwrap()),
            u32::from_le_bytes(sb_raw[100..104].try_into().unwrap()),
        )
    } else {
        (128u16, 0u32)
    };

    let sb = Superblock {
        s_inodes_count: u32::from_le_bytes(sb_raw[0..4].try_into().unwrap()),
        s_blocks_count_lo: u32::from_le_bytes(sb_raw[4..8].try_into().unwrap()),
        s_r_blocks_count_lo: u32::from_le_bytes(sb_raw[8..12].try_into().unwrap()),
        s_free_blocks_count_lo: u32::from_le_bytes(sb_raw[12..16].try_into().unwrap()),
        s_free_inodes_count: u32::from_le_bytes(sb_raw[16..20].try_into().unwrap()),
        s_first_data_block: u32::from_le_bytes(sb_raw[20..24].try_into().unwrap()),
        s_log_block_size: u32::from_le_bytes(sb_raw[24..28].try_into().unwrap()),
        s_log_cluster_size: u32::from_le_bytes(sb_raw[28..32].try_into().unwrap()),
        s_blocks_per_group: u32::from_le_bytes(sb_raw[32..36].try_into().unwrap()),
        s_clusters_per_group: u32::from_le_bytes(sb_raw[36..40].try_into().unwrap()),
        s_inodes_per_group: u32::from_le_bytes(sb_raw[40..44].try_into().unwrap()),
        s_mtime: u32::from_le_bytes(sb_raw[44..48].try_into().unwrap()),
        s_wtime: u32::from_le_bytes(sb_raw[48..52].try_into().unwrap()),
        s_mnt_count: u16::from_le_bytes(sb_raw[52..54].try_into().unwrap()),
        s_max_mnt_count: u16::from_le_bytes(sb_raw[54..56].try_into().unwrap()),
        s_magic: u16::from_le_bytes(sb_raw[56..58].try_into().unwrap()),
        s_state: u16::from_le_bytes(sb_raw[58..60].try_into().unwrap()),
        s_errors: u16::from_le_bytes(sb_raw[60..62].try_into().unwrap()),
        s_minor_rev_level: u16::from_le_bytes(sb_raw[62..64].try_into().unwrap()),
        s_lastcheck: u32::from_le_bytes(sb_raw[64..68].try_into().unwrap()),
        s_checkinterval: u32::from_le_bytes(sb_raw[68..72].try_into().unwrap()),
        s_creator_os: u32::from_le_bytes(sb_raw[72..76].try_into().unwrap()),
        s_rev_level,
        s_def_resuid: u16::from_le_bytes(sb_raw[80..82].try_into().unwrap()),
        s_def_resgid: u16::from_le_bytes(sb_raw[82..84].try_into().unwrap()),
        s_first_ino: u32::from_le_bytes(sb_raw[84..88].try_into().unwrap()),
        s_inode_size,
        s_block_group_nr: u16::from_le_bytes(sb_raw[90..92].try_into().unwrap()),
        s_feature_compat: u32::from_le_bytes(sb_raw[92..96].try_into().unwrap()),
        s_feature_incompat: u32::from_le_bytes(sb_raw[96..100].try_into().unwrap()),
        s_feature_ro_compat,
    };

    let magic = sb.s_magic;
    if magic != SB_MAGIC {
        log::error!("Not ext4 (magic={:#x})", magic);
        return None;
    }

    let block_size = 1024u64 << sb.s_log_block_size;
    log::info!("ext4: block_size={} magic={:#x}", block_size, magic);

    // Compute number of block groups first (needed to size the BGDT read)
    let num_groups =
        ((sb.s_blocks_count_lo as u64 + sb.s_blocks_per_group as u64 - 1) / sb.s_blocks_per_group as u64)
            as usize;
    let bgdt_64bit = (sb.s_feature_ro_compat & EXT4_FEATURE_RO_COMPAT_64BIT) != 0;
    let bgdt_entry_size = if bgdt_64bit { EXT4_64BIT_BGDT_SIZE } else { EXT4_32BIT_BGDT_SIZE };
    let bgdt_bytes = num_groups * bgdt_entry_size;

    // Read block group descriptor table (starts at block after superblock)
    let bgdt_block = if sb.s_log_block_size == 0 { 2 } else { 1 };
    let bgdt_sector = bgdt_block as u64 * block_size / SECTOR_SIZE as u64;
    let bgdt_aligned = (bgdt_bytes + SECTOR_SIZE - 1) / SECTOR_SIZE * SECTOR_SIZE;
    let mut bgdt_raw = alloc::vec![0u8; bgdt_aligned];
    read_ext4_sectors(&mut reader, bgdt_sector, &mut bgdt_raw)?;

    let mut block_group_descs = alloc::vec![BlockGroupDesc::default(); num_groups];
    for i in 0..num_groups {
        let off = i * bgdt_entry_size;
        if off + bgdt_entry_size > bgdt_raw.len() {
            break;
        }
        let bitmap_hi;
        let itable_hi;
        if bgdt_64bit && off + 64 <= bgdt_raw.len() {
            bitmap_hi = u64::from(u32::from_le_bytes(bgdt_raw[off + 32..off + 36].try_into().unwrap())) << 32;
            itable_hi = u64::from(u32::from_le_bytes(bgdt_raw[off + 40..off + 44].try_into().unwrap())) << 32;
        } else {
            bitmap_hi = 0;
            itable_hi = 0;
        }

        let block_bitmap_lo = u32::from_le_bytes(bgdt_raw[off..off + 4].try_into().unwrap());
        let inode_bitmap_lo = u32::from_le_bytes(bgdt_raw[off + 4..off + 8].try_into().unwrap());
        let inode_table_lo = u32::from_le_bytes(bgdt_raw[off + 8..off + 12].try_into().unwrap());

        block_group_descs[i] = BlockGroupDesc {
            bg_block_bitmap_lo: ((bitmap_hi | u64::from(block_bitmap_lo)) & 0xFFFF_FFFF) as u32,
            bg_inode_bitmap_lo: ((bitmap_hi | u64::from(inode_bitmap_lo)) & 0xFFFF_FFFF) as u32,
            bg_inode_table_lo: (itable_hi | u64::from(inode_table_lo)) as u32,
            bg_free_blocks_count_lo: u16::from_le_bytes(
                bgdt_raw[off + 12..off + 14].try_into().unwrap(),
            ),
            bg_free_inodes_count_lo: u16::from_le_bytes(
                bgdt_raw[off + 14..off + 16].try_into().unwrap(),
            ),
            bg_used_dirs_count_lo: u16::from_le_bytes(
                bgdt_raw[off + 16..off + 18].try_into().unwrap(),
            ),
            bg_flags: u16::from_le_bytes(bgdt_raw[off + 18..off + 20].try_into().unwrap()),
            bg_exclude_bitmap_lo: u32::from_le_bytes(
                bgdt_raw[off + 20..off + 24].try_into().unwrap(),
            ),
            bg_block_bitmap_csum_lo: u16::from_le_bytes(
                bgdt_raw[off + 24..off + 26].try_into().unwrap(),
            ),
            bg_inode_bitmap_csum_lo: u16::from_le_bytes(
                bgdt_raw[off + 26..off + 28].try_into().unwrap(),
            ),
            bg_itable_unused_lo: u16::from_le_bytes(
                bgdt_raw[off + 28..off + 30].try_into().unwrap(),
            ),
            bg_checksum: u16::from_le_bytes(bgdt_raw[off + 30..off + 32].try_into().unwrap()),
        };
    }

    // Read root inode (inode 2)
    log::info!("Reading root inode...");
    let root_inode = read_inode(&mut reader, &sb, &block_group_descs, 2)?;

    // Read root directory blocks
    let filename_str = core::str::from_utf8(filename).unwrap_or("???");
    log::info!("Scanning root directory for {}...", filename_str);
    let root_size = (root_inode.i_size_high as u64) << 32 | (root_inode.i_size_lo as u64);
    let mut root_data = alloc::vec![0u8; root_size as usize];
    read_file_into(&mut reader, &sb, &root_inode, &mut root_data)?;

    // Find file in directory entries
    let mut file_inode_num = 0u32;
    let mut dir_off = 0usize;
    while dir_off + 8 <= root_data.len() {
        let entry = DirEntry {
            inode: u32::from_le_bytes(root_data[dir_off..dir_off + 4].try_into().unwrap()),
            rec_len: u16::from_le_bytes(root_data[dir_off + 4..dir_off + 6].try_into().unwrap()),
            name_len: root_data[dir_off + 6],
            file_type: root_data[dir_off + 7],
        };

        // rec_len == 0 means a corrupt entry; we cannot advance safely. Stop
        // the walk to avoid infinite loops.  An entry with inode == 0 and a
        // non-zero rec_len is an unused/deleted slot — skip it.
        if entry.rec_len == 0 {
            log::warn!("Root directory walk: zero rec_len at offset {}, stopping", dir_off);
            break;
        }

        if entry.inode != 0 {
            let name_start = dir_off + 8;
            let name_end = name_start + entry.name_len as usize;
            if name_end <= root_data.len() {
                let name = &root_data[name_start..name_end];
                if name == filename {
                    file_inode_num = entry.inode;
                    log::info!("Found {} (inode {})", filename_str, file_inode_num);
                    break;
                }
            }
        }
        dir_off += entry.rec_len as usize;
    }

    if file_inode_num == 0 {
        log::error!("{} not found in root directory", filename_str);
        return None;
    }

    // Read file inode and file data
    log::info!("Reading {} data...", filename_str);
    let file_inode = read_inode(&mut reader, &sb, &block_group_descs, file_inode_num)?;
    let data = read_file(&mut reader, &sb, &file_inode)?;

    log::info!("{}: {} bytes loaded", filename_str, data.len());
    Some(data)
}

pub fn load_kernel_from_ext4() -> Option<Vec<u8>> {
    load_file_from_ext4(b"kernel.elf")
}
