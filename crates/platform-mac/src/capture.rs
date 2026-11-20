//! 録画本体：ScreenCaptureKit の単一ウィンドウキャプチャ（映像＋アプリ音声＋マイク）を
//! `core::session::Session` に流す。
//!
//! スレッド構成：SCStream のサンプルは専用のシリアルキューに届く。そこで
//! 映像は VideoToolbox に投入（結果は VT のスレッドから Session へ）、
//! 音声は float PCM に直して Session のミキサへ渡す。
//! `Active`（Session・エンコーダ）は Mutex 付きスロットに置き、停止時にメインスレッドが
//! 取り出すことで「停止後にコールバックが触る」ことを防ぐ。

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use meetrec_core::platform::{
    RecordRequest, RecordSettings, RecorderEvent, RecordingSummary, WindowInfo,
};
use meetrec_core::session::{
    canvas_size, AacEncoder, Clock, FrameGate, Session, SessionConfig, SessionEvent, SRC_APP,
    SRC_MIC,
};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_audio_types::{
    kAudioFormatFlagIsBigEndian, kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved,
    kAudioFormatLinearPCM, AudioBuffer, AudioBufferList,
};
use objc2_core_foundation::{CFDictionary, CFNumber};
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMSampleBuffer, CMTime, CMTimeFlags,
};
use objc2_core_video::kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType, SCWindow,
};

use crate::aac::MacAac;
use crate::app::{self, Item};
use crate::encoder::{cmtime_ns, ClockOffset, VideoEncoder};
use crate::sys;

/// 非同期 API の完了を待つ上限。
const WAIT: Duration = Duration::from_secs(3);
/// ソフトウェアエンコード時の解像度上限（長辺）。
const SOFTWARE_MAX_LONG: u32 = 1280;

struct MacClock;
impl Clock for MacClock {
    fn now_ns(&self) -> u64 {
        sys::now_ns()
    }
}

/// メインスレッド以外で受け取った ObjC オブジェクトをチャネルで運ぶための包み。
struct SendRetained<T>(Retained<T>);
// SAFETY: ScreenCaptureKit のモデルオブジェクト（SCShareableContent 等）は不変で、
// どのスレッドからでも読める。
unsafe impl<T> Send for SendRetained<T> {}

fn error_text(error: *mut NSError) -> Option<String> {
    // SAFETY: nil か有効な NSError。
    unsafe { error.as_ref() }.map(|e| format!("{} (code {})", e.localizedDescription(), e.code()))
}

/// 共有可能なウィンドウ一覧を取得する（最小化・別 Space も含む）。完了まで短時間待つ。
fn shareable_content() -> Result<Retained<SCShareableContent>, String> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: content は nil か有効なオブジェクト。retain してから運ぶ。
            let result = match unsafe { Retained::retain(content) } {
                Some(c) => Ok(SendRetained(c)),
                None => Err(error_text(error).unwrap_or_else(|| "不明なエラー".into())),
            };
            let _ = tx.send(result);
        },
    );
    // SAFETY: 完了ハンドラは任意のキューで 1 回呼ばれる。
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false, false, &handler,
        );
    }
    match rx.recv_timeout(WAIT) {
        Ok(Ok(c)) => Ok(c.0),
        Ok(Err(e)) => Err(format!(
            "ウィンドウ一覧を取得できません（画面収録の権限を確認してください）: {e}"
        )),
        Err(_) => Err("ウィンドウ一覧の取得がタイムアウトしました".into()),
    }
}

fn find_window(content: &SCShareableContent, id: u64) -> Option<Retained<SCWindow>> {
    // SAFETY: 取得済みの不変オブジェクトを読むだけ。
    unsafe { content.windows().iter().find(|w| w.windowID() as u64 == id) }
}

/// 完了ハンドラ（エラーのみ）を待つ。
fn wait_completion(
    call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>),
) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |error: *mut NSError| {
        let _ = tx.send(error_text(error));
    });
    call(&handler);
    match rx.recv_timeout(WAIT) {
        Ok(None) => Ok(()),
        Ok(Some(e)) => Err(e),
        Err(_) => Err("タイムアウト".into()),
    }
}

