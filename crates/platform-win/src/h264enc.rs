//! Media Foundation の H.264 エンコーダ MFT。
//! ハードウェア（非同期 MFT、D3D11 テクスチャ入力）を優先し、なければ
//! Microsoft のソフトウェアエンコーダ（同期 MFT）にフォールバックする。

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use meetrec_core::h264;
use windows::core::{Interface, Result as WinResult, GUID};
use windows::Win32::Foundation::{E_FAIL, VARIANT_BOOL};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Main, CLSID_MSH264EncoderMFT,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize, CODECAPI_AVLowLatencyMode,
    ICodecAPI, IMF2DBuffer, IMFActivate, IMFDXGIDeviceManager, IMFMediaEventGenerator,
    IMFMediaType, IMFSample, IMFTransform, METransformDrainComplete, METransformHaveOutput,
    METransformNeedInput, MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT, MF_E_NO_EVENTS_AVAILABLE,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    /// キーフレーム間隔（フレーム数）。
    pub gop: u32,
}

/// 出力の受け取り先：(pts_ns, Annex B の 1 アクセスユニット)。
pub type Sink<'a> = &'a mut dyn FnMut(u64, &[u8]);

pub struct H264Encoder {
    mft: IMFTransform,
    activate: Option<IMFActivate>,
    /// 非同期 MFT のイベント源。同期 MFT では None。
    events: Option<IMFMediaEventGenerator>,
    need_input: u32,
    in_flight: u32,
    drained: bool,
    provides_samples: bool,
    out_size: u32,
    seq_header: Option<Vec<u8>>,
    cfg: Config,
    pub hardware: bool,
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

unsafe fn video_type(cfg: &Config, subtype: &GUID) -> WinResult<IMFMediaType> {
    let t = MFCreateMediaType()?;
    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
    t.SetUINT64(&MF_MT_FRAME_SIZE, pack(cfg.width, cfg.height))?;
    t.SetUINT64(&MF_MT_FRAME_RATE, pack(cfg.fps, 1))?;
    t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
    t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    Ok(t)
}

unsafe fn set_u32(api: &ICodecAPI, key: &GUID, value: u32) {
    let mut v = VARIANT::default();
    (*v.Anonymous.Anonymous).vt = VT_UI4;
    (*v.Anonymous.Anonymous).Anonymous.ulVal = value;
    let _ = api.SetValue(key, &v);
}

unsafe fn set_bool(api: &ICodecAPI, key: &GUID, value: bool) {
    let mut v = VARIANT::default();
    (*v.Anonymous.Anonymous).vt = VT_BOOL;
    (*v.Anonymous.Anonymous).Anonymous.boolVal = VARIANT_BOOL(if value { -1 } else { 0 });
    let _ = api.SetValue(key, &v);
}

impl H264Encoder {
    /// ハードウェアエンコーダを列挙し、最初に設定できたものを使う。
    pub fn hardware(manager: &IMFDXGIDeviceManager, cfg: Config) -> WinResult<H264Encoder> {
        unsafe {
            let output = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            };
            let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                None,
                Some(&output),
                &mut list,
                &mut count,
            )?;
            // 配列の所有権を受け取り、配列自体は解放する。
            let activates: Vec<IMFActivate> = (0..count as usize)
                .filter_map(|i| std::ptr::read(list.add(i)))
                .collect();
            CoTaskMemFree(Some(list as *const _));

