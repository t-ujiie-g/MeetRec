//! 確認ポップアップ：画面右下に出す小さな最前面ウィンドウ（フォーカスは奪わない）。
//! 回答はメインウィンドウへ `answer_msg`（wParam: 1=録画する, 2=今回はしない / lParam: チェック）で送る。

use meetrec_core::platform::PromptModel;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    COLOR_BTNFACE, CreateFontIndirectW, DeleteObject, HBRUSH, HFONT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::{
    BM_GETCHECK, BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CreateWindowExW, DefWindowProcW,
    DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW, HMENU, IDC_ARROW, LoadCursorW,
    NONCLIENTMETRICSW, PostMessageW, RegisterClassExW, SPI_GETNONCLIENTMETRICS, SPI_GETWORKAREA,
    SW_SHOWNOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SendMessageW, SetWindowLongPtrW,
    ShowWindow, SystemParametersInfoW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND,
    WM_SETFONT, WNDCLASSEXW, WS_BORDER, WS_CHILD, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP, WS_TABSTOP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

use crate::sys::wide;

const CLASS: PCWSTR = w!("MeetRecPrompt");
const ID_RECORD: isize = 1;
const ID_SKIP: isize = 2;
const ID_ALWAYS: isize = 3;

pub struct Prompt {
    main: HWND,
    answer_msg: u32,
    hwnd: Option<HWND>,
    font: HFONT,
    registered: bool,
}

/// ポップアップが覚えておく値（GWLP_USERDATA に入れる）。
struct Ctx {
    main: HWND,
    answer_msg: u32,
    checkbox: HWND,
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let id = (wparam.0 & 0xffff) as isize;
                let ctx = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Ctx;
                if !ctx.is_null() && (id == ID_RECORD || id == ID_SKIP) {
                    let ctx = &*ctx;
                    let checked = SendMessageW(ctx.checkbox, BM_GETCHECK, None, None).0 == 1;
                    let _ = PostMessageW(
                        Some(ctx.main),
                        ctx.answer_msg,
                        WPARAM(id as usize),
                        LPARAM(checked as isize),
                    );
                }
                LRESULT(0)
            }
            // Alt+F4 等では閉じない（答えるまで消さない）。
            WM_CLOSE => LRESULT(0),
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

impl Prompt {
    pub fn new(main: HWND, answer_msg: u32) -> Prompt {
        Prompt {
            main,
            answer_msg,
            hwnd: None,
            font: HFONT::default(),
            registered: false,
        }
    }

    fn instance() -> HINSTANCE {
        unsafe {
            GetModuleHandleW(None)
                .map(|m| HINSTANCE(m.0))
                .unwrap_or_default()
        }
    }

    pub fn show(&mut self, model: &PromptModel) {
        self.hide();
        unsafe {
            let inst = Self::instance();
            if !self.registered {
                let class = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    lpfnWndProc: Some(wndproc),
                    hInstance: inst,
                    hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                    hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut _),
                    lpszClassName: CLASS,
                    ..Default::default()
                };
                RegisterClassExW(&class);
                self.registered = true;
            }
            let dpi = GetDpiForSystem().max(96) as i32;
            let s = |v: i32| v * dpi / 96;

            // UI フォント（メッセージボックスと同じもの）。
            let mut ncm = NONCLIENTMETRICSW {
                cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
                ..Default::default()
            };
            if SystemParametersInfoW(
                SPI_GETNONCLIENTMETRICS,
                ncm.cbSize,
                Some(&mut ncm as *mut _ as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .is_ok()
            {
                self.font = CreateFontIndirectW(&ncm.lfMessageFont);
            }

            let (w, h) = (s(380), s(128));
            let mut work = RECT::default();
            let _ = SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some(&mut work as *mut _ as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
            if work.right <= work.left {
                work = RECT {
                    left: 0,
                    top: 0,
                    right: s(1280),
                    bottom: s(720),
                };
            }
            let title = wide(meetrec_core::text::APP_NAME);
            let Ok(hwnd) = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                CLASS,
                PCWSTR(title.as_ptr()),
                WS_POPUP | WS_BORDER,
                work.right - w - s(16),
                work.bottom - h - s(16),
                w,
                h,
                None,
                None,
                Some(inst),
                None,
            ) else {
                meetrec_core::warn!("確認ポップアップを作成できません");
                return;
            };
            let font = self.font;
            let child = |class: PCWSTR, text: &str, style: u32, x, y, cw, ch, id: isize| -> HWND {
                let text = wide(text);
                let c = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    class,
                    PCWSTR(text.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
                    s(x),
                    s(y),
                    s(cw),
                    s(ch),
                    Some(hwnd),
                    Some(HMENU(id as *mut _)),
                    Some(inst),
                    None,
                )
                .unwrap_or_default();
                if !font.is_invalid() {
                    SendMessageW(
                        c,
                        WM_SETFONT,
                        Some(WPARAM(font.0 as usize)),
                        Some(LPARAM(1)),
                    );
                }
                c
            };
            child(w!("STATIC"), &model.message, 0, 14, 12, 350, 38, 0);
            let checkbox = child(
                w!("BUTTON"),
                &model.always_label,
                BS_AUTOCHECKBOX as u32 | WS_TABSTOP.0,
                14,
                54,
                350,
                22,
                ID_ALWAYS,
            );
            child(
                w!("BUTTON"),
                &model.record_label,
                BS_DEFPUSHBUTTON as u32 | WS_TABSTOP.0,
                134,
                86,
                110,
                30,
                ID_RECORD,
            );
            child(
                w!("BUTTON"),
                &model.skip_label,
                BS_PUSHBUTTON as u32 | WS_TABSTOP.0,
                254,
                86,
                110,
                30,
                ID_SKIP,
            );
            let ctx = Box::new(Ctx {
                main: self.main,
                answer_msg: self.answer_msg,
                checkbox,
            });
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(ctx) as isize);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            self.hwnd = Some(hwnd);
        }
    }

    pub fn hide(&mut self) {
        unsafe {
            if let Some(hwnd) = self.hwnd.take() {
                let ctx = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut Ctx;
                let _ = DestroyWindow(hwnd);
                if !ctx.is_null() {
                    drop(Box::from_raw(ctx));
                }
            }
            if !self.font.is_invalid() {
                let _ = DeleteObject(self.font.into());
                self.font = HFONT::default();
            }
        }
    }
}

impl Drop for Prompt {
    fn drop(&mut self) {
        self.hide();
    }
}
