@echo off
cls
call build.bat
if %errorlevel% neq 0 (
    echo Build failed -- aborting.
    exit /b %errorlevel%
)
python create_disk_image.py --full
start "katerm" python katerm\katerm_client.py
call run.bat
pause
