//! OS 層との境界。core はここに定義した型と trait だけを通じて OS に触れる。
//!
//! すべての `Platform` メソッドはメインスレッド（UI スレッド）から呼ばれる。
//! OS 層は自分のイベントループを回し、`UiEvent` をメインスレッドで
//! `App::on_event` に渡す。

use std::path::{Path, PathBuf};

use crate::config::Mode;

pub type Pid = u32;

/// 1 秒のナノ秒数。`Platform::now_ns` と録画セッションの時計はナノ秒単位。
pub const NS_PER_SEC: u64 = 1_000_000_000;

/// `Platform::find_control` で部品が見つからなかったあと、OS 層が探し直すまでの間隔（両 OS で揃える）。
pub const CONTROL_SEARCH_RETRY_NS: u64 = 10 * NS_PER_SEC;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcInfo {
    pub pid: Pid,
    /// 実行ファイル名（パスなし。Windows は `Zoom.exe` のように拡張子付き）。
    pub name: String,
    /// macOS のバンドル ID。Windows では None。
    pub bundle_id: Option<String>,
}

/// マイクを使用中のプロセス。`chain[0]` が本人、以降は親・祖父…（最大数段）。
/// Teams のように子プロセス（WebView）が音声を扱うアプリを親で判定するため。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MicUser {
    pub chain: Vec<ProcInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowInfo {
    /// OS のウィンドウ識別子（macOS: CGWindowID、Windows: HWND）。
    pub id: u64,
    pub owner: ProcInfo,
    pub title: String,
    /// Windows のウィンドウクラス名。macOS では空文字列。
    pub class: String,
    /// 左上の位置（全ディスプレイを通した座標）。どのディスプレイにあるかを見分けるのに使う。
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub minimized: bool,
    /// ウィンドウの階層。0 = 通常。macOS では常に手前に出るパネル（Zoom の共有中の
    /// ビデオパネル等）が 0 以外になる。Windows では常に 0。
    pub layer: i32,
}

