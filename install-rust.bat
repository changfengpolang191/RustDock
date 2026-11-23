@echo off
setlocal
chcp 437 >nul

echo ==================================================
echo   RustDock  -  Rust toolchain installer
echo   Install target : E:\Rust
echo ==================================================
echo.

set "RUSTUP_HOME=E:\Rust\rustup"
set "CARGO_HOME=E:\Rust\cargo"
set "RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static"
set "RUSTUP_UPDATE_ROOT=https://mirrors.ustc.edu.cn/rust-static/rustup"

if not exist "E:\Rust" mkdir "E:\Rust"

if exist "rustup-init.exe" (
    echo [1/3] rustup-init.exe already here, skip download.
) else (
    echo [1/3] Downloading rustup-init.exe ...
    curl -L --fail --connect-timeout 20 -o rustup-init.exe https://win.rustup.rs/x86_64
    if errorlevel 1 (
        echo.
        echo   Download failed.
        echo   Open https://win.rustup.rs/x86_64 in a browser,
        echo   save it as rustup-init.exe next to this script, run again.
        echo.
        pause
        exit /b 1
    )
)

echo.
echo [2/3] Installing stable toolchain to %RUSTUP_HOME% ...
rustup-init.exe -y --default-toolchain stable --profile minimal
if errorlevel 1 (
    echo.
    echo   Install FAILED. Most common reasons:
    echo     1. No MSVC linker
    echo        -^> install "Visual Studio Build Tools" with workload
    echo           "Desktop development with C++"
    echo     2. Network blocked
    echo        -^> turn on your proxy in another window first
    echo     3. Long path / non-ASCII path
    echo        -^> TEMP and the install dir must be plain ASCII
    echo.
    pause
    exit /b 1
)

echo.
echo [3/3] Writing cargo mirror config to %CARGO_HOME% ...
if not exist "%CARGO_HOME%" mkdir "%CARGO_HOME%"
(
    echo [source.crates-io]
    echo replace-with = 'ustc'
    echo.
    echo [source.ustc]
    echo registry = "sparse+https://mirrors.ustc.edu.cn/crates.io-index/"
) > "%CARGO_HOME%\config.toml"

echo.
echo ==================================================
echo   Done.
echo.
echo   CLOSE this window, open a NEW one, then check:
echo      cargo --version
echo      rustc --version
echo.
echo   Then build the dock:
echo      cd /d E:\Projects\rustdock
echo      cargo run --release
echo ==================================================
pause
