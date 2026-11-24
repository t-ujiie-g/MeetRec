//! アプリ全体の制御：検出結果とユーザー操作から、確認ポップアップ・録画開始/停止・
//! ウィンドウ追従・ディスク監視・自己計測を行う。OS には `Platform` 経由でのみ触れる。

use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::config::{Config, Mode};
use crate::detect::{DetectEvent, Detector};
use crate::metrics::Metrics;
use crate::platform::{
    App, MenuAction, NS_PER_SEC as NS, Notice, Permissions, Pid, Platform, PromptAnswer,
    PromptModel, RecordRequest, RecorderEvent, TrayModel, UiEvent,
};
use crate::rules::{Observation, Rules};
use crate::storage::{self, Dirs, RecoveryOutcome};
use crate::text;

mod diagnostics;
mod recording;
#[cfg(test)]
mod tests;

use recording::Rec;

const DISK_CHECK_NS: u64 = 10 * NS;
const METRICS_REC_NS: u64 = 30 * NS;
const METRICS_IDLE_NS: u64 = 60 * NS;
const CLEANUP_NS: u64 = 6 * 3600 * NS;
/// マイク未使用でも会議ウィンドウを探しに行く間隔（待機中の負荷を抑えるため長め）。
const MICLESS_SCAN_NS: u64 = 10 * NS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// 確認ポップアップ表示中。
    Pending,
    Accepted,
    Declined,
    /// モードがオフのため何もしなかった。
    Ignored,
    /// スリープで中断した。復帰後も会議が続いていれば確認なしで再開する。
    ResumeAfterWake,
}

#[derive(Debug, Clone, Copy)]
struct Meeting {
    app: usize,
    root_pid: Pid,
    decision: Decision,
}

pub struct Controller {
    cfg: Config,
    dirs: Dirs,
    rules: Rules,
    detector: Detector,
    meeting: Option<Meeting>,
    prompt_visible: bool,
    rec: Option<Rec>,
    metrics: Metrics,
    last_metrics_ns: u64,
    last_disk_ns: u64,
    last_cleanup_ns: u64,
    last_window_scan_ns: u64,
    last_tray: Option<TrayModel>,
    last_obs: Observation,
    config_error: Option<String>,
}

impl Controller {
    pub fn new(cfg: Config, dirs: Dirs, config_error: Option<String>) -> Controller {
        let rules = Rules::new(&cfg.apps);
        let detector = Detector::new(&cfg.detect);
        Controller {
            cfg,
            dirs,
            rules,
            detector,
            meeting: None,
            prompt_visible: false,
            rec: None,
            metrics: Metrics::default(),
            last_metrics_ns: 0,
            last_disk_ns: 0,
            last_cleanup_ns: 0,
            last_window_scan_ns: 0,
            last_tray: None,
            last_obs: Observation::default(),
            config_error,
        }
    }

    pub fn is_recording(&self) -> bool {
        self.rec.is_some()
    }

    fn output_dir(&self) -> PathBuf {
        self.cfg.output_dir(&self.dirs.default_output)
    }

    /// 録画を続けるのに必要な空き容量（バイト）。
    fn min_free_bytes(&self) -> u64 {
        self.cfg.storage.min_free_mb.saturating_mul(1 << 20)
    }

    fn save_config(&self) {
        if let Err(e) = self.cfg.save(&self.dirs.config_file()) {
            crate::warn!("設定を保存できません: {e}");
        }
    }

    fn notify(&self, p: &mut dyn Platform, body: impl Into<String>, open: Option<PathBuf>) {
        let body = body.into();
        crate::info!("通知: {body}");
        p.notify(&Notice {
            title: text::APP_NAME.into(),
            body,
            open,
        });
    }

    // ----- 起動 -----

