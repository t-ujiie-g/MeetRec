//! AudioToolbox の AudioConverter による AAC-LC エンコーダ（48kHz ステレオ）。
//! セッションスレッド上で作られ、そのスレッドだけで使われる。

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::NonNull;

use meetrec_core::audio::{FRAME, FRAME_LEN, OUT_RATE};
use meetrec_core::session::AacEncoder;
use objc2_audio_toolbox::{
    kAudioConverterCompressionMagicCookie, kAudioConverterEncodeBitRate,
    kAudioConverterPropertyMaximumOutputPacketSize, AudioConverterDispose,
    AudioConverterFillComplexBuffer, AudioConverterGetProperty, AudioConverterNew,
    AudioConverterRef, AudioConverterSetProperty,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked, kAudioFormatLinearPCM,
    kAudioFormatMPEG4AAC, AudioBuffer, AudioBufferList, AudioStreamBasicDescription,
    AudioStreamPacketDescription,
};

/// 入力コールバックが「今は渡せるデータがない」ことを伝えるための独自ステータス。
const NO_MORE_INPUT: i32 = 0x6d72_6e6f; // 'mrno'

struct Input {
    data: *const f32,
    frames: u32,
    eof: bool,
}

pub struct MacAac {
    conv: AudioConverterRef,
    asc: Vec<u8>,
    pcm: Vec<f32>,
    out: Vec<u8>,
}

