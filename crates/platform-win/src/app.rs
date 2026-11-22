//! メインスレッド：非表示ウィンドウのメッセージループ、イベント配送、`Platform` 実装。
//!
//! 再入対策：`dispatch` はイベントをキューに積み、`MAIN` を借用できたときだけ処理する。
//! `Platform` メソッドの実行中（＝借用中）に届いたイベントは、外側の処理が終わってから順に流れる。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use meetrec_core::platform::{
    App, LocalTime, MicUser, Notice, Permissions, Platform, PromptAnswer, PromptModel, RawUsage,
    RecordRequest, RecorderEvent, RecordingSummary, TrayModel, UiEvent, WindowInfo,
};
use meetrec_core::text;
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, FILETIME, GetLastError, HINSTANCE,
    HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, GetProcessTimes};
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem,
    NIN_BALLOONUSERCLICK, NIN_SELECT, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
    ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, IDYES,
    KillTimer, MB_ICONINFORMATION, MB_SETFOREGROUND, MB_YESNO, MSG, MessageBoxW,
    PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PostMessageW, PostQuitMessage, RegisterClassExW,
    RegisterWindowMessageW, SW_SHOWNORMAL, SetCoalescableTimer, TranslateMessage, WINDOW_EX_STYLE,
    WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_ENDSESSION, WM_POWERBROADCAST,
    WM_QUERYENDSESSION, WM_TIMER, WNDCLASSEXW, WS_OVERLAPPED,
};
use windows::core::{PCWSTR, w};

use crate::probe::{self, Probe};
use crate::prompt::Prompt;
use crate::recorder::Recorder;
use crate::sys::{self, wide};
use crate::tray::{self, Tray};

const WM_TRAY: u32 = WM_APP + 1;
const WM_PROMPT_ANSWER: u32 = WM_APP + 2;
const WM_RECORDER: u32 = WM_APP + 3;
const WM_PERMISSIONS: u32 = WM_APP + 4;
/// NIN_KEYSELECT（キーボードでのアイコン選択）。
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;

const TIMER_TICK: usize = 1;
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("MeetRec");

struct Main {
    app: Box<dyn App>,
    plat: WinPlatform,
}

thread_local! {
    static MAIN: RefCell<Option<Main>> = const { RefCell::new(None) };
    static QUEUE: RefCell<VecDeque<UiEvent>> = const { RefCell::new(VecDeque::new()) };
    static TASKBAR_CREATED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// イベントを app に渡す（再入時はキューに積むだけ）。
fn dispatch(ev: UiEvent) {
    QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        // 処理が詰まっている間のティックは 1 つにまとめる。
        if !(ev == UiEvent::Tick && q.back() == Some(&UiEvent::Tick)) {
            q.push_back(ev);
        }
    });
    MAIN.with(|m| {
        let Ok(mut guard) = m.try_borrow_mut() else {
            return;
        };
        let Some(main) = guard.as_mut() else { return };
        while let Some(ev) = QUEUE.with(|q| q.borrow_mut().pop_front()) {
            main.app.on_event(&mut main.plat, ev);
        }
    });
}

/// ワーカースレッドからメインスレッドへ送る録画イベント。
/// ウィンドウメッセージは他プロセスからも送れるので、メッセージにはポインタを載せず
/// 「キューを見よ」という合図だけにする（偽のメッセージで不正なアドレスを触らせない）。
static RECORDER_EVENTS: Mutex<VecDeque<RecorderEvent>> = Mutex::new(VecDeque::new());

