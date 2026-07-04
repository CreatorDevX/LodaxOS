@echo off
python "%~dp0create_disk_image.py --full"
if errorlevel 1 exit /b 1
