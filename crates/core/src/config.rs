//! 設定（TOML 1 ファイル）。未知のキーは無視し、欠けたキーは既定値で埋める。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::platform::RecordSettings;
use crate::rules::AppRule;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// 会議を検出したら確認ポップアップを出す（既定）。
    #[default]
    Ask,
    /// 確認なしで自動録画。
    Auto,
    /// 検出しても何もしない（手動録画のみ）。
    Off,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoConfig {
    pub fps: u32,
    pub max_long_side: u32,
    pub bitrate_kbps: u32,
    pub keyframe_seconds: u32,
    pub show_cursor: bool,
    /// 自分が画面共有している間は、会議ウィンドウの代わりに共有している画面を録る。
    /// false なら参加者のビデオパネルを録る（共有している画面は録らない）。
    pub record_shared_screen: bool,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            fps: 10,
            max_long_side: 1920,
            bitrate_kbps: 1500,
            keyframe_seconds: 4,
            show_cursor: true,
            record_shared_screen: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    pub bitrate_kbps: u32,
    pub capture_mic: bool,
    /// true なら「ミックス」に加えて相手音声・マイクを別トラックでも保存する。
    pub separate_tracks: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            bitrate_kbps: 96,
            capture_mic: true,
            separate_tracks: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DetectConfig {
    /// 検出ポーリング間隔（秒）。2 未満は 2 に丸める。
    pub poll_seconds: u32,
    /// 開始条件がこの秒数続いたら発火。
    pub start_hold_seconds: u32,
    /// 会議ウィンドウが消えてから終了とみなすまでの猶予。
    pub end_grace_seconds: u32,
    /// マイク解放（かつ強い会議ウィンドウなし）から終了とみなすまでの猶予。
    pub mic_release_grace_seconds: u32,
}

impl Default for DetectConfig {
    fn default() -> Self {
        Self {
            poll_seconds: 2,
            start_hold_seconds: 2,
            end_grace_seconds: 8,
            mic_release_grace_seconds: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// fragmented MP4 の 1 フラグメントの長さ。異常終了時に失う最大時間の目安。
    pub fragment_seconds: u32,
    /// 空きがこれを下回ったら録画を安全に停止する。
    pub min_free_mb: u64,
    /// 0 = 自動削除しない。
    pub retention_days: u32,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            fragment_seconds: 2,
            min_free_mb: 500,
            retention_days: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub mode: Mode,
    /// 空なら既定の「ダウンロード」フォルダ（`storage::Dirs::default_output`）。
    /// メニューの「保存先を変更…」で書き換わる。
    pub output_dir: String,
    /// 初回起動時のログイン項目登録を済ませたか。
    pub autostart_initialized: bool,
    pub video: VideoConfig,
    pub audio: AudioConfig,
    pub detect: DetectConfig,
    pub storage: StorageConfig,
    /// 会議アプリ定義の上書き。空なら組み込みの表（`rules::builtin_rules`）を使う。
    /// 同じ `id` を書くとその定義だけ差し替え、新しい `id` は追加される。
    #[serde(rename = "app", skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<AppRule>,
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, String> {
        let mut cfg: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        cfg.sanitize();
        Ok(cfg)
    }

    /// 範囲外の値を安全側に丸める。
    pub fn sanitize(&mut self) {
        self.video.fps = self.video.fps.clamp(1, 30);
        self.video.max_long_side = self.video.max_long_side.clamp(320, 3840);
        self.video.bitrate_kbps = self.video.bitrate_kbps.clamp(100, 20_000);
        self.video.keyframe_seconds = self.video.keyframe_seconds.clamp(1, 30);
        self.audio.bitrate_kbps = self.audio.bitrate_kbps.clamp(32, 320);
        self.detect.poll_seconds = self.detect.poll_seconds.clamp(2, 30);
        self.detect.start_hold_seconds = self.detect.start_hold_seconds.clamp(0, 60);
        self.detect.end_grace_seconds = self.detect.end_grace_seconds.clamp(1, 600);
        self.detect.mic_release_grace_seconds = self.detect.mic_release_grace_seconds.clamp(1, 600);
        self.storage.fragment_seconds = self.storage.fragment_seconds.clamp(1, 30);
    }

    /// 読み込み。ファイルがなければ既定値。壊れていれば既定値＋エラー文字列
    /// （壊れたファイルは `.bad` に退避して上書きで失わないようにする）。
    pub fn load(path: &Path) -> (Config, Option<String>) {
        match fs::read_to_string(path) {
            Ok(text) => match Config::parse(&text) {
                Ok(cfg) => (cfg, None),
                Err(e) => {
                    let _ = fs::copy(path, path.with_extension("toml.bad"));
                    (
                        Config::default(),
                        Some(format!("設定ファイルを読めません: {e}")),
                    )
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => (Config::default(), None),
            Err(e) => (
                Config::default(),
                Some(format!("設定ファイルを開けません: {e}")),
            ),
        }
    }

    /// 一時ファイルに書いてから rename（途中で落ちても壊れない）。
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let text = toml::to_string_pretty(self).map_err(io::Error::other)?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, path)
    }

    pub fn output_dir(&self, default_dir: &Path) -> PathBuf {
        if self.output_dir.trim().is_empty() {
            default_dir.to_path_buf()
        } else {
            PathBuf::from(self.output_dir.trim())
        }
    }

    pub fn record_settings(&self) -> RecordSettings {
        RecordSettings {
            fps: self.video.fps,
            max_long_side: self.video.max_long_side,
            video_bitrate_kbps: self.video.bitrate_kbps,
            keyframe_seconds: self.video.keyframe_seconds,
            show_cursor: self.video.show_cursor,
            audio_bitrate_kbps: self.audio.bitrate_kbps,
            capture_mic: self.audio.capture_mic,
            separate_tracks: self.audio.separate_tracks,
            fragment_ms: self.storage.fragment_seconds * 1000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_default() {
        let cfg = Config::parse("").unwrap();
        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.mode, Mode::Ask);
        assert_eq!(cfg.video.fps, 10);
        assert!(cfg.video.record_shared_screen);
        assert_eq!(cfg.audio.bitrate_kbps, 96);
    }

    #[test]
    fn partial_and_unknown_keys() {
        let cfg = Config::parse(
            "mode = \"auto\"\nfuture_key = 1\n[video]\nfps = 15\n[storage]\nretention_days = 30\n",
        )
        .unwrap();
        assert_eq!(cfg.mode, Mode::Auto);
        assert_eq!(cfg.video.fps, 15);
        assert_eq!(cfg.video.max_long_side, 1920);
        assert_eq!(cfg.storage.retention_days, 30);
    }

    #[test]
    fn sanitize_clamps() {
        let cfg = Config::parse("[video]\nfps = 0\n[detect]\npoll_seconds = 0\n").unwrap();
        assert_eq!(cfg.video.fps, 1);
        assert_eq!(cfg.detect.poll_seconds, 2);
    }

    #[test]
    fn roundtrip_and_save() {
        let dir = crate::testutil::temp_dir("config");
        let path = dir.join("config.toml");
        let cfg = Config {
            mode: Mode::Off,
            output_dir: "/tmp/x".into(),
            apps: vec![crate::rules::builtin_rules().remove(0)],
            ..Default::default()
        };
        cfg.save(&path).unwrap();
        let (loaded, err) = Config::load(&path);
        assert!(err.is_none());
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn broken_file_falls_back_and_is_preserved() {
        let dir = crate::testutil::temp_dir("config-bad");
        let path = dir.join("config.toml");
        fs::write(&path, "mode = [broken").unwrap();
        let (cfg, err) = Config::load(&path);
        assert_eq!(cfg, Config::default());
        assert!(err.is_some());
        assert!(path.with_extension("toml.bad").exists());
    }

    #[test]
    fn missing_file_is_default_without_error() {
        let (cfg, err) = Config::load(Path::new("/nonexistent/meetrec/config.toml"));
        assert_eq!(cfg, Config::default());
        assert!(err.is_none());
    }
}
