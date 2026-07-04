@echo off
cls
call build.bat
python create_disk_image.py --full
start /B python katerm\katerm_client.py
call run.bat
pause