// ---------------------------------------------------------------------------
// サンプルを受ける側
// ---------------------------------------------------------------------------

/// キャプチャ中だけ存在する処理系。サンプルキュー上で使う。
struct Active {
    session: Arc<Session>,
    encoder: Option<VideoEncoder>,
    gate: FrameGate,
    clock: Arc<ClockOffset>,
    raw: Vec<u8>,
    pcm: Vec<f32>,
    abl: Vec<u64>,
    warned_format: bool,
}

// SAFETY: Active は Mutex 越しにのみ触る。中身（Session, VideoEncoder, Vec）はスレッド間で移動可能。
unsafe impl Send for Active {}

/// フレームの状態が「完全（内容あり）」か。idle（変化なし）等は false。
fn frame_complete(sbuf: &CMSampleBuffer) -> bool {
    // SAFETY: 添付配列は辞書の配列。キーは静的な NSString（CFString と toll-free）。
    unsafe {
        let Some(att) = sbuf.sample_attachments_array(false) else {
            return false;
        };
        if att.count() == 0 {
            return false;
        }
        let Some(dict) = (att.value_at_index(0) as *const CFDictionary).as_ref() else {
            return false;
        };
        let key = SCStreamFrameInfoStatus as *const _ as *const c_void;
        let Some(num) = (dict.value(key) as *const CFNumber).as_ref() else {
            return false;
        };
        num.as_i64() == Some(SCFrameStatus::Complete.0 as i64)
    }
}

impl Active {
    fn video(&mut self, sbuf: &CMSampleBuffer) {
        let Some(encoder) = &self.encoder else { return };
        if !frame_complete(sbuf) {
            return; // 画面が変化していない間はエンコードしない
        }
        // SAFETY: 有効なサンプルバッファの読み取り。
        let (pts, image) = unsafe { (sbuf.presentation_time_stamp(), sbuf.image_buffer()) };
        let (Some(ns), Some(image)) = (cmtime_ns(pts), image) else {
            return;
        };
        self.clock.observe(ns, sys::now_ns());
        if self.gate.accept(ns) {
            encoder.encode(&image, pts);
        }
    }

