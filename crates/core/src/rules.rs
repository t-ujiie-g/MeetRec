//! 会議アプリの規則（プロセス名・ウィンドウ・ミュートの見分け方）の型と、観測結果との照合ロジック。
//! Zoom / Teams の具体的な値は `builtin`（組み込みの定数表）にある。

use serde::{Deserialize, Serialize};

use crate::platform::{MicUser, Pid, ProcInfo, WindowInfo};

mod builtin;

pub use builtin::builtin_rules;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WindowRule {
    /// 一致したときの点数。複数一致したら最大値を採る。
    pub score: i32,
    /// false なら「会議継続の証拠」にはなるが録画対象にはしない（ツールバー等）。
    pub capture: bool,
    /// ウィンドウクラス名（Windows）。空でなければいずれかに完全一致が必要。
    /// macOS のウィンドウはクラス名が空なので、これを指定した規則には一致しない。
    pub class_any: Vec<String>,
    /// タイトルにいずれかを含む（大文字小文字無視）。空なら条件なし。
    pub title_any: Vec<String>,
    /// タイトルにいずれかを含んだら不一致。
    pub title_none: Vec<String>,
    /// タイトルがいずれかと完全に一致（大文字小文字無視）したら不一致。
    pub title_not_exact: Vec<String>,
    /// タイトルが空のウィンドウには一致させない。
    pub title_required: bool,
    /// 会議ウィンドウと言い切れない、最後の受け皿の規則（Teams の「任意のウィンドウ」）。
    /// 録画対象を選ぶときは、受け皿でない規則に一致したウィンドウを必ず優先する。
    pub fallback: bool,
    pub min_width: u32,
    pub min_height: u32,
    /// 通常以外の階層（`WindowInfo::layer` が 0 以外）のウィンドウにも一致させる。
    /// 既定は通常の階層だけ（メニューやツールチップを会議ウィンドウと取り違えないため）。
    pub any_layer: bool,
    /// 自分が画面共有している間だけ現れるウィンドウ（共有ツールバー等）。
    /// 映っていれば「共有中」とみなし、共有している画面の録画に切り替える（D31）。
    pub sharing: bool,
    /// `sharing` の目印のうち、共有している画面やウィンドウを囲む枠（共有しているディスプレイに必ずある）。
    /// ツールバーの類（別のディスプレイに出ることがある）より優先して、録るディスプレイを決める。
    pub sharing_frame: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AppRule {
    /// 識別子（設定での上書きキー）。
    pub id: String,
    /// 表示名。ファイル名にも使う（例: `Zoom`）。
    pub name: String,
    /// 実行ファイル名（大文字小文字・`.exe` 無視で完全一致）。
    pub process_names: Vec<String>,
    /// macOS バンドル ID の前方一致。
    pub bundle_prefixes: Vec<String>,
    /// 開始に必要な「録画対象ウィンドウ」の最低点。
    pub start_score: i32,
    /// この点以上のウィンドウがあれば、マイクが解放されても会議継続とみなす。
    pub strong_score: i32,
    /// この点以上のウィンドウが 1 つもなくなったら（猶予後）終了。
    pub keep_score: i32,
    /// この点以上の録画対象ウィンドウがあれば、マイク未使用でも会議開始とみなす（0 = 無効）。
    /// ミュートで参加するとアプリがマイクを開かない場合の保険。会議中にしか現れない
    /// ウィンドウを確実に見分けられるアプリでのみ有効にする。
    pub micless_start_score: i32,
    pub windows: Vec<WindowRule>,
    /// 会議タイトル抽出時にウィンドウタイトルから取り除く文字列。
    pub title_strip: Vec<String>,
    /// 取り除いた結果がこれらに一致（大文字小文字無視）したらタイトルなし扱い。
    pub generic_titles: Vec<String>,
    /// 会議アプリのミュート状態の見分け方。空なら見分けない（自分のマイクを録り続ける）。
    pub mute: MuteRule,
    /// Windows：相手の音として、このアプリの音だけでなく PC から出る音すべてを録る（通知音や他のアプリの音も入る）。
    /// アプリの音だけを録る方式（プロセス単位）で無音しか届かないアプリ用（D36）。macOS では使わない。
    pub system_audio: bool,
}

/// 会議アプリのミュート状態を、画面部品（ボタン・メニュー項目）の名前で見分ける規則。
/// 部品の名前は「押すとどうなるか」を表すので、「ミュートを解除」があればミュート中。
/// 名前は前方一致（大文字小文字無視）で比べる。Windows の Zoom は名前の後ろにショートカットや説明が付くため。
/// 似た名前の別の部品（参加者の「ミュート解除」など）と取り違えないよう、書き出しは省かずに書くこと。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MuteRule {
    /// この名前で始まる部品があれば、自分はミュート中。
    pub muted: Vec<String>,
    /// この名前で始まる部品があれば、自分はミュートしていない。
    pub unmuted: Vec<String>,
}

