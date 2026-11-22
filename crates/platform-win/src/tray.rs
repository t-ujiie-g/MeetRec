//! トレイアイコン・メニュー・バルーン通知。

use meetrec_core::config::Mode;
use meetrec_core::platform::{MenuAction, Notice, TrayModel};
use meetrec_core::text;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos,
    GetSystemMetrics, HICON, HMENU, ICONINFO, MENU_ITEM_FLAGS, MF_CHECKED, MF_GRAYED, MF_POPUP,
    MF_SEPARATOR, MF_STRING, PostMessageW, SM_CXSMICON, SetForegroundWindow, TPM_BOTTOMALIGN,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WM_NULL,
};
use windows::core::PCWSTR;

use crate::sys::{copy_wide, wide};

const ICON_ID: u32 = 1;

const ID_RECORD: usize = 1;
const ID_STOP: usize = 2;
const ID_OPEN: usize = 3;
const ID_AUTOSTART: usize = 4;
const ID_DIAG: usize = 5;
const ID_QUIT: usize = 6;
const ID_FOLDER: usize = 7;
const ID_MIC: usize = 8;
const ID_MODE_ASK: usize = 10;
const ID_MODE_AUTO: usize = 11;
const ID_MODE_OFF: usize = 12;

pub struct Tray {
    hwnd: HWND,
    callback_msg: u32,
    icon_idle: HICON,
    icon_rec: HICON,
    pub model: TrayModel,
    added: bool,
}

/// 円（録画中）または輪（待機中）を 4x4 スーパーサンプリングで描いたアイコンを作る。
fn make_icon(recording: bool, size: i32) -> HICON {
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let Ok(color) = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            return HICON::default();
        };
        if bits.is_null() {
            return HICON::default();
        }
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, (size * size) as usize);
        let c = size as f32 / 2.0;
        let r_out = size as f32 * 0.44;
        let r_in = if recording { 0.0 } else { size as f32 * 0.27 };
        // 録画中＝赤、待機中＝明暗どちらのタスクバーでも見えるグレー。
        let (r, g, b) = if recording {
            (232u32, 17u32, 35u32)
        } else {
            (140u32, 140u32, 140u32)
        };
        for y in 0..size {
            for x in 0..size {
                let mut hit = 0u32;
                for sy in 0..4 {
                    for sx in 0..4 {
                        let dx = x as f32 + (sx as f32 + 0.5) / 4.0 - c;
                        let dy = y as f32 + (sy as f32 + 0.5) / 4.0 - c;
                        let d = (dx * dx + dy * dy).sqrt();
                        if d <= r_out && d >= r_in {
                            hit += 1;
                        }
                    }
                }
                let a = hit * 255 / 16;
                // 乗算済みアルファの BGRA。
                px[(y * size + x) as usize] =
                    (a << 24) | ((r * a / 255) << 16) | ((g * a / 255) << 8) | (b * a / 255);
            }
        }
        let mask = CreateBitmap(size, size, 1, 1, None);
        let info = ICONINFO {
            fIcon: true.into(),
            hbmMask: mask,
            hbmColor: color,
            ..Default::default()
        };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(mask.into());
        let _ = DeleteObject(color.into());
        icon
    }
}

