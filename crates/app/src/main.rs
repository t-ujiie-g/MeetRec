//! MeetRec エントリポイント。ロジックは `meetrec-core`、OS 依存部は各 platform クレートにある。
#![cfg_attr(windows, windows_subsystem = "windows")]

use meetrec_core::config::Config;
use meetrec_core::controller::Controller;
use meetrec_core::storage::Dirs;

fn main() {
    let dirs = Dirs::from_env();
    meetrec_core::log::init(&dirs.log_file());
    // 実機での不具合調査用に、パニックもログへ残す（release は panic=abort）。
    std::panic::set_hook(Box::new(|info| {
        meetrec_core::warn!("PANIC: {info}");
    }));
    let (cfg, config_error) = Config::load(&dirs.config_file());
    let app = Box::new(Controller::new(cfg, dirs, config_error));
    run(app);
}

#[cfg(target_os = "macos")]
fn run(app: Box<Controller>) {
    meetrec_mac::run(app);
}

#[cfg(windows)]
fn run(app: Box<Controller>) {
    meetrec_win::run(app);
}

#[cfg(not(any(target_os = "macos", windows)))]
fn run(_app: Box<Controller>) {
    eprintln!(
        "MeetRec は macOS と Windows 専用です（この OS では core のテストのみ実行できます）。"
    );
    std::process::exit(1);
}