/// ローカル時刻（ファイル名用）。OS 層が提供する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LocalTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RawUsage {
    /// 物理メモリ使用量（macOS: phys_footprint、Windows: Private Working Set 相当）。
    pub rss_bytes: u64,
    /// プロセス累積 CPU 時間（user+system）。
    pub cpu_time_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    pub screen: bool,
    pub microphone: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayModel {
    pub recording: bool,
    /// 「待機中」「録画中 00:12:34」など。
    pub status: String,
    pub mode: Mode,
    pub autostart: bool,
    /// 「自分の声を録る」がオンか。
    pub mic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptModel {
    pub message: String,
    pub record_label: String,
    pub skip_label: String,
    pub always_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub body: String,
    /// クリック時に開くパス（フォルダ）。
    pub open: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    RecordNow,
    StopNow,
    OpenFolder,
    /// 保存先フォルダを選び直す。
    ChooseFolder,
    SetMode(Mode),
    ToggleAutostart,
    /// 「自分の声を録る」の切り替え。
    ToggleMic,
    SaveDiagnostics,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAnswer {
    Record { always: bool },
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecorderEvent {
    /// 録画が続行不能になった。OS 層は既にセッションを確定済みであること。
    Failed(String),
    /// 映像が取得できない（キャプチャ保護・権限なし等）。音声のみで継続中。
    VideoUnavailable(String),
    /// Windows：消音だった出力機器を、消音を外して音量 0 に切り替えた（`RecordRequest::system_audio` の録画。
    /// 録画が終わったら OS 層が元に戻す）。
    OutputUnmuted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    Launched,
    /// 定期ティック（`Config::detect.poll_seconds` 間隔）。
    /// ウィンドウ生成/破棄イベントを受けた OS 層が早めに送ってもよい。
    Tick,
    Menu(MenuAction),
    Prompt(PromptAnswer),
    Recorder(RecorderEvent),
    WillSleep,
    DidWake,
    /// ログアウト・シャットダウン・OS からの終了要求。
    Shutdown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordSettings {
    pub fps: u32,
    pub max_long_side: u32,
    pub video_bitrate_kbps: u32,
    pub keyframe_seconds: u32,
    pub show_cursor: bool,
    pub audio_bitrate_kbps: u32,
    pub capture_mic: bool,
    pub separate_tracks: bool,
    pub fragment_ms: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordRequest {
    pub path: PathBuf,
    /// 進行中マーカーファイルの置き場所（`storage::marker_path` 参照）。
    pub marker_dir: PathBuf,
    /// 録画対象ウィンドウ。None なら音声のみ。
    pub window: Option<WindowInfo>,
    /// 音声を録る会議アプリのルートプロセス（Windows: プロセスツリーの根）。
    pub root_pid: Pid,
    /// Windows：会議アプリの音だけでなく、PC から出る音すべてを録る（`AppRule::system_audio`）。
    pub system_audio: bool,
    /// Windows：`system_audio` の録画で、出力機器が消音のあいだは、消音を外して音量を 0 にする（D37。
    /// 設定 `audio.unmute_output`）。false なら出力機器の設定に触らない。
    pub unmute_output: bool,
    pub settings: RecordSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecordingSummary {
    pub path: PathBuf,
    pub duration_ms: u64,
    pub bytes: u64,
    pub video_frames: u64,
    pub audio_only: bool,
    /// 何も書かれずファイルを削除した場合 true。
    pub empty: bool,
}

pub trait Platform {
    // --- 観測（軽い API のみ。2 秒以上の間隔で呼ばれる） ---
    fn mic_users(&mut self) -> Vec<MicUser>;
    /// トップレベルウィンドウ一覧（`WindowInfo::layer` が 0 以外のものも含む）。
    /// 会議アプリ候補があるときだけ呼ばれる。
    fn windows(&mut self) -> Vec<WindowInfo>;
    fn local_time(&self) -> LocalTime;
    /// 単調時計。録画セッションのタイムスタンプと同じ時計であること。
    fn now_ns(&self) -> u64;
    fn free_space(&self, dir: &Path) -> Option<u64>;
    fn resource_usage(&self) -> Option<RawUsage>;
    /// 実機調査用の追加情報（会議アプリらしきプロセス一覧など）。
    fn diagnostics(&mut self) -> String;

    // --- UI ---
    fn set_tray(&mut self, model: &TrayModel);
    fn show_prompt(&mut self, prompt: &PromptModel);
    fn hide_prompt(&mut self);
    fn notify(&mut self, notice: &Notice);
    fn open_path(&mut self, path: &Path);
    /// フォルダ選択ダイアログを出す（モーダル）。キャンセルなら None。
    fn choose_folder(&mut self, current: &Path) -> Option<PathBuf>;
    fn set_autostart(&mut self, enabled: bool) -> Result<(), String>;
    fn autostart_enabled(&self) -> bool;
    fn permissions(&mut self) -> Permissions;
    /// 権限の案内（ネイティブダイアログ＋設定を開くボタン）を出す。
    fn request_permissions(&mut self, missing: Permissions);

    // --- 録画 ---
    fn start_recording(&mut self, req: &RecordRequest) -> Result<(), String>;
    /// 録画対象ウィンドウを張り替える（ファイルは継続）。画面を録っていた場合はそれをやめる。
    fn retarget(&mut self, window: &WindowInfo);
    /// 映像を、`near` のウィンドウがあるディスプレイ全体に切り替える（自分の画面共有中）。
    /// 音声の録り方は変えない。対応していない・失敗した場合は false。
    /// すでに画面を録っているときにも呼ばれる（共有の目印が動いたとき）。同じディスプレイなら何もせず true、
    /// 別のディスプレイなら切り替える。決められない・失敗した場合は、今の画面を録り続けて false。
    fn retarget_screen(&mut self, near: &WindowInfo) -> bool;
    /// 画面の録画をやめる（張り替え先のウィンドウがないとき。映像は止まり、音声は続く）。
    fn end_screen(&mut self);
    /// 会議アプリ（`pid`）の画面部品（ボタン・メニュー項目）のうち、名前が `names` のどれかで始まる
    /// （大文字小文字無視。`rules::starts_with_ci`）ものを探し、その名前を返す。見つからない・読めない（許可がない、
    /// 未対応）場合は None。2 秒ごとに呼ばれるので、OS 層は見つけた部品を覚えて軽く済ませること。
    fn find_control(&mut self, pid: Pid, names: &[String]) -> Option<String>;
    /// 録画中に、自分のマイクを録る・録らないを切り替える（録らない間は無音になる）。
    /// 録画中でない・マイクを開いていない場合は何もしない。
    fn set_mic_enabled(&mut self, enabled: bool);
    /// 会議に入ってしばらく経った。以後、この録画では出力機器の音量を 0 に戻さない（`RecordRequest::unmute_output`
    /// の録画。消音を外すのは続ける。D37）。出力機器に触らない OS 層は何もしない。
    fn settle_output(&mut self);
    /// 同期的に確定して結果を返す。録画中でなければ None。
    fn stop_recording(&mut self) -> Option<RecordingSummary>;

    fn quit(&mut self);
}

/// OS 層のイベントループから呼ばれるアプリ本体。
pub trait App {
    fn on_event(&mut self, platform: &mut dyn Platform, event: UiEvent);
    /// 次のティックまでの秒数（OS 層のタイマー設定用）。
    fn tick_seconds(&self) -> u32;
}
