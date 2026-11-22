//! 映像スレッド：WGC のフレームを NV12 に変換して H.264 エンコーダへ渡し、
//! 出力をセッションへ流す。D3D・MF の資源は録画中だけ存在する。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use meetrec_core::platform::{RecordSettings, RecorderEvent};
use meetrec_core::session::{canvas_size, SOFTWARE_MAX_LONG_SIDE};
use windows::core::{Interface, Result as WinResult};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Win32::Foundation::{E_FAIL, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;

use crate::app::post_recorder_event;
use crate::capture::{Capture, FrameQueue};
use crate::convert::Converter;
use crate::h264enc::{Config, H264Encoder};
use crate::recorder::Holder;
use crate::sys;

/// 最初のフレームがこの時間来なければ黒フレームを 1 枚入れて映像トラックを確保する
/// （最小化されたウィンドウから録画を始めた場合）。
const BLACK_FRAME_AFTER: Duration = Duration::from_secs(2);
/// 連続でこの回数失敗したら映像を諦める（音声は継続）。
const MAX_ERRORS: u32 = 30;

pub struct Params {
    pub hwnd: isize,
    pub size: (u32, u32),
    pub settings: RecordSettings,
    pub main_hwnd: isize,
}

pub struct VideoShared {
    stop: Arc<AtomicBool>,
    queue: Arc<FrameQueue>,
    retarget: Mutex<Option<isize>>,
    info: Mutex<String>,
}

impl VideoShared {
    pub fn new(stop: Arc<AtomicBool>) -> VideoShared {
        VideoShared {
            stop,
            queue: Arc::new(FrameQueue::default()),
            retarget: Mutex::new(None),
            info: Mutex::new(String::new()),
        }
    }

    /// 録画対象を別のウィンドウに張り替える（映像スレッドが処理する）。
    pub fn retarget(&self, hwnd: isize) {
        *self.retarget.lock().unwrap() = Some(hwnd);
        self.wake();
    }

    pub fn wake(&self) {
        self.queue.ready.notify_all();
    }

    pub fn describe(&self) -> String {
        self.info.lock().unwrap().clone()
    }
}

unsafe fn create_device() -> WinResult<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut context),
    )?;
    let device = device.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
    let context = context.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
    // WGC・変換・エンコーダが別スレッドから同じデバイスを使う。
    let _ = device
        .cast::<ID3D11Multithread>()?
        .SetMultithreadProtected(true);
    Ok((device, context))
}

unsafe fn create_manager(device: &ID3D11Device) -> WinResult<IMFDXGIDeviceManager> {
    let mut token = 0u32;
    let mut manager = None;
    MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
    let manager = manager.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
    manager.ResetDevice(device, token)?;
    Ok(manager)
}

pub fn run(holder: Arc<Holder>, shared: Arc<VideoShared>, params: Params) {
    sys::com_init_mta();
    if let Err(e) = unsafe { run_inner(&holder, &shared, &params) } {
        meetrec_core::warn!("映像を録画できません（音声のみ継続）: {e}");
        // まだ映像トラックを書き始めていなければ、待たずに音声のみで開始させる。
        holder.with(|s| s.video_unavailable());
        post_recorder_event(
            params.main_hwnd,
            RecorderEvent::VideoUnavailable(e.to_string()),
        );
    }
    shared.queue.clear();
}

