//! 进程入口。
//!
//! 只剩两件事：修好 stdio，然后把控制权交给 [`stream_server::app::run`]。
//!
//! 曾经这里有 400 行启动顺序（日志、鉴权、硬件探测、麦克风、监听面）。挪进
//! lib 的理由不是"文件太长"：放在 bin 里的代码 `cargo doc` 看不见、集成测试
//! 调不到、别的 crate 复用不了，而它恰好是"这个服务到底是什么"的主要说明书。
//!
//! ```text
//! main.rs        唯一职责：修 stdio、起运行时
//! boot::prepare  GUI 子系统的三件平台准备（stdio、panic 落盘）
//! app::run       装配顺序、启动日志、启动期失败语义
//! ```
//!
//! # 为什么是 `windows_subsystem = "windows"`
//!
//! `stream-server.exe` 由计划任务每分钟触发一次。console 子系统的程序在这种
//! 场合**会被 Windows 代建一个控制台窗口**：每次触发闪一下，并抢走全屏游戏的
//! 前台焦点。此前靠一个 `run-hidden.vbs`（`WScript.Shell.Run` 带窗口模式 0）
//! 去消掉它，那是绕。
//!
//! GUI 子系统没有这个窗口，代价是启动时标准句柄无效、`println!` 会 panic ——
//! 这一条由 `boot::prepare` 里的三种处境处理（见那个模块的文档）。
//!
//! 其他 bin（`src/bin/` 下的诊断工具）不受影响：这个属性只作用于本文件所属的
//! 那一个 bin。

#![cfg_attr(windows, windows_subsystem = "windows")]

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 必须先做的一件事：在任何一行输出之前。GUI 子系统下 stdout 默认是无效句
    // 柄，而 `println!` 写失败会 panic —— 那会让服务在某句日志处凭空消失。
    stream_server::boot::prepare();
    stream_server::app::run().await
}
