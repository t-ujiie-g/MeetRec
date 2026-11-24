//! 録画中の状態（`Rec`）と、録画対象の追従：ウィンドウの張り替えと、自分の画面共有中の画面への切り替え。

use std::path::PathBuf;

use crate::platform::{NS_PER_SEC as NS, Pid, Platform, WindowInfo};
use crate::rules::{Observation, ScoredWindow};

/// より良いウィンドウが現れてから張り替えるまでの安定待ち。
const RETARGET_HOLD_NS: u64 = 2 * NS;
/// 会議に入ってから、出力機器の音量に触らなくするまでの待ち（D38）。
const OUTPUT_SETTLE_NS: u64 = 10 * NS;
/// ディスプレイの選び直しにこの回数続けて失敗したら、ウィンドウの録画に戻す（D39）。
const SCREEN_RESELECT_MAX_FAILURES: u8 = 3;
/// 枠が見えない確認がこの回数続いたら、ツールバーの類での選び直しを許す（D39）。
const FRAME_MISSING_MAX: u8 = 5;

/// 会議アプリのウィンドウ構成を 1 行にまとめる（ログ用）。位置は、ディスプレイを決める共有の目印にだけ付ける
/// （ウィンドウを動かすたびにログが増えないように）。
fn window_summary(obs: &Observation, app: usize) -> String {
    let base = |w: &WindowInfo| {
        format!(
            "[id={} {:?} class={:?} {}x{} 階層={}",
            w.id, w.title, w.class, w.width, w.height, w.layer
        )
    };
    let mut parts: Vec<String> = obs
        .windows
        .iter()
        .filter(|w| w.app == app)
        .map(|w| {
            let i = &w.info;
            let at = if w.sharing {
                format!(" 位置=({},{})", i.x, i.y)
            } else {
                String::new()
            };
            format!(
                "{}{at} 表示={} {}点 対象={}]",
                base(i),
                !i.minimized,
                w.score,
                w.capture
            )
        })
        .collect();
    parts.extend(
        obs.unmatched
            .iter()
            .filter(|(a, _)| *a == app)
            .map(|(_, w)| format!("{} 規則なし]", base(w))),
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
    /// 画面を選ぶのに使った共有の目印（枠か・識別子・位置・大きさ）。
    pub(super) screen_mark: Option<(bool, u64, i32, i32, u32, u32)>,
    pub(super) frame_missing: u8,
    pub(super) screen_reselect_failures: u8,
    /// 会議名の付いたウィンドウが初めて見えた時刻（参加前の画面には会議名が無い）。
    pub(super) joined_since: Option<u64>,
    /// `Platform::settle_output` を呼び終えた（出力機器に触らない録画では最初から true）。
    pub(super) output_settled: bool,
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

    /// 会議に入ってしばらく経ったら、出力機器の音量にもう触らないよう OS 層に伝える（D38）。
    pub(super) fn settle_output(&mut self, p: &mut dyn Platform, joined: bool, now: u64) {
        if self.output_settled {
            return;
        }
        if joined {
            self.joined_since.get_or_insert(now);
        }
        if self
            .joined_since
            .is_some_and(|since| now.saturating_sub(since) >= OUTPUT_SETTLE_NS)
        {
            p.settle_output();
            self.output_settled = true;
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
        let geometry = |w: &ScoredWindow| {
            let i = &w.info;
            (w.frame, i.id, i.x, i.y, i.width, i.height)
        };
        // 枠で決めたあとは、枠が一時的に消えても、ツールバーの類では選び直さない。
        self.frame_missing = match sharing {
            Some(mark) if self.screen && !mark.frame => self.frame_missing.saturating_add(1),
            _ => 0,
        };
        let chosen_by_frame =
            self.screen_mark.is_some_and(|m| m.0) && self.frame_missing < FRAME_MISSING_MAX;
        match sharing {
            Some(mark) if !self.screen => {
                if p.retarget_screen(&mark.info) {
                    crate::info!(
                        "画面共有を検出：共有中の画面の録画に切り替え（目印 {:?}）",
                        mark.info.title
                    );
                    self.screen = true;
                    self.screen_mark = Some(geometry(mark));
                    self.screen_reselect_failures = 0;
                    self.candidate = None;
                } else {
                    self.screen_failed = true;
                }
            }
            // 目印が動いた・枠が現れた：ディスプレイを選び直す（同じディスプレイなら OS 層は何もしない）。
            Some(mark)
                if self.screen_mark != Some(geometry(mark)) && (mark.frame || !chosen_by_frame) =>
            {
                if p.retarget_screen(&mark.info) {
                    self.screen_mark = Some(geometry(mark));
                    self.screen_reselect_failures = 0;
                } else {
                    // 次の確認でもう一度試す。続けて失敗したら、共有していない画面を録り続けないようにやめる。
                    self.screen_reselect_failures += 1;
                    if self.screen_reselect_failures >= SCREEN_RESELECT_MAX_FAILURES {
                        crate::warn!(
                            "共有しているディスプレイを選び直せないので、ウィンドウの録画に戻します"
                        );
                        self.screen = false;
                        self.screen_failed = true;
                        self.screen_mark = None;
                        match best {
                            Some(b) => self.retarget(p, b),
                            None => p.end_screen(),
                        }
                    }
                }
            }
            None if self.screen => {
                // 共有が終わった：すぐに画面の録画をやめる（共有していない画面を録らない）。
                self.screen = false;
                self.screen_mark = None;
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
