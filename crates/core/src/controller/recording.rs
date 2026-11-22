//! 録画中の状態（`Rec`）と、録画対象の追従：ウィンドウの張り替えと、自分の画面共有中の画面への切り替え。

use std::path::PathBuf;

use crate::platform::{NS_PER_SEC as NS, Pid, Platform};
use crate::rules::{Observation, ScoredWindow};

/// より良いウィンドウが現れてから張り替えるまでの安定待ち。
const RETARGET_HOLD_NS: u64 = 2 * NS;

/// 会議アプリのウィンドウ構成を 1 行にまとめる（ログ用）。
fn window_summary(obs: &Observation, app: usize) -> String {
    let mut parts: Vec<String> = obs
        .windows
        .iter()
        .filter(|w| w.app == app)
        .map(|w| {
            format!(
                "[id={} {:?} class={:?} {}x{} 階層={} 表示={} {}点 対象={}]",
                w.info.id,
                w.info.title,
                w.info.class,
                w.info.width,
                w.info.height,
                w.info.layer,
                !w.info.minimized,
                w.score,
                w.capture
            )
        })
        .collect();
    parts.extend(
        obs.unmatched
            .iter()
            .filter(|(a, _)| *a == app)
            .map(|(_, w)| {
                format!(
                    "[id={} {:?} class={:?} {}x{} 階層={} 規則なし]",
                    w.id, w.title, w.class, w.width, w.height, w.layer
                )
            }),
    );
    parts.sort();
    if parts.is_empty() {
        "なし".into()
    } else {
        parts.join(" ")
    }
}

#[derive(Debug, Clone)]
pub(super) struct Rec {
    pub(super) app: usize,
    /// 手動開始で、会議検出と結び付いていない（ウィンドウが消えたら止める）。
    pub(super) manual: bool,
    pub(super) started_ns: u64,
    pub(super) path: PathBuf,
    pub(super) target: Option<u64>,
    /// 録画対象が、会議ウィンドウと言い切れるウィンドウ（`ScoredWindow::definite`）か。
    pub(super) target_definite: bool,
    pub(super) candidate: Option<(u64, u64)>,
    pub(super) gone_since: Option<u64>,
    pub(super) audio_only_notified: bool,
    /// 会議アプリ（マイクの状態を読む相手）のルート PID。
    pub(super) root_pid: Pid,
    /// 会議アプリでミュート中と分かっている（分からないときは false）。
    pub(super) app_muted: bool,
    /// いま自分のマイクを録っているか（設定とミュート状態から決めた結果）。
    pub(super) mic_on: bool,
    /// 自分の画面共有中で、共有している画面を録っている。
    pub(super) screen: bool,
    /// 画面への切り替えに失敗した（この録画では再試行せず、ウィンドウの録画を続ける）。
    pub(super) screen_failed: bool,
    /// 直前に記録した会議アプリのウィンドウ構成（変わったときだけログに出す）。
    pub(super) last_windows: String,
}

impl Rec {
    /// 張り替えの判断を後から追えるよう、会議アプリのウィンドウ構成が変わったら記録する
    /// （会議アプリ以外のウィンドウは `obs` に入らない）。
    pub(super) fn log_windows(&mut self, obs: &Observation) {
        let summary = window_summary(obs, self.app);
        if summary != self.last_windows {
            crate::info!("録画中のウィンドウ: {summary}");
            self.last_windows = summary;
        }
    }

    /// 録画対象を `w` に張り替える。
    pub(super) fn retarget(&mut self, p: &mut dyn Platform, w: &ScoredWindow) {
        p.retarget(&w.info);
        self.target = Some(w.info.id);
        self.target_definite = w.definite;
        self.candidate = None;
    }

    /// 自分が画面共有している間は、共有している画面を録る（D31）。
    /// 戻り値：画面を録っている間 true。
    pub(super) fn follow_share(
        &mut self,
        p: &mut dyn Platform,
        obs: &Observation,
        best: Option<&ScoredWindow>,
        enabled: bool,
    ) -> bool {
        let sharing = obs
            .sharing_window(self.app)
            .filter(|_| enabled && !self.screen_failed);
        match sharing {
            Some(mark) if !self.screen => {
                if p.retarget_screen(&mark.info) {
                    crate::info!("画面共有を検出：共有中の画面の録画に切り替え");
                    self.screen = true;
                    self.candidate = None;
                } else {
                    self.screen_failed = true;
                }
            }
            None if self.screen => {
                // 共有が終わった：すぐに画面の録画をやめる（共有していない画面を録らない）。
                self.screen = false;
                match best {
                    Some(b) => {
                        crate::info!("画面共有の終了：{:?} の録画に戻す", b.info.title);
                        self.retarget(p, b);
                    }
                    None => {
                        crate::info!("画面共有の終了：戻る先のウィンドウなし");
                        p.end_screen();
                    }
                }
            }
            _ => {}
        }
        self.screen
    }

    /// 録画対象ウィンドウが消えた・より良いウィンドウが現れたときに張り替える。
    pub(super) fn follow_windows(
        &mut self,
        p: &mut dyn Platform,
        obs: &Observation,
        best: Option<&ScoredWindow>,
        now: u64,
    ) {
        let current = self.target.and_then(|id| {
            obs.windows
                .iter()
                .find(|w| w.app == self.app && w.info.id == id)
        });
        match (current, best) {
            // 会議ウィンドウが消え、残りは会議ウィンドウと言い切れないもの（Teams の本体など）だけ：
            // 会議が終わった直後の可能性が高いので移らない（映像は止まり、音声は続く）。
            (None, Some(b)) if self.target_definite && !b.definite => self.candidate = None,
            // 対象が消えた：すぐ張り替える。
            (None, Some(b)) if self.target.is_some() => {
                crate::info!(
                    "録画対象ウィンドウが消えたため張り替え: {:?} ({}点)",
                    b.info.title,
                    b.score
                );
                self.retarget(p, b);
            }
            // より点数の高いウィンドウが安定して存在する：張り替える。
            (Some(c), Some(b))
                if b.info.id != c.info.id
                    && (b.score > c.score
                        || !c.capture
                        || (c.info.minimized && !b.info.minimized)) =>
            {
                match self.candidate {
                    Some((id, since)) if id == b.info.id => {
                        if now.saturating_sub(since) >= RETARGET_HOLD_NS {
                            crate::info!("録画対象を張り替え: {:?} ({}点)", b.info.title, b.score);
                            self.retarget(p, b);
                        }
                    }
                    _ => self.candidate = Some((b.info.id, now)),
                }
            }
            _ => self.candidate = None,
        }
    }
}