    fn launched(&mut self, p: &mut dyn Platform) {
        crate::info!("MeetRec {} 起動 (mode={:?})", crate::VERSION, self.cfg.mode);
        if let Some(e) = self.config_error.take() {
            crate::warn!("{e}");
            self.notify(
                p,
                format!("{e}（既定値で起動しました）"),
                Some(self.dirs.state.clone()),
            );
        }
        // 前回の異常終了で残った録画を修復する。
        let outcomes = storage::recover(&self.dirs.marker_dir());
        let mut repaired = 0;
        for o in &outcomes {
            crate::info!("修復: {o:?}");
            if matches!(
                o,
                RecoveryOutcome::Repaired { .. } | RecoveryOutcome::Intact { .. }
            ) {
                repaired += 1;
            }
        }
        if repaired > 0 {
            self.notify(p, text::NOTICE_RECOVERED, Some(self.output_dir()));
        }
        self.cleanup();
        if !self.cfg.autostart_initialized {
            match p.set_autostart(true) {
                Ok(()) => crate::info!("ログイン時の起動を有効にしました"),
                Err(e) => crate::warn!("ログイン時の起動を有効にできません: {e}"),
            }
            self.cfg.autostart_initialized = true;
            self.save_config();
        }
        let perms = p.permissions();
        crate::info!(
            "権限: 画面収録={} マイク={}",
            perms.screen,
            perms.microphone
        );
        if !perms.screen || !perms.microphone {
            p.request_permissions(Permissions {
                screen: !perms.screen,
                microphone: !perms.microphone,
            });
        }
        self.update_tray(p);
    }

    fn cleanup(&mut self) {
        let keep = self.rec.as_ref().map(|r| r.path.clone());
        let removed = storage::cleanup_old(
            &self.output_dir(),
            self.cfg.storage.retention_days,
            SystemTime::now(),
            keep.as_deref(),
        );
        for r in removed {
            crate::info!("古い録画を削除: {}", r.display());
        }
    }

    // ----- 定期処理 -----

    fn observe(&mut self, p: &mut dyn Platform, force_windows: bool) -> Observation {
        let mic = p.mic_users();
        // ウィンドウ一覧は必要なときだけ取る（待機中の負荷を最小にする）。
        let mic_only = self.rules.observe(&mic, &[]);
        let now = p.now_ns();
        let periodic = self.rules.has_micless_start()
            && now.saturating_sub(self.last_window_scan_ns) >= MICLESS_SCAN_NS;
        let need = force_windows
            || periodic
            || !mic_only.mic_apps.is_empty()
            || self.detector.wants_windows()
            || self.rec.is_some();
        let obs = if need {
            self.last_window_scan_ns = now;
            self.rules.observe(&mic, &p.windows())
        } else {
            mic_only
        };
        self.last_obs = obs.clone();
        obs
    }

    fn tick(&mut self, p: &mut dyn Platform) {
        let now = p.now_ns();
        let obs = self.observe(p, false);
        match self.detector.update(&self.rules, &obs, now) {
            Some(DetectEvent::Started { app, root_pid }) => {
                self.meeting_started(p, app, root_pid, &obs)
            }
            Some(DetectEvent::Ended { app }) => self.meeting_ended(p, app),
            None => {}
        }
        if let Some(m) = self.meeting
            && m.decision == Decision::ResumeAfterWake
            && self.rec.is_none()
        {
            crate::info!("スリープ復帰後も会議が続いているため録画を再開します");
            self.start_for_meeting(p, m, &obs);
        }
        self.follow(p, &obs, now);
        self.follow_mute(p);
        self.housekeeping(p, now);
        self.update_tray(p);
    }

    fn set_decision(&mut self, d: Decision) {
        if let Some(m) = self.meeting.as_mut() {
            m.decision = d;
        }
    }

    /// 検出済みの会議の録画を開始し、成否を判断として記録する（失敗した会議では再試行しない）。
    fn start_for_meeting(&mut self, p: &mut dyn Platform, m: Meeting, obs: &Observation) {
        let ok = self.start(p, m.app, m.root_pid, false, obs);
        self.set_decision(if ok {
            Decision::Accepted
        } else {
            Decision::Declined
        });
    }

    fn meeting_started(
        &mut self,
        p: &mut dyn Platform,
        app: usize,
        root_pid: Pid,
        obs: &Observation,
    ) {
        let name = self.rules.apps[app].name.clone();
        crate::info!("会議を検出: {name} (root pid {root_pid})");
        let mut decision = Decision::Ignored;
        if let Some(rec) = self.rec.as_mut() {
            // 手動録画中に検出した：同じアプリなら会議終了で自動停止するようにする。
            if rec.app == app {
                rec.manual = false;
                decision = Decision::Accepted;
            }
        } else {
            match self.cfg.mode {
                Mode::Off => {}
                Mode::Auto => {
                    decision = if self.start(p, app, root_pid, false, obs) {
                        self.notify(p, format!("{name}: {}", text::NOTICE_STARTED), None);
                        Decision::Accepted
                    } else {
                        Decision::Declined
                    };
                }
                Mode::Ask => {
                    self.show_prompt(p, app);
                    decision = Decision::Pending;
                }
            }
        }
        self.meeting = Some(Meeting {
            app,
            root_pid,
            decision,
        });
    }

