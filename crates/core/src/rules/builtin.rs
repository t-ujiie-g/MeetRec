//! 組み込みの会議アプリの定数表（Zoom / Teams）。プロセス名、ウィンドウの見分け方、ミュートの見分け方など。
//!
//! ここにある値は、実機で確かめたものと、まだ確かめていない仮説の両方を含む（どちらかは各行のコメントと
//! PROGRESS.md の「未検証事項」に書く）。外れた場合は「診断ログを保存」の出力や録画中のログを見て直す。
//! 利用者は `config.toml` の `[[app]]` で、同じ `id` の定義を丸ごと差し替えられる。

use super::{AppRule, MuteRule, WindowRule};

/// Teams 本体のウィンドウのタイトルになるタブ名。Windows は「<タブ名> | … | Microsoft Teams」、
/// macOS はタブ名だけがタイトルになる。
const TEAMS_TABS: &[&str] = &[
    "Chat",
    "Calendar",
    "Activity",
    "Teams",
    "Communities",
    "Calls",
    "Files",
    "OneDrive",
    "Apps",
    "Copilot",
    "Settings",
    "Meet",
    "チャット",
    "カレンダー",
    "予定表",
    "アクティビティ",
    "最新情報",
    "チーム",
    "コミュニティ",
    "通話",
    "ファイル",
    "アプリ",
    "設定",
    "ミーティング",
];

fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(|x| x.to_string()).collect()
}

