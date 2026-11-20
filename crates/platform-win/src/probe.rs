//! 観測：マイクを使用中のプロセス、トップレベルウィンドウ、診断情報、権限。

use std::collections::HashMap;
use std::fmt::Write as _;

use meetrec_core::platform::{MicUser, ProcInfo, WindowInfo};
use windows::core::{w, Interface, BOOL, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Media::Audio::{
    eCapture, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetClientRect, GetWindowPlacement, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, WINDOWPLACEMENT,
};

use crate::sys::{self, ProcEntry};

/// 祖先をたどる最大段数（本人を含まない）。
const MAX_ANCESTORS: usize = 5;

pub struct Probe {
    enumerator: Option<IMMDeviceEnumerator>,
    /// 直近のマイク使用 PID 集合と、その祖先チェーン（同じ集合の間は使い回す）。
    chain_cache: (Vec<u32>, Vec<MicUser>),
}

/// (PID, セッション状態) を全キャプチャエンドポイントから集める。
struct CaptureSession {
    pid: u32,
    active: bool,
}

impl Probe {
    pub fn new() -> Probe {
        Probe { enumerator: None, chain_cache: (Vec::new(), Vec::new()) }
    }

    fn enumerator(&mut self) -> Option<IMMDeviceEnumerator> {
        if self.enumerator.is_none() {
            self.enumerator =
                unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok() };
        }
        self.enumerator.clone()
    }

    fn capture_sessions(&mut self) -> Vec<CaptureSession> {
        let mut out = Vec::new();
        let Some(en) = self.enumerator() else { return out };
        unsafe {
            let Ok(devices) = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else {
                // 列挙子が無効になった可能性があるので作り直す。
                self.enumerator = None;
                return out;
            };
            for i in 0..devices.GetCount().unwrap_or(0) {
                let Ok(device) = devices.Item(i) else { continue };
                // セッション一覧を最新にするため、マネージャは毎回取り直す。
                let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                    continue;
                };
                let Ok(sessions) = manager.GetSessionEnumerator() else { continue };
                for n in 0..sessions.GetCount().unwrap_or(0) {
                    let Ok(control) = sessions.GetSession(n) else { continue };
                    let Ok(control2) = control.cast::<IAudioSessionControl2>() else { continue };
                    let pid = control2.GetProcessId().unwrap_or(0);
                    let active = control.GetState().is_ok_and(|s| s == AudioSessionStateActive);
                    out.push(CaptureSession { pid, active });
                }
            }
        }
        out
    }

    /// マイク（キャプチャエンドポイント）でストリームが動いているプロセス。
    pub fn mic_users(&mut self) -> Vec<MicUser> {
        let me = unsafe { GetCurrentProcessId() };
        let mut pids: Vec<u32> = self
            .capture_sessions()
            .iter()
            .filter(|s| s.active && s.pid != 0 && s.pid != me)
            .map(|s| s.pid)
            .collect();
        pids.sort_unstable();
        pids.dedup();
        if pids.is_empty() {
            self.chain_cache = (Vec::new(), Vec::new());
            return Vec::new();
        }
        if self.chain_cache.0 != pids {
            let procs = sys::snapshot_processes();
            let users = pids.iter().map(|pid| MicUser { chain: chain_of(&procs, *pid) }).collect();
            self.chain_cache = (pids, users);
        }
        self.chain_cache.1.clone()
    }

    pub fn windows(&mut self) -> Vec<WindowInfo> {
        struct Ctx {
            out: Vec<WindowInfo>,
            names: HashMap<u32, String>,
        }
        unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let ctx = &mut *(lparam.0 as *mut Ctx);
            if !IsWindowVisible(hwnd).as_bool() {
                return true.into();
            }
            // 別の仮想デスクトップや UWP の待機ウィンドウ（クローク）は除く。
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut u32 as *mut _,
                std::mem::size_of::<u32>() as u32,
            );
            if cloaked != 0 {
                return true.into();
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == 0 {
                return true.into();
            }
            let minimized = IsIconic(hwnd).as_bool();
            let mut rect = RECT::default();
            if minimized {
                let mut wp = WINDOWPLACEMENT {
                    length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                    ..Default::default()
                };
                if GetWindowPlacement(hwnd, &mut wp).is_ok() {
                    rect = wp.rcNormalPosition;
                }
            } else {
                let _ = GetClientRect(hwnd, &mut rect);
            }
            let width = (rect.right - rect.left).max(0) as u32;
            let height = (rect.bottom - rect.top).max(0) as u32;
            if width == 0 || height == 0 {
                return true.into();
            }
            let mut class = [0u16; 256];
            let n = GetClassNameW(hwnd, &mut class).max(0) as usize;
            let class = String::from_utf16_lossy(&class[..n]);
            let mut title = [0u16; 512];
            let n = GetWindowTextW(hwnd, &mut title).max(0) as usize;
            let title = String::from_utf16_lossy(&title[..n]);
            let name = ctx
                .names
                .entry(pid)
                .or_insert_with(|| sys::process_name(pid).unwrap_or_default())
                .clone();
            ctx.out.push(WindowInfo {
                id: hwnd.0 as usize as u64,
                owner: ProcInfo { pid, name, bundle_id: None },
                title,
                class,
                width,
                height,
                minimized,
            });
            true.into()
        }
        let mut ctx = Ctx { out: Vec::new(), names: HashMap::new() };
        unsafe {
            let _ = EnumWindows(Some(each), LPARAM(&mut ctx as *mut Ctx as isize));
        }
        ctx.out
    }

    /// 実機調査用：会議アプリらしきプロセスと、全キャプチャセッション。
    pub fn diagnostics(&mut self) -> String {
        let mut s = String::new();
        let procs = sys::snapshot_processes();
        let _ = writeln!(s, "-- プロセス（名前に zoom / teams / webview を含むもの） --");
        for p in &procs {
            let lower = p.name.to_lowercase();
            if ["zoom", "teams", "webview"].iter().any(|k| lower.contains(k)) {
                let parent = procs.iter().find(|q| q.pid == p.parent).map(|q| q.name.as_str());
                let _ = writeln!(
                    s,
                    "pid={} parent={}({}) name={}",
                    p.pid,
                    p.parent,
                    parent.unwrap_or("?"),
                    p.name
                );
            }
        }
        let _ = writeln!(s, "-- キャプチャデバイスのオーディオセッション --");
        let me = unsafe { GetCurrentProcessId() };
        let sessions = self.capture_sessions();
        if sessions.is_empty() {
            let _ = writeln!(s, "(なし)");
        }
        for c in sessions {
            let name = procs.iter().find(|p| p.pid == c.pid).map(|p| p.name.as_str());
            let _ = writeln!(
                s,
                "pid={} name={} active={}{}",
                c.pid,
                name.unwrap_or("?"),
                c.active,
                if c.pid == me { " (MeetRec 自身)" } else { "" }
            );
        }
        let _ = writeln!(s, "マイクのプライバシー設定: 許可={}", microphone_allowed());
        s
    }
}

