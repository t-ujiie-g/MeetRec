//! libc だけで済む低レベル処理（時計・プロセス情報・ディスク残量・自己計測）。

use std::ffi::CString;
use std::mem::{size_of, zeroed};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use meetrec_core::platform::{LocalTime, RawUsage};

/// mach_absolute_time をナノ秒にしたもの。ScreenCaptureKit のサンプルの PTS
/// （ホスト時計）と同じ時間軸。スリープ中は進まない。
pub fn now_ns() -> u64 {
    // SAFETY: 出力用の構造体を渡すだけ。CLOCK_UPTIME_RAW は mach_absolute_time と同じ時計。
    unsafe {
        let mut ts: libc::timespec = zeroed();
        libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut ts);
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }
}

pub fn local_time() -> LocalTime {
    // SAFETY: time/localtime_r は有効なポインタを渡せば安全。
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = zeroed();
        libc::localtime_r(&t, &mut tm);
        LocalTime {
            year: (tm.tm_year + 1900) as u16,
            month: (tm.tm_mon + 1) as u8,
            day: tm.tm_mday as u8,
            hour: tm.tm_hour as u8,
            minute: tm.tm_min as u8,
            second: tm.tm_sec as u8,
        }
    }
}

/// 利用可能な空き容量（バイト）。ディレクトリが未作成なら親をたどる。
pub fn free_space(dir: &Path) -> Option<u64> {
    let mut p = Some(dir);
    while let Some(path) = p {
        if let Ok(c) = CString::new(path.as_os_str().as_bytes()) {
            // SAFETY: NUL 終端のパスと出力用構造体を渡す。
            unsafe {
                let mut st: libc::statfs = zeroed();
                if libc::statfs(c.as_ptr(), &mut st) == 0 {
                    return Some(st.f_bavail * st.f_bsize as u64);
                }
            }
        }
        p = path.parent();
    }
    None
}

pub fn resource_usage() -> Option<RawUsage> {
    // SAFETY: 自プロセスの情報を出力用構造体に受け取るだけ。
    unsafe {
        let mut info: libc::rusage_info_v2 = zeroed();
        let rc = libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V2,
            &mut info as *mut _ as *mut libc::rusage_info_t,
        );
        if rc != 0 {
            return None;
        }
        let mut ru: libc::rusage = zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut ru) != 0 {
            return None;
        }
        let tv = |t: libc::timeval| t.tv_sec as u64 * 1_000_000_000 + t.tv_usec as u64 * 1000;
        Some(RawUsage {
            // アクティビティモニタの「メモリ」と同じ phys_footprint。
            rss_bytes: info.ri_phys_footprint,
            cpu_time_ns: tv(ru.ru_utime) + tv(ru.ru_stime),
        })
    }
}

pub fn own_pid() -> u32 {
    std::process::id()
}

/// 実行ファイルのパス。取れなければ None（終了済み・権限なし）。
pub fn proc_path(pid: u32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: バッファ長を正しく渡している。
    let n = unsafe { libc::proc_pidpath(pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    String::from_utf8(buf).ok()
}

/// (親 PID, 短いプロセス名)。
pub fn proc_parent_and_name(pid: u32) -> Option<(u32, String)> {
    // SAFETY: 出力用構造体とそのサイズを渡す。
    unsafe {
        let mut info: libc::proc_bsdinfo = zeroed();
        let size = size_of::<libc::proc_bsdinfo>() as i32;
        let n = libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        );
        if n != size {
            return None;
        }
        let raw = if info.pbi_name[0] != 0 { &info.pbi_name[..] } else { &info.pbi_comm[..] };
        let bytes: Vec<u8> = raw.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
        Some((info.pbi_ppid, String::from_utf8_lossy(&bytes).into_owned()))
    }
}

/// 実行ファイル名（パスの末尾）。取れなければ BSD のプロセス名。
pub fn proc_name(pid: u32) -> Option<String> {
    proc_path(pid)
        .and_then(|p| p.rsplit('/').next().map(str::to_string))
        .filter(|n| !n.is_empty())
        .or_else(|| proc_parent_and_name(pid).map(|(_, n)| n))
}

pub fn all_pids() -> Vec<u32> {
    // SAFETY: バッファのバイト数を正しく渡している。
    unsafe {
        let count = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if count <= 0 {
            return Vec::new();
        }
        let mut pids = vec![0i32; count as usize + 64];
        let n = libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * size_of::<i32>()) as i32,
        );
        pids.truncate(n.max(0) as usize);
        pids.into_iter().filter(|p| *p > 0).map(|p| p as u32).collect()
    }
}
