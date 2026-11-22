//! 録画の開始・停止。`core::session::Session` に、映像スレッド（WGC → H.264）と
//! 音声スレッド（プロセスループバック・マイク）からデータを流し込む。

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use meetrec_core::platform::{RecordRequest, RecorderEvent, RecordingSummary, WindowInfo};
use meetrec_core::session::{Session, SessionConfig, SessionEvent};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromWindow};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFShutdown, MFStartup};

use crate::app::post_recorder_event;
use crate::capture::Target;
use crate::sys::QpcClock;
use crate::video::VideoShared;
use crate::{aac, audio, probe, video};

/// 停止時に各スレッドの終了を待つ上限（UI を長時間止めない）。
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// セッションを複数スレッドから使うための入れ物。停止時に `take` で取り出して確定する。
pub struct Holder(RwLock<Option<Session>>);

impl Holder {
    pub fn with(&self, f: impl FnOnce(&Session)) {
        if let Ok(guard) = self.0.read()
            && let Some(s) = guard.as_ref()
        {
            f(s);
        }
    }

    fn take(&self) -> Option<Session> {
        self.0.write().ok().and_then(|mut g| g.take())
    }
}

struct Worker {
    name: &'static str,
    done: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

fn spawn(name: &'static str, f: impl FnOnce() + Send + 'static) -> Option<Worker> {
    let done = Arc::new(AtomicBool::new(false));
    let d2 = done.clone();
    std::thread::Builder::new()
        .name(format!("meetrec-{name}"))
        .spawn(move || {
            f();
            d2.store(true, Ordering::SeqCst);
        })
        .ok()
        .map(|handle| Worker { name, done, handle })
}

pub struct Recorder {
    holder: Arc<Holder>,
    stop: Arc<AtomicBool>,
    video: Option<Arc<VideoShared>>,
    workers: Vec<Worker>,
    root_pid: u32,
}

impl Recorder {
    pub fn start(req: &RecordRequest, main_hwnd: isize) -> Result<Recorder, String> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
            .map_err(|e| format!("Media Foundation を初期化できません: {e}"))?;

        // 音声を録るプロセスツリーの根を決める（0・不在・子プロセス指定への備え）。
        let root_pid =
            probe::normalize_root_pid(req.root_pid, req.window.as_ref().map(|w| w.owner.pid));
        meetrec_core::info!(
            "音声の対象プロセス: 指定 pid={} → ルート pid={root_pid}",
            req.root_pid
        );

        let s = &req.settings;
        let mut cfg = SessionConfig::new(req.path.clone(), req.marker_dir.clone());
        cfg.fragment_ms = s.fragment_ms;
        cfg.audio_bitrate = s.audio_bitrate_kbps * 1000;
        cfg.separate_tracks = s.separate_tracks;
        cfg.expect_video = req.window.is_some();

        let bitrate = cfg.audio_bitrate;
        let session = Session::start(
            cfg,
            Arc::new(QpcClock),
            Box::new(move || aac::create(bitrate)),
            Box::new(move |ev| match ev {
                SessionEvent::WriteFailed(e) => {
                    post_recorder_event(main_hwnd, RecorderEvent::Failed(e))
                }
                SessionEvent::VideoTimeout => post_recorder_event(
                    main_hwnd,
                    RecorderEvent::VideoUnavailable("映像フレームが届きません".into()),
                ),
                SessionEvent::AudioEncoderFailed(e) => {
                    meetrec_core::warn!("音声エンコーダの失敗（映像のみ継続）: {e}")
                }
            }),
        );
        let session = match session {
            Ok(s) => s,
            Err(e) => {
                unsafe {
                    let _ = MFShutdown();
                }
                return Err(e);
            }
        };
        let holder = Arc::new(Holder(RwLock::new(Some(session))));
        let stop = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::new();

        if root_pid != 0 {
            let (h, st) = (holder.clone(), stop.clone());
            let system_audio = req.system_audio;
            workers.extend(spawn("loopback", move || {
                audio::run_loopback(h, st, root_pid, system_audio)
            }));
        } else {
            meetrec_core::warn!("対象プロセスが不明のため相手音声は録音しません");
        }
        if s.capture_mic {
            let (h, st) = (holder.clone(), stop.clone());
            workers.extend(spawn("mic", move || audio::run_mic(h, st)));
        }
        let mut video_shared = None;
        if let Some(w) = &req.window {
            let shared = Arc::new(VideoShared::new(stop.clone()));
            let params = video::Params {
                hwnd: w.id as isize,
                size: (w.width, w.height),
                settings: s.clone(),
                main_hwnd,
            };
            let (h, sh) = (holder.clone(), shared.clone());
            workers.extend(spawn("video", move || video::run(h, sh, params)));
            video_shared = Some(shared);
        }
        Ok(Recorder {
            holder,
            stop,
            video: video_shared,
            workers,
            root_pid,
        })
    }

    pub fn retarget(&self, window: &WindowInfo) {
        if let Some(v) = &self.video {
            v.retarget(Some(Target::Window(window.id as isize)));
        }
    }

    /// 映像を、`near` のウィンドウがあるモニター全体に切り替える（自分の画面共有中）。
    /// モニターを特定できない・映像を録っていない場合は false（共有していない画面を録らない）。
    /// 音声はプロセス単位で録っているので変わらない。
    pub fn show_screen(&self, near: &WindowInfo) -> bool {
        let Some(v) = &self.video else {
            return false;
        };
        // SAFETY: HWND を渡すだけ。無効なら NULL が返る。
        let monitor =
            unsafe { MonitorFromWindow(HWND(near.id as usize as *mut _), MONITOR_DEFAULTTONULL) };
        if monitor.is_invalid() {
            meetrec_core::warn!("共有しているモニターを特定できません");
            return false;
        }
        v.retarget(Some(Target::Monitor(monitor.0 as isize)));
        true
    }

    /// 自分のマイクを録る・録らないを切り替える（マイクは開いたまま、録らない間は無音にする）。
    pub fn set_mic_enabled(&self, enabled: bool) {
        self.holder.with(|s| s.set_mic_enabled(enabled));
    }

    /// 画面の録画をやめる（張り替え先のウィンドウがないとき）。
    pub fn hide_screen(&self) {
        if let Some(v) = &self.video {
            v.retarget(None);
        }
    }

    pub fn diagnostics(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "-- 録画中 --\n音声ルート pid={}", self.root_pid);
        self.holder.with(|sess| {
            let _ = writeln!(s, "{:?}", sess.stats());
        });
        if let Some(v) = &self.video {
            let _ = writeln!(s, "映像: {}", v.describe());
        }
        s
    }

    /// キャプチャを止め、エンコーダを排出し、ファイルを確定する。
    pub fn stop(self) -> RecordingSummary {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(v) = &self.video {
            v.wake();
        }
        let deadline = Instant::now() + STOP_TIMEOUT;
        while Instant::now() < deadline
            && !self.workers.iter().all(|w| w.done.load(Ordering::SeqCst))
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut all_joined = true;
        for w in self.workers {
            if w.done.load(Ordering::SeqCst) {
                let _ = w.handle.join();
            } else {
                // 応答しないスレッドは切り離す（セッションは取り上げるので書き込みは止まる）。
                meetrec_core::warn!("{} スレッドが時間内に終了しませんでした", w.name);
                all_joined = false;
            }
        }
        let summary = self.holder.take().map(|s| s.finish()).unwrap_or_default();
        if all_joined {
            unsafe {
                let _ = MFShutdown();
            }
        }
        summary
    }
}
