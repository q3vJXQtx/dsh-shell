//! 程序入口。
//!
//! 逻辑全部在 `lib.rs` 及其子模块中，这里只负责调用，
//! 以便 future 需要做单元测试/集成测试时可以直接链接 lib crate。
//!
//! `windows_subsystem = "windows"` 只在 release 下生效：
//! debug 构建保留控制台，方便直接看到 panic 与早期日志。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    dsh_shell_lib::run();
}