impl Tray {
    pub fn new(hwnd: HWND, callback_msg: u32) -> Tray {
        let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64);
        let mut tray = Tray {
            hwnd,
            callback_msg,
            icon_idle: make_icon(false, size),
            icon_rec: make_icon(true, size),
            model: TrayModel {
                recording: false,
                status: text::STATUS_IDLE.into(),
                mode: Mode::Ask,
                autostart: false,
                mic: true,
            },
            added: false,
        };
        tray.add();
        tray
    }

    fn data(&self) -> NOTIFYICONDATAW {
        let mut d = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: ICON_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: self.callback_msg,
            hIcon: if self.model.recording {
                self.icon_rec
            } else {
                self.icon_idle
            },
            ..Default::default()
        };
        copy_wide(
            &mut d.szTip,
            &format!("{} - {}", text::APP_NAME, self.model.status),
        );
        d
    }

    /// アイコンを登録する（エクスプローラー再起動時にも呼ぶ）。
    pub fn add(&mut self) {
        let mut d = self.data();
        unsafe {
            self.added = Shell_NotifyIconW(NIM_ADD, &d).as_bool();
            d.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &d);
        }
    }

    pub fn set(&mut self, model: &TrayModel) {
        self.model = model.clone();
        let d = self.data();
        unsafe {
            if !self.added || !Shell_NotifyIconW(NIM_MODIFY, &d).as_bool() {
                self.add();
            }
        }
    }

    pub fn notify(&mut self, notice: &Notice) {
        let mut d = self.data();
        d.uFlags |= NIF_INFO;
        d.dwInfoFlags = NIIF_INFO;
        copy_wide(&mut d.szInfoTitle, &notice.title);
        copy_wide(&mut d.szInfo, &notice.body);
        unsafe {
            if !Shell_NotifyIconW(NIM_MODIFY, &d).as_bool() {
                self.add();
                let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
            }
        }
    }

    pub fn remove(&mut self) {
        if self.added {
            let d = self.data();
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &d);
            }
            self.added = false;
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
        unsafe {
            let _ = DestroyIcon(self.icon_idle);
            let _ = DestroyIcon(self.icon_rec);
        }
    }
}

unsafe fn item(menu: HMENU, flags: MENU_ITEM_FLAGS, id: usize, label: &str) {
    unsafe {
        let label = wide(label);
        let _ = AppendMenuW(menu, flags, id, PCWSTR(label.as_ptr()));
    }
}

/// メニューを表示し、選ばれた項目を返す（モーダル。呼び出し中に `MAIN` を借用しないこと）。
pub fn show_menu(hwnd: HWND, model: &TrayModel) -> Option<MenuAction> {
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        let modes = CreatePopupMenu().ok()?;
        item(menu, MF_STRING | MF_GRAYED, 0, &model.status);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        if model.recording {
            item(menu, MF_STRING, ID_STOP, text::MENU_STOP);
        } else {
            item(menu, MF_STRING, ID_RECORD, text::MENU_RECORD_NOW);
        }
        item(menu, MF_STRING, ID_OPEN, text::MENU_OPEN_FOLDER);
        item(menu, MF_STRING, ID_FOLDER, text::MENU_CHOOSE_FOLDER);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        for (id, mode) in [
            (ID_MODE_ASK, Mode::Ask),
            (ID_MODE_AUTO, Mode::Auto),
            (ID_MODE_OFF, Mode::Off),
        ] {
            let check = if model.mode == mode {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            item(modes, MF_STRING | check, id, text::mode_label(mode));
        }
        item(menu, MF_POPUP, modes.0 as usize, text::MENU_MODE);
        let checked = |on: bool| if on { MF_CHECKED } else { MENU_ITEM_FLAGS(0) };
        item(menu, MF_STRING | checked(model.mic), ID_MIC, text::MENU_MIC);
        item(
            menu,
            MF_STRING | checked(model.autostart),
            ID_AUTOSTART,
            text::MENU_AUTOSTART,
        );
        item(menu, MF_STRING, ID_DIAG, text::MENU_DIAGNOSTICS);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        item(menu, MF_STRING, ID_QUIT, text::MENU_QUIT);

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // メニュー外クリックで閉じるようにするための定石。
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, Default::default(), Default::default());
        // サブメニューは親と一緒に破棄される。
        let _ = DestroyMenu(menu);
        match cmd.0 as usize {
            ID_RECORD => Some(MenuAction::RecordNow),
            ID_STOP => Some(MenuAction::StopNow),
            ID_OPEN => Some(MenuAction::OpenFolder),
            ID_FOLDER => Some(MenuAction::ChooseFolder),
            ID_AUTOSTART => Some(MenuAction::ToggleAutostart),
            ID_MIC => Some(MenuAction::ToggleMic),
            ID_DIAG => Some(MenuAction::SaveDiagnostics),
            ID_QUIT => Some(MenuAction::Quit),
            ID_MODE_ASK => Some(MenuAction::SetMode(Mode::Ask)),
            ID_MODE_AUTO => Some(MenuAction::SetMode(Mode::Auto)),
            ID_MODE_OFF => Some(MenuAction::SetMode(Mode::Off)),
            _ => None,
        }
    }
}
