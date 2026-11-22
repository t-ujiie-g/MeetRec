//! UI オートメーションで、会議アプリのウィンドウにある押せる部品（ボタンなど）を読む。
//! 会議アプリのミュート状態を知るために使う。読むのは部品の種類・名前・状態だけで、操作はしない。

use std::fmt::Write as _;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCondition, TreeScope_Descendants,
    UIA_ButtonControlTypeId, UIA_CONTROLTYPE_ID, UIA_CheckBoxControlTypeId,
    UIA_ControlTypePropertyId, UIA_MenuItemControlTypeId, UIA_ToggleToggleStatePropertyId,
};
use windows::core::Result as WinResult;

/// 1 ウィンドウぶんの書き出しで出す部品の上限。
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
