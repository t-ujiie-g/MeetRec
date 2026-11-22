//! 小さな共通ヘルパ（文字列変換、時計、ハンドル、プロセス情報）。

use std::sync::OnceLock;

use meetrec_core::platform::NS_PER_SEC;
use meetrec_core::session::Clock;
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// NUL 終端の UTF-16 文字列。
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// NUL までを String にする。
pub fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// 固定長バッファへ（切り詰めて）コピーする。
pub fn copy_wide(dst: &mut [u16], s: &str) {
    let mut n = 0;
    for c in s.encode_utf16() {
        if n + 1 >= dst.len() {
            break;
        }
        dst[n] = c;
        n += 1;
    }
    // サロゲートペアの片割れで終わらないようにする。
    if n > 0 && (0xd800..0xdc00).contains(&dst[n - 1]) {
        n -= 1;
    }
    dst[n] = 0;
}

fn qpc_freq() -> u64 {
    static FREQ: OnceLock<u64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
        }
        (f as u64).max(1)
    })
}

/// QPC をナノ秒にする。WGC の SystemRelativeTime・WASAPI の QPC 位置と同じ時計。
pub fn qpc_ns() -> u64 {
    let mut c = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
    }
    (c as u128 * NS_PER_SEC as u128 / qpc_freq() as u128) as u64
}

pub struct QpcClock;

impl Clock for QpcClock {
    fn now_ns(&self) -> u64 {
        qpc_ns()
    }
}

/// ワーカースレッド用：COM を MTA で初期化する（スレッド終了まで保持）。
pub fn com_init_mta() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

/// CloseHandle を保証するラッパ。
pub struct Handle(pub HANDLE);

unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

pub fn base_name(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or(path).to_string()
}

/// PID から実行ファイル名（パスなし）。権限不足などで取れなければ None。
pub fn process_name(pid: u32) -> Option<String> {
    unsafe {
        let h = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?);
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        QueryFullProcessImageNameW(
            h.0,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .ok()?;
        Some(base_name(&String::from_utf16_lossy(&buf[..len as usize])))
    }
}

#[derive(Debug, Clone)]
pub struct ProcEntry {
    pub pid: u32,
    pub parent: u32,
    pub name: String,
}

/// 全プロセスのスナップショット（数 ms かかるので必要なときだけ呼ぶ）。
pub fn snapshot_processes() -> Vec<ProcEntry> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let snap = Handle(snap);
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap.0, &mut e).is_ok() {
            loop {
                out.push(ProcEntry {
                    pid: e.th32ProcessID,
                    parent: e.th32ParentProcessID,
                    name: from_wide(&e.szExeFile),
                });
                if Process32NextW(snap.0, &mut e).is_err() {
                    break;
                }
            }
        }
    }
    out
}
