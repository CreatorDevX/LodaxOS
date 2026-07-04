#!/usr/bin/env bash
set -euo pipefail

DISK_MB=600
EXT4_MB=512
ESP_MB=64
EXT4_FIRST_SECTOR=2048
SECTOR=512

BASE="$(cd "$(dirname "$0")" && pwd)"
DISK_PATH="$BASE/disk.img"
EXT4_PATH="$BASE/ext4_part.img"
ESP_PATH="$BASE/esp_part.img"
KERNEL_PATH="$BASE/kernel.elf"
BOOT_PATH="$BASE/target/x86_64-unknown-uefi/debug/lodaxos-boot.efi"

for p in "$KERNEL_PATH" "$BOOT_PATH"; do
    [ -f "$p" ] || { echo "Missing: $p"; exit 1; }
done

ext4_sectors=$((EXT4_MB * 1024 * 1024 / SECTOR))
esp_sectors=$((ESP_MB * 1024 * 1024 / SECTOR))
disk_sectors=$((DISK_MB * 1024 * 1024 / SECTOR))
ext4_last=$((EXT4_FIRST_SECTOR + ext4_sectors - 1))
esp_first=$((ext4_last + 1))
esp_last=$((esp_first + esp_sectors - 1))

echo "Creating ${DISK_MB} MB disk image..."
echo "  Partition 0 (ext4): LBA ${EXT4_FIRST_SECTOR}..${ext4_last}"
echo "  Partition 1 (ESP):   LBA ${esp_first}..${esp_last}"

dd if=/dev/zero of="$DISK_PATH" bs="$SECTOR" count="$disk_sectors" status=none 2>/dev/null

# Write GPT using Python for binary precision
python3 -c "
import struct, os, binascii

SECTOR = $SECTOR
DS = $disk_sectors
E1 = $EXT4_FIRST_SECTOR
EL = $ext4_last
EP = $esp_first
EL2 = $esp_last

EXT4_GUID = bytes([0xAF,0x3D,0xC6,0x0F,0x83,0x84,0x72,0x47,0x8E,0x79,0x3D,0x69,0xD8,0x47,0x7D,0xE4])
ESP_GUID  = bytes([0xC1,0x2A,0x73,0x28,0xF8,0x1F,0x11,0xD2,0xBA,0x4B,0x00,0xA0,0xC9,0x3E,0xC9,0x3B])

def crc32(d): return binascii.crc32(d) & 0xFFFFFFFF
def part_entry(t, u, f, l, n):
    e = bytearray(128)
    e[0:16] = t; e[16:32] = u
    struct.pack_into('<Q', e, 32, f); struct.pack_into('<Q', e, 40, l)
    struct.pack_into('<Q', e, 48, (l - f + 1) * SECTOR)
    e[56:56+len(n)*2] = n.encode('utf-16-le')
    return bytes(e)

disk = bytearray(DS * SECTOR)
# Protective MBR
struct.pack_into('<I', disk, 446, 0); disk[450] = 0; disk[451] = 1; disk[452] = 0xEE
struct.pack_into('<I', disk, 454, 0xFF); struct.pack_into('<I', disk, 458, 1)
struct.pack_into('<I', disk, 462, DS - 1)
disk[510:512] = b'\x55\xAA'

# Partition entries at LBA 2
entries = bytearray(128 * 128)
entries[0:128] = part_entry(EXT4_GUID, os.urandom(16), E1, EL, 'Partition Zero')
entries[128:256] = part_entry(ESP_GUID, os.urandom(16), EP, EL2, 'ESP')
disk[SECTOR*2:SECTOR*2+len(entries)] = entries