unsafe extern "C-unwind" fn input_proc(
    _conv: AudioConverterRef,
    io_packets: NonNull<u32>,
    io_data: NonNull<AudioBufferList>,
    _desc: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> i32 {
    // SAFETY: user は fill() のスタック上の Input を指し、呼び出し中のみ使われる。
    let input = unsafe { &mut *(user as *mut Input) };
    if input.frames == 0 {
        unsafe { *io_packets.as_ptr() = 0 };
        // EOF なら noErr で 0 パケットを返す（エンコーダが残りを吐き出す）。
        return if input.eof { 0 } else { NO_MORE_INPUT };
    }
    // SAFETY: io_data は少なくとも 1 バッファぶんの領域を持つ（interleaved 入力）。
    unsafe {
        let abl = io_data.as_ptr();
        (*abl).mNumberBuffers = 1;
        (*abl).mBuffers[0] = AudioBuffer {
            mNumberChannels: 2,
            mDataByteSize: input.frames * 2 * size_of::<f32>() as u32,
            mData: input.data as *mut c_void,
        };
        *io_packets.as_ptr() = input.frames;
    }
    input.frames = 0;
    0
}

/// esds 形式の magic cookie から AudioSpecificConfig（タグ 0x05 の中身）を取り出す。
fn asc_from_cookie(cookie: &[u8]) -> Option<Vec<u8>> {
    let mut i = 0;
    while i < cookie.len() {
        let tag = cookie[i];
        i += 1;
        let mut len = 0usize;
        // 長さは 7 ビットずつ、最上位ビットが継続フラグ。
        for _ in 0..4 {
            let b = *cookie.get(i)?;
            i += 1;
            len = (len << 7) | (b & 0x7f) as usize;
            if b & 0x80 == 0 {
                break;
            }
        }
        match tag {
            0x03 => i += 3,  // ES_ID(2) + flags(1)。中身（0x04）へ進む。
            0x04 => i += 13, // DecoderConfig の固定部。中身（0x05）へ進む。
            0x05 => return cookie.get(i..i + len).map(<[u8]>::to_vec).filter(|a| a.len() >= 2),
            _ => i += len,
        }
    }
    None
}

impl MacAac {
    pub fn new(bitrate: u32) -> Result<MacAac, String> {
        let mut src = AudioStreamBasicDescription {
            mSampleRate: OUT_RATE as f64,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: 8,
            mFramesPerPacket: 1,
            mBytesPerFrame: 8,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        let mut dst = AudioStreamBasicDescription {
            mSampleRate: OUT_RATE as f64,
            mFormatID: kAudioFormatMPEG4AAC,
            mFormatFlags: 0, // 0 = 既定（AAC-LC）
            mBytesPerPacket: 0,
            mFramesPerPacket: FRAME as u32,
            mBytesPerFrame: 0,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 0,
            mReserved: 0,
        };
        let mut conv: AudioConverterRef = std::ptr::null_mut();
        // SAFETY: 有効な ASBD と出力ポインタを渡す。
        let status = unsafe {
            AudioConverterNew(NonNull::from(&mut src), NonNull::from(&mut dst), NonNull::from(&mut conv))
        };
        if status != 0 || conv.is_null() {
            return Err(format!("AudioConverterNew 失敗 (OSStatus {status})"));
        }
        let mut this = MacAac { conv, asc: vec![0x11, 0x90], pcm: vec![0.0; FRAME_LEN], out: Vec::new() };
        // SAFETY: conv は有効。各プロパティは u32 またはバイト列。
        unsafe {
            let mut rate = bitrate;
            let st = AudioConverterSetProperty(
                conv,
                kAudioConverterEncodeBitRate,
                size_of::<u32>() as u32,
                NonNull::from(&mut rate).cast(),
            );
            if st != 0 {
                meetrec_core::warn!("AAC ビットレート {bitrate} を設定できません (OSStatus {st})");
            }
            let mut max_packet = 0u32;
            let mut size = size_of::<u32>() as u32;
            let st = AudioConverterGetProperty(
                conv,
                kAudioConverterPropertyMaximumOutputPacketSize,
                NonNull::from(&mut size),
                NonNull::from(&mut max_packet).cast(),
            );
            let max_packet = if st == 0 && max_packet > 0 { max_packet as usize } else { 8192 };
            this.out = vec![0u8; max_packet.max(2048)];

            let mut cookie = [0u8; 256];
            let mut size = cookie.len() as u32;
            let st = AudioConverterGetProperty(
                conv,
                kAudioConverterCompressionMagicCookie,
                NonNull::from(&mut size),
                NonNull::from(&mut cookie).cast(),
            );
            if st == 0 {
                if let Some(asc) = asc_from_cookie(&cookie[..(size as usize).min(cookie.len())]) {
                    this.asc = asc;
                }
            }
        }
        Ok(this)
    }

    /// 入力を与えて、出せるだけパケットを取り出す。
    fn fill(&mut self, frames: u32, eof: bool) -> Result<Vec<Vec<u8>>, String> {
        let mut input = Input { data: self.pcm.as_ptr(), frames, eof };
        let mut packets = Vec::new();
        // 1 回の入力で出るのは高々数パケット。念のため上限を置く。
        for _ in 0..16 {
            let mut n = 1u32;
            let mut desc = AudioStreamPacketDescription {
                mStartOffset: 0,
                mVariableFramesInPacket: 0,
                mDataByteSize: 0,
            };
            let mut abl = AudioBufferList {
                mNumberBuffers: 1,
                mBuffers: [AudioBuffer {
                    mNumberChannels: 2,
                    mDataByteSize: self.out.len() as u32,
                    mData: self.out.as_mut_ptr().cast(),
                }],
            };
            // SAFETY: すべてのポインタはこの呼び出しの間有効。入力は self.pcm（次の encode まで不変）。
            let status = unsafe {
                AudioConverterFillComplexBuffer(
                    self.conv,
                    Some(input_proc),
                    &mut input as *mut Input as *mut c_void,
                    NonNull::from(&mut n),
                    NonNull::from(&mut abl),
                    &mut desc,
                )
            };
            if n > 0 {
                let len = (abl.mBuffers[0].mDataByteSize as usize).min(self.out.len());
                let start = (desc.mStartOffset.max(0) as usize).min(len);
                if len > start {
                    packets.push(self.out[start..len].to_vec());
                }
            }
            if status == NO_MORE_INPUT || n == 0 {
                break;
            }
            if status != 0 {
                return Err(format!("AudioConverterFillComplexBuffer 失敗 (OSStatus {status})"));
            }
        }
        Ok(packets)
    }
}

impl AacEncoder for MacAac {
    fn asc(&self) -> Vec<u8> {
        self.asc.clone()
    }

    fn encode(&mut self, pcm: &[f32]) -> Result<Vec<Vec<u8>>, String> {
        let n = pcm.len().min(FRAME_LEN);
        self.pcm[..n].copy_from_slice(&pcm[..n]);
        self.pcm[n..].fill(0.0);
        self.fill(FRAME as u32, false)
    }

    fn finish(&mut self) -> Vec<Vec<u8>> {
        self.fill(0, true).unwrap_or_default()
    }
}

impl Drop for MacAac {
    fn drop(&mut self) {
        // SAFETY: new で作った有効なコンバータを 1 度だけ破棄する。
        unsafe { AudioConverterDispose(self.conv) };
    }
}
