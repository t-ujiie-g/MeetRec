//! `Controller` のテスト。OS 層をモック（`Mock`）に差し替え、検出から録画・追従・停止までを通しで確かめる。
//! 実機で分かった会議アプリの挙動は、ここに「実機で確認」と書いて固定する。

use super::*;
use crate::platform::{LocalTime, MicUser, RawUsage, RecordingSummary, WindowInfo};
use crate::rules::tests::{proc_, win};
use crate::testutil::temp_dir;
use std::path::Path;

#[derive(Default)]
struct Mock {
    now: u64,
    mic: Vec<MicUser>,
    windows: Vec<WindowInfo>,
    window_calls: u32,
    free: Option<u64>,
    autostart: bool,
    perms_ok: bool,
    fail_start: bool,
    recording: Option<RecordRequest>,
    calls: Vec<String>,
    chosen: Option<PathBuf>,
    screen_ok: bool,
    /// 会議アプリに今あるミュートの部品の名前。
    control: Option<String>,
    notices: Vec<Notice>,
    tray: Vec<TrayModel>,
}

impl Platform for Mock {
    fn mic_users(&mut self) -> Vec<MicUser> {
        self.mic.clone()
    }
    fn windows(&mut self) -> Vec<WindowInfo> {
        self.window_calls += 1;
        self.windows.clone()
    }
    fn local_time(&self) -> LocalTime {
        LocalTime {
            year: 2026,
            month: 11,
            day: 20,
            hour: 14,
            minute: 30,
            second: 0,
        }
    }
    fn now_ns(&self) -> u64 {
        self.now
    }
    fn free_space(&self, _: &Path) -> Option<u64> {
        self.free
    }
    fn resource_usage(&self) -> Option<RawUsage> {
        Some(RawUsage {
            rss_bytes: 20 << 20,
            cpu_time_ns: self.now / 1000,
        })
    }
    fn diagnostics(&mut self) -> String {
        "mock".into()
    }
    fn set_tray(&mut self, model: &TrayModel) {
        self.tray.push(model.clone());
    }
    fn show_prompt(&mut self, _: &PromptModel) {
        self.calls.push("show_prompt".into());
    }
    fn hide_prompt(&mut self) {
        self.calls.push("hide_prompt".into());
    }
    fn notify(&mut self, notice: &Notice) {
        self.notices.push(notice.clone());
    }
    fn open_path(&mut self, path: &Path) {
        self.calls.push(format!("open {}", path.display()));
    }
    fn choose_folder(&mut self, _: &Path) -> Option<PathBuf> {
        self.chosen.clone()
    }
    fn set_autostart(&mut self, enabled: bool) -> Result<(), String> {
        self.autostart = enabled;
        Ok(())
    }
    fn autostart_enabled(&self) -> bool {
        self.autostart
    }
    fn permissions(&mut self) -> Permissions {
        Permissions {
            screen: self.perms_ok,
            microphone: self.perms_ok,
        }
    }
    fn request_permissions(&mut self, _: Permissions) {
        self.calls.push("request_permissions".into());
    }
    fn start_recording(&mut self, req: &RecordRequest) -> Result<(), String> {
        if self.fail_start {
            return Err("boom".into());
        }
        self.calls.push(format!(
            "start {}",
            req.path.file_name().unwrap().to_string_lossy()
        ));
        self.recording = Some(req.clone());
        Ok(())
    }
    fn retarget(&mut self, window: &WindowInfo) {
        self.calls.push(format!("retarget {}", window.id));
    }
    fn retarget_screen(&mut self, near: &WindowInfo) -> bool {
        self.calls.push(format!("screen {}", near.id));
        self.screen_ok
    }
    fn end_screen(&mut self) {
        self.calls.push("end_screen".into());
    }
    fn set_mic_enabled(&mut self, enabled: bool) {
        self.calls.push(format!("mic {enabled}"));
    }
    fn find_control(&mut self, _: Pid, names: &[String]) -> Option<String> {
        self.control
            .clone()
            .filter(|c| names.iter().any(|n| crate::rules::starts_with_ci(c, n)))
    }
    fn stop_recording(&mut self) -> Option<RecordingSummary> {
        let req = self.recording.take()?;
        self.calls.push("stop".into());
        Some(RecordingSummary {
            path: req.path,
            duration_ms: 65_000,
            bytes: 1,
            ..Default::default()
        })
    }
    fn quit(&mut self) {
        self.calls.push("quit".into());
    }
}

