//! 会議検出の状態機械（OS 非依存）。
//!
//! 開始：会議アプリがマイク使用中 かつ 録画対象ウィンドウ（start_score 以上）が存在、
//!       が `start_hold` 継続。micless_start_score 以上のウィンドウがあればマイク不問。
//! 終了：(a) keep_score 以上のウィンドウが `end_grace` の間ひとつもない、または
//!       (b) マイク未使用 かつ strong_score 以上のウィンドウなし が `mic_release_grace` 継続。
//!       一時的な切断・再接続では (a)(b) のタイマーがリセットされるので分断しない。

use crate::config::DetectConfig;
use crate::platform::Pid;
use crate::rules::{Observation, Rules};

const NS: u64 = 1_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectEvent {
    Started { app: usize, root_pid: Pid },
    Ended { app: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Candidate {
        app: usize,
        since: u64,
    },
    Active {
        app: usize,
        root_pid: Pid,
        gone_since: Option<u64>,
        weak_since: Option<u64>,
    },
}

#[derive(Debug, Clone)]
pub struct Detector {
    phase: Phase,
    start_hold_ns: u64,
    end_grace_ns: u64,
    mic_release_grace_ns: u64,
}

impl Detector {
    pub fn new(cfg: &DetectConfig) -> Detector {
        Detector {
            phase: Phase::Idle,
            start_hold_ns: cfg.start_hold_seconds as u64 * NS,
            end_grace_ns: cfg.end_grace_seconds as u64 * NS,
            mic_release_grace_ns: cfg.mic_release_grace_seconds as u64 * NS,
        }
    }

    /// 会議中ならそのアプリ番号。
    pub fn active_app(&self) -> Option<usize> {
        match self.phase {
            Phase::Active { app, .. } => Some(app),
            _ => None,
        }
    }

    pub fn active_root(&self) -> Option<Pid> {
        match self.phase {
            Phase::Active { root_pid, .. } => Some(root_pid),
            _ => None,
        }
    }

    /// ウィンドウ一覧の取得が必要か（アイドルでマイク使用もなければ不要）。
    pub fn wants_windows(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    pub fn describe(&self) -> String {
        format!("{:?}", self.phase)
    }

    fn start_condition(rules: &Rules, obs: &Observation, app: usize) -> Option<Pid> {
        let rule = &rules.apps[app];
        if let Some(root) = obs.mic_root(app) {
            if obs.best_window(app, rule.start_score).is_some() {
                return Some(root);
            }
        }
        // マイク未使用でも、会議中にしか現れないウィンドウがあれば開始とみなす。
        if rule.micless_start_score > 0 {
            if let Some(w) = obs.best_window(app, rule.micless_start_score) {
                return Some(obs.mic_root(app).unwrap_or(w.info.owner.pid));
            }
        }
        None
    }

    pub fn update(&mut self, rules: &Rules, obs: &Observation, now_ns: u64) -> Option<DetectEvent> {
        match self.phase {
            Phase::Idle => {
                let app = (0..rules.apps.len())
                    .find(|&a| Self::start_condition(rules, obs, a).is_some())?;
                self.phase = Phase::Candidate { app, since: now_ns };
                // start_hold = 0 なら即発火。
                if self.start_hold_ns == 0 {
                    return self.update(rules, obs, now_ns);
                }
                None
            }
            Phase::Candidate { app, since } => match Self::start_condition(rules, obs, app) {
                None => {
                    self.phase = Phase::Idle;
                    None
                }
                Some(root_pid) => {
                    if now_ns.saturating_sub(since) >= self.start_hold_ns {
                        self.phase = Phase::Active {
                            app,
                            root_pid,
                            gone_since: None,
                            weak_since: None,
                        };
                        Some(DetectEvent::Started { app, root_pid })
                    } else {
                        None
                    }
                }
            },
            Phase::Active {
                app,
                root_pid,
                gone_since,
                weak_since,
            } => {
                let rule = &rules.apps[app];
                let max = obs.max_score(app);
                let present = max.is_some_and(|s| s >= rule.keep_score);
                let strong = max.is_some_and(|s| s >= rule.strong_score);
                let mic = obs.mic_root(app).is_some();

                let gone_since = if present {
                    None
                } else {
                    Some(gone_since.unwrap_or(now_ns))
                };
                let weak_since = if mic || strong {
                    None
                } else {
                    Some(weak_since.unwrap_or(now_ns))
                };

                let ended = gone_since
                    .is_some_and(|t| now_ns.saturating_sub(t) >= self.end_grace_ns)
                    || weak_since
                        .is_some_and(|t| now_ns.saturating_sub(t) >= self.mic_release_grace_ns);
                if ended {
                    self.phase = Phase::Idle;
                    Some(DetectEvent::Ended { app })
                } else {
                    self.phase = Phase::Active {
                        app,
                        root_pid,
                        gone_since,
                        weak_since,
                    };
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{MicUser, WindowInfo};
    use crate::rules::tests::{proc_, win};

    fn zoom_mic() -> Vec<MicUser> {
        vec![MicUser {
            chain: vec![proc_(7, "Zoom.exe", None)],
        }]
    }
    fn zoom_meeting() -> WindowInfo {
        win(
            1,
            proc_(7, "Zoom.exe", None),
            "Zoom Meeting",
            "ZPContentViewWndClass",
            1280,
            720,
        )
    }
    fn zoom_home() -> WindowInfo {
        win(
            2,
            proc_(7, "Zoom.exe", None),
            "Zoom Workplace",
            "ZPPTMainFrmWndClassEx",
            900,
            600,
        )
    }
    fn zoom_toolbar() -> WindowInfo {
        win(
            3,
            proc_(7, "Zoom.exe", None),
            "",
            "ZPToolBarParentWndClass",
            600,
            60,
        )
    }
    fn teams_mic() -> Vec<MicUser> {
        vec![MicUser {
            chain: vec![
                proc_(31, "msedgewebview2.exe", None),
                proc_(30, "ms-teams.exe", None),
            ],
        }]
    }
    fn teams_main() -> WindowInfo {
        win(
            10,
            proc_(30, "ms-teams.exe", None),
            "Chat | Microsoft Teams",
            "TeamsWebView",
            1200,
            800,
        )
    }
    fn teams_meeting() -> WindowInfo {
        win(
            11,
            proc_(30, "ms-teams.exe", None),
            "定例 | Microsoft Teams",
            "TeamsWebView",
            1200,
            800,
        )
    }

    struct H {
        rules: Rules,
        det: Detector,
        t: u64,
    }
    impl H {
        fn new() -> H {
            H {
                rules: Rules::new(&[]),
                det: Detector::new(&DetectConfig::default()),
                t: 0,
            }
        }
        /// 2 秒進めて観測を与える。
        fn step(&mut self, mic: &[MicUser], wins: &[WindowInfo]) -> Option<DetectEvent> {
            self.t += 2 * NS;
            let obs = self.rules.observe(mic, wins);
            self.det.update(&self.rules, &obs, self.t)
        }
    }

    #[test]
    fn starts_after_hold() {
        let mut h = H::new();
        assert_eq!(h.step(&zoom_mic(), &[zoom_meeting()]), None);
        assert_eq!(
            h.step(&zoom_mic(), &[zoom_meeting()]),
            Some(DetectEvent::Started {
                app: 0,
                root_pid: 7
            })
        );
        // 同一会議で二重に発火しない。
        for _ in 0..10 {
            assert_eq!(h.step(&zoom_mic(), &[zoom_meeting()]), None);
        }
        assert_eq!(h.det.active_app(), Some(0));
    }

    #[test]
    fn blip_does_not_start() {
        // 条件が一瞬だけ成立しても（継続しなければ）発火しない。
        let mut h = H::new();
        assert_eq!(h.step(&zoom_mic(), &[zoom_meeting()]), None);
        assert_eq!(h.step(&zoom_mic(), &[zoom_home()]), None);
        assert_eq!(h.step(&zoom_mic(), &[zoom_meeting()]), None);
        assert!(h.det.active_app().is_none());
        let mut h = H::new();
        assert_eq!(h.step(&teams_mic(), &[teams_main()]), None);
        assert_eq!(h.step(&[], &[teams_main()]), None);
        assert_eq!(h.step(&teams_mic(), &[teams_main()]), None);
        assert!(h.det.active_app().is_none());
    }

    #[test]
    fn mic_only_or_window_only_does_not_start() {
        let mut h = H::new();
        // 設定画面のマイクテスト：マイク使用中だが会議ウィンドウなし。
        for _ in 0..5 {
            assert_eq!(h.step(&zoom_mic(), &[zoom_home()]), None);
        }
        // Teams はウィンドウだけでは会議かどうか分からないので、マイクなしでは始めない。
        for _ in 0..5 {
            assert_eq!(h.step(&[], &[teams_main(), teams_meeting()]), None);
        }
    }

    #[test]
    fn zoom_meeting_window_starts_even_without_mic() {
        // ミュートで参加してマイクが開かれない場合の保険。
        let mut h = H::new();
        assert_eq!(h.step(&[], &[zoom_meeting()]), None);
        assert_eq!(
            h.step(&[], &[zoom_meeting()]),
            Some(DetectEvent::Started {
                app: 0,
                root_pid: 7
            })
        );
        // ホーム画面だけでは始まらない。
        let mut h = H::new();
        for _ in 0..5 {
            assert_eq!(h.step(&[], &[zoom_home()]), None);
        }
    }

    #[test]
    fn ends_after_window_gone_grace() {
        let mut h = H::new();
        h.step(&zoom_mic(), &[zoom_meeting()]);
        h.step(&zoom_mic(), &[zoom_meeting()]);
        // ウィンドウ消滅から 8 秒の猶予。
        assert_eq!(h.step(&[], &[zoom_home()]), None); // gone_since = t
        assert_eq!(h.step(&[], &[zoom_home()]), None); // 2s
        assert_eq!(h.step(&[], &[zoom_home()]), None); // 4s
        assert_eq!(h.step(&[], &[zoom_home()]), None); // 6s
        assert_eq!(
            h.step(&[], &[zoom_home()]),
            Some(DetectEvent::Ended { app: 0 })
        );
        assert!(h.det.active_app().is_none());
    }

    #[test]
    fn reconnect_within_grace_keeps_meeting() {
        let mut h = H::new();
        h.step(&zoom_mic(), &[zoom_meeting()]);
        h.step(&zoom_mic(), &[zoom_meeting()]);
        assert_eq!(h.step(&[], &[]), None);
        assert_eq!(h.step(&[], &[]), None);
        assert_eq!(h.step(&zoom_mic(), &[zoom_meeting()]), None);
        // タイマーはリセットされている。
        assert_eq!(h.step(&[], &[]), None);
        assert_eq!(h.step(&[], &[]), None);
        assert_eq!(h.step(&[], &[]), None);
        assert_eq!(h.det.active_app(), Some(0));
    }

    #[test]
    fn zoom_muted_with_meeting_window_continues() {
        let mut h = H::new();
        h.step(&zoom_mic(), &[zoom_meeting()]);
        h.step(&zoom_mic(), &[zoom_meeting()]);
        // マイクが解放されても会議ウィンドウ（strong）がある限り継続。
        for _ in 0..60 {
            assert_eq!(h.step(&[], &[zoom_meeting()]), None);
        }
    }

    #[test]
    fn zoom_screen_share_keeps_meeting_via_toolbar() {
        let mut h = H::new();
        h.step(&zoom_mic(), &[zoom_meeting()]);
        h.step(&zoom_mic(), &[zoom_meeting()]);
        // 画面共有：会議ウィンドウが消え、ツールバーだけ残る。マイクは使用中。
        for _ in 0..30 {
            assert_eq!(h.step(&zoom_mic(), &[zoom_toolbar()]), None);
        }
        // ツールバーのみ＋マイク解放 → mic_release_grace（10 秒）後に終了。
        let mut ended = None;
        for i in 0..10 {
            if let Some(e) = h.step(&[], &[zoom_toolbar()]) {
                ended = Some((i, e));
                break;
            }
        }
        assert_eq!(ended, Some((5, DetectEvent::Ended { app: 0 })));
    }

    #[test]
    fn teams_starts_with_main_window_and_ends_on_mic_release() {
        let mut h = H::new();
        assert_eq!(h.step(&teams_mic(), &[teams_main()]), None);
        assert_eq!(
            h.step(&teams_mic(), &[teams_main()]),
            Some(DetectEvent::Started {
                app: 1,
                root_pid: 30
            })
        );
        // 会議ウィンドウがある間はマイク解放でも継続。
        for _ in 0..20 {
            assert_eq!(h.step(&[], &[teams_main(), teams_meeting()]), None);
        }
        // 会議ウィンドウが閉じ、マイクも解放 → 10 秒後に終了（メインウィンドウは残っている）。
        let mut n = 0;
        loop {
            n += 1;
            if h.step(&[], &[teams_main()]).is_some() {
                break;
            }
            assert!(n < 20);
        }
        assert_eq!(n, 6);
    }

    #[test]
    fn restart_after_end_fires_again() {
        let mut h = H::new();
        h.step(&zoom_mic(), &[zoom_meeting()]);
        h.step(&zoom_mic(), &[zoom_meeting()]);
        for _ in 0..6 {
            h.step(&[], &[]);
        }
        assert!(h.det.active_app().is_none());
        assert_eq!(h.step(&zoom_mic(), &[zoom_meeting()]), None);
        assert!(matches!(
            h.step(&zoom_mic(), &[zoom_meeting()]),
            Some(DetectEvent::Started { .. })
        ));
    }
}