impl MuteRule {
    /// 探す部品の名前（両方の状態ぶん）。
    pub fn names(&self) -> Vec<String> {
        self.muted.iter().chain(&self.unmuted).cloned().collect()
    }

    /// 見つかった部品の名前から、ミュート中かを決める。「マイクのミュート」は「マイクのミュートを解除」の
    /// 書き出しでもあるので、ミュート中の名前に当たるかだけを見る。
    pub fn is_muted(&self, control: &str) -> bool {
        self.muted.iter().any(|m| starts_with_ci(control, m))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScoredWindow {
    pub app: usize,
    pub score: i32,
    pub capture: bool,
    /// 自分が画面共有中であることを示すウィンドウ（`WindowRule::sharing`）。
    pub sharing: bool,
    /// 共有しているもの（画面やウィンドウ）を囲む枠（`WindowRule::sharing_frame`）。
    pub frame: bool,
    /// 会議ウィンドウ（またはその代わり）と言い切れるウィンドウ。受け皿の規則
    /// （`WindowRule::fallback`）にしか一致しなかったものは false。
    pub definite: bool,
    pub info: WindowInfo,
}

/// 1 回の観測を規則に通した結果。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Observation {
    /// マイク使用中の会議アプリ（アプリ番号, ルート PID）。
    pub mic_apps: Vec<(usize, Pid)>,
    pub windows: Vec<ScoredWindow>,
    /// 会議アプリのウィンドウのうち、どの規則にも一致しなかったもの（映っているものだけ）。
    /// 判定には使わない。録画中のログに出して、規則の不足を実機で見つけるためのもの。
    pub unmatched: Vec<(usize, WindowInfo)>,
}

impl Observation {
    pub fn mic_root(&self, app: usize) -> Option<Pid> {
        self.mic_apps
            .iter()
            .find(|(a, _)| *a == app)
            .map(|(_, p)| *p)
    }

    /// 録画対象として最良のウィンドウ（会議ウィンドウらしさ → 非最小化 → 点数 → 面積の順）。
    /// 最小化中のウィンドウは映像が止まるので、映っているウィンドウ（Zoom のミニウィンドウ等）
    /// があればそちらを優先する。ただし会議ウィンドウが隠れていても、会議ウィンドウと言い切れない
    /// ウィンドウ（Teams の本体など）へは移らない。
    pub fn best_window(&self, app: usize, min_score: i32) -> Option<&ScoredWindow> {
        self.windows
            .iter()
            .filter(|w| w.app == app && w.capture && w.score >= min_score)
            .max_by_key(|w| {
                (
                    w.definite,
                    !w.info.minimized,
                    w.score,
                    w.info.width as u64 * w.info.height as u64,
                )
            })
    }

    /// 自分が画面共有中なら、その目印になるウィンドウ（映っているもの）を返す。
    /// 目印が複数あれば、共有しているものを囲む枠（`frame`）を優先し、その中でいちばん大きいものを選ぶ：
    /// 枠は共有しているディスプレイにあるが、ツールバーの類は別のディスプレイに出ることがある
    /// （共有していない画面を録らないため）。
    pub fn sharing_window(&self, app: usize) -> Option<&ScoredWindow> {
        let rank =
            |w: &&ScoredWindow| (w.frame, u64::from(w.info.width) * u64::from(w.info.height));
        self.windows
            .iter()
            .filter(|w| w.app == app && w.sharing && !w.info.minimized)
            // 同じ順位なら、先に並んでいるほう（`max_by_key` は最後を返すので逆順にする）。
            .rev()
            .max_by_key(rank)
    }