/// 本人 → 親 → 祖父…のチェーン。
pub fn chain_of(procs: &[ProcEntry], pid: u32) -> Vec<ProcInfo> {
    let mut chain = Vec::new();
    let mut cur = pid;
    while chain.len() <= MAX_ANCESTORS {
        let Some(p) = procs.iter().find(|p| p.pid == cur) else { break };
        if chain.iter().any(|c: &ProcInfo| c.pid == p.pid) {
            break;
        }
        chain.push(ProcInfo { pid: p.pid, name: p.name.clone(), bundle_id: None });
        if p.parent == 0 || p.parent == p.pid {
            break;
        }
        cur = p.parent;
    }
    if chain.is_empty() {
        // スナップショットにいない（終了直後など）。名前だけでも取る。
        chain.push(ProcInfo {
            pid,
            name: sys::process_name(pid).unwrap_or_default(),
            bundle_id: None,
        });
    }
    chain
}

/// プロセスループバックの対象ツリーの根を決める。
/// 0 や存在しない PID なら `fallback`（ウィンドウ所有プロセス）を使い、
/// 実行ファイル名が同じ祖先が続く限り最上位まで上がる（子 Zoom.exe → 親 Zoom.exe）。
pub fn normalize_root_pid(pid: u32, fallback: Option<u32>) -> u32 {
    let procs = sys::snapshot_processes();
    let exists = |p: u32| p != 0 && procs.iter().any(|e| e.pid == p);
    let mut cur = if exists(pid) {
        pid
    } else {
        match fallback {
            Some(f) if exists(f) => f,
            _ => return 0,
        }
    };
    for _ in 0..16 {
        let Some(me) = procs.iter().find(|e| e.pid == cur) else { break };
        let Some(parent) = procs.iter().find(|e| e.pid == me.parent && e.pid != me.pid) else {
            break;
        };
        if !parent.name.eq_ignore_ascii_case(&me.name) {
            break;
        }
        cur = parent.pid;
    }
    cur
}

fn consent_value(subkey: PCWSTR) -> Option<String> {
    let mut buf = [0u16; 64];
    let mut size = std::mem::size_of_val(&buf) as u32;
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey,
            w!("Value"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    err.is_ok().then(|| sys::from_wide(&buf))
}

/// 設定 > プライバシー > マイク で拒否されていないか。
/// 未検証: ConsentStore のレジストリ値（"Allow"/"Deny"）で判定している。
pub fn microphone_allowed() -> bool {
    let global = consent_value(w!(
        "Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore\\microphone"
    ));
    let desktop = consent_value(w!(
        "Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore\\microphone\\NonPackaged"
    ));
    ![global, desktop].iter().flatten().any(|v| v.eq_ignore_ascii_case("Deny"))
}