/// ワーカースレッドからメインスレッドへ録画イベントを送る。
pub fn post_recorder_event(hwnd: isize, event: RecorderEvent) {
    RECORDER_EVENTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push_back(event);
    unsafe {
        let _ = PostMessageW(
            Some(HWND(hwnd as *mut _)),
            WM_RECORDER,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

/// 処理中はロックを持たない（イベント処理の中から次のイベントが積まれることがある）。
fn next_recorder_event() -> Option<RecorderEvent> {
    RECORDER_EVENTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .pop_front()
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER if wparam.0 == TIMER_TICK => {
                dispatch(UiEvent::Tick);
                LRESULT(0)
            }
            WM_TRAY => {
                // NOTIFYICON_VERSION_4：イベント種別は lParam の下位ワード。
                match (lparam.0 as u32) & 0xffff {
                    WM_CONTEXTMENU | NIN_SELECT | NIN_KEYSELECT => {
                        let model = MAIN.with(|m| {
                            m.try_borrow()
                                .ok()
                                .and_then(|g| g.as_ref().map(|x| x.plat.tray.model.clone()))
                        });
                        if let Some(model) = model
                            && let Some(action) = tray::show_menu(hwnd, &model)
                        {
                            dispatch(UiEvent::Menu(action));
                        }
                    }
                    NIN_BALLOONUSERCLICK => {
                        let path = MAIN.with(|m| {
                            m.try_borrow()
                                .ok()
                                .and_then(|g| g.as_ref().and_then(|x| x.plat.last_open.clone()))
                        });
                        if let Some(path) = path {
                            open_path(&path);
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_PROMPT_ANSWER => {
                let answer = match wparam.0 {
                    1 => PromptAnswer::Record {
                        always: lparam.0 != 0,
                    },
                    _ => PromptAnswer::Skip,
                };
                dispatch(UiEvent::Prompt(answer));
                LRESULT(0)
            }
            WM_RECORDER => {
                while let Some(event) = next_recorder_event() {
                    dispatch(UiEvent::Recorder(event));
                }
                LRESULT(0)
            }
            WM_PERMISSIONS => {
                let text = wide(
                    "マイクへのアクセスが Windows のプライバシー設定で許可されていません。\n\
                 自分の声を録音するには「デスクトップ アプリにマイクへのアクセスを許可する」をオンにしてください。\n\n\
                 設定を開きますか？",
                );
                let title = wide(text::APP_NAME);
                let r = MessageBoxW(
                    Some(hwnd),
                    PCWSTR(text.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    MB_YESNO | MB_ICONINFORMATION | MB_SETFOREGROUND,
                );
                if r == IDYES {
                    ShellExecuteW(
                        None,
                        w!("open"),
                        w!("ms-settings:privacy-microphone"),
                        None,
                        None,
                        SW_SHOWNORMAL,
                    );
                }
                LRESULT(0)
            }
            WM_POWERBROADCAST => {
                match wparam.0 as u32 {
                    PBT_APMSUSPEND => dispatch(UiEvent::WillSleep),
                    PBT_APMRESUMEAUTOMATIC => dispatch(UiEvent::DidWake),
                    _ => {}
                }
                LRESULT(1)
            }
            WM_QUERYENDSESSION => LRESULT(1),
            WM_ENDSESSION => {
                if wparam.0 != 0 {
                    // ログアウト・シャットダウン：戻る前に同期的に確定する。
                    dispatch(UiEvent::Shutdown);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                // taskkill（/F なし）やインストーラ等からの終了要求：録画を確定してから閉じる。
                dispatch(UiEvent::Shutdown);
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => {
                let created = TASKBAR_CREATED.with(|c| c.get());
                if created != 0 && msg == created {
                    // エクスプローラーが再起動した：アイコンを登録し直す。
                    MAIN.with(|m| {
                        if let Ok(mut g) = m.try_borrow_mut()
                            && let Some(main) = g.as_mut()
                        {
                            main.plat.tray.add();
                        }
                    });
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
    }
}

fn open_path(path: &Path) {
    let p = wide(&path.to_string_lossy());
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(p.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

pub fn run(app: Box<dyn App>) {
    unsafe {
        // 多重起動防止。
        let _mutex = CreateMutexW(None, false, w!("Local\\MeetRec.SingleInstance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            meetrec_core::info!("既に起動しているため終了します");
            std::process::exit(0);
        }
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let inst = GetModuleHandleW(None)
            .map(|m| HINSTANCE(m.0))
            .unwrap_or_default();
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: inst,
            lpszClassName: w!("MeetRecMain"),
            ..Default::default()
        };
        RegisterClassExW(&class);
        // メッセージ専用ウィンドウ（HWND_MESSAGE）は電源・セッション終了・TaskbarCreated の
        // ブロードキャストを受け取れないので、表示しない通常のトップレベルウィンドウにする。
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("MeetRecMain"),
            w!("MeetRec"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(inst),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                meetrec_core::warn!("メインウィンドウを作成できません: {e}");
                std::process::exit(1);
            }
        };
        TASKBAR_CREATED.with(|c| c.set(RegisterWindowMessageW(w!("TaskbarCreated"))));

        let tick_ms = app.tick_seconds().max(2) * 1000;
        let mut plat = WinPlatform {
            hwnd,
            tray: Tray::new(hwnd, WM_TRAY),
            prompt: Prompt::new(hwnd, WM_PROMPT_ANSWER),
            probe: Probe::new(),
            recorder: None,
            last_open: None,
        };
        // 「ログイン時に起動」が有効なら、今の実行ファイルの場所で登録し直す。置き場所を変えた・
        // インストーラで入れ直した場合に、古い場所を指したままにしない。
        if plat.autostart_enabled()
            && let Err(e) = plat.set_autostart(true)
        {
            meetrec_core::warn!("ログイン時の起動を登録し直せません: {e}");
        }
        MAIN.with(|m| *m.borrow_mut() = Some(Main { app, plat }));
        dispatch(UiEvent::Launched);
        // 許容遅延を付けて他のタイマーとまとめてもらう（省電力）。
        SetCoalescableTimer(Some(hwnd), TIMER_TICK, tick_ms, None, tick_ms / 4);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        // どの経路でループを抜けても、録画中なら必ず確定してから終了する。
        MAIN.with(|m| {
            if let Some(main) = m.borrow_mut().as_mut()
                && let Some(summary) = main.plat.stop_recording()
            {
                meetrec_core::info!("終了時に録画を確定しました: {summary:?}");
            }
        });
        // トレイアイコン等を片付けてから終了する。
        MAIN.with(|m| m.borrow_mut().take());
    }
    std::process::exit(0);
}

pub struct WinPlatform {
    hwnd: HWND,
    tray: Tray,
    prompt: Prompt,
    probe: Probe,
    recorder: Option<Recorder>,
    last_open: Option<PathBuf>,
}

fn filetime_100ns(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// 自分の実行ファイルパスを引用符付きで返す（Run キー用）。
fn quoted_exe() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\"", exe.display()))
}

impl Platform for WinPlatform {
    fn mic_users(&mut self) -> Vec<MicUser> {
        self.probe.mic_users()
    }

    fn windows(&mut self) -> Vec<WindowInfo> {
        self.probe.windows()
    }

    fn local_time(&self) -> LocalTime {
        let t = unsafe { GetLocalTime() };
        LocalTime {
            year: t.wYear,
            month: t.wMonth as u8,
            day: t.wDay as u8,
            hour: t.wHour as u8,
            minute: t.wMinute as u8,
            second: t.wSecond as u8,
        }
    }

    fn now_ns(&self) -> u64 {
        sys::qpc_ns()
    }

    fn free_space(&self, dir: &Path) -> Option<u64> {
        let p = wide(&dir.to_string_lossy());
        let mut free = 0u64;
        unsafe { GetDiskFreeSpaceExW(PCWSTR(p.as_ptr()), Some(&mut free), None, None).ok()? };
        Some(free)
    }

    fn resource_usage(&self) -> Option<RawUsage> {
        unsafe {
            let process = GetCurrentProcess();
            let mut mem = PROCESS_MEMORY_COUNTERS::default();
            GetProcessMemoryInfo(process, &mut mem, std::mem::size_of_val(&mem) as u32).ok()?;
            let (mut c, mut e, mut k, mut u) = Default::default();
            GetProcessTimes(process, &mut c, &mut e, &mut k, &mut u).ok()?;
            Some(RawUsage {
                rss_bytes: mem.WorkingSetSize as u64,
                cpu_time_ns: (filetime_100ns(k) + filetime_100ns(u)) * 100,
            })
        }
    }

    fn diagnostics(&mut self) -> String {
        let mut s = self.probe.diagnostics();
        if let Some(r) = &self.recorder {
            s.push_str(&r.diagnostics());
        }
        s
    }

    fn set_tray(&mut self, model: &TrayModel) {
        self.tray.set(model);
    }

    fn show_prompt(&mut self, prompt: &PromptModel) {
        self.prompt.show(prompt);
    }

    fn hide_prompt(&mut self) {
        self.prompt.hide();
    }

    fn notify(&mut self, notice: &Notice) {
        self.last_open = notice.open.clone();
        self.tray.notify(notice);
    }

    fn open_path(&mut self, path: &Path) {
        open_path(path);
    }

    fn choose_folder(&mut self, current: &Path) -> Option<PathBuf> {
        // SAFETY: メインスレッドは STA で COM を初期化済み。文字列は呼び出しの間だけ生きていればよい。
        unsafe {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            let options = dialog.GetOptions().ok()?;
            dialog
                .SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
                .ok()?;
            let title = wide(text::CHOOSE_FOLDER_TITLE);
            let _ = dialog.SetTitle(PCWSTR(title.as_ptr()));
            let start = wide(&current.to_string_lossy());
            if let Ok(item) =
                SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(start.as_ptr()), None)
            {
                let _ = dialog.SetFolder(&item);
            }
            // キャンセルはエラー（ERROR_CANCELLED）として返る。
            dialog.Show(Some(self.hwnd)).ok()?;
            let name = dialog
                .GetResult()
                .ok()?
                .GetDisplayName(SIGDN_FILESYSPATH)
                .ok()?;
            let path = name.to_string().ok().map(PathBuf::from);
            CoTaskMemFree(Some(name.0 as *const _));
            path
        }
    }

    fn set_autostart(&mut self, enabled: bool) -> Result<(), String> {
        unsafe {
            let mut key = HKEY::default();
            let err = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut key,
                None,
            );
            if err != ERROR_SUCCESS {
                return Err(format!("レジストリを開けません ({})", err.0));
            }
            let result = if enabled {
                match quoted_exe() {
                    Some(exe) => {
                        let data = wide(&exe);
                        let bytes =
                            std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2);
                        RegSetValueExW(key, RUN_VALUE, None, REG_SZ, Some(bytes))
                    }
                    None => {
                        let _ = RegCloseKey(key);
                        return Err("実行ファイルのパスを取得できません".into());
                    }
                }
            } else {
                let e = RegDeleteValueW(key, RUN_VALUE);
                // もともと無い場合は成功扱い。
                if e == ERROR_FILE_NOT_FOUND {
                    ERROR_SUCCESS
                } else {
                    e
                }
            };
            let _ = RegCloseKey(key);
            if result == ERROR_SUCCESS {
                Ok(())
            } else {
                Err(format!("レジストリに書き込めません ({})", result.0))
            }
        }
    }

    fn autostart_enabled(&self) -> bool {
        unsafe {
            let mut key = HKEY::default();
            if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_QUERY_VALUE, &mut key)
                != ERROR_SUCCESS
            {
                return false;
            }
            let found = RegQueryValueExW(key, RUN_VALUE, None, None, None, None) == ERROR_SUCCESS;
            let _ = RegCloseKey(key);
            found
        }
    }

    fn permissions(&mut self) -> Permissions {
        // Windows には画面収録の事前許可はない。マイクはプライバシー設定のみ確認する。
        Permissions {
            screen: true,
            microphone: probe::microphone_allowed(),
        }
    }

    fn request_permissions(&mut self, missing: Permissions) {
        if missing.microphone {
            // モーダルダイアログは on_event の外で出す（イベント処理を止めない）。
            unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_PERMISSIONS, WPARAM(0), LPARAM(0));
            }
        }
    }

    fn start_recording(&mut self, req: &RecordRequest) -> Result<(), String> {
        if self.recorder.is_some() {
            return Err("既に録画中です".into());
        }
        self.recorder = Some(Recorder::start(req, self.hwnd.0 as isize)?);
        Ok(())
    }

    fn retarget(&mut self, window: &WindowInfo) {
        if let Some(r) = &self.recorder {
            r.retarget(window);
        }
    }

    fn retarget_screen(&mut self, near: &WindowInfo) -> bool {
        self.recorder.as_ref().is_some_and(|r| r.show_screen(near))
    }

    fn end_screen(&mut self) {
        if let Some(r) = &self.recorder {
            r.hide_screen();
        }
    }

    fn stop_recording(&mut self) -> Option<RecordingSummary> {
        self.recorder.take().map(|r| r.stop())
    }

    fn quit(&mut self) {
        self.prompt.hide();
        self.tray.remove();
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_TICK);
            // WM_DESTROY → PostQuitMessage でメッセージループを抜ける。
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
