#!/usr/bin/env python3
"""Create LodaxOS disk image: GPT with ext4 (Partition Zero) + ESP (FAT32)."""

import binascii
import os
import struct
import subprocess
import sys

SECTOR = 512
EXT4_GUID = bytes([0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4])
ESP_GUID = bytes([0xC1, 0x2A, 0x73, 0x28, 0xF8, 0x1F, 0x11, 0xD2, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B])
WSL_DISTRO = "Ubuntu"


def crc32(data):
    return binascii.crc32(data) & 0xFFFFFFFF


def to_wsl(path):
    path = path.replace("\\", "/")
    if len(path) >= 2 and path[1] == ":":
        return f"/mnt/{path[0].lower()}{path[2:]}"
    return path


def run_wsl_ubuntu(script):
    distro = os.environ.get("LODAXOS_WSL_DISTRO", WSL_DISTRO)
    subprocess.run(["wsl.exe", "-d", distro, "--", "bash", "-lc", script], check=True)


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
    disk[entries_lba * SECTOR: entries_lba * SECTOR + len(entries)] = entries

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
    disk[backup_entries_lba * SECTOR: backup_entries_lba * SECTOR + len(entries)] = entries
    backup_hdr = bytearray(hdr)
    struct.pack_into("<Q", backup_hdr, 24, disk_sectors - 1)
    struct.pack_into("<Q", backup_hdr, 32, 1)
    struct.pack_into("<Q", backup_hdr, 72, backup_entries_lba)
    backup_hdr[16:20] = b"\x00\x00\x00\x00"
    struct.pack_into("<I", backup_hdr, 88, crc32(bytes(entries)))
    struct.pack_into("<I", backup_hdr, 16, crc32(bytes(backup_hdr[:92])))
    disk[(disk_sectors - 1) * SECTOR: disk_sectors * SECTOR] = backup_hdr


def main():
    base = os.path.dirname(os.path.abspath(__file__))
    disk_path = os.path.join(base, "disk.img")
    kernel_path = os.path.join(base, "kernel.elf")
    boot_path = os.path.join(base, "target", "x86_64-unknown-uefi", "debug", "lodaxos-boot.efi")
    chain_path = os.path.join(base, "target", "x86_64-unknown-uefi", "debug", "lodaxos-chain.efi")
    sr_path = os.path.join(base, "sr.elf")

    for name, path in [("kernel.elf", kernel_path), ("Bootloader.efi", boot_path), ("Chainloader.efi", chain_path)]:
        if not os.path.exists(path):
            print(f"ERROR: {name} not found")
            sys.exit(1)
    if not os.path.exists(sr_path):
        print(f"WARNING: sr.elf not found; building without Secure Runtime")

    disk_mb = 600
    ext4_mb = 512
    esp_mb = 64
    disk_sectors = disk_mb * 1024 * 1024 // SECTOR
    ext4_sectors = ext4_mb * 1024 * 1024 // SECTOR
    esp_sectors = esp_mb * 1024 * 1024 // SECTOR

    ext4_first = 2048
    ext4_last = ext4_first + ext4_sectors - 1
    esp_first = ext4_last + 1
    esp_last = esp_first + esp_sectors - 1

    print(f"Creating {disk_mb} MB disk image...")
    print(f"  Partition 0 (ext4): LBA {ext4_first}..{ext4_last}")
    print(f"  Partition 1 (ESP):   LBA {esp_first}..{esp_last}")

    disk = bytearray(disk_sectors * SECTOR)
    disk[446:462] = struct.pack("<BBBBBBBBII", 0, 0, 0, 1, 0xEE, 0xFF, 0xFF, 0xFF, 1, disk_sectors - 1)
    disk[510:512] = b"\x55\xAA"
    write_gpt(disk, disk_sectors, ext4_first, ext4_last, esp_first, esp_last)
    print("GPT written (primary + backup)")

    print("Formatting ext4...")
    ext4_part = os.path.join(base, "ext4_part.img")
    ext4_wsl = to_wsl(ext4_part)
    kernel_wsl = to_wsl(kernel_path)
    boot_wsl = to_wsl(boot_path)
    sr_wsl = to_wsl(sr_path) if os.path.exists(sr_path) else None
    ext4_prep = (
        "set -euo pipefail; "
        f"dd if=/dev/zero of='{ext4_wsl}' bs=1M count={ext4_mb}; "
        f"mkdir -p /tmp/lodaxos_ext4; "
        f"cp '{kernel_wsl}' /tmp/lodaxos_ext4/kernel.elf; "
        f"cp '{boot_wsl}' /tmp/lodaxos_ext4/Bootloader.efi; "
    )
    if sr_wsl:
        ext4_prep += f"cp '{sr_wsl}' /tmp/lodaxos_ext4/sr.elf; "
    ext4_prep += (
        f"mkfs.ext4 -F -L LodaxOS -d /tmp/lodaxos_ext4 '{ext4_wsl}'; "
        f"rm -rf /tmp/lodaxos_ext4"
    )
    run_wsl_ubuntu(ext4_prep)
    with open(ext4_part, "rb") as f:
        ext4_data = f.read(ext4_sectors * SECTOR)
    disk[ext4_first * SECTOR: ext4_first * SECTOR + len(ext4_data)] = ext4_data
    
    print(f"  ext4: {len(ext4_data) // 1024} KB written")

    print("Formatting ESP...")
    esp_part = os.path.join(base, "esp_part.img")
    esp_wsl = to_wsl(esp_part)
    chain_wsl = to_wsl(chain_path)
    boot_wsl = to_wsl(boot_path)
    kernel_wsl = to_wsl(kernel_path)
    run_wsl_ubuntu(
        "set -euo pipefail; "
        f"dd if=/dev/zero of='{esp_wsl}' bs=1M count={esp_mb}; "
        f"mkfs.fat -F 32 -n ESP '{esp_wsl}'; "
        f"mmd -i '{esp_wsl}' ::/EFI; "
        f"mmd -i '{esp_wsl}' ::/EFI/BOOT; "
        f"mcopy -i '{esp_wsl}' '{chain_wsl}' ::/EFI/BOOT/BOOTX64.EFI; "
        f"mcopy -i '{esp_wsl}' '{boot_wsl}' ::/Bootloader.efi; "
        f"mcopy -i '{esp_wsl}' '{kernel_wsl}' ::/kernel.elf; "
        f"mdir -i '{esp_wsl}' ::/EFI/BOOT"
    )
    with open(esp_part, "rb") as f:
        esp_data = f.read(esp_sectors * SECTOR)
    disk[esp_first * SECTOR: esp_first * SECTOR + len(esp_data)] = esp_data
    
    print(f"  ESP: {len(esp_data) // 1024} KB written")

    with open(disk_path, "wb") as f:
        f.write(bytes(disk))

    size_bytes = os.path.getsize(disk_path)
    print(f"\nDisk image: {disk_path} ({size_bytes // 1024} KB)")
    print("  Partition 0: ext4 - kernel.elf, sr.elf, Bootloader.efi")
    print("  Partition 1: ESP - EFI/BOOT/BOOTX64.EFI, Bootloader.efi, kernel.elf")


if __name__ == "__main__":
    main()
