@echo off
setlocal

REM Deploy RustDock to E:\RustDock, register autostart, and start it.

set "SRC=%~dp0"
set "DST=E:\RustDock"

set "EXE=%SRC%dist\rustdock.exe"
if not exist "%EXE%" set "EXE=%SRC%target\release\rustdock.exe"

echo.
echo [1/5] target: %DST%
if not exist "%DST%" mkdir "%DST%"
if not exist "%DST%\assets" mkdir "%DST%\assets"

echo [2/5] copying files...
copy /Y "%EXE%" "%DST%\rustdock.exe" >nul
if errorlevel 1 exit /b 1

if exist "%SRC%apps.txt" (
    copy /Y "%SRC%apps.txt" "%DST%\apps.txt" >nul
) else (
    copy /Y "%SRC%apps.example.txt" "%DST%\apps.txt" >nul
)

if exist "%SRC%assets\chatgpt.png" copy /Y "%SRC%assets\chatgpt.png" "%DST%\assets\chatgpt.png" >nul

echo [3/5] setting integrity level...
icacls "%DST%" /setintegritylevel "(OI)(CI)Medium" >nul 2>&1
icacls "%DST%\rustdock.exe" /setintegritylevel Medium >nul 2>&1

echo [4/5] registering autostart...
"%DST%\rustdock.exe" --install-autostart

echo [5/5] starting...
start "" "%DST%\rustdock.exe"
exit /b 0
