//! アプリ全体の制御：検出結果とユーザー操作から、確認ポップアップ・録画開始/停止・
//! ウィンドウ追従・ディスク監視・自己計測を行う。OS には `Platform` 経由でのみ触れる。

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::config::{Config, Mode};
use crate::detect::{DetectEvent, Detector};
use crate::metrics::Metrics;
use crate::platform::{
    App, MenuAction, Notice, Permissions, Pid, Platform, PromptAnswer, PromptModel, RecordRequest,
    RecorderEvent, TrayModel, UiEvent,
};
use crate::rules::{Observation, Rules};
use crate::storage::{self, Dirs, RecoveryOutcome};
use crate::text;

const NS: u64 = 1_000_000_000;
/// より良いウィンドウが現れてから張り替えるまでの安定待ち。
const RETARGET_HOLD_NS: u64 = 2 * NS;
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

#[derive(Debug, Clone)]
struct Rec {
    app: usize,
    /// 手動開始で、会議検出と結び付いていない（ウィンドウが消えたら止める）。
    manual: bool,
    started_ns: u64,
    path: PathBuf,
    target: Option<u64>,
    candidate: Option<(u64, u64)>,
    gone_since: Option<u64>,
    audio_only_notified: bool,
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

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn is_recording(&self) -> bool {
        self.rec.is_some()
    }

    fn output_dir(&self) -> PathBuf {
        self.cfg.output_dir(&self.dirs.default_output)
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
        if let Some(m) = self.meeting {
            if m.decision == Decision::ResumeAfterWake && self.rec.is_none() {
                crate::info!("スリープ復帰後も会議が続いているため録画を再開します");
                let ok = self.start(p, m.app, m.root_pid, false, &obs);
                self.set_decision(if ok {
                    Decision::Accepted
                } else {
                    Decision::Declined
                });
            }
        }
        self.follow(p, &obs, now);
        self.housekeeping(p, now);
        self.update_tray(p);
    }

