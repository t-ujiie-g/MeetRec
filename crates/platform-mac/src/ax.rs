//! アクセシビリティ API（AXUIElement）で、会議アプリの画面部品（メニューとボタン）を読む。
//! 会議アプリのミュート状態を知るために使う。「アクセシビリティ」の許可が要る。
//! 読むのは部品の種類・名前・状態だけで、操作はしない。

use std::ffi::c_void;
use std::fmt::Write as _;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

use meetrec_core::platform::CONTROL_SEARCH_RETRY_NS;
use meetrec_core::rules::starts_with_ci;

use crate::sys;

/// 1 アプリぶんの書き出しで調べる部品の上限（WebView の木は非常に大きくなりうる）。
const MAX_NODES: usize = 6000;
const MAX_DEPTH: usize = 60;
/// 応答しないアプリを待つ上限（秒）。
const TIMEOUT_SECONDS: f32 = 1.0;
/// 書き出す部品の種類（押せるもの・選べるものだけ。本文やチャットの文字は出さない）。
const ROLES: &[&str] = &[
    "AXButton",
    "AXCheckBox",
    "AXMenuButton",
    "AXMenuItem",
    "AXRadioButton",
    "AXPopUpButton",
];

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;
    fn AXUIElementCreateApplication(pid: i32) -> *mut c_void;
    fn AXUIElementCopyAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        value: *mut *mut c_void,
    ) -> i32;
    fn AXUIElementSetAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        value: *const c_void,
    ) -> i32;
    fn AXUIElementSetMessagingTimeout(element: *const c_void, seconds: f32) -> i32;
    static kAXTrustedCheckOptionPrompt: *const c_void;
}

fn raw(object: &AnyObject) -> *const c_void {
    object as *const AnyObject as *const c_void
}

/// アクセシビリティの許可があるか。`prompt` が true なら、無いときに OS の案内を出す。
pub fn trusted(prompt: bool) -> bool {
    // SAFETY: キーは静的な CFString（NSString と toll-free）。値は NSNumber（CFBoolean と toll-free）。
    unsafe {
        let key = &*(kAXTrustedCheckOptionPrompt as *const NSString);
        let value = NSNumber::numberWithBool(prompt);
        let options = NSDictionary::<NSString, NSNumber>::from_slices(&[key], &[&*value]);
        AXIsProcessTrustedWithOptions(Retained::as_ptr(&options) as *const c_void) != 0
    }
}

/// 部品の属性を読む。無い・読めない場合は None。
fn attribute(element: &AnyObject, name: &str) -> Option<Retained<AnyObject>> {
    let name = NSString::from_str(name);
    let mut value: *mut c_void = std::ptr::null_mut();
    // SAFETY: element は AXUIElement、name は CFString（toll-free）。成功時 value は +1 で返る。
    unsafe {
        if AXUIElementCopyAttributeValue(raw(element), raw(&name), &mut value) != 0 {
            return None;
        }
        Retained::from_raw(value as *mut AnyObject)
    }
}

fn text(element: &AnyObject, name: &str) -> Option<String> {
    let value = attribute(element, name)?;
    match value.downcast::<NSString>() {
        Ok(s) => Some(s.to_string()),
        Err(other) => other
            .downcast::<NSNumber>()
            .ok()
            .map(|n| n.doubleValue().to_string()),
    }
    .filter(|s| !s.is_empty())
}

