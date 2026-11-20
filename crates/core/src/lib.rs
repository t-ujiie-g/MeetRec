//! MeetRec の OS 非依存ロジック。
//!
//! - `rules` / `detect`：会議アプリの定数表と検出の状態機械
//! - `controller`：モード・確認ポップアップ・録画開始/停止の制御（`platform::App` 実装）
//! - `session` / `audio` / `mp4` / `h264`：録画パイプライン（ミキサ、fMP4 muxer、修復）
//! - `storage` / `config` / `log` / `metrics`：保存・設定・ログ・自己計測
//! - `platform`：OS 層が実装する trait

pub mod audio;
pub mod config;
pub mod controller;
pub mod detect;
pub mod h264;
pub mod log;
pub mod metrics;
pub mod mp4;
pub mod platform;
pub mod rules;
pub mod session;
pub mod storage;
pub mod text;

#[cfg(test)]
pub(crate) mod testutil;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
