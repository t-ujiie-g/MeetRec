//! UI オートメーションで、会議アプリのウィンドウにある押せる部品（ボタンなど）を読む。
//! 会議アプリのミュート状態を知るために使う。読むのは部品の種類・名前・状態だけで、操作はしない。

use std::fmt::Write as _;

use meetrec_core::platform::CONTROL_SEARCH_RETRY_NS;
use meetrec_core::rules::starts_with_ci;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCondition, IUIAutomationElement,
    TreeScope_Descendants, UIA_ButtonControlTypeId, UIA_CONTROLTYPE_ID, UIA_CheckBoxControlTypeId,
    UIA_ControlTypePropertyId, UIA_MenuItemControlTypeId, UIA_ToggleToggleStatePropertyId,
};
use windows::core::Result as WinResult;

use crate::sys;

/// 1 ウィンドウぶんで調べる部品の上限。
const MAX_CONTROLS: i32 = 400;
/// 書き出す部品の種類（押せるもの・選べるものだけ。本文やチャットの文字は出さない）。
const CONTROL_TYPES: [UIA_CONTROLTYPE_ID; 3] = [
    UIA_ButtonControlTypeId,
    UIA_CheckBoxControlTypeId,
    UIA_MenuItemControlTypeId,
];

/// 「`CONTROL_TYPES` のどれか」という検索条件を作る。
unsafe fn control_condition(uia: &IUIAutomation) -> WinResult<IUIAutomationCondition> {
    unsafe {
        let of = |kind: UIA_CONTROLTYPE_ID| {
            uia.CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(kind.0))
        };
        let mut condition = of(CONTROL_TYPES[0])?;
        for kind in &CONTROL_TYPES[1..] {
            condition = uia.CreateOrCondition(&condition, &of(*kind)?)?;
        }
        Ok(condition)
    }
}

unsafe fn dump_window(uia: &IUIAutomation, hwnd: isize, out: &mut String) -> WinResult<()> {
    unsafe {
        let root = uia.ElementFromHandle(HWND(hwnd as *mut _))?;
        let found = root.FindAll(TreeScope_Descendants, &control_condition(uia)?)?;
        let count = found.Length()?;
        for i in 0..count.min(MAX_CONTROLS) {
            let element = found.GetElement(i)?;
            // ToggleState：0 = オフ、1 = オン、2 = 不定。対応しない部品は値が入らない。
            let toggle = element
                .GetCurrentPropertyValue(UIA_ToggleToggleStatePropertyId)
                .ok()
                .and_then(|v| i32::try_from(&v).ok());
            let _ = writeln!(
                out,
                " type={} name={:?} id={:?} toggle={toggle:?}",
                element.CurrentControlType().map_or(0, |t| t.0),
                element.CurrentName().unwrap_or_default().to_string(),
                element
                    .CurrentAutomationId()
                    .unwrap_or_default()
                    .to_string(),
            );
        }
        if count > MAX_CONTROLS {
            let _ = writeln!(
                out,
                " (部品が多いため {MAX_CONTROLS} 個で打ち切り。全 {count} 個)"
            );
        }
        Ok(())
    }
}

/// 会議アプリのミュートの部品を見張る。見つけた部品を覚えておき、次からは名前を読み直すだけにする
/// （毎回ウィンドウ全体を探すと重い）。メインスレッド（COM 初期化済み）で使う。
#[derive(Default)]
pub struct ControlWatch {
    pid: u32,
    uia: Option<IUIAutomation>,
    found: Option<IUIAutomationElement>,
    /// 直前の探索が空振りだった時刻。しばらくは探し直さない。
    missed_ns: Option<u64>,
}

impl ControlWatch {
    /// 名前が `names` のどれかで始まる部品を探し、今の名前を返す。`windows` は、探し直しが必要に
    /// なったときだけ呼ばれ、会議アプリのウィンドウ（HWND）を返す。
    pub fn find(
        &mut self,
        pid: u32,
        names: &[String],
        windows: impl FnOnce() -> Vec<isize>,
    ) -> Option<String> {
        if self.pid != pid {
            *self = ControlWatch {
                pid,
                uia: self.uia.take(),
                ..Default::default()
            };
        }
        let matching = |name: String| {
            names
                .iter()
                .any(|n| starts_with_ci(&name, n))
                .then_some(name)
        };
        // 覚えている部品の名前が変わっただけなら、読み直すだけで済む（ミュートの切り替え）。
        if let Some(found) = &self.found {
            // SAFETY: 取得済みの部品の名前を読むだけ。無効になっていればエラーが返る。
            let name = unsafe { found.CurrentName() }.ok().map(|n| n.to_string());
            if let Some(name) = name.and_then(matching) {
                return Some(name);
            }
            self.found = None;
        } else if self
            .missed_ns
            .is_some_and(|t| sys::qpc_ns().saturating_sub(t) < CONTROL_SEARCH_RETRY_NS)
        {
            return None;
        }
        // SAFETY: COM は初期化済み。HWND が無効になっていれば各呼び出しがエラーを返す。
        let found = unsafe {
            if self.uia.is_none() {
                self.uia = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok();
            }
            let uia = self.uia.as_ref()?;
            windows()
                .into_iter()
                .find_map(|hwnd| search_window(uia, hwnd, &matching).ok().flatten())
        };
        match found {
            Some((element, name)) => {
                self.found = Some(element);
                self.missed_ns = None;
                Some(name)
            }
            None => {
                self.missed_ns = Some(sys::qpc_ns());
                None
            }
        }
    }
}

/// ウィンドウの中から、`matching` が名前を返す最初の部品を探す。
unsafe fn search_window(
    uia: &IUIAutomation,
    hwnd: isize,
    matching: &impl Fn(String) -> Option<String>,
) -> WinResult<Option<(IUIAutomationElement, String)>> {
    unsafe {
        let root = uia.ElementFromHandle(HWND(hwnd as *mut _))?;
        let found = root.FindAll(TreeScope_Descendants, &control_condition(uia)?)?;
        for i in 0..found.Length()?.min(MAX_CONTROLS) {
            let element = found.GetElement(i)?;
            let name = element.CurrentName().unwrap_or_default().to_string();
            if let Some(name) = matching(name) {
                return Ok(Some((element, name)));
            }
        }
        Ok(None)
    }
}

/// 実機調査用：ウィンドウ（HWND と見出し）ごとに、押せる部品の一覧を書き出す。
/// メインスレッド（COM 初期化済み）から呼ぶこと。
pub fn dump_controls(windows: &[(isize, String)]) -> String {
    let mut out = String::new();
    // SAFETY: COM は初期化済み。HWND が無効になっていれば各呼び出しがエラーを返す。
    unsafe {
        let uia: IUIAutomation = match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
        {
            Ok(uia) => uia,
            Err(e) => {
                let _ = writeln!(out, "(UI オートメーションを使えません: {e})");
                return out;
            }
        };
        for (hwnd, label) in windows {
            let _ = writeln!(out, "[ウィンドウ {label}]");
            if let Err(e) = dump_window(&uia, *hwnd, &mut out) {
                let _ = writeln!(out, " (読めません: {e})");
            }
        }
    }
    out
}
