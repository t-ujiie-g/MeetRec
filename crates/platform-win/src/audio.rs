//! WASAPI キャプチャ：会議アプリのプロセスループバック（相手音声）とマイク。
//! どちらもイベント駆動の専用スレッドで動き、PCM を float に直してミキサへ渡す。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use meetrec_core::output::{self, OutputUnmute, Restore};
use meetrec_core::platform::{NS_PER_SEC, RecorderEvent};
use meetrec_core::session::{SRC_APP, SRC_MIC};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDIO_VOLUME_NOTIFICATION_DATA, AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eCapture, eCommunications, eConsole, eRender,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{GUID, HRESULT, Interface, Ref, Result as WinResult, implement};

use crate::app::post_recorder_event;
use crate::probe;
use crate::recorder::Holder;
use crate::sys::{self, Handle, from_wide};

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;
/// KSDATAFORMAT_SUBTYPE_IEEE_FLOAT の先頭 4 バイト（Data1）。PCM は 1。
const SUBTYPE_FLOAT_DATA1: u32 = 3;
const BUFFERFLAGS_SILENT: u32 = 2;
const BUFFERFLAGS_TIMESTAMP_ERROR: u32 = 4;
const VT_BLOB: u16 = 65;
/// 共有モードのバッファ長（100ns 単位）。
const BUFFER_HNS: i64 = 2_000_000;

#[derive(Debug, Clone, Copy)]
struct Format {
    rate: u32,
    channels: u16,
    bits: u16,
    float: bool,
}

impl Format {
    /// デバイスのミックスフォーマットを読む。扱えない形式なら None。
    unsafe fn parse(p: *const WAVEFORMATEX) -> Option<Format> {
        unsafe {
            let f = std::ptr::read_unaligned(p);
            let (tag, bits) = (f.wFormatTag, f.wBitsPerSample);
            let float = match tag {
                WAVE_FORMAT_PCM => false,
                WAVE_FORMAT_IEEE_FLOAT => true,
                WAVE_FORMAT_EXTENSIBLE => {
                    let ext = std::ptr::read_unaligned(p as *const WAVEFORMATEXTENSIBLE);
                    let sub: GUID = ext.SubFormat;
                    match sub.data1 {
                        1 => false,
                        SUBTYPE_FLOAT_DATA1 => true,
                        _ => return None,
                    }
                }
                _ => return None,
            };
            let ok = if float {
                bits == 32
            } else {
                matches!(bits, 16 | 24 | 32)
            };
            (ok && f.nChannels > 0 && f.nSamplesPerSec > 0).then_some(Format {
                rate: f.nSamplesPerSec,
                channels: f.nChannels,
                bits,
                float,
            })
        }
    }

    fn frame_bytes(&self) -> usize {
        self.channels as usize * self.bits as usize / 8
    }

    /// 生バイト列を interleaved float にする。
    fn to_f32(self, data: &[u8], out: &mut Vec<f32>) {
        out.clear();
        match (self.float, self.bits) {
            (true, _) => out.extend(
                data.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b)),
            ),
            (false, 16) => out.extend(
                data.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes(*b) as f32 / 32768.0),
            ),
            (false, 24) => out.extend(
                data.as_chunks::<3>()
                    .0
                    .iter()
                    .map(|b| (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0),
            ),
            _ => out.extend(
                data.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| i32::from_le_bytes(*b) as f32 / 2_147_483_648.0),
            ),
        }
    }
}

