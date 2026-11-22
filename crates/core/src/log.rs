//! 小さなファイルロガー（ローテーション付き）。外部 crate なし。
//!
//! `meetrec.log` が上限を超えたら `meetrec.log.1` に回す（1 世代のみ、合計約 1MB）。
//! 直近の行はメモリにも保持し、診断ログに含める。

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 512 * 1024;
const RECENT_LINES: usize = 200;

struct Logger {
    path: Option<PathBuf>,
    file: Option<File>,
    size: u64,
    recent: VecDeque<String>,
}

static LOGGER: Mutex<Logger> = Mutex::new(Logger {
    path: None,
    file: None,
    size: 0,
    recent: VecDeque::new(),
});

/// ログファイルを開く。失敗してもアプリは続行する（メモリ上の履歴のみ）。
pub fn init(path: &Path) {
    let mut l = LOGGER.lock().unwrap();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    l.file = OpenOptions::new().create(true).append(true).open(path).ok();
    l.size = l
        .file
        .as_ref()
        .and_then(|f| f.metadata().ok())
        .map_or(0, |m| m.len());
    l.path = Some(path.to_path_buf());
}

/// UNIX 秒を `YYYY-MM-DDTHH:MM:SSZ` にする。
pub fn format_utc(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Howard Hinnant の civil_from_days。
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

pub fn write(level: &str, msg: std::fmt::Arguments) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let line = format!("{} {level} {msg}", format_utc(secs));
    let mut l = LOGGER.lock().unwrap();
    if l.recent.len() >= RECENT_LINES {
        l.recent.pop_front();
    }
    l.recent.push_back(line.clone());
    if l.size > MAX_BYTES
        && let Some(path) = l.path.clone()
    {
        l.file = None;
        let mut old = path.clone().into_os_string();
        old.push(".1");
        let _ = fs::rename(&path, PathBuf::from(old));
        l.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        l.size = 0;
    }
    if let Some(f) = l.file.as_mut()
        && writeln!(f, "{line}").is_ok()
    {
        l.size += line.len() as u64 + 1;
    }
}

/// 直近のログ行（診断ログ用）。
pub fn recent() -> Vec<String> {
    LOGGER.lock().unwrap().recent.iter().cloned().collect()
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log::write("INFO", format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log::write("WARN", format_args!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_format() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_795_184_999), "2026-11-20T14:29:59Z");
    }

    #[test]
    fn writes_rotates_and_keeps_recent() {
        let dir = crate::testutil::temp_dir("log");
        let path = dir.join("meetrec.log");
        init(&path);
        for i in 0..6000 {
            crate::info!("line {i} {}", "x".repeat(100));
        }
        let rotated = dir.join("meetrec.log.1");
        assert!(rotated.exists());
        assert!(fs::metadata(&path).unwrap().len() <= MAX_BYTES + 200);
        assert!(fs::metadata(&rotated).unwrap().len() <= MAX_BYTES + 200);
        let r = recent();
        assert_eq!(r.len(), RECENT_LINES);
        assert!(r.last().unwrap().contains("line 5999"));
    }
}
