@echo off
REM ===========================================================================
REM  Windows 构建包装脚本
REM
REM  用途：先进入 Visual Studio 官方开发者环境（vcvars64.bat），再执行 cargo。
REM
REM  为什么需要它：
REM  Rust 的 x86_64-pc-windows-msvc 目标要调用微软的 link.exe。在本机的
REM  Git Bash 环境下，/usr/bin/link.exe 是 GNU coreutils 的 link 工具，
REM  会冒名顶替真正的链接器，导致：
REM
REM      error: linking with `link.exe` failed: exit code: 1
REM      note: `link.exe` returned an unexpected error
REM      note: you may need to install Visual Studio build tools ...
REM
REM  这个提示会让人误以为没装 VS 构建工具，其实早已装好。
REM  走 vcvars64.bat 可以一次性把 PATH / INCLUDE / LIB 全部配好，
REM  彻底避免这类环境歧义。
REM
REM  用法（在 Git Bash 中）：
REM      source tools/env.sh
REM      cmd /c "tools\\build.cmd" check --message-format short
REM ===========================================================================

setlocal

set "VSVCVARS=D:\Software\Code\VSBuildTools\VC\Auxiliary\Build\vcvars64.bat"
if not exist "%VSVCVARS%" (
  echo [build] 找不到 vcvars64.bat: %VSVCVARS%
  exit /b 1
)

REM vcvars64 会输出一大段环境配置信息，这里静音掉
call "%VSVCVARS%" >nul 2>&1
if errorlevel 1 (
  echo [build] vcvars64.bat 执行失败
  exit /b 1
)

REM Rust 工具链装在 D 盘
set "RUSTUP_HOME=D:\Software\Code\Rust\rustup"
set "CARGO_HOME=D:\Software\Code\Rust\cargo"
set "RUSTUP_DIST_SERVER=https://mirrors.tuna.tsinghua.edu.cn/rustup"

REM cargo 的 shim（rustup 本体装在系统盘）
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"

cd /d "%~dp0..\src-tauri"

cargo %*
exit /b %errorlevel%