            let mut last = windows::core::Error::from(E_FAIL);
            for activate in activates {
                let result = activate
                    .ActivateObject::<IMFTransform>()
                    .and_then(|mft| Self::configure(mft, Some(manager), cfg));
                match result {
                    Ok(mut enc) => {
                        enc.activate = Some(activate);
                        enc.hardware = true;
                        return Ok(enc);
                    }
                    Err(e) => {
                        let _ = activate.ShutdownObject();
                        last = e;
                    }
                }
            }
            Err(last)
        }
    }

    /// Microsoft のソフトウェアエンコーダ。
    pub fn software(cfg: Config) -> WinResult<H264Encoder> {
        unsafe {
            let mft: IMFTransform =
                CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)?;
            Self::configure(mft, None, cfg)
        }
    }

    unsafe fn configure(
        mft: IMFTransform,
        manager: Option<&IMFDXGIDeviceManager>,
        cfg: Config,
    ) -> WinResult<H264Encoder> {
        let mut is_async = false;
        if let Ok(attrs) = mft.GetAttributes() {
            is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
            if is_async {
                attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            }
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        }
        if let Some(manager) = manager {
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
        }
        // エンコーダによっては未対応の項目があるので、個々の失敗は無視する。
        if let Ok(api) = mft.cast::<ICodecAPI>() {
            set_bool(&api, &CODECAPI_AVLowLatencyMode, true);
            set_u32(
                &api,
                &CODECAPI_AVEncCommonRateControlMode,
                eAVEncCommonRateControlMode_CBR.0 as u32,
            );
            set_u32(&api, &CODECAPI_AVEncCommonMeanBitRate, cfg.bitrate);
            set_u32(&api, &CODECAPI_AVEncMPVGOPSize, cfg.gop);
            set_u32(&api, &CODECAPI_AVEncMPVDefaultBPictureCount, 0);
        }

        // エンコーダは出力タイプを先に設定する。
        let out_type = video_type(&cfg, &MFVideoFormat_H264)?;
        out_type.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate)?;
        out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
        mft.SetOutputType(0, &out_type, 0)?;
        mft.SetInputType(0, &video_type(&cfg, &MFVideoFormat_NV12)?, 0)?;

        let events = if is_async {
            Some(mft.cast::<IMFMediaEventGenerator>()?)
        } else {
            None
        };
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

        let info = mft.GetOutputStreamInfo(0)?;
        let mut enc = H264Encoder {
            mft,
            activate: None,
            events,
            need_input: 0,
            in_flight: 0,
            drained: false,
            provides_samples: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0,
            out_size: info.cbSize.max(cfg.width * cfg.height * 3 / 2).max(1 << 16),
            seq_header: None,
            cfg,
            hardware: false,
        };
        enc.load_seq_header();
        Ok(enc)
    }

    /// 出力タイプから SPS/PPS（Annex B）を読む。最初の出力後にしか入らない実装もある。
    unsafe fn load_seq_header(&mut self) {
        if let Ok(t) = self.mft.GetOutputCurrentType(0) {
            if let Ok(size) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) {
                let mut blob = vec![0u8; size as usize];
                if size > 0
                    && t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None)
                        .is_ok()
                {
                    self.seq_header = Some(blob);
                }
            }
        }
    }

    /// まだ出力されていない入力があるか（イベントをこまめに見る必要があるか）。
    pub fn pending(&self) -> bool {
        self.events.is_some() && self.in_flight > 0
    }

    unsafe fn make_sample(&self, texture: &ID3D11Texture2D, pts_ns: u64) -> WinResult<IMFSample> {
        let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
        // 一部のエンコーダは CurrentLength が 0 のバッファを空とみなす。
        if let Ok(len) = buffer
            .cast::<IMF2DBuffer>()
            .and_then(|b| b.GetContiguousLength())
        {
            let _ = buffer.SetCurrentLength(len);
        }
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime((pts_ns / 100) as i64)?;
        sample.SetSampleDuration(10_000_000 / self.cfg.fps.max(1) as i64)?;
        Ok(sample)
    }

    /// 1 フレームをエンコーダへ渡す。エンコーダが受け付けず捨てた場合は Ok(false)。
    pub fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        pts_ns: u64,
        sink: Sink,
    ) -> WinResult<bool> {
        unsafe {
            let sample = self.make_sample(texture, pts_ns)?;
            if self.events.is_some() {
                // 非同期 MFT：NeedInput イベントを受け取ってから入力する。
                let deadline = Instant::now() + Duration::from_millis(200);
                loop {
                    self.pump(sink)?;
                    if self.need_input > 0 {
                        break;
                    }
                    if Instant::now() > deadline {
                        return Ok(false);
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                self.need_input -= 1;
                self.mft.ProcessInput(0, &sample, 0)?;
                self.in_flight += 1;
                self.pump(sink)?;
            } else {
                self.mft.ProcessInput(0, &sample, 0)?;
                while self.output_once(sink)? {}
            }
            Ok(true)
        }
    }

    /// 非同期 MFT のイベントを処理し、出来上がった出力を取り出す（ブロックしない）。
    pub fn pump(&mut self, sink: Sink) -> WinResult<()> {
        let Some(events) = self.events.clone() else {
            return Ok(());
        };
        unsafe {
            loop {
                let event = match events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                    Ok(e) => e,
                    Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                    Err(e) => return Err(e),
                };
                let kind = event.GetType()?;
                if kind == METransformNeedInput.0 as u32 {
                    self.need_input += 1;
                } else if kind == METransformHaveOutput.0 as u32 {
                    self.output_once(sink)?;
                    self.in_flight = self.in_flight.saturating_sub(1);
                } else if kind == METransformDrainComplete.0 as u32 {
                    self.drained = true;
                }
            }
        }
    }

    /// 出力を 1 つ取り出す。もう出力がなければ Ok(false)。
    unsafe fn output_once(&mut self, sink: Sink) -> WinResult<bool> {
        for _ in 0..2 {
            let sample = if self.provides_samples {
                None
            } else {
                let sample = MFCreateSample()?;
                sample.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
                Some(sample)
            };
            let mut data = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.mft.ProcessOutput(0, &mut data, &mut status);
            let sample = ManuallyDrop::take(&mut data[0].pSample);
            ManuallyDrop::drop(&mut data[0].pEvents);
            match result {
                Ok(()) => {
                    if let Some(sample) = sample {
                        self.deliver(&sample, sink)?;
                    }
                    return Ok(true);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // 出力タイプの再交渉を求められた：提示されたタイプを受け入れてやり直す。
                    let t = self.mft.GetOutputAvailableType(0, 0)?;
                    self.mft.SetOutputType(0, &t, 0)?;
                    self.load_seq_header();
                }
                Err(e) => return Err(e),
            }
        }
        Ok(false)
    }

    unsafe fn deliver(&mut self, sample: &IMFSample, sink: Sink) -> WinResult<()> {
        let pts_ns = sample.GetSampleTime().unwrap_or(0).max(0) as u64 * 100;
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        let data = std::slice::from_raw_parts(ptr, len as usize);
        // キーフレームに SPS がない実装では、出力タイプのシーケンスヘッダを前に付ける。
        let nals = h264::nal_units(data);
        let key = nals.iter().any(|n| n[0] & 0x1f == 5);
        let has_sps = nals.iter().any(|n| n[0] & 0x1f == 7);
        if key && !has_sps {
            if self.seq_header.is_none() {
                self.load_seq_header();
            }
            match &self.seq_header {
                Some(header) => {
                    let mut joined = header.clone();
                    joined.extend_from_slice(data);
                    sink(pts_ns, &joined);
                }
                None => sink(pts_ns, data),
            }
        } else if !data.is_empty() {
            sink(pts_ns, data);
        }
        buffer.Unlock()
    }

    /// 残りの出力を排出する（停止時）。
    pub fn finish(&mut self, sink: Sink) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            if self
                .mft
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
                .is_err()
            {
                return;
            }
            if self.events.is_some() {
                let deadline = Instant::now() + Duration::from_millis(1000);
                while !self.drained && Instant::now() < deadline {
                    if self.pump(sink).is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            } else {
                while let Ok(true) = self.output_once(sink) {}
            }
        }
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        // ハードウェア MFT は明示的にシャットダウンしてドライバ側の資源を返す。
        if let Some(activate) = self.activate.take() {
            unsafe {
                let _ = activate.ShutdownObject();
            }
        }
    }
}