    pub fn max_score(&self, app: usize) -> Option<i32> {
        self.windows
            .iter()
            .filter(|w| w.app == app)
            .map(|w| w.score)
            .max()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rules {
    pub apps: Vec<AppRule>,
}

fn norm_proc(name: &str) -> String {
    let lower = name.to_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
}

/// `text` が `prefix` で始まるか（大文字小文字無視）。OS 層が部品を探すときにも使う。
pub fn starts_with_ci(text: &str, prefix: &str) -> bool {
    text.to_lowercase().starts_with(&prefix.to_lowercase())
}

fn contains_ci(haystack_lower: &str, needle: &str) -> bool {
    haystack_lower.contains(&needle.to_lowercase())
}

impl Rules {
    /// マイク未使用でも開始とみなす規則を持つアプリがあるか（定期的なウィンドウ確認が必要か）。
    pub fn has_micless_start(&self) -> bool {
        self.apps.iter().any(|a| a.micless_start_score > 0)
    }

    /// 組み込み表に設定の `[[app]]` を重ねる（同じ id は差し替え、新しい id は追加）。
    pub fn new(overrides: &[AppRule]) -> Rules {
        let mut apps = builtin_rules();
        for o in overrides {
            if o.id.is_empty() {
                continue;
            }
            match apps.iter_mut().find(|a| a.id == o.id) {
                Some(slot) => *slot = o.clone(),
                None => apps.push(o.clone()),
            }
        }
        Rules { apps }
    }

    pub fn app_for_proc(&self, proc_: &ProcInfo) -> Option<usize> {
        let name = norm_proc(&proc_.name);
        self.apps.iter().position(|a| {
            a.process_names.iter().any(|p| norm_proc(p) == name)
                || proc_.bundle_id.as_deref().is_some_and(|b| {
                    let b = b.to_lowercase();
                    a.bundle_prefixes
                        .iter()
                        .any(|p| b.starts_with(&p.to_lowercase()))
                })
        })
    }

    /// プロセスの祖先チェーンから会議アプリを探す。ルート PID は一致した最も遠い祖先。
    pub fn app_for_chain(&self, chain: &[ProcInfo]) -> Option<(usize, Pid)> {
        let mut found: Option<(usize, Pid)> = None;
        for p in chain {
            if let Some(app) = self.app_for_proc(p) {
                match found {
                    Some((a, _)) if a != app => {}
                    _ => found = Some((app, p.pid)),
                }
            }
        }
        found
    }

    pub fn score_window(&self, app: usize, w: &WindowInfo) -> Option<(i32, bool)> {
        self.best_rule(app, w).map(|r| (r.score, r.capture))
    }

    /// ウィンドウに一致する規則のうち、最も点数の高いもの（同点なら先に書かれたもの）。
    fn best_rule(&self, app: usize, w: &WindowInfo) -> Option<&WindowRule> {
        // 通常以外の階層のパネルは、会議の外でも隠れたまま残る（Zoom のビデオパネルで実機確認）。
        // 映っていないものは「無い」ものとして扱う。通常の階層の最小化とは区別する。
        if w.layer != 0 && w.minimized {
            return None;
        }
        let title = w.title.to_lowercase();
        let mut best: Option<&WindowRule> = None;
        for r in &self.apps[app].windows {
            if w.layer != 0 && !r.any_layer {
                continue;
            }
            if w.width < r.min_width || w.height < r.min_height {
                continue;
            }
            if !r.class_any.is_empty()
                && !r.class_any.iter().any(|c| c.eq_ignore_ascii_case(&w.class))
            {
                continue;
            }
            if !r.title_any.is_empty() && !r.title_any.iter().any(|t| contains_ci(&title, t)) {
                continue;
            }
            if r.title_none.iter().any(|t| contains_ci(&title, t)) {
                continue;
            }
            if r.title_required && title.trim().is_empty() {
                continue;
            }
            if r.title_not_exact
                .iter()
                .any(|t| t.to_lowercase() == title.trim())
            {
                continue;
            }
            if best.is_none_or(|b| r.score > b.score) {
                best = Some(r);
            }
        }
        best
    }

    pub fn observe(&self, mic_users: &[MicUser], windows: &[WindowInfo]) -> Observation {
        let mut obs = Observation::default();
        for m in mic_users {
            if let Some((app, root)) = self.app_for_chain(&m.chain)
                && obs.mic_root(app).is_none()
            {
                obs.mic_apps.push((app, root));
            }
        }
        for w in windows {
            if w.width == 0 || w.height == 0 {
                continue;
            }
            let Some(app) = self.app_for_proc(&w.owner) else {
                continue;
            };
            match self.best_rule(app, w) {
                Some(rule) => obs.windows.push(ScoredWindow {
                    app,
                    score: rule.score,
                    capture: rule.capture,
                    sharing: rule.sharing,
                    frame: rule.sharing && rule.sharing_frame,
                    definite: !rule.fallback,
                    info: w.clone(),
                }),
                None if !w.minimized => obs.unmatched.push((app, w.clone())),
                None => {}
            }
        }
        obs
    }

    /// ウィンドウタイトルから会議タイトルを取り出す（取れなければ None）。
    pub fn meeting_title(&self, app: usize, window_title: &str) -> Option<String> {
        let rule = &self.apps[app];
        let mut t = window_title.to_string();
        for strip in &rule.title_strip {
            if strip.is_empty() {
                continue;
            }
            // 大文字小文字を無視して、出てくるぶんをすべて取り除く（Windows の Teams の参加前の画面は
            // 「… | Microsoft Teams 会議 | Microsoft Teams」と 2 回含む）。
            let needle = strip.to_lowercase();
            while let Some(pos) = t.to_lowercase().find(&needle) {
                if !(t.is_char_boundary(pos) && t.is_char_boundary(pos + strip.len())) {
                    break;
                }
                t.replace_range(pos..pos + strip.len(), "");
            }
        }
        let t = t
            .trim_matches(|c: char| c.is_whitespace() || c == '|' || c == '-')
            .to_string();
        if t.is_empty()
            || rule
                .generic_titles
                .iter()
                .any(|g| g.eq_ignore_ascii_case(&t))
        {
            None
        } else {
            Some(t)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn proc_(pid: Pid, name: &str, bundle: Option<&str>) -> ProcInfo {
        ProcInfo {
            pid,
            name: name.into(),
            bundle_id: bundle.map(|b| b.into()),
        }
    }

    pub fn win(id: u64, owner: ProcInfo, title: &str, class: &str, w: u32, h: u32) -> WindowInfo {
        WindowInfo {
            id,
            owner,
            title: title.into(),
            class: class.into(),
            x: 0,
            y: 0,
            width: w,
            height: h,
            minimized: false,
            layer: 0,
        }
    }

    #[test]
    fn process_matching() {
        let r = Rules::new(&[]);
        assert_eq!(r.app_for_proc(&proc_(1, "Zoom.exe", None)), Some(0));
        assert_eq!(
            r.app_for_proc(&proc_(1, "zoom.us", Some("us.zoom.xos"))),
            Some(0)
        );
        assert_eq!(
            r.app_for_proc(&proc_(1, "CptHost", Some("us.zoom.xos.CptHost"))),
            Some(0)
        );
        assert_eq!(r.app_for_proc(&proc_(1, "ms-teams.exe", None)), Some(1));
        assert_eq!(
            r.app_for_proc(&proc_(
                1,
                "Microsoft Teams WebView Helper",
                Some("com.microsoft.teams2.helper")
            )),
            Some(1)
        );
        assert_eq!(r.app_for_proc(&proc_(1, "chrome.exe", None)), None);
        assert_eq!(r.app_for_proc(&proc_(1, "zoomit.exe", None)), None);
    }

    #[test]
    fn chain_uses_farthest_matching_ancestor() {
        let r = Rules::new(&[]);
        let chain = vec![
            proc_(30, "msedgewebview2.exe", None),
            proc_(20, "msedgewebview2.exe", None),
            proc_(10, "ms-teams.exe", None),
            proc_(1, "explorer.exe", None),
        ];
        assert_eq!(r.app_for_chain(&chain), Some((1, 10)));
        let zoom = vec![proc_(5, "Zoom.exe", None), proc_(4, "Zoom.exe", None)];
        assert_eq!(r.app_for_chain(&zoom), Some((0, 4)));
        assert_eq!(r.app_for_chain(&[proc_(9, "obs64.exe", None)]), None);
    }

    #[test]
    fn zoom_windows() {
        let r = Rules::new(&[]);
        let z = proc_(1, "Zoom.exe", None);
        let meeting = win(
            1,
            z.clone(),
            "Zoom Meeting",
            "ZPContentViewWndClass",
            1280,
            720,
        );
        assert_eq!(r.score_window(0, &meeting), Some((100, true)));
        let jp_mac = win(
            2,
            proc_(1, "zoom.us", Some("us.zoom.xos")),
            "Zoom ミーティング",
            "",
            1280,
            720,
        );
        assert_eq!(r.score_window(0, &jp_mac), Some((100, true)));
        let home = win(
            3,
            z.clone(),
            "Zoom Workplace",
            "ZPPTMainFrmWndClassEx",
            900,
            600,
        );
        assert_eq!(r.score_window(0, &home), None);
        let home_mac = win(3, proc_(1, "zoom.us", None), "Zoom Workplace", "", 900, 600);
        assert_eq!(r.score_window(0, &home_mac), None);
        let toolbar = win(4, z.clone(), "", "ZPToolBarParentWndClass", 600, 60);
        assert_eq!(r.score_window(0, &toolbar), Some((40, false)));
        let float = win(5, z, "", "ZPFloatVideoWndClass", 240, 135);
        assert_eq!(r.score_window(0, &float), Some((70, true)));
    }

    #[test]
    fn floating_layers_match_only_rules_that_allow_them() {
        let r = Rules::new(&[]);
        let mac = || proc_(1, "zoom.us", Some("us.zoom.xos"));
        // macOS：共有中のビデオパネルは常に手前に出る階層にある。
        let mut float = win(1, mac(), "zoom floating video window", "", 240, 135);
        float.layer = 3;
        assert_eq!(r.score_window(0, &float), Some((70, true)));
        // 隠れたまま残っているパネルは数えない（会議が終わっても残るため）。
        float.minimized = true;
        assert_eq!(r.score_window(0, &float), None);
        // 「常に手前に表示」の会議ウィンドウ。
        let mut meeting = win(2, mac(), "Zoom ミーティング", "", 1280, 720);
        meeting.layer = 3;
        assert_eq!(r.score_window(0, &meeting), Some((100, true)));
        // Teams の「任意のウィンドウ」規則は、メニューなど通常以外の階層には一致させない。
        let teams = r.apps.iter().position(|a| a.id == "teams").unwrap();
        let mut popup = win(3, proc_(2, "MSTeams", None), "", "", 400, 300);
        assert_eq!(r.score_window(teams, &popup), Some((40, true)));
        popup.layer = 101;
        assert_eq!(r.score_window(teams, &popup), None);
    }

    #[test]
    fn only_teams_records_all_pc_audio_on_windows() {
        // Windows の Teams は、アプリの音だけを録る方式だと無音しか届かなかった（実機で確認）。Zoom は録れる。
        let r = Rules::new(&[]);
        let flag = |id: &str| r.apps.iter().find(|a| a.id == id).unwrap().system_audio;
        assert!(flag("teams"));
        assert!(!flag("zoom"));
    }

    #[test]
    fn mute_state_is_read_from_exact_control_names() {
        let r = Rules::new(&[]);
        let teams = &r.apps.iter().find(|a| a.id == "teams").unwrap().mute;
        // 実機（macOS・日本語の Teams）で見えたボタンの名前。
        assert!(teams.is_muted("マイクのミュートを解除"));
        assert!(!teams.is_muted("マイクのミュート"));
        // 実機（macOS・日本語の Zoom）で見えたメニュー項目の名前。
        let zoom = &r.apps.iter().find(|a| a.id == "zoom").unwrap().mute;
        assert!(zoom.is_muted("オーディオのミュート解除"));
        assert!(!zoom.is_muted("オーディオのミュート"));
        // 実機（Windows・日本語の Zoom）のボタンは、後ろにショートカットや説明が続く。
        assert!(zoom.is_muted(
            "ミュート解除、現在ミュート中, Alt+A, 自分のオーディオのミュートを解除します（Alt+A）。"
        ));
        assert!(!zoom.is_muted("ミュート、現在ミュート解除中, Alt+A"));
        // 同じ画面にある別の部品（Teams の参加者向け「ミュート解除」、Zoom の「全員をミュート」）は、
        // どの名前の書き出しにも当たらない。
        for other in ["ミュート解除", "全員をミュート", "全員にミュート解除を要請"]
        {
            for rule in [teams, zoom] {
                assert!(
                    !rule.names().iter().any(|n| starts_with_ci(other, n)),
                    "{other}"
                );
            }
        }
    }

    #[test]
    fn teams_windows() {
        let r = Rules::new(&[]);
        let t = proc_(1, "ms-teams.exe", None);
        let main = win(
            1,
            t.clone(),
            "Chat | Microsoft Teams",
            "TeamsWebView",
            1200,
            800,
        );
        assert_eq!(r.score_window(1, &main), Some((40, true)));
        let main_jp = win(
            1,
            t.clone(),
            "チャット | Microsoft Teams",
            "TeamsWebView",
            1200,
            800,
        );
        assert_eq!(r.score_window(1, &main_jp), Some((40, true)));
        // タブ名の一覧（`TEAMS_TABS`）にあるものは、どれも本体のウィンドウとして扱う。
        let meet_tab = win(
            1,
            t.clone(),
            "Meet | Microsoft Teams",
            "TeamsWebView",
            1200,
            800,
        );
        assert_eq!(r.score_window(1, &meet_tab), Some((40, true)));
        let meeting = win(
            2,
            t.clone(),
            "週次定例 | Microsoft Teams",
            "TeamsWebView",
            1200,
            800,
        );
        assert_eq!(r.score_window(1, &meeting), Some((90, true)));
        let compact = win(
            3,
            t,
            "Meeting compact view | 週次定例 | Microsoft Teams",
            "TeamsWebView",
            300,
            200,
        );
        assert_eq!(r.score_window(1, &compact), Some((100, true)));
    }

    #[test]
    fn teams_mac_meeting_window_is_preferred_over_the_main_window() {
        let r = Rules::new(&[]);
        let t = || proc_(1, "MSTeams", Some("com.microsoft.teams2"));
        // 実機（macOS・日本語）で見えた構成：本体はタブ名、会議は会議名、ほかに無題や URL のウィンドウ。
        let main = win(1, t(), "ミーティング", "", 1368, 769);
        let meeting = win(2, t(), "test", "", 1276, 797);
        let blank = win(3, t(), "", "", 1368, 769);
        let web = win(4, t(), "teams.microsoft.com/v2/", "", 1368, 769);
        let prejoin = win(5, t(), "会議への参加 | Microsoft Teams 会議", "", 1280, 800);
        assert_eq!(r.score_window(1, &main), Some((40, true)));
        assert_eq!(r.score_window(1, &meeting), Some((80, true)));
        assert_eq!(r.score_window(1, &blank), Some((40, true)));
        assert_eq!(r.score_window(1, &web), Some((40, true)));
        assert_eq!(r.score_window(1, &prejoin), Some((90, true)));
        // 「チーム定例」のようにタブ名を含むだけの会議名は会議ウィンドウ。
        let named = win(6, t(), "チーム定例", "", 1276, 797);
        assert_eq!(r.score_window(1, &named), Some((80, true)));

        // 本体のほうが大きくても、会議ウィンドウを録る。
        let obs = r.observe(&[], &[main.clone(), meeting.clone(), blank, web]);
        assert_eq!(obs.best_window(1, 40).unwrap().info.id, 2);
        // 会議ウィンドウが隠れていても（別のデスクトップ等）、本体へは移らない。
        let mut hidden = meeting;
        hidden.minimized = true;
        let obs = r.observe(&[], &[main, hidden]);
        assert_eq!(obs.best_window(1, 40).unwrap().info.id, 2);

        // 自分の画面共有中：共有している画面を覆う枠が目印になる（録画対象にはしない）。
        let mut frame = win(7, t(), "Sharing Indicator", "", 1920, 1080);
        frame.layer = 2_147_483_631;
        let mut bar = win(8, t(), "共有コントロール バー", "", 297, 35);
        bar.layer = 19;
        let obs = r.observe(&[], &[win(2, t(), "test", "", 1276, 797), frame, bar]);
        assert_eq!(obs.sharing_window(1).unwrap().info.id, 7);
        assert_eq!(obs.best_window(1, 40).unwrap().info.id, 2);

        // Windows（実機のログで確認）：共有中は、タイトルが空の枠のウィンドウと「会議のコンパクト ビュー」が出て、
        // 会議ウィンドウは隠れる。枠が目印になり、録画対象にはならない。
        let w = || proc_(3, "ms-teams.exe", None);
        let mut hidden = win(
            10,
            w(),
            "test | Microsoft Teams",
            "TeamsWebView",
            1920,
            1015,
        );
        hidden.minimized = true;
        let border = win(11, w(), "", "ScreenBorderWindow", 1920, 1008);
        let compact = win(
            12,
            w(),
            "会議のコンパクト ビュー | test | Microsoft Teams",
            "TeamsWebView",
            286,
            459,
        );
        let obs = r.observe(&[], &[hidden, border.clone(), compact]);
        assert_eq!(obs.sharing_window(1).unwrap().info.id, 11);
        assert_eq!(r.score_window(1, &border), Some((40, false)));

        assert_eq!(r.meeting_title(1, "test"), Some("test".into()));
        assert_eq!(r.meeting_title(1, "ミーティング"), None);
        assert_eq!(
            r.meeting_title(1, "会議への参加 | Microsoft Teams 会議"),
            None
        );
        // Windows（実機で確認）：参加前の画面は「| Microsoft Teams」を 2 回含む。
        assert_eq!(
            r.meeting_title(1, "会議への参加 | Microsoft Teams 会議 | Microsoft Teams"),
            None
        );
        assert_eq!(
            r.meeting_title(1, "test | Microsoft Teams"),
            Some("test".into())
        );
    }

    #[test]
    fn observe_and_best_window() {
        let r = Rules::new(&[]);
        let z = proc_(7, "Zoom.exe", None);
        let mic = vec![
            MicUser {
                chain: vec![z.clone()],
            },
            MicUser {
                chain: vec![proc_(8, "obs.exe", None)],
            },
        ];
        let wins = vec![
            win(
                1,
                z.clone(),
                "Zoom Workplace",
                "ZPPTMainFrmWndClassEx",
                900,
                600,
            ),
            win(
                2,
                z.clone(),
                "Zoom Meeting",
                "ZPContentViewWndClass",
                1280,
                720,
            ),
            win(3, z, "", "ZPToolBarParentWndClass", 600, 60),
            win(
                4,
                proc_(9, "chrome.exe", None),
                "Zoom Meeting - Chrome",
                "Chrome_WidgetWin_1",
                800,
                600,
            ),
        ];
        let obs = r.observe(&mic, &wins);
        assert_eq!(obs.mic_apps, vec![(0, 7)]);
        assert_eq!(obs.windows.len(), 2);
        assert_eq!(obs.best_window(0, 30).unwrap().info.id, 2);
        assert_eq!(obs.max_score(0), Some(100));
        assert_eq!(obs.max_score(1), None);
    }

    #[test]
    fn meeting_titles() {
        let r = Rules::new(&[]);
        assert_eq!(r.meeting_title(0, "Zoom Meeting"), None);
        assert_eq!(r.meeting_title(0, "Zoom ミーティング"), None);
        assert_eq!(
            r.meeting_title(1, "週次定例 | Microsoft Teams"),
            Some("週次定例".into())
        );
        assert_eq!(
            r.meeting_title(1, "Meeting compact view | Design review | Microsoft Teams"),
            Some("Design review".into())
        );
        assert_eq!(r.meeting_title(1, "Microsoft Teams"), None);
    }

    #[test]
    fn overrides_replace_and_add() {
        let mut custom = builtin_rules().remove(0);
        custom.process_names = vec!["zoomx".into()];
        let extra = AppRule {
            id: "meet".into(),
            name: "Meet".into(),
            ..Default::default()
        };
        let r = Rules::new(&[custom, extra]);
        assert_eq!(r.apps.len(), 3);
        assert_eq!(r.app_for_proc(&proc_(1, "zoomx", None)), Some(0));
        // バンドル ID の一致は残る（差し替えた定義にも us.zoom.xos がある）。
        assert_eq!(r.app_for_proc(&proc_(1, "Zoom.exe", None)), None);
    }
}