    fn show_prompt(&mut self, p: &mut dyn Platform, app: usize) {
        p.show_prompt(&PromptModel {
            message: text::prompt_message(&self.rules.apps[app].name),
            record_label: text::PROMPT_RECORD.into(),
            skip_label: text::PROMPT_SKIP.into(),
            always_label: text::PROMPT_ALWAYS.into(),
        });
        self.prompt_visible = true;
    }

    fn hide_prompt(&mut self, p: &mut dyn Platform) {
        if self.prompt_visible {
            p.hide_prompt();
            self.prompt_visible = false;
        }
    }

    fn meeting_ended(&mut self, p: &mut dyn Platform, app: usize) {
        crate::info!("会議の終了を検出: {}", self.rules.apps[app].name);
        self.hide_prompt(p);
        self.meeting = None;
        if self.rec.as_ref().is_some_and(|r| r.app == app && !r.manual) {
            self.stop(p, true);
        }
    }

    /// 録画中：ウィンドウ追従・手動録画の自動停止・ディスク残量。
    fn follow(&mut self, p: &mut dyn Platform, obs: &Observation, now: u64) {
        let Some(rec) = self.rec.as_mut() else { return };
        rec.log_windows(obs);
        let keep = self.rules.apps[rec.app].keep_score;
        let best = obs.best_window(rec.app, keep);
        // 画面を録っている間はウィンドウを追わない。
        if !rec.follow_share(p, obs, best, self.cfg.video.record_shared_screen) {
            rec.follow_windows(p, obs, best, now);
        }
        let joined =
            best.is_some_and(|w| self.rules.meeting_title(rec.app, &w.info.title).is_some());
        rec.settle_output(p, joined, now);

        let mut stop_reason = None;
        if rec.manual {
            let present = obs.windows.iter().any(|w| w.app == rec.app);
            rec.gone_since = if present {
                None
            } else {
                Some(rec.gone_since.unwrap_or(now))
            };
            let grace = self.cfg.detect.end_grace_seconds as u64 * NS;
            if rec
                .gone_since
                .is_some_and(|t| now.saturating_sub(t) >= grace)
            {
                crate::info!("会議アプリのウィンドウがなくなったため手動録画を停止");
                stop_reason = Some(None);
            }
        }
        if now.saturating_sub(self.last_disk_ns) >= DISK_CHECK_NS {
            self.last_disk_ns = now;
            let min = self.min_free_bytes();
            if p.free_space(&self.output_dir())
                .is_some_and(|free| free < min)
            {
                crate::warn!("ディスク残量不足のため停止");
                stop_reason = Some(Some(text::NOTICE_DISK_LOW));
            }
        }
        if let Some(extra) = stop_reason {
            if let Some(msg) = extra {
                self.notify(p, msg, None);
                // 同じ会議で自動再開してディスクを食い潰さない。
                self.set_decision(Decision::Declined);
            }
            self.stop(p, true);
        }
    }

    fn housekeeping(&mut self, p: &mut dyn Platform, now: u64) {
        let recording = self.rec.is_some();
        let every = if recording {
            METRICS_REC_NS
        } else {
            METRICS_IDLE_NS
        };
        if self.last_metrics_ns == 0 || now.saturating_sub(self.last_metrics_ns) >= every {
            self.last_metrics_ns = now.max(1);
            if let Some(u) = p.resource_usage() {
                let cpu = self.metrics.sample(now, u, recording);
                if recording {
                    crate::info!(
                        "計測[録画中] メモリ {:.1}MB CPU {}",
                        u.rss_bytes as f64 / (1 << 20) as f64,
                        cpu.map_or("-".into(), |c| format!("{c:.2}%"))
                    );
                }
            }
        }
        if now.saturating_sub(self.last_cleanup_ns) >= CLEANUP_NS {
            self.last_cleanup_ns = now;
            self.cleanup();
        }
    }

