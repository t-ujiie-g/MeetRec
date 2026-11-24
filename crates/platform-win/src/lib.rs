#![cfg(windows)]
//! MeetRec の Windows 層（ファイルの役割は AGENTS.md の「構成」と各ファイルの先頭）。

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