# Primary GPT header at LBA 1
hdr = bytearray(SECTOR)
hdr[0:8] = b'EFI PART'; struct.pack_into('<I', hdr, 8, 0x00010000)
struct.pack_into('<I', hdr, 12, 92); struct.pack_into('<Q', hdr, 24, 1)
struct.pack_into('<Q', hdr, 32, DS - 1); struct.pack_into('<Q', hdr, 40, 34)
struct.pack_into('<Q', hdr, 48, DS - 34); hdr[56:72] = os.urandom(16)
struct.pack_into('<Q', hdr, 72, 2); struct.pack_into('<I', hdr, 80, 128)
struct.pack_into('<I', hdr, 84, 128); struct.pack_into('<I', hdr, 88, crc32(bytes(entries)))
struct.pack_into('<I', hdr, 16, crc32(bytes(hdr[:92])))
disk[SECTOR:2*SECTOR] = hdr

# Backup GPT
bl = DS - 33; disk[bl*SECTOR:bl*SECTOR+len(entries)] = entries
bh = bytearray(hdr); struct.pack_into('<Q', bh, 72, bl)
bh[16:20] = b'\x00\x00\x00\x00'
struct.pack_into('<I', bh, 88, crc32(bytes(entries)))
struct.pack_into('<I', bh, 16, crc32(bytes(bh[:92])))
disk[(DS-1)*SECTOR:DS*SECTOR] = bh

with open('$DISK_PATH', 'r+b') as f: f.write(bytes(disk))
print('GPT written (primary + backup)')
" || exit 1

# Build partition images inside WSL tmpfs
to_wsl() {
    local p="${1//\\//}"
    [[ "$p" =~ ^([A-Za-z]):(.*) ]] && echo "/mnt/${BASH_REMATCH[1],,}${BASH_REMATCH[2]}" || echo "$p"
}

KW="$(to_wsl "$KERNEL_PATH")"; BW="$(to_wsl "$BOOT_PATH")"
EW="$(to_wsl "$EXT4_PATH")"; PW="$(to_wsl "$ESP_PATH")"
WSL="wsl.exe -d Ubuntu"

echo "Formatting ext4..."
$WSL -- bash -lc "set -euo pipefail
mkdir -p /tmp/lodaxos_ext4
cp '$KW' /tmp/lodaxos_ext4/kernel.elf
dd if=/dev/zero of=/tmp/lodaxos_ext4.img bs=1M count=$EXT4_MB status=none 2>/dev/null
mkfs.ext4 -F -L LodaxOS -d /tmp/lodaxos_ext4 /tmp/lodaxos_ext4.img
cp /tmp/lodaxos_ext4.img '$EW'
rm -rf /tmp/lodaxos_ext4 /tmp/lodaxos_ext4.img
"

echo "Formatting ESP..."
$WSL -- bash -lc "set -euo pipefail
dd if=/dev/zero of=/tmp/lodaxos_esp.img bs=1M count=$ESP_MB status=none 2>/dev/null
mkfs.fat -F 32 -n ESP /tmp/lodaxos_esp.img
mmd -i /tmp/lodaxos_esp.img ::/EFI
mmd -i /tmp/lodaxos_esp.img ::/EFI/BOOT
mcopy -i /tmp/lodaxos_esp.img '$BW' ::/EFI/BOOT/BOOTX64.EFI
mcopy -i /tmp/lodaxos_esp.img '$KW' ::/kernel.elf
cp /tmp/lodaxos_esp.img '$PW'
rm /tmp/lodaxos_esp.img
"

echo "Splicing partitions into disk image..."
ext4_offset=$((EXT4_FIRST_SECTOR * SECTOR))
esp_offset=$((ext4_offset + ext4_sectors * SECTOR))
python3 -c "
with open('$EXT4_PATH', 'rb') as f: d1 = f.read()
with open('$ESP_PATH', 'rb') as f: d2 = f.read()
with open('$DISK_PATH', 'r+b') as f:
    f.seek($ext4_offset); f.write(d1)
    f.seek($esp_offset); f.write(d2)
print(f'  ext4: {len(d1)//1024} KB written')
print(f'  ESP:  {len(d2)//1024} KB written')
"

size_kb=$(python3 -c "import os; print(os.path.getsize('$DISK_PATH') // 1024)")
echo ""
echo "Disk image: $DISK_PATH (${size_kb} KB)"
echo "Done."
