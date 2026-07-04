#!/usr/bin/env python3
"""Create / update LodaxOS disk image: GPT with ext4 (Partition Zero) + ESP (FAT32).

Behavior:
  --full      Always recreate disk.img, ext4_part.img, esp_part.img from scratch.
  (default)   Incremental: if disk.img + cache partitions exist with expected sizes,
              update files in-place via debugfs/mcopy and splice into disk.img.
              Falls back to full creation when cache is missing or wrong size.
"""

import argparse
import binascii
import hashlib
import json
import os
import struct
import subprocess
import sys

SECTOR = 512
EXT4_GUID = bytes([0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4])
ESP_GUID = bytes([0xC1, 0x2A, 0x73, 0x28, 0xF8, 0x1F, 0x11, 0xD2, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B])
WSL_DISTRO = "Ubuntu"
DISK_MB = 600
EXT4_MB = 512
ESP_MB = 64
EXT4_FIRST_SECTOR = 2048
HASH_CACHE_FILE = ".disk_cache.json"


# ── helpers ─────────────────────────────────────────────────────

def crc32(data):
    return binascii.crc32(data) & 0xFFFFFFFF


def to_wsl(path):
    path = path.replace("\\", "/")
    if len(path) >= 2 and path[1] == ":":
        return f"/mnt/{path[0].lower()}{path[2:]}"
    return path


def run_wsl(script, **kwargs):
    distro = os.environ.get("LODAXOS_WSL_DISTRO", WSL_DISTRO)
    # Using 'bash -c' instead of 'bash -lc' to prevent .bashrc/.profile 
    # errors from breaking execution prematurely.
    try:
        subprocess.run(
            ["wsl.exe", "-d", distro, "--", "bash", "-c", script],
            check=True, 
            **kwargs
        )
    except subprocess.CalledProcessError as e:
        print(f"\n[ERROR] WSL command failed with exit code {e.returncode}.", file=sys.stderr)
        print("Ensure packages 'e2fsprogs' and 'mtools' are installed in your WSL distro.", file=sys.stderr)
        raise e


def file_md5(path):
    h = hashlib.md5()
    with open(path, "rb") as f:
        while True:
            chunk = f.read(65536)
            if not chunk:
                break
            h.update(chunk)
    return h.hexdigest()


def expected_size_mb(mb):
    return mb * 1024 * 1024


def part_entry(type_guid, unique_guid, first, last, name):
    entry = bytearray(128)
    entry[0:16] = type_guid
    entry[16:32] = unique_guid
    struct.pack_into("<Q", entry, 32, first)
    struct.pack_into("<Q", entry, 40, last)
    struct.pack_into("<Q", entry, 48, (last - first + 1) * SECTOR)
    encoded = name.encode("utf-16-le")
    entry[56:56 + len(encoded)] = encoded
    return entry


def write_gpt(disk, disk_sectors, ext4_first, ext4_last, esp_first, esp_last):
    entries_lba = 2
    disk_guid = os.urandom(16)
    entries = bytearray(128 * 128)
    entries[0:128] = part_entry(EXT4_GUID, os.urandom(16), ext4_first, ext4_last, "Partition Zero")
    entries[128:256] = part_entry(ESP_GUID, os.urandom(16), esp_first, esp_last, "ESP")
    disk[entries_lba * SECTOR:entries_lba * SECTOR + len(entries)] = entries

    hdr = bytearray(SECTOR)
    hdr[0:8] = b"EFI PART"
    struct.pack_into("<I", hdr, 8, 0x00010000)
    struct.pack_into("<I", hdr, 12, 92)
    struct.pack_into("<Q", hdr, 24, 1)
    struct.pack_into("<Q", hdr, 32, disk_sectors - 1)
    struct.pack_into("<Q", hdr, 40, 34)
    struct.pack_into("<Q", hdr, 48, disk_sectors - 34)
    hdr[56:72] = disk_guid
    struct.pack_into("<Q", hdr, 72, entries_lba)
    struct.pack_into("<I", hdr, 80, 128)
    struct.pack_into("<I", hdr, 84, 128)
    hdr[16:20] = b"\x00\x00\x00\x00"
    hdr[88:92] = b"\x00\x00\x00\x00"
    struct.pack_into("<I", hdr, 88, crc32(bytes(entries)))
    struct.pack_into("<I", hdr, 16, crc32(bytes(hdr[:92])))
    disk[SECTOR:2 * SECTOR] = hdr

    backup_entries_lba = disk_sectors - 33
    disk[backup_entries_lba * SECTOR:backup_entries_lba * SECTOR + len(entries)] = entries
    backup_hdr = bytearray(hdr)
    struct.pack_into("<Q", backup_hdr, 24, disk_sectors - 1)
    struct.pack_into("<Q", backup_hdr, 32, 1)
    struct.pack_into("<Q", backup_hdr, 72, backup_entries_lba)
    backup_hdr[16:20] = b"\x00\x00\x00\x00"
    struct.pack_into("<I", backup_hdr, 88, crc32(bytes(entries)))
    struct.pack_into("<I", backup_hdr, 16, crc32(bytes(backup_hdr[:92])))
    disk[(disk_sectors - 1) * SECTOR:disk_sectors * SECTOR] = backup_hdr


