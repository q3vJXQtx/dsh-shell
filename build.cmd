@echo off
REM Build helper for DSH Shell (Windows).
REM
REM Two environment problems on this machine, both handled below:
REM   1. Rust lives on D:, so CARGO_HOME/RUSTUP_HOME must be set explicitly.
REM      Otherwise cargo resolves to the broken stub in ~/.rustup and fails
REM      with "missing manifest in toolchain".
REM   2. Git Bash ships /usr/bin/link, which shadows MSVC link.exe. Without
REM      vcvars64 the build dies with LNK1181: cannot open kernel32.lib.
REM
REM Edit the paths below if you move Visual Studio Build Tools or Rust.
call "D:/Software/Code/VSBuildTools/VC/Auxiliary/Build/vcvars64.bat" >nul 2>&1
if errorlevel 1 echo [warn] could not activate MSVC environment; link may fail with LNK1181
set "CARGO_HOME=D:/Software/Code/Rust/cargo"
set "RUSTUP_HOME=D:/Software/Code/Rust/rustup"
set "PATH=D:/Software/Code/Rust/rustup/toolchains/stable-x86_64-pc-windows-msvc/bin;%PATH%"
cd /d "%~dp0src-tauri"
if "%~1"=="" (
  echo Usage: build.cmd ^<cargo args^>
  echo   build.cmd check
  echo   build.cmd test --lib
  echo   build.cmd build --release --features custom-protocol
  exit /b 1
)
cargo %*
