#![cfg(windows)]

use meetrec_core::platform::App;

/// OS のイベントループを開始する（戻らない）。
pub fn run(_app: Box<dyn App>) {
    unimplemented!("M2 で実装");
}