# ── full creation ───────────────────────────────────────────────

def do_full_creation(base, disk_path, ext4_path, esp_path,
                     kernel_path, boot_path, drivers_path):
    ext4_sectors = EXT4_MB * 1024 * 1024 // SECTOR
    esp_sectors = ESP_MB * 1024 * 1024 // SECTOR
    disk_sectors = DISK_MB * 1024 * 1024 // SECTOR

    ext4_first = EXT4_FIRST_SECTOR
    ext4_last = ext4_first + ext4_sectors - 1
    esp_first = ext4_last + 1
    esp_last = esp_first + esp_sectors - 1

    print(f"Creating {DISK_MB} MB disk image...")
    print(f"  Partition 0 (ext4): LBA {ext4_first}..{ext4_last}")
    print(f"  Partition 1 (ESP):   LBA {esp_first}..{esp_last}")

    disk = bytearray(disk_sectors * SECTOR)
    disk[446:462] = struct.pack("<BBBBBBBBII", 0, 0, 0, 1, 0xEE, 0xFF, 0xFF, 0xFF, 1, disk_sectors - 1)
    disk[510:512] = b"\x55\xAA"
    write_gpt(disk, disk_sectors, ext4_first, ext4_last, esp_first, esp_last)
    print("GPT written (primary + backup)")

    print("Formatting ext4...")
    kernel_wsl = to_wsl(kernel_path)
    drivers_wsl = to_wsl(drivers_path) if os.path.exists(drivers_path) else None
    
    ext4_cmd = (
        "set -euo pipefail; "
        "mkdir -p /tmp/lodaxos_ext4; "
        f"cp '{kernel_wsl}' /tmp/lodaxos_ext4/kernel.elf; "
    )
    if drivers_wsl:
        ext4_cmd += f"cp '{drivers_wsl}' /tmp/lodaxos_ext4/drivers.elf; "
        
    ext4_cmd += (
        "echo -n 'Hello, World!' > /tmp/lodaxos_ext4/file.txt; "
        f"dd if=/dev/zero of=/tmp/lodaxos_ext4.img bs=1M count={EXT4_MB}; "
        f"mkfs.ext4 -F -L LodaxOS -d /tmp/lodaxos_ext4 /tmp/lodaxos_ext4.img; "
        f"cp /tmp/lodaxos_ext4.img '{to_wsl(ext4_path)}'; "
        "rm -rf /tmp/lodaxos_ext4 /tmp/lodaxos_ext4.img"
    )
    run_wsl(ext4_cmd)
    
    with open(ext4_path, "rb") as f:
        offset = ext4_first * SECTOR
        total = 0
        while True:
            chunk = f.read(1024 * 1024)
            if not chunk:
                break
            disk[offset:offset + len(chunk)] = chunk
            offset += len(chunk)
            total += len(chunk)
    print(f"  ext4: {total // 1024} KB written")

    print("Formatting ESP...")
    boot_wsl = to_wsl(boot_path)
    run_wsl(
        "set -euo pipefail; "
        f"dd if=/dev/zero of=/tmp/lodaxos_esp.img bs=1M count={ESP_MB}; "
        f"mkfs.fat -F 32 -n ESP /tmp/lodaxos_esp.img; "
        f"mmd -i /tmp/lodaxos_esp.img ::/EFI; "
        f"mmd -i /tmp/lodaxos_esp.img ::/EFI/BOOT; "
        f"mcopy -i /tmp/lodaxos_esp.img '{boot_wsl}' ::/EFI/BOOT/BOOTX64.EFI; "
        f"mcopy -i /tmp/lodaxos_esp.img '{kernel_wsl}' ::/kernel.elf; "
        f"mdir -i /tmp/lodaxos_esp.img ::/EFI/BOOT; "
        f"cp /tmp/lodaxos_esp.img '{to_wsl(esp_path)}'; "
        f"rm /tmp/lodaxos_esp.img"
    )
    with open(esp_path, "rb") as f:
        esp_data = f.read(esp_sectors * SECTOR)
    disk[esp_first * SECTOR:esp_first * SECTOR + len(esp_data)] = esp_data
    print(f"  ESP: {len(esp_data) // 1024} KB written")

    with open(disk_path, "wb") as f:
        f.write(bytes(disk))

    size_bytes = os.path.getsize(disk_path)
    print(f"\nDisk image: {disk_path} ({size_bytes // 1024} KB)")

    cache = {
        "ext4:kernel.elf": file_md5(kernel_path),
        "ext4:drivers.elf": file_md5(drivers_path) if os.path.exists(drivers_path) else "missing",
        "esp:kernel.elf": file_md5(kernel_path),
        "esp:BOOTX64.EFI": file_md5(boot_path),
    }
    _write_hash_cache(base, cache)