    fn update_tray(&mut self, p: &mut dyn Platform) {
        let status = if let Some(rec) = &self.rec {
            text::status_recording(p.now_ns().saturating_sub(rec.started_ns) / NS)
        } else if self.prompt_visible {
            text::STATUS_PROMPT.into()
        } else if self.cfg.mode == Mode::Off {
            text::STATUS_OFF.into()
        } else {
            text::STATUS_IDLE.into()
        };
        let model = TrayModel {
            recording: self.rec.is_some(),
            status,
            mode: self.cfg.mode,
            autostart: p.autostart_enabled(),
            mic: self.cfg.audio.capture_mic,
        };
        if self.last_tray.as_ref() != Some(&model) {
            p.set_tray(&model);
            self.last_tray = Some(model);
        }
    }

    // ----- 録画の開始・停止 -----

    fn start(
        &mut self,
        p: &mut dyn Platform,
        app: usize,
        root_pid: Pid,
        manual: bool,
        obs: &Observation,
    ) -> bool {
        if self.rec.is_some() {
            return true;
        }
        let dir = self.output_dir();
        if let Err(e) = fs::create_dir_all(&dir) {
            crate::warn!("保存先を作成できません: {e}");
            self.notify(p, format!("{}: {e}", text::NOTICE_START_FAILED), None);
            return false;
        }
        if p.free_space(&dir)
            .is_some_and(|free| free < self.min_free_bytes())
        {
            self.notify(p, text::NOTICE_DISK_LOW_START, None);
            return false;
        }
        let rule = &self.rules.apps[app];
        let best = obs.best_window(app, rule.keep_score);
        let target_definite = best.is_some_and(|w| w.definite);
        let window = best.map(|w| w.info.clone());
        let title = window
            .as_ref()
            .and_then(|w| self.rules.meeting_title(app, &w.title));
        let stem = storage::file_stem(p.local_time(), &rule.name, title.as_deref());
        let path = storage::unique_path(&dir, &stem, "mp4");
        let req = RecordRequest {
            path: path.clone(),
            marker_dir: self.dirs.marker_dir(),
            window: window.clone(),
            root_pid,
            system_audio: rule.system_audio,
            unmute_output: rule.system_audio && self.cfg.audio.unmute_output,
            settings: self.cfg.record_settings(),
        };
        match p.start_recording(&req) {
            Ok(()) => {
                crate::info!(
                    "録画開始: {} (window={:?}, manual={manual})",
                    path.display(),
                    window
                        .as_ref()
                        .map(|w| (&w.title, &w.class, w.width, w.height))
                );
                self.metrics.reset_recording();
                self.last_metrics_ns = 0;
                self.last_disk_ns = p.now_ns();
                self.rec = Some(Rec {
                    app,
                    manual,
                    started_ns: p.now_ns(),
                    path,
                    target: window.map(|w| w.id),
                    target_definite,
                    root_pid,
                    app_muted: false,
                    mic_on: self.cfg.audio.capture_mic,
                    candidate: None,
                    gone_since: None,
                    audio_only_notified: false,
                    screen: false,
                    screen_failed: false,
                    screen_mark: None,
                    screen_reselect_failures: 0,
                    frame_missing: 0,
                    joined_since: None,
                    output_settled: !req.unmute_output,
                    last_windows: String::new(),
                });
                true
            }
            Err(e) => {
                crate::warn!("録画を開始できません: {e}");
                self.notify(p, format!("{}: {e}", text::NOTICE_START_FAILED), None);
                false
            }
        }
    }

    fn stop(&mut self, p: &mut dyn Platform, notify: bool) {
        if self.rec.take().is_none() {
            return;
        }
        let summary = p.stop_recording();
        crate::info!("録画停止: {summary:?}");
        crate::info!("録画中の計測:\n{}", self.metrics.report());
        if let Some(s) = summary
            && notify
            && !s.empty
        {
            let name = s
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.notify(
                p,
                format!(
                    "{}\n{name}（{}）",
                    text::NOTICE_SAVED,
                    text::duration_text(s.duration_ms)
                ),
                Some(self.output_dir()),
            );
        }
    }

    // ----- ユーザー操作 -----

