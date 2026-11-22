//! 診断ログ：検出がうまくいかないときに、実機で何が見えているかを書き出す（会議アプリ以外のウィンドウ名は出さない）。

use std::fmt::Write as _;
use std::fs;

use super::Controller;
use crate::platform::Platform;
use crate::text;

impl Controller {
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
                        "[{}] id={} pid={} proc={:?} bundle={:?} class={:?} title={:?} {}x{} layer={} min={} => {}",
                        self.rules.apps[a].id,
                        w.id,
                        w.owner.pid,
                        w.owner.name,
                        w.owner.bundle_id,
                        w.class,
                        w.title,
                        w.width,
                        w.height,
                        w.layer,
                        w.minimized,
                        score.map_or("一致なし".into(), |(sc, cap)| format!(
                            "{sc}点 capture={cap}"
                        )),
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

    pub(super) fn save_diagnostics(&mut self, p: &mut dyn Platform) {
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