struct H {
    c: Controller,
    p: Mock,
    dirs: Dirs,
}

fn zoom() -> crate::platform::ProcInfo {
    proc_(7, "Zoom.exe", None)
}
fn meeting_win() -> WindowInfo {
    win(
        1,
        zoom(),
        "Zoom Meeting",
        "ZPContentViewWndClass",
        1280,
        720,
    )
}

impl H {
    fn new(name: &str, mode: Mode) -> H {
        let d = temp_dir(name);
        let dirs = Dirs {
            state: d.join("state"),
            default_output: d.join("out"),
        };
        let cfg = Config {
            mode,
            autostart_initialized: true,
            ..Default::default()
        };
        let p = Mock {
            perms_ok: true,
            now: 1000 * NS,
            ..Default::default()
        };
        let mut h = H {
            c: Controller::new(cfg, dirs.clone(), None),
            p,
            dirs,
        };
        h.ev(UiEvent::Launched);
        h
    }
    fn ev(&mut self, e: UiEvent) {
        self.c.on_event(&mut self.p, e);
    }
    fn tick(&mut self) {
        self.p.now += 2 * NS;
        self.ev(UiEvent::Tick);
    }
    fn ticks(&mut self, n: usize) {
        for _ in 0..n {
            self.tick();
        }
    }
    /// macOS の Teams で会議に入って録画が始まった状態にする（実機で見えたウィンドウ構成）。
    /// 戻り値は（本体のウィンドウ, 会議ウィンドウ）。
    fn join_teams_mac(&mut self) -> (WindowInfo, WindowInfo) {
        let teams = || proc_(9, "MSTeams", Some("com.microsoft.teams2"));
        let main = win(1, teams(), "ミーティング", "", 1368, 769);
        let meeting = win(2, teams(), "test", "", 1277, 798);
        self.p.mic = vec![MicUser {
            chain: vec![teams()],
        }];
        self.p.windows = vec![main.clone(), meeting.clone()];
        self.ticks(3);
        assert!(self.c.is_recording());
        (main, meeting)
    }
    fn join(&mut self) {
        self.p.mic = vec![MicUser {
            chain: vec![zoom()],
        }];
        self.p.windows = vec![meeting_win()];
    }
    fn leave(&mut self) {
        self.p.mic.clear();
        self.p.windows.clear();
    }
    fn count(&self, call: &str) -> usize {
        self.p.calls.iter().filter(|c| c.starts_with(call)).count()
    }
    fn bodies(&self) -> String {
        self.p
            .notices
            .iter()
            .map(|n| n.body.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[test]
fn idle_rarely_enumerates_windows() {
    let mut h = H::new("ctl-idle", Mode::Ask);
    let home = win(
        2,
        zoom(),
        "Zoom Workplace",
        "ZPPTMainFrmWndClassEx",
        900,
        600,
    );
    h.p.windows = vec![home];
    h.ticks(50);
    // 100 秒で 10 回程度（マイク未使用時の保険の走査）だけ。
    assert!(h.p.window_calls <= 11, "{}", h.p.window_calls);
    assert_eq!(h.p.tray.len(), 1);
    assert_eq!(h.p.tray[0].status, text::STATUS_IDLE);
    assert_eq!(h.count("show_prompt"), 0);
}

#[test]
fn muted_join_is_detected_by_window_scan() {
    let mut h = H::new("ctl-micless", Mode::Ask);
    h.p.windows = vec![meeting_win()]; // マイクは誰も使っていない
    h.ticks(8);
    assert_eq!(h.count("show_prompt"), 1);
    h.ev(UiEvent::Prompt(PromptAnswer::Record { always: false }));
    assert_eq!(h.p.recording.as_ref().unwrap().root_pid, 7);
}

#[test]
fn ask_mode_prompts_once_and_records_on_accept() {
    let mut h = H::new("ctl-ask", Mode::Ask);
    h.join();
    h.ticks(5);
    assert_eq!(h.count("show_prompt"), 1);
    assert!(!h.c.is_recording());
    assert_eq!(h.p.tray.last().unwrap().status, text::STATUS_PROMPT);
    h.ev(UiEvent::Prompt(PromptAnswer::Record { always: false }));
    assert!(h.c.is_recording());
    assert_eq!(
        h.p.calls
            .iter()
            .filter(|c| *c == "start 2026-11-20_1430_Zoom.mp4")
            .count(),
        1
    );
    assert_eq!(h.c.cfg.mode, Mode::Ask);
    let req = h.p.recording.clone().unwrap();
    assert_eq!(req.root_pid, 7);
    assert_eq!(req.window.unwrap().id, 1);
    h.ticks(30);
    assert_eq!(h.p.tray.last().unwrap().status, "録画中 00:01:00");
    assert!(h.p.tray.last().unwrap().recording);
    // 会議終了 → 猶予後に自動停止して通知。
    h.leave();
    h.ticks(4);
    assert!(h.c.is_recording());
    h.ticks(1);
    assert!(!h.c.is_recording());
    assert_eq!(h.count("stop"), 1);
    let last = h.p.notices.last().unwrap();
    assert!(last.body.contains("保存しました") && last.body.contains("1分5秒"));
    assert_eq!(last.open.as_deref(), Some(h.dirs.default_output.as_path()));
    assert_eq!(h.count("show_prompt"), 1);
}

#[test]
fn decline_suppresses_until_next_meeting() {
    let mut h = H::new("ctl-decline", Mode::Ask);
    h.join();
    h.ticks(3);
    h.ev(UiEvent::Prompt(PromptAnswer::Skip));
    h.ticks(50);
    assert_eq!(h.count("show_prompt"), 1);
    assert!(!h.c.is_recording());
    h.leave();
    h.ticks(6);
    h.join();
    h.ticks(3);
    assert_eq!(h.count("show_prompt"), 2);
}

#[test]
fn prompt_is_hidden_when_meeting_ends_unanswered() {
    let mut h = H::new("ctl-unanswered", Mode::Ask);
    h.join();
    h.ticks(3);
    h.leave();
    h.ticks(6);
    assert_eq!(h.count("hide_prompt"), 1);
    assert!(!h.c.is_recording());
    // 終了後に届いた回答は無視する。
    h.ev(UiEvent::Prompt(PromptAnswer::Record { always: false }));
    assert!(!h.c.is_recording());
}

#[test]
fn always_checkbox_switches_to_auto_and_persists() {
    let mut h = H::new("ctl-always", Mode::Ask);
    h.join();
    h.ticks(3);
    h.ev(UiEvent::Prompt(PromptAnswer::Record { always: true }));
    assert!(h.c.is_recording());
    assert_eq!(h.c.cfg.mode, Mode::Auto);
    let (saved, _) = Config::load(&h.dirs.config_file());
    assert_eq!(saved.mode, Mode::Auto);
}

#[test]
fn auto_mode_records_without_prompt() {
    let mut h = H::new("ctl-auto", Mode::Auto);
    h.join();
    h.ticks(3);
    assert!(h.c.is_recording());
    assert_eq!(h.count("show_prompt"), 0);
    assert!(h.bodies().contains(text::NOTICE_STARTED));
    // 手動停止したら同じ会議では再開しない。
    h.ev(UiEvent::Menu(MenuAction::StopNow));
    h.ticks(20);
    assert!(!h.c.is_recording());
    assert_eq!(h.count("start"), 1);
}

#[test]
fn off_mode_ignores_but_manual_works_and_ends_with_meeting() {
    let mut h = H::new("ctl-off", Mode::Off);
    h.join();
    h.ticks(5);
    assert!(!h.c.is_recording());
    assert_eq!(h.count("show_prompt"), 0);
    h.ev(UiEvent::Menu(MenuAction::RecordNow));
    assert!(h.c.is_recording());
    h.leave();
    h.ticks(6);
    assert!(!h.c.is_recording());
}

#[test]
fn manual_record_without_meeting() {
    let mut h = H::new("ctl-manual", Mode::Ask);
    h.ev(UiEvent::Menu(MenuAction::RecordNow));
    assert!(!h.c.is_recording());
    assert!(h.bodies().contains(text::NOTICE_NO_WINDOW));
    // マイク未使用でも会議アプリのウィンドウがあれば録れる。
    let teams = proc_(30, "ms-teams.exe", None);
    h.p.windows = vec![win(
        10,
        teams,
        "週次定例 | Microsoft Teams",
        "TeamsWebView",
        1200,
        800,
    )];
    h.ev(UiEvent::Menu(MenuAction::RecordNow));
    assert!(h.c.is_recording());
    assert_eq!(h.count("start 2026-11-20_1430_Teams_週次定例.mp4"), 1);
    assert_eq!(h.p.recording.as_ref().unwrap().root_pid, 30);
    h.ticks(20);
    assert!(h.c.is_recording());
    // ウィンドウが全部消えたら猶予後に止まる。
    h.p.windows.clear();
    h.ticks(5);
    assert!(!h.c.is_recording());
}

#[test]
fn mic_toggle_is_saved_and_applied_to_the_running_recording() {
    let mut h = H::new("ctl-mic", Mode::Auto);
    h.join();
    h.ticks(3);
    assert!(h.p.recording.as_ref().unwrap().settings.capture_mic);
    h.ev(UiEvent::Menu(MenuAction::ToggleMic));
    assert_eq!(h.count("mic false"), 1);
    assert!(!Config::load(&h.dirs.config_file()).0.audio.capture_mic);
    assert!(!h.p.tray.last().unwrap().mic);
    h.ev(UiEvent::Menu(MenuAction::ToggleMic));
    assert_eq!(h.count("mic true"), 1);
    assert!(h.p.tray.last().unwrap().mic);
}

#[test]
fn mic_follows_the_meeting_app_mute_button() {
    let mut h = H::new("ctl-mute", Mode::Auto);
    h.join_teams_mac();
    // 部品が読めない間は録り続ける。
    assert_eq!(h.count("mic"), 0);
    // ミュート中：ボタンは「解除」を表す。
    h.p.control = Some("マイクのミュートを解除".into());
    h.ticks(2);
    assert_eq!(h.count("mic false"), 1);
    // ミュート中に「自分の声を録る」をオフ → オンにしても、ミュートが解けるまでは録らない。
    h.ev(UiEvent::Menu(MenuAction::ToggleMic));
    h.ev(UiEvent::Menu(MenuAction::ToggleMic));
    assert_eq!(h.count("mic true"), 0);
    // 解除：録り始める。
    h.p.control = Some("マイクのミュート".into());
    h.tick();
    assert_eq!(h.count("mic true"), 1);
    // ミュート中に部品が読めなくなったら、録る側に倒す。
    h.p.control = Some("マイクのミュートを解除".into());
    h.tick();
    h.p.control = None;
    h.tick();
    assert_eq!(h.count("mic true"), 2);
    // 設定でオフにしていれば、ミュートには合わせない。
    h.c.cfg.audio.follow_app_mute = false;
    h.p.control = Some("マイクのミュートを解除".into());
    h.ticks(2);
    assert_eq!(h.count("mic false"), 2);
}

#[test]
fn choosing_a_folder_changes_where_recordings_go() {
    let mut h = H::new("ctl-folder", Mode::Auto);
    // キャンセル：何も変わらない。
    h.ev(UiEvent::Menu(MenuAction::ChooseFolder));
    assert_eq!(h.c.cfg.output_dir, "");
    let picked = h.dirs.state.join("picked");
    h.p.chosen = Some(picked.clone());
    h.ev(UiEvent::Menu(MenuAction::ChooseFolder));
    assert_eq!(h.c.output_dir(), picked);
    assert!(picked.is_dir());
    let (saved, _) = Config::load(&h.dirs.config_file());
    assert_eq!(saved.output_dir, picked.to_string_lossy());
    assert_eq!(h.p.notices.last().unwrap().open.as_deref(), Some(&*picked));
    h.join();
    h.ticks(3);
    assert_eq!(
        h.p.recording.as_ref().unwrap().path.parent(),
        Some(&*picked)
    );
    // 既定の保存先を選び直すと、設定は空（既定に追従）に戻る。
    h.p.chosen = Some(h.dirs.default_output.clone());
    h.ev(UiEvent::Menu(MenuAction::ChooseFolder));
    assert_eq!(h.c.cfg.output_dir, "");
}

#[test]
fn hidden_floating_panel_left_after_the_meeting_does_not_keep_recording() {
    let mut h = H::new("ctl-leftover-panel", Mode::Auto);
    h.join();
    h.ticks(3);
    assert!(h.c.is_recording());
    // 退出：会議ウィンドウは消え、マイクも解放されるが、隠れたビデオパネルだけが残る（macOS の Zoom）。
    let mut panel = win(6, zoom(), "zoom floating video window", "", 244, 139);
    panel.layer = 26;
    panel.minimized = true;
    h.p.windows = vec![panel];
    h.p.mic.clear();
    h.ticks(10);
    assert!(!h.c.is_recording());
    assert_eq!(h.count("retarget"), 0);
}

#[test]
fn own_screen_share_records_the_screen_and_returns_to_the_meeting() {
    let mut h = H::new("ctl-share-screen", Mode::Auto);
    h.p.screen_ok = true;
    h.join();
    h.ticks(3);
    // 共有開始：会議ウィンドウは隠れ、ビデオパネルとツールバーが出る。
    let mut hidden = meeting_win();
    hidden.minimized = true;
    let float = win(5, zoom(), "", "ZPFloatVideoWndClass", 240, 135);
    let toolbar = win(4, zoom(), "", "ZPToolBarParentWndClass", 600, 60);
    h.p.windows = vec![hidden, float.clone(), toolbar];
    h.ticks(5);
    assert_eq!(h.count("screen 4"), 1);
    // 画面を録っている間はビデオパネルへ張り替えない。
    assert_eq!(h.count("retarget"), 0);
    // 共有終了：すぐ会議ウィンドウへ戻す。
    h.p.windows = vec![meeting_win(), float];
    h.tick();
    assert_eq!(h.count("retarget 1"), 1);
    assert_eq!(h.count("end_screen"), 0);
    assert!(h.c.is_recording());
}

#[test]
fn leaving_while_sharing_stops_recording_the_screen_at_once() {
    let mut h = H::new("ctl-share-leave", Mode::Auto);
    h.p.screen_ok = true;
    h.join();
    h.ticks(3);
    let toolbar = win(4, zoom(), "", "ZPToolBarParentWndClass", 600, 60);
    h.p.windows = vec![toolbar];
    h.tick();
    assert_eq!(h.count("screen 4"), 1);
    // 共有したまま退出：ウィンドウがすべて消える。会議の終了判定を待たずに画面の録画をやめる。
    h.p.windows.clear();
    h.tick();
    assert_eq!(h.count("end_screen"), 1);
}

#[test]
fn does_not_move_from_a_closed_meeting_window_to_the_app_main_window() {
    let mut h = H::new("ctl-teams-main", Mode::Auto);
    let (main, _) = h.join_teams_mac();
    assert_eq!(
        h.p.recording.as_ref().unwrap().window.as_ref().unwrap().id,
        2
    );
    // Teams は「PC から出る音すべて」を録る指定で始まる（Windows の OS 層だけが使う）。
    assert!(h.p.recording.as_ref().unwrap().system_audio);
    // 退出：会議ウィンドウが閉じ、本体だけが残る。本体の録画には移らない。
    h.p.windows = vec![main];
    h.ticks(2);
    assert_eq!(h.count("retarget"), 0);
}

#[test]
fn hidden_teams_meeting_window_left_after_leaving_does_not_keep_recording() {
    let mut h = H::new("ctl-teams-leftover", Mode::Auto);
    let (main, meeting) = h.join_teams_mac();
    // 退出：マイクは解放されるが、会議ウィンドウは隠れたまま残る（macOS の Teams で実機確認）。
    let mut hidden = meeting;
    hidden.minimized = true;
    h.p.windows = vec![main, hidden];
    h.p.mic.clear();
    h.ticks(10);
    assert!(!h.c.is_recording());
}

#[test]
fn retargets_when_window_changes() {
    let mut h = H::new("ctl-retarget", Mode::Auto);
    h.join();
    h.ticks(3);
    // 画面共有：会議ウィンドウが消え、フローティングビデオとツールバーになる。
    let float = win(5, zoom(), "", "ZPFloatVideoWndClass", 240, 135);
    let toolbar = win(4, zoom(), "", "ZPToolBarParentWndClass", 600, 60);
    h.p.windows = vec![float.clone(), toolbar];
    h.tick();
    assert_eq!(h.count("retarget 5"), 1);
    // 共有終了：会議ウィンドウ（別 ID）が戻る。安定を待ってから張り替える。
    let back = win(
        9,
        zoom(),
        "Zoom Meeting",
        "ZPContentViewWndClass",
        1280,
        720,
    );
    h.p.windows = vec![float, back];
    h.tick();
    assert_eq!(h.count("retarget 9"), 0);
    h.tick();
    assert_eq!(h.count("retarget 9"), 1);
    h.ticks(10);
    assert_eq!(h.count("retarget"), 2);
    assert!(h.c.is_recording());
}

#[test]
fn minimized_meeting_switches_to_mini_window_and_back() {
    let mut h = H::new("ctl-mini", Mode::Auto);
    h.join();
    h.ticks(3);
    // 会議ウィンドウを最小化すると Zoom がミニウィンドウを出す：映っている方へ張り替える。
    h.p.windows[0].minimized = true;
    h.p.windows
        .push(win(5, zoom(), "", "ZPFloatVideoWndClass", 240, 135));
    h.ticks(2);
    assert_eq!(h.count("retarget 5"), 1);
    // 元に戻したら会議ウィンドウへ戻す。
    h.p.windows[0].minimized = false;
    h.ticks(2);
    assert_eq!(h.count("retarget 1"), 1);
    h.ticks(10);
    assert_eq!(h.count("retarget"), 2);
    assert!(h.c.is_recording());
}

#[test]
fn minimized_window_keeps_recording_without_retarget() {
    let mut h = H::new("ctl-min", Mode::Auto);
    h.join();
    h.ticks(3);
    h.p.windows[0].minimized = true;
    h.ticks(10);
    assert!(h.c.is_recording());
    assert_eq!(h.count("retarget"), 0);
}

#[test]
fn low_disk_refuses_start_and_stops_recording() {
    let mut h = H::new("ctl-disk", Mode::Auto);
    h.p.free = Some(100 << 20);
    h.join();
    h.ticks(3);
    assert!(!h.c.is_recording());
    assert!(h.bodies().contains(text::NOTICE_DISK_LOW_START));
    h.leave();
    h.ticks(6);
    h.p.free = Some(10 << 30);
    h.join();
    h.ticks(3);
    assert!(h.c.is_recording());
    h.p.free = Some(100 << 20);
    h.ticks(6);
    assert!(!h.c.is_recording());
    assert!(h.bodies().contains(text::NOTICE_DISK_LOW));
    // 同じ会議では再開しない。
    h.p.free = Some(10 << 30);
    h.ticks(10);
    assert!(!h.c.is_recording());
}

#[test]
fn sleep_stops_and_resumes_after_wake() {
    let mut h = H::new("ctl-sleep", Mode::Ask);
    h.join();
    h.ticks(3);
    h.ev(UiEvent::Prompt(PromptAnswer::Record { always: false }));
    h.ev(UiEvent::WillSleep);
    assert!(!h.c.is_recording());
    h.ev(UiEvent::DidWake);
    h.tick();
    assert!(h.c.is_recording());
    assert_eq!(h.count("show_prompt"), 1);
    assert_eq!(h.count("start 2026-11-20_1430_Zoom"), 2);
}

#[test]
fn start_failure_notifies_and_does_not_retry() {
    let mut h = H::new("ctl-fail", Mode::Auto);
    h.p.fail_start = true;
    h.join();
    h.ticks(10);
    assert!(!h.c.is_recording());
    assert_eq!(
        h.p.notices
            .iter()
            .filter(|n| n.body.contains("boom"))
            .count(),
        1
    );
}

#[test]
fn recorder_failure_and_audio_only_notice() {
    let mut h = H::new("ctl-recfail", Mode::Auto);
    h.join();
    h.ticks(3);
    h.ev(UiEvent::Recorder(RecorderEvent::VideoUnavailable(
        "protected".into(),
    )));
    h.ev(UiEvent::Recorder(RecorderEvent::VideoUnavailable(
        "protected".into(),
    )));
    assert_eq!(
        h.p.notices
            .iter()
            .filter(|n| n.body == text::NOTICE_AUDIO_ONLY)
            .count(),
        1
    );
    assert!(h.c.is_recording());
    h.ev(UiEvent::Recorder(RecorderEvent::Failed("disk".into())));
    assert!(!h.c.is_recording());
    assert!(h.bodies().contains(text::NOTICE_FAILED));
    h.ticks(10);
    assert!(!h.c.is_recording());
}

#[test]
fn output_unmuted_is_notified_each_time_while_recording() {
    let mut h = H::new("ctl-unmuted", Mode::Auto);
    let count = |h: &H| {
        h.p.notices
            .iter()
            .filter(|n| n.body == text::NOTICE_OUTPUT_UNMUTED)
            .count()
    };
    // 録画していないときに届いても知らせない（停止と行き違った場合）。
    h.ev(UiEvent::Recorder(RecorderEvent::OutputUnmuted));
    assert_eq!(count(&h), 0);
    h.join();
    h.ticks(3);
    assert!(h.c.is_recording());
    // PC の設定を変えたことは、切り替えるたびに知らせる。
    h.ev(UiEvent::Recorder(RecorderEvent::OutputUnmuted));
    h.ev(UiEvent::Recorder(RecorderEvent::OutputUnmuted));
    assert_eq!(count(&h), 2);
    assert!(h.c.is_recording());
}

#[test]
fn mode_switch_during_meeting() {
    let mut h = H::new("ctl-mode", Mode::Ask);
    h.join();
    h.ticks(3);
    h.ev(UiEvent::Menu(MenuAction::SetMode(Mode::Off)));
    assert_eq!(h.count("hide_prompt"), 1);
    h.ev(UiEvent::Menu(MenuAction::SetMode(Mode::Ask)));
    assert_eq!(h.count("show_prompt"), 2);
    h.ev(UiEvent::Menu(MenuAction::SetMode(Mode::Auto)));
    assert!(h.c.is_recording());
    assert_eq!(h.p.tray.last().unwrap().mode, Mode::Auto);
}

#[test]
fn first_launch_enables_autostart_and_requests_permissions() {
    let d = temp_dir("ctl-first");
    let dirs = Dirs {
        state: d.join("state"),
        default_output: d.join("out"),
    };
    let mut p = Mock::default();
    let mut c = Controller::new(Config::default(), dirs.clone(), Some("設定エラー".into()));
    c.on_event(&mut p, UiEvent::Launched);
    assert!(p.autostart);
    assert!(p.calls.contains(&"request_permissions".to_string()));
    assert!(p.notices[0].body.contains("設定エラー"));
    assert!(Config::load(&dirs.config_file()).0.autostart_initialized);
    // 2 回目以降はユーザーの選択を尊重して触らない。
    p.autostart = false;
    let (cfg, _) = Config::load(&dirs.config_file());
    let mut c = Controller::new(cfg, dirs, None);
    c.on_event(&mut p, UiEvent::Launched);
    assert!(!p.autostart);
    c.on_event(&mut p, UiEvent::Menu(MenuAction::ToggleAutostart));
    assert!(p.autostart);
    assert!(p.tray.last().unwrap().autostart);
}

#[test]
fn quit_and_shutdown_finalize_recording() {
    let mut h = H::new("ctl-quit", Mode::Auto);
    h.join();
    h.ticks(3);
    h.ev(UiEvent::Shutdown);
    assert_eq!(h.count("stop"), 1);
    h.ticks(1);
    h.ev(UiEvent::Menu(MenuAction::Quit));
    assert_eq!(h.count("quit"), 1);
}

#[test]
fn diagnostics_are_saved_and_hide_other_apps() {
    let mut h = H::new("ctl-diag", Mode::Ask);
    h.join();
    h.p.windows.push(win(
        99,
        proc_(50, "notes.exe", None),
        "秘密のメモ",
        "Notepad",
        800,
        600,
    ));
    h.ticks(2);
    h.ev(UiEvent::Menu(MenuAction::SaveDiagnostics));
    let dir = h.dirs.state.join("diagnostics");
    let file = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
    let body = fs::read_to_string(file).unwrap();
    assert!(body.contains("Zoom.exe[7]"));
    assert!(body.contains("ZPContentViewWndClass"));
    assert!(body.contains("100点"));
    assert!(!body.contains("秘密のメモ"));
    assert!(body.contains("会議アプリ以外のウィンドウ: 1 個"));
    assert!(body.contains("待機中"));
    assert!(h.count("open") == 1);
}

#[test]
fn recovery_on_launch_notifies() {
    let d = temp_dir("ctl-recover");
    let dirs = Dirs {
        state: d.join("state"),
        default_output: d.join("out"),
    };
    fs::create_dir_all(&dirs.default_output).unwrap();
    // finish していない録画ファイルを作る。
    let path = dirs.default_output.join("2026-11-19_1000_Zoom.mp4");
    let mut w = crate::mp4::FragWriter::new(
        fs::File::create(&path).unwrap(),
        vec![crate::mp4::tests::audio_spec(true)],
    )
    .unwrap();
    for _ in 0..100 {
        w.write_fixed(0, 1024, &[1; 50]);
    }
    w.flush(2000).unwrap();
    w.write_fixed(0, 1024, &[1; 50]);
    drop(w);
    storage::write_marker(&dirs.marker_dir(), &path).unwrap();
    let mut p = Mock {
        perms_ok: true,
        ..Default::default()
    };
    let cfg = Config {
        autostart_initialized: true,
        ..Default::default()
    };
    let mut c = Controller::new(cfg, dirs.clone(), None);
    c.on_event(&mut p, UiEvent::Launched);
    assert!(p.notices.iter().any(|n| n.body == text::NOTICE_RECOVERED));
    assert!(fs::read_dir(dirs.marker_dir()).unwrap().next().is_none());
    assert!(!crate::mp4::repair_file(&path).unwrap().duration_patched);
}
