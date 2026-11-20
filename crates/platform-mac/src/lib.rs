//! MeetRec の macOS 層。
//!
//! - `app`：NSApplication・イベント配送・`Platform` 実装
//! - `tray` / `prompt` / `notify`：メニューバー、確認ポップアップ、通知
//! - `probe`：マイク使用プロセス（CoreAudio）とウィンドウ一覧（CGWindowList）
//! - `capture` / `encoder` / `aac`：ScreenCaptureKit → VideoToolbox / AudioConverter → core::session
//! - `sys`：時計・プロセス情報・自己計測
#![cfg(target_os = "macos")]

mod aac;
mod app;
mod capture;
mod encoder;
mod notify;
mod probe;
mod prompt;
mod sys;
mod tray;

use meetrec_core::platform::App;

/// OS のイベントループを開始する（戻らない）。メインスレッドから呼ぶこと。
pub fn run(app: Box<dyn App>) {
    app::run(app);
}