    fn set_decision(&mut self, d: Decision) {
        if let Some(m) = self.meeting.as_mut() {
            m.decision = d;
        }
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
        let keep = self.rules.apps[rec.app].keep_score;
        let best = obs.best_window(rec.app, keep);
        let current = rec.target.and_then(|id| {
            obs.windows
                .iter()
                .find(|w| w.app == rec.app && w.info.id == id)
        });
        match (current, best) {
            // 対象が消えた：すぐ張り替える。
            (None, Some(b)) if rec.target.is_some() => {
                crate::info!(
                    "録画対象ウィンドウが消えたため張り替え: {:?} ({}点)",
                    b.info.title,
                    b.score
                );
                p.retarget(&b.info);
                rec.target = Some(b.info.id);
                rec.candidate = None;
            }
            // より点数の高いウィンドウが安定して存在する：張り替える。
            (Some(c), Some(b)) if b.info.id != c.info.id && (b.score > c.score || !c.capture) => {
                match rec.candidate {
                    Some((id, since)) if id == b.info.id => {
                        if now.saturating_sub(since) >= RETARGET_HOLD_NS {
                            crate::info!("録画対象を張り替え: {:?} ({}点)", b.info.title, b.score);
                            p.retarget(&b.info);
                            rec.target = Some(b.info.id);
                            rec.candidate = None;
                        }
                    }
                    _ => rec.candidate = Some((b.info.id, now)),
                }
            }
            _ => rec.candidate = None,
        }

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
            let min = self.cfg.storage.min_free_mb << 20;
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
        let min = self.cfg.storage.min_free_mb << 20;
        if p.free_space(&dir).is_some_and(|free| free < min) {
            self.notify(p, text::NOTICE_DISK_LOW_START, None);
            return false;
        }
        let rule = &self.rules.apps[app];
        let window = obs
            .best_window(app, rule.keep_score)
            .map(|w| w.info.clone());
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
                    candidate: None,
                    gone_since: None,
                    audio_only_notified: false,
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
        if let Some(s) = summary {
            if notify && !s.empty {
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
                let ok = self.start(p, m.app, m.root_pid, false, &obs);
                self.set_decision(if ok {
                    Decision::Accepted
                } else {
                    Decision::Declined
                });
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
                let ok = self.start(p, m.app, m.root_pid, false, &obs);
                self.set_decision(if ok {
                    Decision::Accepted
                } else {
                    Decision::Declined
                });
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
            MenuAction::SetMode(mode) => self.set_mode(p, mode),
            MenuAction::ToggleAutostart => {
                let want = !p.autostart_enabled();
                if let Err(e) = p.set_autostart(want) {
                    crate::warn!("ログイン時の起動を変更できません: {e}");
                    self.notify(p, format!("{}: {e}", text::NOTICE_AUTOSTART_FAILED), None);
                }
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
                if let Some(rec) = self.rec.as_mut() {
                    if !rec.audio_only_notified {
                        rec.audio_only_notified = true;
                        self.notify(p, text::NOTICE_AUDIO_ONLY, None);
                    }
                }
            }
        }
    }

    // ----- 診断 -----

    pub fn diagnostics_text(&mut self, p: &mut dyn Platform) -> String {
        let mut s = String::new();
        let t = p.local_time();
        let _ = writeln!(s, "MeetRec {} 診断ログ", crate::VERSION);
        let _ = writeln!(
            s,
            "日時: {:04}-{:02}-{:02} {:02}:{:02}:{:02} / OS: {} {}",
            t.year,
            t.month,
            t.day,
            t.hour,
            t.minute,
            t.second,
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        let perms = p.permissions();
        let _ = writeln!(
            s,
            "権限: 画面収録={} マイク={}",
            perms.screen, perms.microphone
        );
        let _ = writeln!(
            s,
            "検出状態: {} / meeting={:?}",
            self.detector.describe(),
            self.meeting
        );
        let _ = writeln!(s, "録画状態: {:?}", self.rec);

        let _ = writeln!(s, "\n== マイク使用中のプロセス（祖先チェーン） ==");
        let mic = p.mic_users();
        if mic.is_empty() {
            let _ = writeln!(s, "(なし)");
        }
        for m in &mic {
            let chain: Vec<String> = m
                .chain
                .iter()
                .map(|c| {
                    format!(
                        "{}[{}]{}",
                        c.name,
                        c.pid,
                        c.bundle_id
                            .as_deref()
                            .map(|b| format!("<{b}>"))
                            .unwrap_or_default()
                    )
                })
                .collect();
            let matched = self
                .rules
                .app_for_chain(&m.chain)
                .map(|(a, root)| format!("{} root={root}", self.rules.apps[a].id));
            let _ = writeln!(
                s,
                "{}  => {}",
                chain.join(" <- "),
                matched.unwrap_or_else(|| "一致なし".into())
            );
        }

        let _ = writeln!(s, "\n== 会議アプリのウィンドウ ==");
        let windows = p.windows();
        let mut others = 0;
        for w in &windows {
            match self.rules.app_for_proc(&w.owner) {
                Some(a) => {
                    let score = self.rules.score_window(a, w);
                    let _ = writeln!(
                        s,
                        "[{}] id={} pid={} proc={:?} bundle={:?} class={:?} title={:?} {}x{} min={} => {}",
                        self.rules.apps[a].id,
                        w.id,
                        w.owner.pid,
                        w.owner.name,
                        w.owner.bundle_id,
                        w.class,
                        w.title,
                        w.width,
                        w.height,
                        w.minimized,
                        score.map_or("一致なし".into(), |(sc, cap)| format!("{sc}点 capture={cap}")),
                    );
                }
                None => others += 1,
            }
        }
        // 他アプリのウィンドウタイトルはプライバシーのため出力しない。
        let _ = writeln!(
            s,
            "(会議アプリ以外のウィンドウ: {others} 個・内容は記録しません)"
        );

        let _ = writeln!(s, "\n== OS 層の追加情報 ==\n{}", p.diagnostics());
        let _ = writeln!(s, "\n== 自己計測 ==\n{}", self.metrics.report());
        let _ = writeln!(
            s,
            "== 設定 ==\n{}",
            toml::to_string_pretty(&self.cfg).unwrap_or_default()
        );
        let _ = writeln!(s, "== 有効な会議アプリ定義 ==");
        for a in &self.rules.apps {
            let _ = writeln!(s, "{}", toml::to_string(a).unwrap_or_default());
        }
        let _ = writeln!(s, "== 直近のログ ==");
        for line in crate::log::recent() {
            let _ = writeln!(s, "{line}");
        }
        s
    }

    fn save_diagnostics(&mut self, p: &mut dyn Platform) {
        // 計測値を最新にしてから出力する。
        if let Some(u) = p.resource_usage() {
            self.metrics.sample(p.now_ns(), u, self.rec.is_some());
        }
        let body = self.diagnostics_text(p);
        let dir = self.dirs.state.join("diagnostics");
        let t = p.local_time();
        let name = format!(
            "MeetRec-diagnostics-{:04}{:02}{:02}-{:02}{:02}{:02}.txt",
            t.year, t.month, t.day, t.hour, t.minute, t.second
        );
        let result = fs::create_dir_all(&dir).and_then(|_| fs::write(dir.join(&name), body));
        match result {
            Ok(()) => {
                self.notify(
                    p,
                    format!("{}\n{name}", text::NOTICE_DIAGNOSTICS),
                    Some(dir.clone()),
                );
                p.open_path(&dir);
            }
            Err(e) => self.notify(p, format!("診断ログを保存できません: {e}"), None),
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

#[cfg(test)]
mod tests {
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
        assert_eq!(h.c.config().mode, Mode::Ask);
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
        assert_eq!(h.c.config().mode, Mode::Auto);
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
}
