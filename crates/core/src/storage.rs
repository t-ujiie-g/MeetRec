//! 保存先・ファイル命名・進行中マーカー・異常終了後の修復・古い録画の削除。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::mp4;
use crate::platform::LocalTime;

/// アプリの各種ディレクトリ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    /// 設定・ログ・マーカーの置き場所。
    pub state: PathBuf,
    /// 録画の既定保存先。
    pub default_output: PathBuf,
}

impl Dirs {
    /// 環境変数から決める。macOS: ~/Library/Application Support/MeetRec と ~/Movies/MeetRec、
    /// Windows: %APPDATA%\MeetRec と %USERPROFILE%\Videos\MeetRec。
    pub fn from_env() -> Dirs {
        let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
        if cfg!(windows) {
            let profile = var("USERPROFILE").unwrap_or_else(|| PathBuf::from("C:\\"));
            let appdata = var("APPDATA").unwrap_or_else(|| profile.join("AppData").join("Roaming"));
            Dirs {
                state: appdata.join("MeetRec"),
                default_output: profile.join("Videos").join("MeetRec"),
            }
        } else {
            let home = var("HOME").unwrap_or_else(|| PathBuf::from("/tmp"));
            let state = if cfg!(target_os = "macos") {
                home.join("Library")
                    .join("Application Support")
                    .join("MeetRec")
            } else {
                home.join(".config").join("meetrec")
            };
            Dirs {
                state,
                default_output: home.join("Movies").join("MeetRec"),
            }
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.state.join("config.toml")
    }
    pub fn log_file(&self) -> PathBuf {
        self.state.join("meetrec.log")
    }
    pub fn marker_dir(&self) -> PathBuf {
        self.state.join("inprogress")
    }
}

/// ファイル名に使えない文字を除き、長さを制限する。
pub fn sanitize_component(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut last_us = false;
    for c in s.chars() {
        let bad =
            c.is_control() || matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|');
        let c = if bad || c.is_whitespace() { '_' } else { c };
        if c == '_' && last_us {
            continue;
        }
        last_us = c == '_';
        out.push(c);
        if out.chars().count() >= max_chars {
            break;
        }
    }
    out.trim_matches(|c| c == '_' || c == '.' || c == ' ')
        .to_string()
}

/// `2026-11-20_1430_Zoom.mp4` / `2026-11-20_1430_Teams_週次定例.mp4`。
pub fn file_stem(t: LocalTime, app: &str, title: Option<&str>) -> String {
    let mut stem = format!(
        "{:04}-{:02}-{:02}_{:02}{:02}_{}",
        t.year,
        t.month,
        t.day,
        t.hour,
        t.minute,
        sanitize_component(app, 20)
    );
    if let Some(title) = title
        .map(|t| sanitize_component(t, 40))
        .filter(|t| !t.is_empty())
    {
        stem.push('_');
        stem.push_str(&title);
    }
    stem
}

/// 既存ファイルと衝突しないパス（`_2`, `_3`… を付ける）。
pub fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let mut n = 1;
    loop {
        let name = if n == 1 {
            format!("{stem}.{ext}")
        } else {
            format!("{stem}_{n}.{ext}")
        };
        let p = dir.join(name);
        if !p.exists() {
            return p;
        }
        n += 1;
    }
}

/// 進行中マーカーのパス。中身は録画ファイルの絶対パス。
pub fn marker_path(marker_dir: &Path, recording: &Path) -> PathBuf {
    let name = recording
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    marker_dir.join(format!("{name}.inprogress"))
}

pub fn write_marker(marker_dir: &Path, recording: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(marker_dir)?;
    let p = marker_path(marker_dir, recording);
    fs::write(&p, recording.to_string_lossy().as_bytes())?;
    Ok(p)
}