/// 組み込みの定数表。
pub fn builtin_rules() -> Vec<AppRule> {
    vec![
        AppRule {
            id: "zoom".into(),
            name: "Zoom".into(),
            // macOS: zoom.us / Windows: Zoom.exe
            process_names: s(&["zoom.us", "zoom"]),
            bundle_prefixes: s(&["us.zoom.xos"]),
            start_score: 90,
            strong_score: 60,
            keep_score: 30,
            micless_start_score: 100,
            windows: vec![
                // 会議メインウィンドウ（英語/日本語 UI）。
                WindowRule {
                    score: 100,
                    capture: true,
                    title_any: s(&[
                        "Zoom Meeting",
                        "Zoom ミーティング",
                        "Zoom Webinar",
                        "Zoom ウェビナー",
                    ]),
                    min_width: 200,
                    min_height: 150,
                    // 「常に手前に表示」にしていると通常以外の階層になる。
                    any_layer: true,
                    ..Default::default()
                },
                // Windows の会議ウィンドウクラス（タイトルが会議名に変わる版への備え）。
                WindowRule {
                    score: 100,
                    capture: true,
                    class_any: s(&["ZPContentViewWndClass", "ConfMultiTabContentWndClass"]),
                    title_none: s(&["Zoom Workplace"]),
                    min_width: 200,
                    min_height: 150,
                    ..Default::default()
                },
                // 自分が画面共有中に出る参加者ビデオのフローティングパネル。
                WindowRule {
                    score: 70,
                    capture: true,
                    class_any: s(&["ZPFloatVideoWndClass", "VideoFrameWndClass"]),
                    ..Default::default()
                },
                WindowRule {
                    score: 70,
                    capture: true,
                    title_any: s(&["zoom floating video window", "zoom floating video"]),
                    // macOS では常に手前に出るパネル（通常以外の階層）。
                    any_layer: true,
                    ..Default::default()
                },
                // 画面共有中のツールバー類（録画対象にはしない）。
                WindowRule {
                    score: 40,
                    capture: false,
                    class_any: s(&["ZPToolBarParentWndClass", "ZPFloatToolbarClass"]),
                    sharing: true,
                    ..Default::default()
                },
                WindowRule {
                    score: 40,
                    capture: false,
                    title_any: s(&["zoom share toolbar window", "zoom share statusbar window"]),
                    any_layer: true,
                    sharing: true,
                    ..Default::default()
                },
            ],
            title_strip: s(&[]),
            generic_titles: s(&[
                "Zoom Meeting",
                "Zoom ミーティング",
                "Zoom Webinar",
                "Zoom ウェビナー",
                "Zoom",
                "Zoom Workplace",
                "zoom floating video window",
            ]),
            // macOS：メニューバーの「ミーティング」にある項目。Windows：会議ウィンドウのボタン
            // （後ろにショートカットや説明が続く）。日本語は両 OS とも実機で確認。英語は未検証。
            mute: MuteRule {
                muted: s(&[
                    "オーディオのミュート解除",
                    "ミュート解除、現在ミュート中",
                    "Unmute Audio",
                    "Unmute, currently muted",
                ]),
                unmuted: s(&[
                    "オーディオのミュート",
                    "ミュート、現在ミュート解除中",
                    "Mute Audio",
                    "Mute, currently unmuted",
                ]),
            },
            system_audio: false,
        },
        AppRule {
            id: "teams".into(),
            name: "Teams".into(),
            // 新 Teams。Windows: ms-teams.exe（音を出すのは子の ms-teams.exe。実機で確認）、
            // macOS: MSTeams（バンドル com.microsoft.teams2、音声はヘルパー）。
            process_names: s(&["ms-teams", "msteams", "microsoft teams"]),
            bundle_prefixes: s(&["com.microsoft.teams2"]),
            // Teams は会議ウィンドウをタイトルで確実に見分けられないため、
            // 「マイク使用中 + Teams のウィンドウがある」で開始する。
            start_score: 40,
            strong_score: 90,
            keep_score: 40,
            micless_start_score: 0,
            windows: vec![
                WindowRule {
                    score: 100,
                    capture: true,
                    title_any: s(&["compact view", "コンパクト ビュー", "コンパクトビュー"]),
                    ..Default::default()
                },
                // メインウィンドウのタブ名で始まらない「… | Microsoft Teams」を会議ウィンドウとみなす。
                WindowRule {
                    score: 90,
                    capture: true,
                    title_any: s(&["| Microsoft Teams"]),
                    title_none: TEAMS_TABS.iter().map(|t| format!("{t} |")).collect(),
                    min_width: 200,
                    min_height: 150,
                    ..Default::default()
                },
                // macOS の会議ウィンドウ：タイトルが会議名だけ（「| Microsoft Teams」が付かない）。
                // 本体のウィンドウはタブ名だけがタイトルになるので、それ以外を会議ウィンドウとみなす（実機で確認）。
                // 退出後もこのウィンドウは隠れたまま残ることがあるので、strong_score（90）未満にして
                // 「マイクが解放されても会議継続」の根拠にはしない（実機で録画が止まらなかった）。
                WindowRule {
                    score: 80,
                    capture: true,
                    title_required: true,
                    title_none: s(&["| Microsoft Teams", "teams.microsoft.com"]),
                    title_not_exact: TEAMS_TABS
                        .iter()
                        .chain(&["Microsoft Teams"])
                        .map(|t| t.to_string())
                        .collect(),
                    min_width: 200,
                    min_height: 150,
                    ..Default::default()
                },
                // 自分が画面共有している間、共有している画面を覆う枠（macOS で実機確認。常に最前面の階層）。
                // 共有しているディスプレイの目印にする。録画対象にはしない。
                WindowRule {
                    score: 40,
                    capture: false,
                    title_any: s(&["Sharing Indicator"]),
                    any_layer: true,
                    sharing: true,
                    ..Default::default()
                },
                // Windows：自分が画面共有している間、共有している画面を囲む枠（タイトルは空。実機のログで確認）。
                WindowRule {
                    score: 40,
                    capture: false,
                    class_any: s(&["ScreenBorderWindow"]),
                    sharing: true,
                    ..Default::default()
                },
                // フォールバック：Teams の任意のウィンドウ。
                WindowRule {
                    score: 40,
                    capture: true,
                    fallback: true,
                    min_width: 300,
                    min_height: 200,
                    ..Default::default()
                },
            ],
            title_strip: s(&[
                "| Microsoft Teams",
                "Meeting compact view |",
                "会議のコンパクト ビュー |",
            ]),
            generic_titles: s(&[
                "Microsoft Teams",
                "Meeting",
                "会議",
                "Call",
                "通話",
                "Meet",
                "ミーティング",
                // 参加前の画面（「会議への参加 | Microsoft Teams 会議」から「| Microsoft Teams」を除いた残り）。
                "会議への参加  会議",
                "Join a meeting  meeting",
            ]),
            // 会議ウィンドウのボタン（日本語は両 OS とも実機で確認。英語は未検証）。
            mute: MuteRule {
                muted: s(&["マイクのミュートを解除", "Unmute mic"]),
                unmuted: s(&["マイクのミュート", "Mute mic"]),
            },
            // Windows の Teams は、アプリの音だけを録る方式だと無音しか届かない（実機で確認）。
            system_audio: true,
        },
    ]
}