/// 開始済みのクライアントからパケットを読み続ける。`keep_going` が false を返したら抜ける。
unsafe fn capture_loop(
    client: &IAudioClient,
    format: Format,
    source: usize,
    holder: &Holder,
    stop: &AtomicBool,
    mut keep_going: impl FnMut() -> bool,
) -> WinResult<()> {
    unsafe {
        let event = Handle(CreateEventW(None, false, false, None)?);
        client.SetEventHandle(event.0)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        let mut pcm: Vec<f32> = Vec::new();
        // 音が実際に届いたかを後から確かめられるよう、最初の 1 回だけログに残す。
        let mut heard = false;
        let result = (|| -> WinResult<()> {
            while !stop.load(Ordering::SeqCst) && keep_going() {
                WaitForSingleObject(event.0, 200);
                loop {
                    if capture.GetNextPacketSize()? == 0 {
                        break;
                    }
                    let mut data = std::ptr::null_mut();
                    let (mut frames, mut flags, mut qpc) = (0u32, 0u32, 0u64);
                    capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                    if frames > 0 && flags & BUFFERFLAGS_SILENT == 0 && !data.is_null() {
                        let bytes = std::slice::from_raw_parts(
                            data,
                            frames as usize * format.frame_bytes(),
                        );
                        format.to_f32(bytes, &mut pcm);
                        if !heard {
                            heard = true;
                            meetrec_core::info!(
                                "音声の最初のデータが届きました（{}）",
                                if source == SRC_APP {
                                    "相手音声"
                                } else {
                                    "マイク"
                                }
                            );
                        }
                        // QPC 位置は 100ns 単位。無効・異常値なら現在時刻から逆算する。
                        let now = sys::qpc_ns();
                        let mut pts = qpc.wrapping_mul(100);
                        if qpc == 0
                            || flags & BUFFERFLAGS_TIMESTAMP_ERROR != 0
                            || pts.abs_diff(now) > NS_PER_SEC
                        {
                            pts =
                                now.saturating_sub(frames as u64 * NS_PER_SEC / format.rate as u64);
                        }
                        holder
                            .with(|s| s.push_pcm(source, pts, format.rate, format.channels, &pcm));
                    }
                    capture.ReleaseBuffer(frames)?;
                }
            }
            Ok(())
        })();
        let _ = client.Stop();
        result
    }
}

// ---------------------------------------------------------------------------
// プロセスループバック（相手音声）
// ---------------------------------------------------------------------------

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivateDone(Arc<(Mutex<bool>, Condvar)>);

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivateDone_Impl {
    fn ActivateCompleted(
        &self,
        _operation: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> WinResult<()> {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
        Ok(())
    }
}

/// VT_BLOB の PROPVARIANT と同じメモリ配置（x64/ARM64 で 24 バイト）。
#[repr(C)]
struct BlobVariant {
    vt: u16,
    reserved: [u16; 3],
    size: u32,
    data: *const u8,
}

unsafe fn activate_loopback(pid: u32, stop: &AtomicBool) -> WinResult<IAudioClient> {
    unsafe {
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        let variant = BlobVariant {
            vt: VT_BLOB,
            reserved: [0; 3],
            size: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            data: &params as *const _ as *const u8,
        };
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let handler: IActivateAudioInterfaceCompletionHandler = ActivateDone(done.clone()).into();
        let operation = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&variant as *const BlobVariant as *const PROPVARIANT),
            &handler,
        )?;
        // 停止要求にすぐ応じられるよう、短い間隔で区切って最大 5 秒待つ。
        let mut guard = done.0.lock().unwrap();
        let mut waited = 0;
        while !*guard {
            if waited >= 50 || stop.load(Ordering::SeqCst) {
                return Err(E_FAIL.into());
            }
            guard = done
                .1
                .wait_timeout(guard, Duration::from_millis(100))
                .unwrap()
                .0;
            waited += 1;
        }
        drop(guard);
        let mut hr = HRESULT(0);
        let mut unknown = None;
        operation.GetActivateResult(&mut hr, &mut unknown)?;
        hr.ok()?;
        unknown
            .ok_or_else(|| windows::core::Error::from(E_FAIL))?
            .cast()
    }
}

/// 会議アプリ（子プロセスを含むツリー）が出す音だけを録る。Windows 10 2004 以降。
unsafe fn process_loopback_once(pid: u32, holder: &Holder, stop: &AtomicBool) -> WinResult<()> {
    unsafe {
        // プロセスループバックはミックスフォーマットを問い合わせられないので自分で指定する。
        let format = Format {
            rate: 48_000,
            channels: 2,
            bits: 16,
            float: false,
        };
        let wave = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM,
            nChannels: format.channels,
            nSamplesPerSec: format.rate,
            nAvgBytesPerSec: format.rate * format.frame_bytes() as u32,
            nBlockAlign: format.frame_bytes() as u16,
            wBitsPerSample: format.bits,
            cbSize: 0,
        };
        let client = activate_loopback(pid, stop)?;
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            BUFFER_HNS,
            0,
            &wave,
            None,
        )?;
        meetrec_core::info!("相手音声のキャプチャ開始（プロセスループバック pid={pid}）");
        capture_loop(&client, format, SRC_APP, holder, stop, || true)
    }
}

/// 出力機器の音量の変化を受け取り、「0 から一気に上がった」（`output::is_jump`）ことと、
/// 「利用者が自分で音量を決めた」（`output::is_user_set`）ことだけを覚える。
/// 通知は OS のスレッドで届くので、ここでは印を付けるだけにして、音量の操作は録音スレッドでする。
#[implement(IAudioEndpointVolumeCallback)]
struct VolumeJump(Arc<JumpState>);