    fn record_now(&mut self, p: &mut dyn Platform) {
        if self.rec.is_some() {
            return;
        }
        let obs = self.observe(p, true);
        // 会議中ならそのアプリ、そうでなければ最も点数の高いウィンドウを持つアプリ。
        let app = self.meeting.map(|m| m.app).or_else(|| {
            (0..self.rules.apps.len())
                .filter_map(|a| obs.best_window(a, i32::MIN).map(|w| (w.score, a)))
                .max()
                .map(|(_, a)| a)
        });
        let Some(app) = app else {
            self.notify(p, text::NOTICE_NO_WINDOW, None);
            return;
        };
        let root = self
            .meeting
            .filter(|m| m.app == app)
            .map(|m| m.root_pid)
            .or_else(|| obs.mic_root(app))
            .or_else(|| obs.best_window(app, i32::MIN).map(|w| w.info.owner.pid))
            .unwrap_or(0);
        let in_meeting = self.meeting.is_some_and(|m| m.app == app);
        if self.start(p, app, root, !in_meeting, &obs) && in_meeting {
            self.hide_prompt(p);
            self.set_decision(Decision::Accepted);
        }
    }

    fn set_mode(&mut self, p: &mut dyn Platform, mode: Mode) {
        if self.cfg.mode == mode {
            return;
        }
        crate::info!("モード変更: {:?} -> {mode:?}", self.cfg.mode);
        self.cfg.mode = mode;
        self.save_config();
        let Some(m) = self.meeting else { return };
        if self.rec.is_some() {
            return;
        }
        match (mode, m.decision) {
            (Mode::Auto, Decision::Pending | Decision::Ignored) => {
                self.hide_prompt(p);
                let obs = self.observe(p, true);
                self.start_for_meeting(p, m, &obs);
            }
            (Mode::Off, Decision::Pending) => {
                self.hide_prompt(p);
                self.set_decision(Decision::Ignored);
            }
            (Mode::Ask, Decision::Ignored) => {
                self.show_prompt(p, m.app);
                self.set_decision(Decision::Pending);
            }
            _ => {}
        }
    }

    fn prompt_answer(&mut self, p: &mut dyn Platform, answer: PromptAnswer) {
        self.hide_prompt(p);
        let Some(m) = self.meeting else { return };
        if m.decision != Decision::Pending {
            return;
        }
        match answer {
            PromptAnswer::Skip => {
                crate::info!("確認: 今回は録画しない");
                self.set_decision(Decision::Declined);
            }
            PromptAnswer::Record { always } => {
                crate::info!("確認: 録画する (always={always})");
                if always {
                    self.cfg.mode = Mode::Auto;
                    self.save_config();
                }
                let obs = self.observe(p, true);
                self.start_for_meeting(p, m, &obs);
            }
        }
    }

    fn menu(&mut self, p: &mut dyn Platform, action: MenuAction) {
        match action {
            MenuAction::RecordNow => self.record_now(p),
            MenuAction::StopNow => {
                self.stop(p, true);
                // 同じ会議の間は再度聞かない・自動再開しない。
                self.set_decision(Decision::Declined);
            }
            MenuAction::OpenFolder => {
                let dir = self.output_dir();
                let _ = fs::create_dir_all(&dir);
                p.open_path(&dir);
            }
            MenuAction::ChooseFolder => self.choose_folder(p),
            MenuAction::SetMode(mode) => self.set_mode(p, mode),
            MenuAction::ToggleAutostart => {
                let want = !p.autostart_enabled();
                if let Err(e) = p.set_autostart(want) {
                    crate::warn!("ログイン時の起動を変更できません: {e}");
                    self.notify(p, format!("{}: {e}", text::NOTICE_AUTOSTART_FAILED), None);
                }
            }
            MenuAction::ToggleMic => {
                self.cfg.audio.capture_mic = !self.cfg.audio.capture_mic;
                crate::info!("自分の声を録る: {}", self.cfg.audio.capture_mic);
                self.save_config();
                // 録画中ならすぐ反映する。マイクを開かずに始めた録画では、オンにしても次の録画から。
                self.apply_mic(p);
            }
            MenuAction::SaveDiagnostics => self.save_diagnostics(p),
            MenuAction::Quit => {
                self.hide_prompt(p);
                self.stop(p, false);
                crate::info!("終了");
                p.quit();
            }
        }
    }

