//! Microsoft AAC エンコーダ MFT（同期 MFT）による `AacEncoder` 実装。

use meetrec_core::audio::{DEFAULT_ASC, FRAME_LEN, OUT_RATE};
use meetrec_core::session::AacEncoder;
use windows::core::Result as WinResult;
use windows::Win32::Media::MediaFoundation::{
    AACMFTEncoder, IMFMediaType, IMFSample, IMFTransform, MFAudioFormat_AAC, MFAudioFormat_PCM,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Audio,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_MT_AAC_PAYLOAD_TYPE, MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_MT_USER_DATA,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

use crate::sys;

/// HEAACWAVEINFO のうち WAVEFORMATEX より後ろの固定部分の長さ。この後ろが AudioSpecificConfig。
const HEAAC_INFO_TAIL: usize = 12;

struct MfAac {
    mft: IMFTransform,
    asc: Vec<u8>,
    out_size: u32,
    /// 入力サンプル時刻（100ns）。
    time: i64,
}

/// セッションスレッド上で呼ばれるファクトリ。
pub fn create(bitrate: u32) -> Result<Box<dyn AacEncoder>, String> {
    sys::com_init_mta();
    unsafe { MfAac::new(bitrate) }
        .map(|e| Box::new(e) as Box<dyn AacEncoder>)
        .map_err(|e| format!("AAC エンコーダ MFT: {e}"))
}

unsafe fn pcm_type() -> WinResult<IMFMediaType> {
    let t = MFCreateMediaType()?;
    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
    t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
    t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
    t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, OUT_RATE)?;
    t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
    t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
    t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, OUT_RATE * 4)?;
    Ok(t)
}

unsafe fn aac_type(bytes_per_sec: u32) -> WinResult<IMFMediaType> {
    let t = MFCreateMediaType()?;
    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
    t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
    t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
    t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, OUT_RATE)?;
    t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
    t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bytes_per_sec)?;
    // 0 = raw AAC（ADTS ヘッダなし）。
    t.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
    Ok(t)
}

impl MfAac {
    unsafe fn new(bitrate: u32) -> WinResult<MfAac> {
        let mft: IMFTransform = CoCreateInstance(&AACMFTEncoder, None, CLSCTX_INPROC_SERVER)?;
        // MS AAC エンコーダが受け付けるのは 96/128/160/192 kbps のみ。最も近い値にする。
        let bytes = [12_000u32, 16_000, 20_000, 24_000]
            .into_iter()
            .min_by_key(|b| (*b as i64 * 8 - bitrate as i64).abs())
            .unwrap_or(12_000);
        mft.SetInputType(0, &pcm_type()?, 0)?;
        mft.SetOutputType(0, &aac_type(bytes)?, 0)?;

        // AudioSpecificConfig は確定した出力タイプの MF_MT_USER_DATA の末尾にある。
        let mut asc = DEFAULT_ASC.to_vec();
        if let Ok(out) = mft.GetOutputCurrentType(0) {
            if let Ok(size) = out.GetBlobSize(&MF_MT_USER_DATA) {
                let mut blob = vec![0u8; size as usize];
                if out.GetBlob(&MF_MT_USER_DATA, &mut blob, None).is_ok()
                    && blob.len() >= HEAAC_INFO_TAIL + 2
                {
                    asc = blob[HEAAC_INFO_TAIL..].to_vec();
                }
            }
        }
        let out_size = mft
            .GetOutputStreamInfo(0)
            .map(|i| i.cbSize)
            .unwrap_or(0)
            .max(8192);
        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        Ok(MfAac {
            mft,
            asc,
            out_size,
            time: 0,
        })
    }

    unsafe fn input_sample(&mut self, pcm: &[f32]) -> WinResult<IMFSample> {
        let bytes = (pcm.len() * 2) as u32;
        let buffer = MFCreateMemoryBuffer(bytes)?;
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        let dst = std::slice::from_raw_parts_mut(ptr as *mut i16, pcm.len());
        for (d, s) in dst.iter_mut().zip(pcm) {
            *d = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        }
        buffer.Unlock()?;
        buffer.SetCurrentLength(bytes)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        let frames = (pcm.len() / 2) as i64;
        let duration = frames * 10_000_000 / OUT_RATE as i64;
        sample.SetSampleTime(self.time)?;
        sample.SetSampleDuration(duration)?;
        self.time += duration;
        Ok(sample)
    }

    /// 出せるだけ出力を取り出す。
    unsafe fn drain_output(&mut self, out: &mut Vec<Vec<u8>>) -> WinResult<()> {
        loop {
            let sample = MFCreateSample()?;
            let buffer = MFCreateMemoryBuffer(self.out_size)?;
            sample.AddBuffer(&buffer)?;
            let mut data = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(Some(sample)),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.mft.ProcessOutput(0, &mut data, &mut status);
            let sample = std::mem::ManuallyDrop::take(&mut data[0].pSample);
            std::mem::ManuallyDrop::drop(&mut data[0].pEvents);
            match result {
                Ok(()) => {
                    let Some(sample) = sample else { continue };
                    let buf = sample.ConvertToContiguousBuffer()?;
                    let mut ptr = std::ptr::null_mut();
                    let mut len = 0u32;
                    buf.Lock(&mut ptr, None, Some(&mut len))?;
                    if len > 0 {
                        out.push(std::slice::from_raw_parts(ptr, len as usize).to_vec());
                    }
                    buf.Unlock()?;
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }
}

impl AacEncoder for MfAac {
    fn asc(&self) -> Vec<u8> {
        self.asc.clone()
    }

    fn encode(&mut self, pcm: &[f32]) -> Result<Vec<Vec<u8>>, String> {
        debug_assert_eq!(pcm.len(), FRAME_LEN);
        let mut out = Vec::new();
        unsafe {
            let sample = self.input_sample(pcm).map_err(|e| e.to_string())?;
            self.mft
                .ProcessInput(0, &sample, 0)
                .map_err(|e| e.to_string())?;
            self.drain_output(&mut out).map_err(|e| e.to_string())?;
        }
        Ok(out)
    }

    fn finish(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
            let _ = self.drain_output(&mut out);
        }
        out
    }
}
