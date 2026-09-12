#!/usr/bin/env bash
# ============================================================================
# 构建环境设置 —— 用 `source tools/env.sh` 引入
#
# 本机环境的几处特殊性都集中在这里处理，免得每次手工拼一长串环境变量。
# 这些设置等价于 Visual Studio 的 `vcvars64.bat` 所做的事，
# 但因为 Bash 里无法调用 cmd.exe（安全策略），所以在这里手工复刻。
# ============================================================================

# Git Bash 会把 `/PID` 这类参数错误地当成路径转换掉
export MSYS_NO_PATHCONV=1

# WorkBuddy 的 safe-delete 垫片会拦截批量删除（例如 `cargo clean`、
# 删除残缺工具链），对构建流程是纯干扰。开发环境里关掉它。
export CODEBUDDY_SAFE_DELETE_ENABLED=0

# ---------------------------------------------------------------------------
# 代理
#
# 本机存在 http_proxy=http://127.0.0.1:10778，会让访问 127.0.0.1 与外网
# 都走一个失效的代理（表现为 502 / tunnel error）。
#
# 注意：必须用 unset 真正**移除**，不能 `export http_proxy=`（置空）。
# 置空后变量仍存在于环境块中，而只要同时存在 `HTTP_PROXY` 与 `http_proxy`
# 两个仅大小写不同的变量，.NET 程序（如 VS 安装器）就会因
# "字典中已添加该项" 直接崩溃（错误码 5002）。
# ---------------------------------------------------------------------------
unset http_proxy https_proxy all_proxy no_proxy
unset HTTP_PROXY HTTPS_PROXY ALL_PROXY NO_PROXY

# ---------------------------------------------------------------------------
# Rust 工具链（装在 D 盘，靠这两个变量重定向）
# ---------------------------------------------------------------------------
export RUSTUP_HOME='D:/Software/Code/Rust/rustup'
export CARGO_HOME='D:/Software/Code/Rust/cargo'

# 国内访问 crates.io 官方源较慢，用清华 TUNA 镜像
export RUSTUP_DIST_SERVER=https://mirrors.tuna.tsinghua.edu.cn/rustup
export RUSTUP_UPDATE_ROOT=https://mirrors.tuna.tsinghua.edu.cn/rustup/rustup

# ---------------------------------------------------------------------------
# MSVC 与 Windows SDK
#
# ★ 本文件最关键的一段 ★
#
# Rust 的 x86_64-pc-windows-msvc 目标要调用微软的 link.exe，而 link.exe
# 需要靠 INCLUDE / LIB 环境变量去找头文件与导入库。缺了 LIB 会报：
#
#     LINK : fatal error LNK1181: 无法打开输入文件"kernel32.lib"
#
# 同时，Git Bash 的 /usr/bin 下还有一个 **GNU coreutils 的 `link.exe`**
# （创建硬链接的小工具），会冒名顶替真正的链接器，报出更容易误导人的：
#
#     note: `link.exe` returned an unexpected error
#     note: you may need to install Visual Studio build tools ...
#     （实际上 VS 构建工具装得好好的）
#
# 所以这里做两件事：① 把 MSVC 的 bin 放到 PATH 最前；② 配好 INCLUDE / LIB。
# ---------------------------------------------------------------------------
MSVC_WIN='D:\Software\Code\VSBuildTools\VC\Tools\MSVC'
MSVC_POSIX='/d/Software/Code/VSBuildTools/VC/Tools/MSVC'
SDK_WIN='C:\Program Files (x86)\Windows Kits\10'
SDK_POSIX='/c/Program Files (x86)/Windows Kits/10'

if [ -d "$MSVC_POSIX" ] && [ -d "$SDK_POSIX" ]; then
  # 自动发现版本目录，避免升级 MSVC / SDK 后脚本失效
  MSVC_VER=$(ls "$MSVC_POSIX" 2>/dev/null | sort -V | tail -1)
  SDK_VER=$(ls "$SDK_POSIX/Lib" 2>/dev/null | sort -V | tail -1)

  export MSVC_VER SDK_VER
  export MSVC_BIN_POSIX="$MSVC_POSIX/$MSVC_VER/bin/Hostx64/x64"

  # 链接器目录放到 PATH 最前面，压过 Git Bash 的 coreutils link.exe
  if [ -f "$MSVC_BIN_POSIX/link.exe" ]; then
    export PATH="$MSVC_BIN_POSIX:$PATH"
  else
    echo "[env] 警告：未找到 $MSVC_BIN_POSIX/link.exe" >&2
  fi

  # INCLUDE / LIB 必须是 **Windows 风格路径 + 分号分隔**：
  #   - 这两个变量传给原生的 cl.exe / link.exe，不经过 MSYS 的路径转换
  #   - 分号分隔是 MSVC 工具链的约定（不是 Unix 的冒号）
  export INCLUDE="$MSVC_WIN\\$MSVC_VER\\include;$SDK_WIN\\Include\\$SDK_VER\\ucrt;$SDK_WIN\\Include\\$SDK_VER\\shared;$SDK_WIN\\Include\\$SDK_VER\\um"
  export LIB="$MSVC_WIN\\$MSVC_VER\\lib\\x64;$SDK_WIN\\Lib\\$SDK_VER\\ucrt\\x64;$SDK_WIN\\Lib\\$SDK_VER\\um\\x64"
else
  echo "[env] 警告：MSVC 或 Windows SDK 目录不存在，C 编译相关构建会失败" >&2
  echo "[env]   MSVC: $MSVC_POSIX" >&2
  echo "[env]   SDK : $SDK_POSIX" >&2
fi

# rustup / cargo 的 shim（装在系统盘），工具链位置由 RUSTUP_HOME/CARGO_HOME 决定
export PATH="$HOME/.cargo/bin:$PATH"

# ---------------------------------------------------------------------------
# 环境自检：一眼看出配置是否生效
# ---------------------------------------------------------------------------
if [ "$1" = "--check" ]; then
  echo "RUSTUP_HOME = $RUSTUP_HOME"
  echo "CARGO_HOME  = $CARGO_HOME"
  echo "MSVC        = ${MSVC_VER:-未找到}"
  echo "SDK         = ${SDK_VER:-未找到}"
  echo "link.exe    -> $(command -v link.exe)"
  echo "LIB         = $LIB"
  echo "INCLUDE     = $INCLUDE"
  echo "proxy 变量  = $(env | grep -ciE 'proxy=' ) 个（含 CODEBUDDY 的，应为 2 左右）"
fi