fn children(element: &AnyObject, name: &str) -> Vec<Retained<AnyObject>> {
    attribute(element, name)
        .and_then(|v| v.downcast::<NSArray>().ok())
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

/// アプリの AXUIElement を作る。
fn application(pid: u32) -> Option<Retained<AnyObject>> {
    // SAFETY: 戻り値は +1 の AXUIElement（CF オブジェクト。retain / release できる）。
    unsafe {
        let app = Retained::from_raw(AXUIElementCreateApplication(pid as i32) as *mut AnyObject)?;
        AXUIElementSetMessagingTimeout(raw(&app), TIMEOUT_SECONDS);
        Some(app)
    }
}

/// Chromium 系（Teams の WebView）は、支援技術がいると分かるまで画面部品の木を作らない。作らせる。
fn enable_web_tree(app: &AnyObject) {
    let yes = NSNumber::numberWithBool(true);
    for name in ["AXManualAccessibility", "AXEnhancedUserInterface"] {
        let name = NSString::from_str(name);
        // SAFETY: 属性名は CFString、値は CFBoolean（どちらも toll-free）。対応しないアプリではエラーが返るだけ。
        unsafe {
            AXUIElementSetAttributeValue(raw(app), raw(&name), raw(&yes));
        }
    }
}

fn describe(element: &AnyObject, role: &str, depth: usize, out: &mut String) {
    let field = |name: &str| text(element, name).unwrap_or_default();
    let _ = writeln!(
        out,
        "{:indent$}{role} title={:?} desc={:?} value={:?} id={:?} mark={:?}",
        "",
        field("AXTitle"),
        field("AXDescription"),
        field("AXValue"),
        field("AXIdentifier"),
        field("AXMenuItemMarkChar"),
        indent = depth.min(12)
    );
}

fn walk(element: &AnyObject, depth: usize, budget: &mut usize, out: &mut String) {
    if *budget == 0 || depth > MAX_DEPTH {
        return;
    }
    *budget -= 1;
    let role = text(element, "AXRole").unwrap_or_default();
    if ROLES.contains(&role.as_str()) {
        describe(element, &role, depth, out);
    }
    for child in children(element, "AXChildren") {
        walk(&child, depth + 1, budget, out);
    }
}

/// 名前が `names` のどれかで始まる（大文字小文字無視）、押せる部品を探す。
fn search(
    element: &AnyObject,
    names: &[String],
    depth: usize,
    budget: &mut usize,
) -> Option<Retained<AnyObject>> {
    if *budget == 0 || depth > MAX_DEPTH {
        return None;
    }
    *budget -= 1;
    let role = text(element, "AXRole").unwrap_or_default();
    if ROLES.contains(&role.as_str()) && matching_name(element, names).is_some() {
        // SAFETY: 生きている AXUIElement を retain して返す。
        return unsafe { Retained::retain(element as *const AnyObject as *mut AnyObject) };
    }
    children(element, "AXChildren")
        .iter()
        .find_map(|child| search(child, names, depth + 1, budget))
}

/// 部品の名前が `names` のどれかで始まれば、その名前を返す。
fn matching_name(element: &AnyObject, names: &[String]) -> Option<String> {
    let title = text(element, "AXTitle")?;
    names
        .iter()
        .any(|n| starts_with_ci(&title, n))
        .then_some(title)
}

/// 会議アプリのミュートの部品を見張る。見つけた部品を覚えておき、次からは名前を読み直すだけにする
/// （毎回アプリ全体を探すと重い）。
#[derive(Default)]
pub struct ControlWatch {
    pid: u32,
    found: Option<Retained<AnyObject>>,
    /// 直前の探索が空振りだった時刻。しばらくは探し直さない。
    missed_ns: Option<u64>,
    /// 許可の案内をこの起動で出したか。
    prompted: bool,
}

impl ControlWatch {
    /// 名前が `names` のどれかで始まる部品を探し、今の名前を返す。
    pub fn find(&mut self, pid: u32, names: &[String]) -> Option<String> {
        // 許可が無ければ、起動ごとに 1 回だけ OS の案内を出す。
        if !trusted(!self.prompted) {
            self.prompted = true;
            return None;
        }
        if self.pid != pid {
            *self = ControlWatch {
                pid,
                prompted: self.prompted,
                ..Default::default()
            };
        }
        // 覚えている部品の名前が変わっただけなら、読み直すだけで済む（ミュートの切り替え）。
        if let Some(found) = &self.found {
            if let Some(name) = matching_name(found, names) {
                return Some(name);
            }
            self.found = None;
        } else if self
            .missed_ns
            .is_some_and(|t| sys::now_ns().saturating_sub(t) < CONTROL_SEARCH_RETRY_NS)
        {
            return None;
        }
        let app = application(pid)?;
        let mut budget = MAX_NODES;
        // メニューバー（Zoom のメニュー項目。小さいので先に見る）→ ウィンドウ（Teams のボタン）の順に探す。
        // Zoom のウィンドウは部品が深く入れ子になっていて、先に探すと時間がかかる（実機で確認）。
        let found = attribute(&app, "AXMenuBar")
            .and_then(|m| search(&m, names, 1, &mut budget))
            .or_else(|| {
                children(&app, "AXWindows")
                    .iter()
                    .find_map(|w| search(w, names, 1, &mut budget))
            });
        match found {
            Some(found) => {
                let name = matching_name(&found, names);
                self.found = Some(found);
                self.missed_ns = None;
                name
            }
            None => {
                // Chromium 系は、支援技術がいると分かるまで部品の木を作らない。次の探索に備えて頼んでおく。
                enable_web_tree(&app);
                self.missed_ns = Some(sys::now_ns());
                None
            }
        }
    }
}

/// 実機調査用：アプリのメニューバーとウィンドウにある、押せる部品の一覧。
pub fn dump_controls(pid: u32) -> String {
    let mut out = String::new();
    let Some(app) = application(pid) else {
        return out;
    };
    enable_web_tree(&app);
    let mut budget = MAX_NODES;
    if let Some(menu_bar) = attribute(&app, "AXMenuBar") {
        let _ = writeln!(out, "[メニューバー]");
        for item in children(&menu_bar, "AXChildren") {
            let _ = writeln!(
                out,
                " メニュー {:?}",
                text(&item, "AXTitle").unwrap_or_default()
            );
            walk(&item, 2, &mut budget, &mut out);
        }
    }
    for window in children(&app, "AXWindows") {
        let _ = writeln!(
            out,
            "[ウィンドウ {:?}]",
            text(&window, "AXTitle").unwrap_or_default()
        );
        walk(&window, 1, &mut budget, &mut out);
    }
    if budget == 0 {
        let _ = writeln!(out, "(部品が多いため途中で打ち切り)");
    }
    out
}
