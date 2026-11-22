//! 観測：マイク使用中のプロセス（CoreAudio）とウィンドウ一覧（CGWindowList）。
//! どちらも軽い同期 API で、メインスレッドから呼ばれる。

use std::collections::HashMap;
use std::ffi::c_void;
use std::fmt::Write as _;
use std::mem::size_of;
use std::ptr::NonNull;

use meetrec_core::platform::{MicUser, ProcInfo, WindowInfo};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::NSRunningApplication;
use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectPropertyAddress,
    kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioProcessPropertyBundleID,
    kAudioProcessPropertyIsRunningInput, kAudioProcessPropertyIsRunningOutput,
    kAudioProcessPropertyPID,
};
use objc2_core_foundation::{CFRetained, CFString};
use objc2_core_graphics::{
    CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID, kCGWindowBounds,
    kCGWindowIsOnscreen, kCGWindowLayer, kCGWindowName, kCGWindowNumber, kCGWindowOwnerName,
    kCGWindowOwnerPID,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

use crate::sys;

/// 祖先チェーンの最大段数（本人を含む）。
const MAX_CHAIN: usize = 6;

fn address(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// 固定長のプロパティを読む。
fn audio_prop<T: Copy + Default>(object: u32, selector: u32) -> Option<T> {
    let mut addr = address(selector);
    let mut value = T::default();
    let mut size = size_of::<T>() as u32;
    // SAFETY: T のサイズぶんの出力バッファを渡している。
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast::<c_void>(),
        )
    };
    (status == 0 && size as usize == size_of::<T>()).then_some(value)
}

/// AudioObjectID の配列プロパティを読む。
fn audio_prop_ids(object: u32, selector: u32) -> Vec<u32> {
    let mut addr = address(selector);
    let mut size = 0u32;
    // SAFETY: サイズ取得 → そのサイズ以上のバッファで本体取得。
    unsafe {
        let status = AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        );
        if status != 0 || size == 0 {
            return Vec::new();
        }
        // 取得までの間に増えても溢れないよう余裕を持たせる。
        let mut ids = vec![0u32; size as usize / size_of::<u32>() + 16];
        let mut size = (ids.len() * size_of::<u32>()) as u32;
        let status = AudioObjectGetPropertyData(
            object,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(ids.as_mut_ptr()).unwrap().cast::<c_void>(),
        );
        if status != 0 {
            return Vec::new();
        }
        ids.truncate(size as usize / size_of::<u32>());
        ids
    }
}

/// CFString のプロパティを読む（呼び出し側が所有権を受け取る規約）。
fn audio_prop_string(object: u32, selector: u32) -> Option<String> {
    let mut addr = address(selector);
    let mut ptr: *const CFString = std::ptr::null();
    let mut size = size_of::<*const CFString>() as u32;
    // SAFETY: ポインタ 1 個ぶんの出力バッファ。返った CFString は +1 なので from_raw で受け取る。
    unsafe {
        let status = AudioObjectGetPropertyData(
            object,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut ptr).cast::<c_void>(),
        );
        if status != 0 {
            return None;
        }
        let s = CFRetained::from_raw(NonNull::new(ptr as *mut CFString)?);
        Some(s.to_string()).filter(|s| !s.is_empty())
    }
}

struct AudioProc {
    pid: u32,
    bundle_id: Option<String>,
    input: bool,
    output: bool,
}

/// CoreAudio を使っているプロセスの一覧（macOS 14.2 以降の「プロセスオブジェクト」）。
fn audio_processes() -> Vec<AudioProc> {
    audio_prop_ids(
        kAudioObjectSystemObject as u32,
        kAudioHardwarePropertyProcessObjectList,
    )
    .into_iter()
    .filter_map(|obj| {
        let pid: i32 = audio_prop(obj, kAudioProcessPropertyPID)?;
        let input: u32 = audio_prop(obj, kAudioProcessPropertyIsRunningInput).unwrap_or(0);
        let output: u32 = audio_prop(obj, kAudioProcessPropertyIsRunningOutput).unwrap_or(0);
        Some(AudioProc {
            pid: pid as u32,
            bundle_id: audio_prop_string(obj, kAudioProcessPropertyBundleID),
            input: input != 0,
            output: output != 0,
        })
    })
    .collect()
}

#[derive(Default)]
pub struct Probe {
    /// PID → バンドル ID。PID の再利用に備えて一定数で捨てる。
    bundles: HashMap<u32, Option<String>>,
}

impl Probe {
    fn bundle_id(&mut self, pid: u32) -> Option<String> {
        if self.bundles.len() > 512 {
            self.bundles.clear();
        }
        self.bundles
            .entry(pid)
            .or_insert_with(|| {
                NSRunningApplication::runningApplicationWithProcessIdentifier(pid as i32)
                    .and_then(|app| app.bundleIdentifier())
                    .map(|s| s.to_string())
            })
            .clone()
    }

    fn proc_info(&mut self, pid: u32, known_bundle: Option<String>) -> ProcInfo {
        ProcInfo {
            pid,
            name: sys::proc_name(pid).unwrap_or_default(),
            bundle_id: known_bundle.or_else(|| self.bundle_id(pid)),
        }
    }