    /// CMSampleBuffer の LPCM を interleaved float（最大 2ch）にして Session へ渡す。
    fn audio(&mut self, source: usize, sbuf: &CMSampleBuffer) {
        // SAFETY: フォーマット記述と ASBD は sbuf が生きている間有効。
        let (asbd, frames, pts) = unsafe {
            let Some(fmt) = sbuf.format_description() else {
                return;
            };
            let Some(asbd) = CMAudioFormatDescriptionGetStreamBasicDescription(&fmt).as_ref()
            else {
                return;
            };
            (*asbd, sbuf.num_samples(), sbuf.presentation_time_stamp())
        };
        let Some(pts_ns) = cmtime_ns(pts) else { return };
        self.clock.observe(pts_ns, sys::now_ns());
        let pts_ns = self.clock.apply(pts_ns);
        let channels = asbd.mChannelsPerFrame as usize;
        let bytes = (asbd.mBitsPerChannel / 8) as usize;
        let float = asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0;
        let supported = asbd.mFormatID == kAudioFormatLinearPCM
            && asbd.mFormatFlags & kAudioFormatFlagIsBigEndian == 0
            && (1..=16).contains(&channels)
            && ((float && bytes == 4) || (!float && (bytes == 2 || bytes == 4)))
            && asbd.mSampleRate >= 8000.0;
        if !supported || frames <= 0 || frames > 192_000 {
            if !supported && !self.warned_format {
                self.warned_format = true;
                meetrec_core::warn!("未対応の音声フォーマット (source {source}): {asbd:?}");
            }
            return;
        }
        let n = frames as usize;
        let planar = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0 && channels > 1;
        self.raw.resize(n * channels * bytes, 0);

        // 自前のバッファを指す AudioBufferList を組み立て、そこへコピーさせる。
        let buffers = if planar { channels } else { 1 };
        let words = (std::mem::size_of::<AudioBufferList>()
            + (buffers - 1) * std::mem::size_of::<AudioBuffer>())
        .div_ceil(8);
        self.abl.clear();
        self.abl.resize(words, 0);
        let abl = self.abl.as_mut_ptr() as *mut AudioBufferList;
        // SAFETY: abl は buffers 個の AudioBuffer を収める大きさ・アラインメントを持つ。
        // 各バッファは self.raw の重ならない領域を指す。
        let status = unsafe {
            (*abl).mNumberBuffers = buffers as u32;
            let list = std::ptr::addr_of_mut!((*abl).mBuffers) as *mut AudioBuffer;
            let per = self.raw.len() / buffers;
            for i in 0..buffers {
                list.add(i).write(AudioBuffer {
                    mNumberChannels: if planar { 1 } else { channels as u32 },
                    mDataByteSize: per as u32,
                    mData: self.raw.as_mut_ptr().add(i * per).cast(),
                });
            }
            sbuf.copy_pcm_data_into_audio_buffer_list(0, n as i32, NonNull::new_unchecked(abl))
        };
        if status != 0 {
            return;
        }

        let out_ch = channels.min(2);
        self.pcm.clear();
        self.pcm.reserve(n * out_ch);
        let raw = &self.raw;
        let sample = |frame: usize, ch: usize| -> f32 {
            let at = if planar {
                ch * n + frame
            } else {
                frame * channels + ch
            } * bytes;
            match (float, bytes) {
                (true, _) => f32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]),
                (false, 2) => i16::from_le_bytes([raw[at], raw[at + 1]]) as f32 / 32768.0,
                _ => {
                    i32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]) as f32
                        / 2_147_483_648.0
                }
            }
        };
        for frame in 0..n {
            for ch in 0..out_ch {
                self.pcm.push(sample(frame, ch));
            }
        }
        self.session.push_pcm(
            source,
            pts_ns,
            asbd.mSampleRate as u32,
            out_ch as u16,
            &self.pcm,
        );
    }
}

type Slot = Arc<Mutex<Option<Active>>>;

struct OutputIvars {
    slot: Slot,
    stopped: Arc<AtomicBool>,
}

define_class!(
    // SCStream の出力先兼デリゲート。任意のスレッドから呼ばれる。
    #[unsafe(super(NSObject))]
    #[name = "MeetRecStreamOutput"]
    #[ivars = OutputIvars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sbuf: &CMSampleBuffer, kind: SCStreamOutputType) {
            let mut guard = self.ivars().slot.lock().unwrap();
            let Some(active) = guard.as_mut() else { return };
            if kind == SCStreamOutputType::Screen {
                active.video(sbuf);
            } else if kind == SCStreamOutputType::Audio {
                active.audio(SRC_APP, sbuf);
            } else if kind == SCStreamOutputType::Microphone {
                active.audio(SRC_MIC, sbuf);
            }
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            let msg = format!("{} (code {})", error.localizedDescription(), error.code());
            self.ivars().stopped.store(true, Ordering::SeqCst);
            app::post(Item::StreamStopped(msg));
        }
    }
);

impl StreamOutput {
    fn new(slot: Slot, stopped: Arc<AtomicBool>) -> Retained<StreamOutput> {
        let this = Self::alloc().set_ivars(OutputIvars { slot, stopped });
        // SAFETY: NSObject の init を呼ぶだけ。
        unsafe { msg_send![super(this), init] }
    }
}

// ---------------------------------------------------------------------------
// メインスレッド側
// ---------------------------------------------------------------------------