    /// 録画中：会議アプリのミュート状態を読み、マイクを録るかどうかに反映する（D35）。
    /// 状態が分からないときは「ミュートしていない」として扱う（自分の発言を録り逃さないため）。
    fn follow_mute(&mut self, p: &mut dyn Platform) {
        let Some(rec) = self.rec.as_mut() else { return };
        let audio = &self.cfg.audio;
        if !audio.capture_mic || !audio.follow_app_mute {
            return;
        }
        let rule = &self.rules.apps[rec.app].mute;
        let names = rule.names();
        if names.is_empty() {
            return;
        }
        let muted = p
            .find_control(rec.root_pid, &names)
            .is_some_and(|name| rule.is_muted(&name));
        if muted != rec.app_muted {
            rec.app_muted = muted;
            crate::info!(
                "会議アプリのミュート: {}",
                if muted {
                    "ミュート中（自分の声を録らない）"
                } else {
                    "解除（自分の声を録る）"
                }
            );
            self.apply_mic(p);
        }
    }

    /// 設定（自分の声を録る）とミュート状態から、マイクを録るかを決めて OS 層に伝える。
    fn apply_mic(&mut self, p: &mut dyn Platform) {
        let Some(rec) = self.rec.as_mut() else { return };
        let want = self.cfg.audio.capture_mic && !rec.app_muted;
        if rec.mic_on != want {
            rec.mic_on = want;
            p.set_mic_enabled(want);
        }
    }

    /// 保存先を選び直す。録画中のファイルは動かさず、次の録画から新しい保存先を使う。
    fn choose_folder(&mut self, p: &mut dyn Platform) {
        let Some(dir) = p.choose_folder(&self.output_dir()) else {
            return;
        };
        if let Err(e) = fs::create_dir_all(&dir) {
            crate::warn!("保存先を使えません: {} ({e})", dir.display());
            self.notify(p, text::NOTICE_FOLDER_UNUSABLE, None);
            return;
        }
        crate::info!("保存先を変更: {}", dir.display());
        // 既定の保存先に戻したときは空にして、既定に追従させる。
        self.cfg.output_dir = if dir == self.dirs.default_output {
            String::new()
        } else {
            dir.to_string_lossy().into_owned()
        };
        self.save_config();
        self.notify(p, text::NOTICE_FOLDER_CHANGED, Some(dir));
    }

    fn recorder_event(&mut self, p: &mut dyn Platform, ev: RecorderEvent) {
        match ev {
            RecorderEvent::Failed(msg) => {
                crate::warn!("録画が中断されました: {msg}");
                if self.rec.is_some() {
                    self.stop(p, false);
                    self.notify(
                        p,
                        format!("{}: {msg}", text::NOTICE_FAILED),
                        Some(self.output_dir()),
                    );
                    self.set_decision(Decision::Declined);
                }
            }
            RecorderEvent::VideoUnavailable(msg) => {
                crate::warn!("映像を取得できません: {msg}");
                if let Some(rec) = self.rec.as_mut()
                    && !rec.audio_only_notified
                {
                    rec.audio_only_notified = true;
                    self.notify(p, text::NOTICE_AUDIO_ONLY, None);
                }
            }
            // PC の設定を変えたことは、そのつど知らせる（OS 層は実際に切り替えたときだけ送る）。
            RecorderEvent::OutputUnmuted => {
                if self.rec.is_some() {
                    self.notify(p, text::NOTICE_OUTPUT_UNMUTED, None);
                }
            }
        }
    }
}

impl App for Controller {
    fn on_event(&mut self, p: &mut dyn Platform, event: UiEvent) {
        match event {
            UiEvent::Launched => self.launched(p),
            UiEvent::Tick => self.tick(p),
            UiEvent::Menu(action) => {
                self.menu(p, action);
                if action != MenuAction::Quit {
                    self.update_tray(p);
                }
            }
            UiEvent::Prompt(answer) => {
                self.prompt_answer(p, answer);
                self.update_tray(p);
            }
            UiEvent::Recorder(ev) => {
                self.recorder_event(p, ev);
                self.update_tray(p);
            }
            UiEvent::WillSleep => {
                crate::info!("スリープ");
                if let Some(rec) = &self.rec {
                    let resume = !rec.manual;
                    self.stop(p, false);
                    if resume {
                        self.set_decision(Decision::ResumeAfterWake);
                    }
                }
                self.update_tray(p);
            }
            UiEvent::DidWake => crate::info!("スリープから復帰"),
            UiEvent::Shutdown => {
                crate::info!("OS からの終了要求");
                self.hide_prompt(p);
                self.stop(p, false);
            }
        }
    }

    fn tick_seconds(&self) -> u32 {
        self.cfg.detect.poll_seconds
    }
}
