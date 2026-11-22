#![cfg(windows)]
//! MeetRec の Windows 層。
//!
//! - `app`：メッセージループ、イベント配送、`Platform` 実装
//! - `tray` / `prompt`：トレイアイコン・メニュー・通知、確認ポップアップ
//! - `probe`：マイク使用プロセスとウィンドウの観測
//! - `recorder`：録画の開始・停止（`video` / `audio` スレッドの管理）
//! - `capture` / `convert` / `h264enc`：WGC → D3D11 Video Processor → H.264 MFT
//! - `uia`：会議アプリの画面部品（ミュートボタンなど）の読み取り（UI オートメーション）
//! - `audio` / `aac`：WASAPI（プロセスループバック・マイク）と AAC MFT

mod aac;
mod app;
mod audio;
mod capture;
mod convert;
mod h264enc;
mod probe;
mod prompt;
mod recorder;
mod sys;
mod tray;
mod uia;
mod video;

use meetrec_core::platform::App;

/// Win32 メッセージループを開始する（終了時はプロセスを終了し、戻らない）。
pub fn run(app: Box<dyn App>) {
    app::run(app);
}