struct JumpState {
    /// 直前に通知された音量（f32 のビット列）。
    last: AtomicU32,
    jumped: AtomicBool,
    user_set: AtomicBool,
}

impl IAudioEndpointVolumeCallback_Impl for VolumeJump_Impl {
    fn OnNotify(&self, data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> WinResult<()> {
        // SAFETY: 呼び出しの間だけ有効なポインタを、その場で読むだけ。
        let Some(d) = (unsafe { data.as_ref() }) else {
            return Ok(());
        };
        let now = d.fMasterVolume;
        let prev = f32::from_bits(self.0.last.swap(now.to_bits(), Ordering::SeqCst));
        let muted = d.bMuted.as_bool();
        if output::is_jump(prev, now, muted) {
            self.0.jumped.store(true, Ordering::SeqCst);
        } else if output::is_user_set(prev, now, muted) {
            self.0.user_set.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// 録っている出力機器の消音と音量。いつ・何をするかは `core::output` が決め、ここは機器を読み書きする（D37）。
struct OutputVolume {
    volume: IAudioEndpointVolume,
    state: OutputUnmute,
    jump: Arc<JumpState>,
    callback: IAudioEndpointVolumeCallback,
    /// 会議に入ってしばらく経った（`Recorder::settle_output`）。以後は音量を 0 に戻さない。
    settled: Arc<AtomicBool>,
}

impl OutputVolume {
    unsafe fn new(volume: IAudioEndpointVolume, settled: Arc<AtomicBool>) -> OutputVolume {
        let jump = Arc::new(JumpState {
            // 0 が通知されるまでは「0 から上がった」と数えない。
            last: AtomicU32::new(1f32.to_bits()),
            jumped: AtomicBool::new(false),
            user_set: AtomicBool::new(false),
        });
        let callback: IAudioEndpointVolumeCallback = VolumeJump(jump.clone()).into();
        // 受け取れなくても、消音を外すことはできる（一気に上げられた音量を戻せないだけ）。
        if let Err(e) = unsafe { volume.RegisterControlChangeNotify(&callback) } {
            meetrec_core::warn!("出力機器の音量の変化を受け取れません: {e}");
        }
        OutputVolume {
            volume,
            state: OutputUnmute::default(),
            jump,
            callback,
            settled,
        }
    }

    /// 会議に入ってしばらく経っていたら、`core::output` に伝える（以後は音量を戻さない）。
    fn sync_settled(&mut self) {
        if self.settled.load(Ordering::SeqCst) {
            self.state.settle();
        }
    }

    /// 消音なら、音量を 0 にしてから消音を外す。切り替えたら true。
    unsafe fn unmute_if_muted(&mut self) -> WinResult<bool> {
        unsafe {
            if !self.volume.GetMute()?.as_bool() {
                return Ok(false);
            }
            let level = self.volume.GetMasterVolumeLevelScalar()?;
            // 先に音量を 0 にする（消音を先に外すと、その間だけ音が出る）。
            self.volume
                .SetMasterVolumeLevelScalar(0.0, std::ptr::null())?;
            self.volume.SetMute(false, std::ptr::null())?;
            // 消音にする前の利用者の操作は、数えない（ここから数え直す）。
            self.jump.user_set.store(false, Ordering::SeqCst);
            self.jump.jumped.store(false, Ordering::SeqCst);
            self.sync_settled();
            self.state.unmuted(level);
            Ok(true)
        }
    }

    /// 音量が 0 から一気に上げられていたら、`core::output` の決まりに従って 0 に戻す。
    unsafe fn revert_jump(&mut self) {
        self.sync_settled();
        // 利用者が自分で音量を決めていたら、以後は触らない（参加前に、聞くつもりで上げた場合など）。
        if self.jump.user_set.swap(false, Ordering::SeqCst) {
            self.state.user_set();
        }
        if !self.jump.jumped.swap(false, Ordering::SeqCst) || !self.state.jumped() {
            return;
        }
        match unsafe {
            self.volume
                .SetMasterVolumeLevelScalar(0.0, std::ptr::null())
        } {
            Ok(()) => {
                meetrec_core::info!("出力機器の音量が 0 から一気に上げられたので、0 に戻しました")
            }
            Err(e) => meetrec_core::warn!("出力機器の音量を 0 に戻せません: {e}"),
        }
    }

    /// 音量の変化の受け取りをやめ、消音と音量を元に戻す（利用者が自分で音量を上げていたら、戻さない）。
    unsafe fn finish(&mut self) {
        unsafe {
            let _ = self.volume.UnregisterControlChangeNotify(&self.callback);
            let now = self.volume.GetMasterVolumeLevelScalar().unwrap_or(f32::NAN);
            match self.state.finish(now) {
                Restore::Untouched => {}
                Restore::Keep => {
                    meetrec_core::info!("出力機器の音量が変えられていたので、消音は元に戻しません")
                }
                Restore::Remute(level) => {
                    // 先に消音にする（音量を先に戻すと、その間だけ音が出る）。
                    let muted = self.volume.SetMute(true, std::ptr::null());
                    let set = self
                        .volume
                        .SetMasterVolumeLevelScalar(level, std::ptr::null());
                    match muted.and(set) {
                        Ok(()) => meetrec_core::info!("出力機器の消音と音量を元に戻しました"),
                        Err(e) => meetrec_core::warn!("出力機器の消音を元に戻せません: {e}"),
                    }
                }
            }
        }
    }
}

/// PC から出る音すべて（会議アプリが音を出している出力機器のループバック）を録る。
/// プロセスループバックで録れないアプリ用（D36）。出力機器が変わったら Ok で戻る（開き直す）。
/// `unmute` なら、録っているあいだ、出力機器が消音なら消音を外して音量を 0 にする（D37。`main_hwnd` へ知らせる）。
unsafe fn system_loopback_once(
    root: u32,
    holder: &Holder,
    stop: &AtomicBool,
    unmute: bool,
    settled: &Arc<AtomicBool>,
    main_hwnd: isize,
) -> WinResult<()> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        // 会議アプリが音を出している機器。まだ出していなければ既定の機器（通話用 → 通常）。
        let pick = || -> WinResult<(IMMDevice, String)> {
            let device = match probe::render_device_of(root) {
                Some(device) => device,
                None => en
                    .GetDefaultAudioEndpoint(eRender, eCommunications)
                    .or_else(|_| en.GetDefaultAudioEndpoint(eRender, eConsole))?,
            };
            let id = device_id(&device)?;
            Ok((device, id))
        };
        let (device, id) = pick()?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let mix = client.GetMixFormat()?;
        let format = Format::parse(mix);
        let init = match format {
            Some(_) => client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                BUFFER_HNS,
                0,
                mix,
                None,
            ),
            None => Err(E_FAIL.into()),
        };
        CoTaskMemFree(Some(mix as *const _));
        init?;
        let format = format.unwrap();
        meetrec_core::info!("相手音声のキャプチャ開始（PC から出る音すべて）: {format:?}");
        // 消音を外せなくても（機器が対応していないなど）、録音は続ける。
        // 設定（`audio.unmute_output = false`）で止められているときは、出力機器の設定を読みも書きもしない。
        let mut endpoint = if unmute {
            device
                .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
                .ok()
                .map(|volume| OutputVolume::new(volume, settled.clone()))
        } else {
            meetrec_core::info!(
                "設定により、出力機器の消音は外しません（消音のあいだ、相手の音は録れません）"
            );
            None
        };
        let unmute_if_muted = |endpoint: &mut Option<OutputVolume>| {
            if let Some(e) = endpoint {
                match e.unmute_if_muted() {
                    Ok(true) => {
                        meetrec_core::info!("出力機器の消音を外して、音量を 0 にしました");
                        post_recorder_event(main_hwnd, RecorderEvent::OutputUnmuted);
                    }
                    Ok(false) => {}
                    Err(err) => meetrec_core::warn!("出力機器の消音を外せません: {err}"),
                }
            }
        };
        unmute_if_muted(&mut endpoint);
        // 出力機器の変更（イヤホン接続など）と消音を 2 秒ごとに確認する。機器が変わったら開き直す。
        // 一気に上げられた音量は、待たずに戻す（音が出る時間を短くする）。
        let mut last_check = Instant::now();
        let result = capture_loop(&client, format, SRC_APP, holder, stop, || {
            if let Some(e) = &mut endpoint {
                e.revert_jump();
            }
            if last_check.elapsed() < Duration::from_secs(2) {
                return true;
            }
            last_check = Instant::now();
            unmute_if_muted(&mut endpoint);
            pick().is_ok_and(|(_, current)| current == id)
        });
        if let Some(e) = &mut endpoint {
            e.finish();
        }
        result
    }
}

/// 相手の音の録り方。
pub enum Loopback {
    /// 会議アプリ（子プロセスを含む）が出す音だけ。
    Process,
    /// PC から出る音すべて（プロセス単位では録れないアプリ用。D36）。
    System {
        /// 出力機器が消音なら、消音を外して音量を 0 にする（D37）。
        unmute: bool,
        /// 会議に入ってしばらく経った（以後は音量を 0 に戻さない）。
        settled: Arc<AtomicBool>,
        /// 消音を外したことを知らせる先。
        main_hwnd: isize,
    },
}

/// 相手の音を録る。
pub fn run_loopback(holder: Arc<Holder>, stop: Arc<AtomicBool>, pid: u32, how: Loopback) {
    sys::com_init_mta();
    // デバイス無効化などで止まっても、録画が続く限り開き直す（マイクと同じ方針）。
    let mut failures = 0u32;
    while !stop.load(Ordering::SeqCst) {
        let result = unsafe {
            match &how {
                Loopback::Process => process_loopback_once(pid, &holder, &stop),
                Loopback::System {
                    unmute,
                    settled,
                    main_hwnd,
                } => system_loopback_once(pid, &holder, &stop, *unmute, settled, *main_hwnd),
            }
        };
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match result {
            // 出力機器の変更：待たずに開き直す。
            Ok(()) => continue,
            Err(e) => {
                // 未対応の OS などでは相手音声なしで続行する。ログは最初と、以後は間引いて出す。
                if failures.is_multiple_of(20) {
                    meetrec_core::warn!("相手音声を録音できません（再試行します）: {e}");
                }
                failures += 1;
            }
        }
        // 3 秒おいて再試行（停止要求には 100ms 以内に応じる）。
        for _ in 0..30 {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

// ---------------------------------------------------------------------------
// マイク
// ---------------------------------------------------------------------------

unsafe fn default_mic(en: &IMMDeviceEnumerator) -> WinResult<(IMMDevice, String)> {
    unsafe {
        let device = en
            .GetDefaultAudioEndpoint(eCapture, eCommunications)
            .or_else(|_| en.GetDefaultAudioEndpoint(eCapture, eConsole))?;
        let id = device_id(&device)?;
        Ok((device, id))
    }
}

/// 機器の識別子（機器が変わったかを比べるため）。
unsafe fn device_id(device: &IMMDevice) -> WinResult<String> {
    unsafe {
        let id = device.GetId()?;
        let name = from_wide(id.as_wide());
        CoTaskMemFree(Some(id.0 as *const _));
        Ok(name)
    }
}

unsafe fn mic_once(en: &IMMDeviceEnumerator, holder: &Holder, stop: &AtomicBool) -> WinResult<()> {
    unsafe {
        let (device, id) = default_mic(en)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let mix = client.GetMixFormat()?;
        let format = Format::parse(mix);
        let init = match format {
            Some(_) => client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                BUFFER_HNS,
                0,
                mix,
                None,
            ),
            None => Err(E_FAIL.into()),
        };
        CoTaskMemFree(Some(mix as *const _));
        init?;
        let format = format.unwrap();
        meetrec_core::info!("マイクのキャプチャ開始: {format:?}");
        // 既定デバイスの変更（イヤホン接続など）を 2 秒ごとに確認し、変わったら開き直す。
        let mut last_check = Instant::now();
        capture_loop(&client, format, SRC_MIC, holder, stop, || {
            if last_check.elapsed() < Duration::from_secs(2) {
                return true;
            }
            last_check = Instant::now();
            match default_mic(en) {
                Ok((_, current)) => current == id,
                Err(_) => false,
            }
        })
    }
}

/// 既定のマイクを録る。デバイス切替・取り外し時は開き直す。
pub fn run_mic(holder: Arc<Holder>, stop: Arc<AtomicBool>) {
    sys::com_init_mta();
    let mut last_error = String::new();
    while !stop.load(Ordering::SeqCst) {
        let result = unsafe {
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .and_then(|en| mic_once(&en, &holder, &stop))
        };
        match result {
            // 録画の停止で抜けた場合は、開き直さない（ログにも出さない）。
            Ok(()) if stop.load(Ordering::SeqCst) => return,
            Ok(()) => meetrec_core::info!("マイクを開き直します（既定デバイスの変更）"),
            Err(e) => {
                // 同じエラーを繰り返しログに出さない。
                let msg = e.to_string();
                if msg != last_error {
                    meetrec_core::warn!("マイクを録音できません（再試行します）: {msg}");
                    last_error = msg;
                }
                for _ in 0..20 {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}
