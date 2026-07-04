SECTOR = 512
EXT4_FIRST_SECTOR = 2048
EXT4_MB = 512
ESP_MB = 128
ext4_sectors = EXT4_MB * 1024 * 1024 // SECTOR
esp_sectors = ESP_MB * 1024 * 1024 // SECTOR
esp_first = EXT4_FIRST_SECTOR + ext4_sectors
# Disk must fit ESP: (esp_first + esp_sectors) sectors
disk_sectors = esp_first + esp_sectors
DISK_MB = disk_sectors * SECTOR // (1024 * 1024)
print(f'disk_sectors={disk_sectors} DISK_MB={DISK_MB}')

import os, binascii, struct

disk = bytearray(disk_sectors * SECTOR)
# MBR
disk[446:462] = struct.pack('<BBBBBBBBII', 0, 0, 0, 1, 0xEE, 0xFF, 0xFF, 0xFF, 1, disk_sectors - 1)
disk[510:512] = b'\x55\xAA'

# GPT header
hdr = bytearray(92)
hdr[0:8] = b'EFI PART'
hdr[8:12] = struct.pack('<I', 0x10000)
hdr[12:16] = struct.pack('<I', 92)
hdr[24:32] = struct.pack('<Q', 1)
hdr[32:40] = struct.pack('<Q', disk_sectors - 1)
hdr[40:48] = struct.pack('<Q', 2)
hdr[48:52] = struct.pack('<I', 128)
hdr[52:56] = struct.pack('<I', 128)

entries = bytearray(128 * 128)
entries[0:16] = b'\xAF\x3D\xC6\x0F\x83\x84\x72\x47\x8E\x79\x3D\x69\xD8\x47\x7D\xE4'
entries[16:32] = b'\xE2\x1B\xF2\x5C\x0C\x6F\xD6\x44\xB2\xEC\x08\xA0\xA4\x80\x57\x1D'
entries[32:40] = struct.pack('<Q', EXT4_FIRST_SECTOR)
entries[40:48] = struct.pack('<Q', EXT4_FIRST_SECTOR + ext4_sectors - 1)
entries[56:72] = b'ext4\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00'
entries[128:144] = b'\xC1\x2A\x73\x28\xF8\x1F\x11\xD2\xBA\x4B\x00\xA0\xC9\x3E\xC9\x3B'
entries[144:160] = b'\x00\xC2\xCE\x2D\xAF\x31\xC7\x49\x87\x91\x38\x69\x12\x9C\xFA\x57'
entries[160:168] = struct.pack('<Q', esp_first)
entries[168:176] = struct.pack('<Q', esp_first + esp_sectors - 1)

crc = binascii.crc32(hdr[:16] + b'\x00\x00\x00\x00' + hdr[20:]) & 0xFFFFFFFF
hdr[16:20] = struct.pack('<I', crc)
disk[512:604] = hdr
disk[1024:1024+128*128] = entries

# Backup GPT
backup_lba = disk_sectors - 1
off = backup_lba * 512
bhdr = bytearray(92)
bhdr[0:8] = b'EFI PART'
bhdr[8:12] = struct.pack('<I', 0x10000)
bhdr[12:16] = struct.pack('<I', 92)
bhdr[24:32] = struct.pack('<Q', backup_lba)
bhdr[32:40] = struct.pack('<Q', 1)
bhdr[40:48] = struct.pack('<Q', 2)
bhdr[48:52] = struct.pack('<I', 128)
bhdr[52:56] = struct.pack('<I', 128)
bcrc = binascii.crc32(bhdr[:16] + b'\x00\x00\x00\x00' + bhdr[20:]) & 0xFFFFFFFF
bhdr[16:20] = struct.pack('<I', bcrc)
disk[off:off+512] = bhdr
disk[off-128*128:off] = entries

# Splice partitions
with open('ext4_part.img', 'rb') as f:
    d = f.read()
    off = EXT4_FIRST_SECTOR * SECTOR
    disk[off:off+len(d)] = d

with open('esp_part.img', 'rb') as f:
    d = f.read()
    off = esp_first * SECTOR
    disk[off:off+len(d)] = d

with open('disk.img', 'wb') as f:
    f.write(bytes(disk))
s = os.path.getsize('disk.img')
print(f'disk.img: {s} bytes ({s//1024//1024} MB)')
