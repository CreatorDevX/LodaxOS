@echo off
cargo +nightly clean
echo === Cleaning disk cache ===
if exist "%~dp0disk.img" del "%~dp0disk.img"
if exist "%~dp0ext4_part.img" del "%~dp0ext4_part.img"
if exist "%~dp0esp_part.img" del "%~dp0esp_part.img"
if exist "%~dp0.disk_cache.json" del "%~dp0.disk_cache.json"
echo === All clean ===
