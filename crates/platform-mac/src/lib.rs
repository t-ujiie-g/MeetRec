//! MeetRec の macOS 層。
//!
//! - `app`：NSApplication・イベント配送・`Platform` 実装
//! - `tray` / `prompt` / `notify` / `toast`：メニューバー、確認ポップアップ、通知、通知の代わりのパネル
//! - `panel`：`prompt` と `toast` が使う、フォーカスを奪わないパネルの共通部分
//! - `probe`：マイク使用プロセス（CoreAudio）とウィンドウ一覧（CGWindowList）
//! - `capture` / `encoder` / `aac`：ScreenCaptureKit → VideoToolbox / AudioConverter → core::session
//! - `sys`：時計・プロセス情報・自己計測
#![cfg(target_os = "macos")]

mod aac;
mod app;
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