pub fn remove_marker(marker_dir: &Path, recording: &Path) {
    let _ = fs::remove_file(marker_path(marker_dir, recording));
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// 修復して再生可能になった。
    Repaired { path: PathBuf, duration_ms: u64 },
    /// もともと正常だった（マーカーだけ残っていた）。
    Intact { path: PathBuf },
    /// 中身がなく削除した。
    RemovedEmpty { path: PathBuf },
    /// 修復できなかった（ファイルは残す）。
    Failed { path: PathBuf, error: String },
    /// ファイルが見つからない。
    Missing { path: PathBuf },
    /// マーカーが MeetRec の録画ファイル以外を指していた（ファイルには触れない）。
    NotRecording { path: PathBuf },
}

/// 起動時：残っているマーカーを調べ、未確定ファイルを修復する。
pub fn recover(marker_dir: &Path) -> Vec<RecoveryOutcome> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(marker_dir) else {
        return out;
    };
    for e in entries.flatten() {
        let marker = e.path();
        if marker.extension().and_then(|x| x.to_str()) != Some("inprogress") {
            continue;
        }
        let Ok(target) = fs::read_to_string(&marker) else {
            let _ = fs::remove_file(&marker);
            continue;
        };
        let path = PathBuf::from(target.trim());
        // 修復は切り詰め・削除を伴う。マーカーの中身を書き換えられても、録画以外のファイルを壊さない。
        let is_recording = path
            .file_name()
            .is_some_and(|n| is_recording_name(&n.to_string_lossy()));
        let outcome = if !is_recording {
            RecoveryOutcome::NotRecording { path }
        } else if !path.is_file() {
            RecoveryOutcome::Missing { path }
        } else {
            match mp4::repair_file(&path) {
                Ok(r) if r.fragments == 0 => {
                    let _ = fs::remove_file(&path);
                    RecoveryOutcome::RemovedEmpty { path }
                }
                Ok(r) if r.truncated_bytes == 0 && !r.duration_patched => {
                    RecoveryOutcome::Intact { path }
                }
                Ok(r) => RecoveryOutcome::Repaired {
                    path,
                    duration_ms: r.duration_ms,
                },
                Err(e) => {
                    // moov すら書かれていない極小ファイルは削除する。
                    if fs::metadata(&path).map(|m| m.len() < 4096).unwrap_or(false) {
                        let _ = fs::remove_file(&path);
                        RecoveryOutcome::RemovedEmpty { path }
                    } else {
                        RecoveryOutcome::Failed {
                            path,
                            error: e.to_string(),
                        }
                    }
                }
            }
        };
        let _ = fs::remove_file(&marker);
        out.push(outcome);
    }
    out
}

/// MeetRec が作った録画ファイル名か（`YYYY-MM-DD_HHMM_*.mp4`）。自動削除の誤爆防止。
pub fn is_recording_name(name: &str) -> bool {
    let b = name.as_bytes();
    let digits =
        |r: std::ops::Range<usize>| b.get(r).is_some_and(|s| s.iter().all(u8::is_ascii_digit));
    name.ends_with(".mp4")
        && digits(0..4)
        && b.get(4) == Some(&b'-')
        && digits(5..7)
        && b.get(7) == Some(&b'-')
        && digits(8..10)
        && b.get(10) == Some(&b'_')
        && digits(11..15)
        && b.get(15) == Some(&b'_')
}

