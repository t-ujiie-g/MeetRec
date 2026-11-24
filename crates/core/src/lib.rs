//! MeetRec の OS 非依存ロジック（モジュールの役割は AGENTS.md の「構成」）。

pub mod audio;
pub mod config;
pub mod controller;
pub mod detect;
pub mod h264;
pub mod log;
pub mod metrics;
pub mod mp4;
pub mod output;
pub mod platform;
pub mod rules;
pub mod session;
pub mod storage;
pub mod text;

#[cfg(test)]
pub(crate) mod testutil;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
