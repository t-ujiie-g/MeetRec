//! UI 文言（日本語）。両 OS で同じ文言を使う。

use crate::config::Mode;

pub const APP_NAME: &str = "MeetRec";

pub const STATUS_IDLE: &str = "待機中";
pub const STATUS_OFF: &str = "待機中（自動検出オフ）";
pub const STATUS_PROMPT: &str = "会議を検出（確認待ち）";

pub const MENU_RECORD_NOW: &str = "今すぐ録画";
pub const MENU_STOP: &str = "録画を停止";
pub const MENU_OPEN_FOLDER: &str = "録画フォルダを開く";
pub const MENU_CHOOSE_FOLDER: &str = "保存先を変更…";
pub const MENU_MODE: &str = "会議を検出したとき";
pub const MENU_AUTOSTART: &str = "ログイン時に起動";
pub const MENU_DIAGNOSTICS: &str = "診断ログを保存";
pub const MENU_QUIT: &str = "MeetRec を終了";

pub fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "確認する",
        Mode::Auto => "常に自動録画",
        Mode::Off => "オフ",
    }
}

pub const PROMPT_RECORD: &str = "録画する";
pub const PROMPT_SKIP: &str = "今回はしない";
pub const PROMPT_ALWAYS: &str = "次回から確認せず録画";

pub fn prompt_message(app: &str) -> String {
    format!("{app} の会議を検出しました。録画しますか？")
}

pub fn status_recording(elapsed_secs: u64) -> String {
    format!(
        "録画中 {:02}:{:02}:{:02}",
        elapsed_secs / 3600,
        elapsed_secs % 3600 / 60,
        elapsed_secs % 60
    )
}

pub fn duration_text(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}時間{}分", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{}分{}秒", s / 60, s % 60)
    } else {
        format!("{s}秒")
    }
}

pub const NOTICE_STARTED: &str = "録画を開始しました";
pub const NOTICE_SAVED: &str = "保存しました（クリックでフォルダを開く）";
pub const NOTICE_AUDIO_ONLY: &str = "映像を取得できないため、音声のみ録音しています";
pub const NOTICE_NO_WINDOW: &str = "会議アプリ（Zoom / Teams）のウィンドウが見つかりません";
pub const NOTICE_DISK_LOW: &str = "ディスクの空きが少ないため録画を停止しました";
pub const NOTICE_DISK_LOW_START: &str = "ディスクの空きが足りないため録画を開始できません";
pub const NOTICE_START_FAILED: &str = "録画を開始できませんでした";
pub const NOTICE_FAILED: &str = "録画が中断されました";
pub const NOTICE_RECOVERED: &str = "前回正常に終了しなかった録画を修復しました";
pub const NOTICE_DIAGNOSTICS: &str = "診断ログを保存しました（クリックでフォルダを開く）";
pub const NOTICE_FOLDER_CHANGED: &str = "保存先を変更しました（クリックでフォルダを開く）";
pub const NOTICE_FOLDER_UNUSABLE: &str = "このフォルダには保存できません";
pub const CHOOSE_FOLDER_TITLE: &str = "録画の保存先を選んでください";
/// OS の通知の代わりに出すアプリ内パネルのボタン（macOS）。
pub const TOAST_OPEN: &str = "フォルダを開く";
pub const TOAST_CLOSE: &str = "閉じる";
pub const NOTICE_AUTOSTART_FAILED: &str = "ログイン時の起動を設定できませんでした";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(status_recording(754), "録画中 00:12:34");
        assert_eq!(status_recording(3600 * 2 + 5), "録画中 02:00:05");
        assert_eq!(duration_text(45_000), "45秒");
        assert_eq!(duration_text(125_000), "2分5秒");
        assert_eq!(duration_text(3_725_000), "1時間2分");
    }
}