/// `days` 日より古い録画を削除する。`keep` は削除しない（録画中のファイル）。
pub fn cleanup_old(dir: &Path, days: u32, now: SystemTime, keep: Option<&Path>) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    if days == 0 {
        return removed;
    }
    let limit = Duration::from_secs(days as u64 * 86_400);
    let Ok(entries) = fs::read_dir(dir) else {
        return removed;
    };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if !is_recording_name(&name) || keep == Some(p.as_path()) {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > limit);
        if old && fs::remove_file(&p).is_ok() {
            removed.push(p);
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;

    fn t() -> LocalTime {
        LocalTime {
            year: 2026,
            month: 11,
            day: 20,
            hour: 14,
            minute: 30,
            second: 5,
        }
    }

    #[test]
    fn stems() {
        assert_eq!(file_stem(t(), "Zoom", None), "2026-11-20_1430_Zoom");
        assert_eq!(
            file_stem(t(), "Teams", Some("週次 定例")),
            "2026-11-20_1430_Teams_週次_定例"
        );
        assert_eq!(
            file_stem(t(), "Teams", Some("a/b\\c:d*e?f\"g<h>i|j")),
            "2026-11-20_1430_Teams_a_b_c_d_e_f_g_h_i_j"
        );
        assert_eq!(
            file_stem(t(), "Teams", Some("  ///  ")),
            "2026-11-20_1430_Teams"
        );
        let long = "あ".repeat(100);
        let stem = file_stem(t(), "Teams", Some(&long));
        assert_eq!(
            stem.chars().count(),
            "2026-11-20_1430_Teams_".chars().count() + 40
        );
    }

    #[test]
    fn unique_paths() {
        let d = temp_dir("unique");
        let p1 = unique_path(&d, "2026-11-20_1430_Zoom", "mp4");
        assert_eq!(p1.file_name().unwrap(), "2026-11-20_1430_Zoom.mp4");
        fs::write(&p1, b"x").unwrap();
        let p2 = unique_path(&d, "2026-11-20_1430_Zoom", "mp4");
        assert_eq!(p2.file_name().unwrap(), "2026-11-20_1430_Zoom_2.mp4");
    }

    #[test]
    fn recording_names() {
        assert!(is_recording_name("2026-11-20_1430_Zoom.mp4"));
        assert!(is_recording_name("2026-11-20_1430_Teams_定例_2.mp4"));
        assert!(!is_recording_name("holiday.mp4"));
        assert!(!is_recording_name("2026-11-20_1430_Zoom.mov"));
        assert!(!is_recording_name("2026-11-20.mp4"));
    }

    #[test]
    fn cleanup_only_touches_old_recordings() {
        let d = temp_dir("cleanup");
        let old = d.join("2020-01-01_0900_Zoom.mp4");
        let new = d.join("2026-11-20_1430_Zoom.mp4");
        let other = d.join("family.mp4");
        for p in [&old, &new, &other] {
            fs::write(p, b"x").unwrap();
        }
        let future = SystemTime::now() + Duration::from_secs(40 * 86_400);
        // days = 0 は何もしない。
        assert!(cleanup_old(&d, 0, future, None).is_empty());
        // 40 日後の時点で 30 日保持：2 つとも対象だが keep は残す。
        let removed = cleanup_old(&d, 30, future, Some(&new));
        assert_eq!(removed, vec![old.clone()]);
        assert!(new.exists() && other.exists() && !old.exists());
    }

    #[test]
    fn marker_roundtrip_and_missing_file() {
        let d = temp_dir("marker");
        let md = d.join("inprogress");
        let rec = d.join("2026-11-20_1430_Zoom.mp4");
        let m = write_marker(&md, &rec).unwrap();
        assert!(m.exists());
        let out = recover(&md);
        assert_eq!(out, vec![RecoveryOutcome::Missing { path: rec.clone() }]);
        assert!(!m.exists());
        // 極小の壊れたファイルは削除。
        fs::write(&rec, b"junk").unwrap();
        write_marker(&md, &rec).unwrap();
        assert_eq!(
            recover(&md),
            vec![RecoveryOutcome::RemovedEmpty { path: rec.clone() }]
        );
        assert!(!rec.exists());
    }

    #[test]
    fn recover_does_not_touch_files_that_are_not_recordings() {
        let d = temp_dir("marker-foreign");
        let md = d.join("inprogress");
        let other = d.join("notes.mp4");
        fs::write(&other, b"junk").unwrap();
        // マーカーの中身だけを、録画ではないファイルに書き換えられた場合。
        fs::create_dir_all(&md).unwrap();
        let marker = md.join("2026-11-20_1430_Zoom.mp4.inprogress");
        fs::write(&marker, other.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(
            recover(&md),
            vec![RecoveryOutcome::NotRecording {
                path: other.clone()
            }]
        );
        assert_eq!(fs::read(&other).unwrap(), b"junk");
        assert!(!marker.exists());
    }
}