unsafe fn run_inner(holder: &Holder, shared: &VideoShared, params: &Params) -> WinResult<()> {
    let s = &params.settings;
    let (device, context) = create_device()?;
    let manager = create_manager(&device)?;

    let mut canvas = canvas_size(params.size.0, params.size.1, s.max_long_side);
    let mut fps = s.fps.max(1);
    let config = |canvas: (u32, u32), fps: u32| Config {
        width: canvas.0,
        height: canvas.1,
        fps,
        bitrate: s.video_bitrate_kbps * 1000,
        gop: fps * s.keyframe_seconds.max(1),
    };
    let mut encoder = match H264Encoder::hardware(&manager, config(canvas, fps)) {
        Ok(e) => e,
        Err(e) => {
            // ソフトウェアエンコード時は負荷を抑えるため fps と解像度を下げる。
            canvas = canvas_size(
                params.size.0,
                params.size.1,
                s.max_long_side.min(SOFTWARE_MAX_LONG_SIDE),
            );
            fps = fps.min(5);
            meetrec_core::warn!(
                "ハードウェア H.264 エンコーダを使えません（{e}）。ソフトウェアで {}x{} {fps}fps に下げます",
                canvas.0,
                canvas.1
            );
            H264Encoder::software(config(canvas, fps))?
        }
    };
    let mut converter = Converter::new(&device, &context, canvas, fps)?;
    let winrt_device: IDirect3DDevice =
        CreateDirect3D11DeviceFromDXGIDevice(&device.cast::<IDXGIDevice>()?)?.cast()?;
    let mut capture = Some(Capture::start(
        params.hwnd,
        &winrt_device,
        s.show_cursor,
        fps,
        shared.queue.clone(),
    )?);

    let info = format!(
        "{}x{} {fps}fps hardware={} window={:#x}",
        canvas.0, canvas.1, encoder.hardware, params.hwnd
    );
    meetrec_core::info!("映像キャプチャ開始: {info}");
    *shared.info.lock().unwrap() = info;

    let mut sink = |pts_ns: u64, data: &[u8]| {
        holder.with(|sess| sess.push_video_annexb(pts_ns, canvas.0, canvas.1, data));
    };
    let started = Instant::now();
    let mut got_frame = false;
    let mut errors = 0u32;
    let mut warned = false;
    loop {
        let frame = {
            let mut queue = shared.queue.lock();
            if queue.is_empty()
                && !shared.stop.load(Ordering::SeqCst)
                && shared.retarget.lock().unwrap().is_none()
            {
                // 出力待ちがある間だけ短い間隔で起きる。静止画面ではほぼ眠っている。
                let wait = Duration::from_millis(if encoder.pending() { 15 } else { 500 });
                queue = shared.queue.ready.wait_timeout(queue, wait).unwrap().0;
            }
            queue.pop_front()
        };
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        let new_target = shared.retarget.lock().unwrap().take();
        if let Some(hwnd) = new_target {
            // 先に古いキャプチャを閉じてから新しいウィンドウで作り直す（エンコーダは継続）。
            capture = None;
            shared.queue.clear();
            match Capture::start(
                hwnd,
                &winrt_device,
                s.show_cursor,
                fps,
                shared.queue.clone(),
            ) {
                Ok(c) => {
                    meetrec_core::info!("キャプチャ対象を張り替えました: window={hwnd:#x}");
                    capture = Some(c);
                }
                Err(e) => meetrec_core::warn!("張り替え先をキャプチャできません: {e}"),
            }
        }

        let had_frame = frame.is_some();
        let result = if let Some(frame) = frame {
            let converted = converter.convert(&frame.texture, frame.content);
            let pts = frame.pts_ns;
            // 変換が済んだらすぐフレームをプールへ返す。
            drop(frame);
            got_frame = true;
            converted
                .and_then(|texture| encoder.encode(&texture, pts, &mut sink))
                .map(|_| ())
        } else if !got_frame && started.elapsed() > BLACK_FRAME_AFTER {
            got_frame = true;
            meetrec_core::info!("最初のフレームが届かないため黒フレームで開始します");
            encoder
                .encode(&converter.black(), sys::qpc_ns(), &mut sink)
                .map(|_| ())
        } else {
            Ok(())
        };
        match result.and_then(|_| encoder.pump(&mut sink)) {
            // フレームを実際に処理できたときだけ失敗回数を戻す（フレームのない起床で
            // 戻すと、壊れたエンコーダをいつまでも見限れない）。
            Ok(()) => {
                if had_frame {
                    errors = 0;
                }
            }
            Err(e) => {
                errors += 1;
                if !warned {
                    warned = true;
                    meetrec_core::warn!("映像の変換/エンコードに失敗: {e}");
                }
                if errors >= MAX_ERRORS {
                    return Err(e);
                }
            }
        }
    }
    drop(capture);
    shared.queue.clear();
    encoder.finish(&mut sink);
    Ok(())
}
