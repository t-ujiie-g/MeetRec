#![cfg(target_os = "macos")]
#![allow(dead_code)]

mod aac;
mod encoder;
mod probe;
mod sys;

use meetrec_core::platform::App;

/// OS のイベントループを開始する（戻らない）。
pub fn run(_app: Box<dyn App>) {
    unimplemented!()
}