# ── incremental update ──────────────────────────────────────────

def _read_hash_cache(base):
    path = os.path.join(base, HASH_CACHE_FILE)
    if os.path.exists(path):
        try:
            with open(path) as f:
                return json.load(f)
        except (json.JSONDecodeError, OSError):
            pass
    return {}


def _write_hash_cache(base, cache):
    path = os.path.join(base, HASH_CACHE_FILE)
    try:
        with open(path, "w") as f:
            json.dump(cache, f, indent=2)
    except OSError:
        pass


def _bin_hashes_changed(source_path, cache_key, cache):
    if source_path is None:
        return False
    if not os.path.exists(source_path):
        return True
    cur = file_md5(source_path)
    prev = cache.get(cache_key)
    return cur != prev


def _partition_changed(part_path, expected_size):
    if not os.path.exists(part_path):
        return True
    return os.path.getsize(part_path) != expected_size


def _splice_into_disk(disk_path, ext4_path, esp_path, ext4_offset, esp_offset):
    print("  Reading partition images...")
    with open(ext4_path, "rb") as f:
        ext4_data = f.read()
    with open(esp_path, "rb") as f:
        esp_data = f.read()

    print(f"  Writing {len(ext4_data) + len(esp_data)} bytes to disk.img...")
    with open(disk_path, "r+b") as f:
        f.seek(ext4_offset)
        f.write(ext4_data)
        f.seek(esp_offset)
        f.write(esp_data)
        f.flush()
        os.fsync(f.fileno())
    print("  Splice done.")


