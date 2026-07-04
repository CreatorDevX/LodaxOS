#!/usr/bin/env python3
"""Verify all files extracted from disk.img ESP match source binaries."""
import hashlib, os

SECTOR = 512
EXT4_MB = 512
ESP_MB = 64
EXT4_OFF = 2048 * SECTOR
ESP_OFF = EXT4_OFF + EXT4_MB * 1024 * 1024
ESP_SIZE = ESP_MB * 1024 * 1024

BASE = os.path.dirname(os.path.abspath(__file__))

with open(os.path.join(BASE, "disk.img"), "rb") as f:
    f.seek(ESP_OFF)
    esp = f.read(ESP_SIZE)

# Source binaries
sources = {
    "kernel.elf": os.path.join(BASE, "kernel.elf"),
    "lodaxos-boot.efi": os.path.join(BASE, "target", "x86_64-unknown-uefi", "debug", "lodaxos-boot.efi"),
}

# Find ELF and PE files in the ESP partition by magic
# kernel.elf is ELF (7fELF), chainloader is ELF, bootloader is PE (PE\0\0)
positions = []
# Find all ELF magics
pos = 0
while True:
    pos = esp.find(b'\x7fELF', pos)
    if pos < 0:
        break
    positions.append(("ELF", pos))
    pos += 1
# Find all PE magics
pos = 0
while True:
    pos = esp.find(b'PE\x00\x00', pos)
    if pos < 0:
        break
    positions.append(("PE", pos))
    pos += 1

positions.sort(key=lambda x: x[1])
print("Files found in ESP:")
for kind, pos in positions:
    print(f"  {kind} at byte offset {pos} (sector {pos//SECTOR})")

# Match files to positions by size
# kernel.elf is 4594152, chainloader is ~150KB, bootloader is ~220KB
k_size = os.path.getsize(sources["kernel.elf"])
b_size = os.path.getsize(sources["lodaxos-boot.efi"])
print(f"\nkernel.elf size: {k_size}")
print(f"bootloader size: {b_size}")

# kernel.elf is the largest (4.6MB), so the first ELF is likely kernel
# chainloader is second ELF
elf_positions = [p for p in positions if p[0] == "ELF"]
pe_positions = [p for p in positions if p[0] == "PE"]

# Verify kernel.elf at first ELF position
with open(sources["kernel.elf"], "rb") as f:
    src_k = f.read()
    src_k_hash = hashlib.md5(src_k).hexdigest()

pos = elf_positions[0][1]
disk_k = esp[pos:pos + k_size]
disk_k_hash = hashlib.md5(disk_k).hexdigest()
print(f"\nkernel.elf:  disk={disk_k_hash}  source={src_k_hash}  match={disk_k_hash == src_k_hash}")

# Verify bootloader at first PE position
with open(sources["lodaxos-boot.efi"], "rb") as f:
    src_b = f.read()
    src_b_hash = hashlib.md5(src_b).hexdigest()

pos = pe_positions[0][1]
disk_b = esp[pos:pos + b_size]
disk_b_hash = hashlib.md5(disk_b).hexdigest()
print(f"bootloader:  disk={disk_b_hash}  source={src_b_hash}  match={disk_b_hash == src_b_hash}")


