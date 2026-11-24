//! MeetRec の macOS 層（ファイルの役割は AGENTS.md の「構成」と各ファイルの先頭）。
#![cfg(target_os = "macos")]

mod aac;
mod app;
mod ax;
mod capture;
mod encoder;
mod notify;
mod panel;
mod probe;
mod prompt;
mod sys;
mod toast;
mod tray;

use meetrec_core::platform::App;

/// OS のイベントループを開始する（戻らない）。メインスレッドから呼ぶこと。
pub fn run(app: Box<dyn App>) {
    app::run(app);
}