pub struct Recorder {
    slot: Slot,
    output: Retained<StreamOutput>,
    queue: DispatchRetained<DispatchQueue>,
    stream: Option<Retained<SCStream>>,
    /// ストリームが（エラーで）止まっている。
    stopped: Arc<AtomicBool>,
    settings: RecordSettings,
    fps: u32,
    canvas: (u32, u32),
    target: Option<u64>,
    root_pid: u32,
    audio_only: bool,
    /// 現在のストリームが映像出力付きか（音声だけの代替ストリームなら false）。
    stream_video: bool,
    last_restart_ns: u64,
}

impl Recorder {
    pub fn start(req: &RecordRequest) -> Result<Recorder, String> {
        let s = &req.settings;
        let mut cfg = SessionConfig::new(req.path.clone(), req.marker_dir.clone());
        cfg.fragment_ms = s.fragment_ms;
        cfg.audio_bitrate = s.audio_bitrate_kbps * 1000;
        cfg.separate_tracks = s.separate_tracks;

        // 先に対象ウィンドウを確定する（失敗したらファイルを作らずに返す）。
        let content = shareable_content()?;
        let window = req
            .window
            .as_ref()
            .and_then(|w| find_window(&content, w.id));
        if req.window.is_some() && window.is_none() {
            meetrec_core::warn!("対象ウィンドウが共有可能一覧にありません。音声のみで録ります");
        }
        cfg.expect_video = window.is_some();

        let bitrate = cfg.audio_bitrate;
        let session = Session::start(
            cfg,
            Arc::new(MacClock),
            Box::new(move || MacAac::new(bitrate).map(|e| Box::new(e) as Box<dyn AacEncoder>)),
            Box::new(|event| match event {
                SessionEvent::WriteFailed(e) => app::post_recorder(RecorderEvent::Failed(e)),
                SessionEvent::VideoTimeout => app::post_recorder(RecorderEvent::VideoUnavailable(
                    "映像フレームが届きません".into(),
                )),
                SessionEvent::AudioEncoderFailed(e) => {
                    meetrec_core::warn!("音声エンコーダの異常: {e}")
                }
            }),
        )?;
        let session = Arc::new(session);

        let slot: Slot = Arc::new(Mutex::new(None));
        let stopped = Arc::new(AtomicBool::new(false));
        let mut rec = Recorder {
            output: StreamOutput::new(slot.clone(), stopped.clone()),
            slot,
            queue: DispatchQueue::new("meetrec.capture", None),
            stream: None,
            stopped,
            settings: RecordSettings {
                // 未許可のままマイクを要求するとストリーム開始が失敗しうるので外す。
                capture_mic: s.capture_mic && app::mic_authorized(),
                ..s.clone()
            },
            fps: s.fps,
            canvas: (2, 2),
            target: None,
            root_pid: req.root_pid,
            audio_only: window.is_none(),
            stream_video: false,
            last_restart_ns: 0,
        };

        let clock = Arc::new(ClockOffset::new());
        // 映像エンコーダ（ハードウェア必須 → だめならソフトウェアで fps と解像度を下げる）。
        let mut encoder = None;
        let filter = match &window {
            Some(w) => {
                // SAFETY: 取得済みの SCWindow からフィルタを作る。
                let filter = unsafe {
                    SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), w)
                };
                // ウィンドウの実ピクセル数（Retina では point の 2 倍）からキャンバスを決める。
                let (pw, ph) = unsafe {
                    let rect = filter.contentRect();
                    let scale = filter.pointPixelScale().max(1.0) as f64;
                    (
                        (rect.size.width * scale) as u32,
                        (rect.size.height * scale) as u32,
                    )
                };
                let canvas = canvas_size(pw.max(2), ph.max(2), s.max_long_side);
                match VideoEncoder::create(
                    session.clone(),
                    canvas.0,
                    canvas.1,
                    s.fps,
                    s,
                    true,
                    clock.clone(),
                ) {
                    Ok(e) => {
                        rec.canvas = canvas;
                        encoder = Some(e);
                    }
                    Err(e) => {
                        meetrec_core::warn!(
                            "ハードウェアエンコーダを使えません: {e}。ソフトウェアで継続します"
                        );
                        let canvas = canvas_size(
                            pw.max(2),
                            ph.max(2),
                            s.max_long_side.min(SOFTWARE_MAX_LONG),
                        );
                        let fps = (s.fps / 2).max(5).min(s.fps);
                        match VideoEncoder::create(
                            session.clone(),
                            canvas.0,
                            canvas.1,
                            fps,
                            s,
                            false,
                            clock.clone(),
                        ) {
                            Ok(e) => {
                                rec.canvas = canvas;
                                rec.fps = fps;
                                encoder = Some(e);
                            }
                            Err(e) => {
                                meetrec_core::warn!("映像エンコーダを作成できません: {e}");
                                rec.audio_only = true;
                            }
                        }
                    }
                }
                Some(filter)
            }
            None => None,
        };
        if rec.audio_only {
            session.video_unavailable();
            app::post_recorder(RecorderEvent::VideoUnavailable(
                "録画対象のウィンドウを取得できません".into(),
            ));
        }
        let filter = match filter.filter(|_| !rec.audio_only) {
            Some(f) => f,
            None => match app_audio_filter(&content, rec.root_pid) {
                Ok(f) => f,
                Err(e) => {
                    drop(encoder);
                    finish_session(session);
                    return Err(e);
                }
            },
        };
        meetrec_core::info!(
            "キャプチャ設定: canvas={}x{} fps={} hardware={:?} audio_only={}",
            rec.canvas.0,
            rec.canvas.1,
            rec.fps,
            encoder.as_ref().map(|e| e.hardware),
            rec.audio_only
        );
        *rec.slot.lock().unwrap() = Some(Active {
            session,
            encoder,
            gate: FrameGate::new(rec.fps),
            clock,
            raw: Vec::new(),
            pcm: Vec::new(),
            abl: Vec::new(),
            warned_format: false,
        });
        rec.target = window
            .as_ref()
            .filter(|_| !rec.audio_only)
            .map(|w| unsafe { w.windowID() } as u64);
        let video = !rec.audio_only;
        if let Err(e) = rec.start_stream(&filter, video) {
            rec.stop();
            return Err(format!("キャプチャを開始できません: {e}"));
        }
        Ok(rec)
    }

    fn configuration(&self, video: bool) -> Retained<SCStreamConfiguration> {
        // SAFETY: 設定オブジェクトのプロパティを設定するだけ。
        unsafe {
            let c = SCStreamConfiguration::new();
            if !video {
                // 映像は使わない。最小サイズ・最低頻度にして負荷を抑える。
                c.setWidth(2);
                c.setHeight(2);
                c.setMinimumFrameInterval(CMTime {
                    value: 1,
                    timescale: 1,
                    flags: CMTimeFlags(1),
                    epoch: 0,
                });
            } else {
                c.setWidth(self.canvas.0 as usize);
                c.setHeight(self.canvas.1 as usize);
                c.setMinimumFrameInterval(CMTime {
                    value: 1,
                    timescale: self.fps.max(1) as i32,
                    flags: CMTimeFlags(1),
                    epoch: 0,
                });
                c.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
                c.setScalesToFit(true);
                c.setShowsCursor(self.settings.show_cursor);
                c.setQueueDepth(4);
            }
            c.setCapturesAudio(true);
            c.setExcludesCurrentProcessAudio(true);
            c.setSampleRate(48_000);
            c.setChannelCount(2);
            if self.settings.capture_mic {
                c.setCaptureMicrophone(true);
            }
            c
        }
    }

    /// フィルタから SCStream を作って開始する（初回・再構築の両方で使う）。
    fn start_stream(&mut self, filter: &SCContentFilter, video: bool) -> Result<(), String> {
        let config = self.configuration(video);
        // SAFETY: 出力オブジェクトは self.output が保持し続ける。
        let stream = unsafe {
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                filter,
                &config,
                Some(ProtocolObject::from_ref(&*self.output)),
            );
            let output: &ProtocolObject<dyn SCStreamOutput> =
                ProtocolObject::from_ref(&*self.output);
            let mut kinds = vec![SCStreamOutputType::Audio];
            if video {
                kinds.push(SCStreamOutputType::Screen);
            }
            if self.settings.capture_mic {
                kinds.push(SCStreamOutputType::Microphone);
            }
            for kind in kinds {
                stream
                    .addStreamOutput_type_sampleHandlerQueue_error(output, kind, Some(&self.queue))
                    .map_err(|e| format!("出力を追加できません: {}", e.localizedDescription()))?;
            }
            stream
        };
        // SAFETY: 完了ハンドラは任意のキューで呼ばれる。メインスレッドは短時間待つだけ。
        let started =
            wait_completion(|h| unsafe { stream.startCaptureWithCompletionHandler(Some(h)) });
        // 開始できたときだけ「動作中」にする（失敗時は停止扱いのままにして復旧経路を残す）。
        self.stopped.store(started.is_err(), Ordering::SeqCst);
        started?;
        self.stream = Some(stream);
        self.stream_video = video;
        Ok(())
    }

    /// 映像ストリームがない間も音声を録り続けるための代替ストリーム（アプリ音声＋マイク）。
    /// 対象ウィンドウが消えた・張り替えに失敗した場合に使う。成功すれば true。
    fn fallback_audio(&mut self) -> bool {
        if self.stream.is_some() && !self.stopped.load(Ordering::SeqCst) {
            return true;
        }
        self.stream = None;
        let result = shareable_content()
            .and_then(|content| app_audio_filter(&content, self.root_pid))
            .and_then(|filter| self.start_stream(&filter, false));
        match result {
            Ok(()) => {
                meetrec_core::info!("映像なしで音声の録音を継続します");
                true
            }
            Err(e) => {
                meetrec_core::warn!("音声の録音を継続できません: {e}");
                false
            }
        }
    }

    /// 録画対象ウィンドウを張り替える。ストリームが止まっていれば作り直す。
    /// 戻り値 false：映像も音声も取れない状態になった（録画を続けても何も入らない）。
    pub fn retarget(&mut self, window: &WindowInfo) -> bool {
        if self.audio_only {
            return true;
        }
        let result = shareable_content().and_then(|content| {
            let w =
                find_window(&content, window.id).ok_or("ウィンドウが共有可能一覧にありません")?;
            // SAFETY: 取得済みの SCWindow からフィルタを作る。
            let filter = unsafe {
                SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &w)
            };
            let running = self.stream.is_some() && !self.stopped.load(Ordering::SeqCst);
            match &self.stream {
                Some(stream) if running && self.stream_video => {
                    // SAFETY: 実行中のストリームのフィルタ差し替え。
                    wait_completion(|h| unsafe {
                        stream.updateContentFilter_completionHandler(&filter, Some(h))
                    })
                }
                _ => {
                    // 止まっている、または音声だけの代替ストリーム：映像付きで作り直す。
                    if let Some(old) = self.stream.take() {
                        if running {
                            // SAFETY: 完了ハンドラは任意のキューで呼ばれる。
                            let _ = wait_completion(|h| unsafe {
                                old.stopCaptureWithCompletionHandler(Some(h))
                            });
                        }
                    }
                    self.stopped.store(true, Ordering::SeqCst);
                    self.start_stream(&filter, true)
                }
            }
        });
        match result {
            Ok(()) => {
                self.target = Some(window.id);
                true
            }
            Err(e) => {
                meetrec_core::warn!("録画対象を張り替えられません: {e}");
                // 張り替えに失敗しても音声だけは録り続ける。
                self.fallback_audio()
            }
        }
    }

    /// ストリームがエラーで止まったときの処理（メインスレッド）。
    /// 戻り値：None = 復旧済み、Some(true) = 対象が消えたので張り替え待ち、Some(false) = 続行不能。
    pub fn stream_stopped(&mut self, target_exists: impl FnOnce(u64) -> bool) -> Option<bool> {
        if !self.stopped.load(Ordering::SeqCst) {
            return None; // 既に作り直した後に届いた古い通知
        }
        self.stream = None;
        if let Some(id) = self.target {
            if !target_exists(id) {
                // 未検証: 対象ウィンドウが閉じられるとストリームがエラー停止する想定。
                // controller が次のティックで別のウィンドウへ張り替える。代わりのウィンドウが
                // 現れるまでの間も、音声だけは録り続ける。
                if !self.fallback_audio() {
                    return Some(false);
                }
                return Some(true);
            }
        }
        // 対象が残っているのに止まった：30 秒に 1 回までは作り直しを試みる。
        let now = sys::now_ns();
        if self.last_restart_ns != 0 && now.saturating_sub(self.last_restart_ns) < 30_000_000_000 {
            return Some(false);
        }
        self.last_restart_ns = now.max(1);
        let result = shareable_content().and_then(|content| {
            let filter = match self.target {
                Some(id) => {
                    let w =
                        find_window(&content, id).ok_or("ウィンドウが共有可能一覧にありません")?;
                    // SAFETY: 取得済みの SCWindow からフィルタを作る。
                    unsafe {
                        SCContentFilter::initWithDesktopIndependentWindow(
                            SCContentFilter::alloc(),
                            &w,
                        )
                    }
                }
                None => app_audio_filter(&content, self.root_pid)?,
            };
            let video = self.target.is_some();
            self.start_stream(&filter, video)
        });
        match result {
            Ok(()) => {
                meetrec_core::info!("キャプチャを再開しました");
                None
            }
            Err(e) => {
                meetrec_core::warn!("キャプチャを再開できません: {e}");
                Some(false)
            }
        }
    }

    /// ストリームを止め、エンコーダを出し切り、ファイルを確定する。
    pub fn stop(&mut self) -> Option<RecordingSummary> {
        if let Some(stream) = self.stream.take() {
            if !self.stopped.load(Ordering::SeqCst) {
                // SAFETY: 完了ハンドラは任意のキューで呼ばれる。
                if let Err(e) =
                    wait_completion(|h| unsafe { stream.stopCaptureWithCompletionHandler(Some(h)) })
                {
                    meetrec_core::warn!("キャプチャ停止時のエラー: {e}");
                }
            }
        }
        // スロットを空にする。以後のコールバックは何もしない（実行中のものはロックで待ち合わせ済み）。
        let active = self.slot.lock().unwrap().take()?;
        let Active {
            session, encoder, ..
        } = active;
        drop(encoder); // 残りフレームを出し切って無効化（Session への参照も手放す）
        Some(finish_session(session))
    }
}