    /// マイク（入力）を使用中のプロセスと、その祖先チェーン。
    pub fn mic_users(&mut self) -> Vec<MicUser> {
        let me = sys::own_pid();
        let mut out = Vec::new();
        for p in audio_processes() {
            if !p.input || p.pid == me || p.pid == 0 {
                continue;
            }
            let mut chain = vec![self.proc_info(p.pid, p.bundle_id)];
            let mut pid = p.pid;
            while chain.len() < MAX_CHAIN {
                match sys::proc_parent_and_name(pid) {
                    // launchd（1）まで来たら打ち切る。
                    Some((parent, _)) if parent > 1 && parent != pid => {
                        chain.push(self.proc_info(parent, None));
                        pid = parent;
                    }
                    _ => break,
                }
            }
            out.push(MicUser { chain });
        }
        out
    }

    /// 通常レイヤー（0）のウィンドウ一覧。最小化・別 Space のウィンドウも含む。
    /// タイトルは画面収録の権限がないと空になる。
    pub fn windows(&mut self) -> Vec<WindowInfo> {
        let Some(list) = CGWindowListCopyWindowInfo(
            CGWindowListOption::OptionAll | CGWindowListOption::ExcludeDesktopElements,
            kCGNullWindowID,
        ) else {
            return Vec::new();
        };
        // SAFETY: CFArray<CFDictionary> は NSArray<NSDictionary> と toll-free bridge。
        // `list` が生きている間だけ参照する。
        let array: &NSArray<NSDictionary<NSString, AnyObject>> =
            unsafe { &*(CFRetained::as_ptr(&list).as_ptr() as *const _) };
        // SAFETY: CFString は NSString と toll-free bridge。キーは静的に生存する。
        let key = |k: &'static CFString| -> &'static NSString {
            unsafe { &*(k as *const CFString as *const NSString) }
        };
        let (k_num, k_layer, k_pid, k_owner, k_name, k_bounds, k_on) = unsafe {
            (
                key(kCGWindowNumber),
                key(kCGWindowLayer),
                key(kCGWindowOwnerPID),
                key(kCGWindowOwnerName),
                key(kCGWindowName),
                key(kCGWindowBounds),
                key(kCGWindowIsOnscreen),
            )
        };
        let number = |d: &NSDictionary<NSString, AnyObject>, k: &NSString| -> Option<f64> {
            d.objectForKey(k)?
                .downcast::<NSNumber>()
                .ok()
                .map(|n| n.doubleValue())
        };
        let string = |d: &NSDictionary<NSString, AnyObject>, k: &NSString| -> Option<String> {
            d.objectForKey(k)?
                .downcast::<NSString>()
                .ok()
                .map(|s| s.to_string())
        };
        let me = sys::own_pid();
        let mut out = Vec::new();
        for d in array.iter() {
            if number(&d, k_layer) != Some(0.0) {
                continue;
            }
            let (Some(id), Some(pid)) = (number(&d, k_num), number(&d, k_pid)) else {
                continue;
            };
            let pid = pid as u32;
            if pid == me {
                continue;
            }
            let bounds: Option<Retained<NSDictionary<NSString, AnyObject>>> = d
                .objectForKey(k_bounds)
                .and_then(|b| b.downcast::<NSDictionary>().ok())
                // SAFETY: kCGWindowBounds は文字列キーの辞書（CGRect の辞書表現）。
                .map(|b| unsafe { Retained::cast_unchecked(b) });
            let dim = |name: &str| -> u32 {
                bounds
                    .as_ref()
                    .and_then(|b| number(b, &NSString::from_str(name)))
                    .map_or(0, |v| v.max(0.0) as u32)
            };
            let (width, height) = (dim("Width"), dim("Height"));
            if width == 0 || height == 0 {
                continue;
            }
            let owner_name = string(&d, k_owner).unwrap_or_default();
            let mut owner = self.proc_info(pid, None);
            if owner.name.is_empty() {
                owner.name = owner_name;
            }
            out.push(WindowInfo {
                id: id as u64,
                owner,
                title: string(&d, k_name).unwrap_or_default(),
                class: String::new(),
                width,
                height,
                minimized: number(&d, k_on).unwrap_or(0.0) == 0.0,
            });
        }
        out
    }

    /// 実機調査用：会議アプリらしきプロセスと CoreAudio の使用状況。
    pub fn diagnostics(&mut self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "-- 名前/パス/バンドル ID に zoom・teams を含むプロセス --"
        );
        for pid in sys::all_pids() {
            let path = sys::proc_path(pid).unwrap_or_default();
            let lower = path.to_lowercase();
            if !(lower.contains("zoom") || lower.contains("teams")) {
                continue;
            }
            let parent = sys::proc_parent_and_name(pid).map_or(0, |(p, _)| p);
            let _ = writeln!(
                s,
                "pid={pid} ppid={parent} name={:?} bundle={:?} path={path:?}",
                sys::proc_name(pid).unwrap_or_default(),
                self.bundle_id(pid),
            );
        }
        let _ = writeln!(s, "-- CoreAudio プロセスオブジェクト --");
        for p in audio_processes() {
            let _ = writeln!(
                s,
                "pid={} name={:?} bundle={:?} input={} output={}",
                p.pid,
                sys::proc_name(p.pid).unwrap_or_default(),
                p.bundle_id,
                p.input,
                p.output,
            );
        }
        s
    }
}