def do_incremental_update(base, disk_path, ext4_path, esp_path,
                          kernel_path, boot_path, drivers_path):
    ext4_sectors = EXT4_MB * 1024 * 1024 // SECTOR
    esp_sectors = ESP_MB * 1024 * 1024 // SECTOR
    ext4_offset = EXT4_FIRST_SECTOR * SECTOR
    esp_offset = ext4_offset + ext4_sectors * SECTOR

    print("Updating disk image incrementally...")

    ext4_ok = not _partition_changed(ext4_path, ext4_sectors * SECTOR)
    esp_ok = not _partition_changed(esp_path, esp_sectors * SECTOR)
    if not ext4_ok or not esp_ok:
        print("  Partition cache missing or wrong size — falling back to full creation.")
        return False

    cache = _read_hash_cache(base)

    ext4_files = [
        ("kernel.elf", kernel_path, "ext4:kernel.elf"),
        ("file.txt", None, "ext4:file.txt"),
    ]
    if os.path.exists(drivers_path):
        ext4_files.append(("drivers.elf", drivers_path, "ext4:drivers.elf"))
    esp_files = [
        ("kernel.elf", kernel_path, "esp:kernel.elf"),
        ("BOOTX64.EFI", boot_path, "esp:BOOTX64.EFI"),
    ]

    ext4_dirty = any(_bin_hashes_changed(src, key, cache) for _, src, key in ext4_files)
    esp_dirty = any(_bin_hashes_changed(src, key, cache) for _, src, key in esp_files)

    if not ext4_dirty and not esp_dirty:
        print("  No binaries changed — disk is already up to date.")
        return True

    if ext4_dirty:
        print("  Updating ext4 partition...")
        ext4_wsl = to_wsl(ext4_path)
        ext4_script_parts = []
        for name, src_path, _ in ext4_files:
            if name == "file.txt":
                ext4_script_parts.append(
                    f"echo -n 'Hello, World!' > /tmp/lodaxos_file.txt && "
                    f"debugfs -w -R 'write /tmp/lodaxos_file.txt /file.txt' '{ext4_wsl}' 2>/dev/null || true"
                )
            else:
                src_wsl = to_wsl(src_path)
                ext4_script_parts.append(
                    f"debugfs -w -R 'write {src_wsl} /{name}' '{ext4_wsl}' 2>/dev/null || true"
                )
        run_wsl(" && ".join(ext4_script_parts))
        print("  ext4 files updated.")

    if esp_dirty:
        print("  Updating ESP partition...")
        esp_wsl = to_wsl(esp_path)
        esp_script_parts = []
        for name, src_path, _ in esp_files:
            src_wsl = to_wsl(src_path)
            if name == "BOOTX64.EFI":
                esp_script_parts.append(
                    f"mcopy -o -i '{esp_wsl}' '{src_wsl}' ::/EFI/BOOT/BOOTX64.EFI"
                )
            else:
                esp_script_parts.append(
                    f"mcopy -o -i '{esp_wsl}' '{src_wsl}' ::/{name}"
                )
        run_wsl(" && ".join(esp_script_parts))
        print("  ESP files updated.")

    if ext4_dirty or esp_dirty:
        _splice_into_disk(disk_path, ext4_path, esp_path, ext4_offset, esp_offset)

    for _, src_path, key in ext4_files + esp_files:
        if src_path is not None:
            cache[key] = file_md5(src_path)
    _write_hash_cache(base, cache)

    print("  Incremental update complete.")
    return True


# ── main ────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(description="Create/update LodaxOS disk image")
    parser.add_argument("--full", action="store_true",
                        help="Force full recreation from scratch")
    args = parser.parse_args()

    base = os.path.dirname(os.path.abspath(__file__))
    disk_path = os.path.join(base, "disk.img")
    ext4_path = os.path.join(base, "ext4_part.img")
    esp_path = os.path.join(base, "esp_part.img")
    kernel_path = os.path.join(base, "kernel.elf")
    boot_path = os.path.join(base, "target", "x86_64-unknown-uefi", "debug", "lodaxos-boot.efi")
    drivers_path = os.path.join(base, "drivers.elf")

    for name, path in [("kernel.elf", kernel_path),
                       ("lodaxos-boot.efi", boot_path)]:
        if not os.path.exists(path):
            print(f"ERROR: {name} not found at {path}")
            sys.exit(1)

    if not os.path.exists(drivers_path):
        print(f"WARNING: drivers.elf not found at {drivers_path} — no driver support")

    expected_disk_size = DISK_MB * 1024 * 1024
    disk_exists = os.path.exists(disk_path) and os.path.getsize(disk_path) == expected_disk_size

    if args.full or not disk_exists:
        do_full_creation(base, disk_path, ext4_path, esp_path,
                         kernel_path, boot_path, drivers_path)
    else:
        ok = do_incremental_update(base, disk_path, ext4_path, esp_path,
                                    kernel_path, boot_path, drivers_path)
        if not ok:
            print("Falling back to full creation...")
            do_full_creation(base, disk_path, ext4_path, esp_path,
                             kernel_path, boot_path, drivers_path)

    size_bytes = os.path.getsize(disk_path)
    print(f"\nFinal: {disk_path} ({size_bytes // 1024} KB)")


if __name__ == "__main__":
    main()