/// 他の参照がなくなるのを（通常は即時）待って Session を確定する。
fn finish_session(mut session: Arc<Session>) -> RecordingSummary {
    for _ in 0..200 {
        match Arc::try_unwrap(session) {
            Ok(s) => return s.finish(),
            Err(arc) => {
                session = arc;
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    // ここには来ないはず。来た場合ファイルは進行中マーカー付きで残り、次回起動時に修復される。
    meetrec_core::warn!("録画セッションを確定できませんでした（次回起動時に修復します）");
    RecordingSummary {
        path: session.path().to_path_buf(),
        ..Default::default()
    }
}

/// 映像なしでアプリの音声だけを録るためのフィルタ（ディスプレイ＋対象アプリ）。
fn app_audio_filter(
    content: &SCShareableContent,
    root_pid: u32,
) -> Result<Retained<SCContentFilter>, String> {
    // SAFETY: 取得済みの不変オブジェクトを読むだけ。
    unsafe {
        let display = content
            .displays()
            .iter()
            .next()
            .ok_or("ディスプレイが見つかりません")?;
        let apps: Vec<_> = content
            .applications()
            .iter()
            .filter(|a| a.processID() as u32 == root_pid)
            .collect();
        if apps.is_empty() {
            return Err("会議アプリが共有可能一覧にありません".into());
        }
        Ok(
            SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::from_retained_slice(&apps),
                &NSArray::new(),
            ),
        )
    }
}
