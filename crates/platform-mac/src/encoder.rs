//! VideoToolbox による H.264 エンコード。IOSurface 上のフレームをそのまま渡し、
//! 出力（AVCC）を `Session` へ流す。

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use meetrec_core::platform::{RecordSettings, NS_PER_SEC};
use meetrec_core::session::Session;
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    kCMSampleAttachmentKey_NotSync, kCMVideoCodecType_H264, CMSampleBuffer, CMTime, CMTimeFlags,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
};
use objc2_core_video::CVImageBuffer;
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTProfileLevel_H264_High_AutoLevel,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, VTCompressionSession,
    VTEncodeInfoFlags, VTSessionSetProperty,
};

/// キャプチャの PTS とホスト時計の差がこれ以上なら、別の時計とみなして補正する。
const CLOCK_MISMATCH_NS: i64 = 2 * NS_PER_SEC as i64;

/// CMTime をナノ秒にする（無効な時刻は None）。
pub fn cmtime_ns(t: CMTime) -> Option<u64> {
    if t.timescale <= 0 || t.value < 0 || !t.flags.contains(CMTimeFlags::Valid) {
        return None;
    }
    Some((t.value as i128 * NS_PER_SEC as i128 / t.timescale as i128) as u64)
}

/// `value / timescale` 秒を表す有効な CMTime。
pub fn cmtime(value: i64, timescale: i32) -> CMTime {
    CMTime {
        value,
        timescale,
        flags: CMTimeFlags::Valid,
        epoch: 0,
    }
}

/// kCMTimeInvalid（長さ・期限を指定しないことを表す）。
const CMTIME_INVALID: CMTime = CMTime {
    value: 0,
    timescale: 0,
    flags: CMTimeFlags(0),
    epoch: 0,
};

/// 出力コールバックに渡すコンテキスト。エンコーダが生きている間だけ有効。
struct Ctx {
    session: Arc<Session>,
    width: u32,
    height: u32,
    errors: AtomicU64,
    clock: Arc<ClockOffset>,
}

/// キャプチャの PTS とセッション時計（`sys::now_ns`）の差の補正。
/// 未検証: ScreenCaptureKit の PTS は mach_absolute_time と同じホスト時計の想定。
/// 最初のサンプルで `CLOCK_MISMATCH_NS` 以上ずれていたら別の時計とみなし、以後その差を足して使う。
pub struct ClockOffset(AtomicI64);

impl ClockOffset {
    const UNSET: i64 = i64::MIN;

    pub fn new() -> ClockOffset {
        ClockOffset(AtomicI64::new(Self::UNSET))
    }

    /// サンプル受信時（キャプチャキュー上）に呼ぶ。
    pub fn observe(&self, pts_ns: u64, now_ns: u64) {
        if self.0.load(Ordering::Relaxed) != Self::UNSET {
            return;
        }
        let diff = now_ns as i64 - pts_ns as i64;
        let offset = if diff.abs() < CLOCK_MISMATCH_NS {
            0
        } else {
            diff
        };
        if offset != 0 {
            meetrec_core::warn!("キャプチャの時計がホスト時計と {diff}ns ずれています。補正します");
        }
        let _ = self
            .0
            .compare_exchange(Self::UNSET, offset, Ordering::Relaxed, Ordering::Relaxed);
    }

    pub fn apply(&self, pts_ns: u64) -> u64 {
        match self.0.load(Ordering::Relaxed) {
            Self::UNSET | 0 => pts_ns,
            off => (pts_ns as i64).saturating_add(off).max(0) as u64,
        }
    }
}

pub struct VideoEncoder {
    vt: CFRetained<VTCompressionSession>,
    ctx: *mut Ctx,
    pub hardware: bool,
}

// SAFETY: VTCompressionSession はスレッドセーフ。Ctx は Arc<Session>（Sync）と原子変数のみ。
unsafe impl Send for VideoEncoder {}

unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    _source: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sbuf: *mut CMSampleBuffer,
) {
    // SAFETY: refcon は VideoEncoder が所有する Ctx。invalidate 後にコールバックは来ない。
    let ctx = unsafe { &*(refcon as *const Ctx) };
    let Some(sbuf) = (unsafe { sbuf.as_ref() }) else {
        return;
    };
    if status != 0 {
        if ctx.errors.fetch_add(1, Ordering::Relaxed) == 0 {
            meetrec_core::warn!("映像エンコード失敗 (OSStatus {status})");
        }
        return;
    }
    // SAFETY: 以下は有効な CMSampleBuffer に対する読み取りのみ。
    unsafe {
        let Some(pts) = cmtime_ns(sbuf.presentation_time_stamp()) else {
            return;
        };
        let Some(block) = sbuf.data_buffer() else {
            return;
        };
        let len = block.data_length();
        if len == 0 {
            return;
        }
        let mut data = vec![0u8; len];
        if block.copy_data_bytes(0, len, NonNull::new(data.as_mut_ptr()).unwrap().cast()) != 0 {
            return;
        }
        // NotSync が無い（または false）ならキーフレーム。
        let mut key = true;
        if let Some(att) = sbuf.sample_attachments_array(false) {
            if att.count() > 0 {
                let dict = att.value_at_index(0) as *const CFDictionary;
                if let Some(dict) = dict.as_ref() {
                    let k = kCMSampleAttachmentKey_NotSync as *const CFString as *const c_void;
                    let v = dict.value(k) as *const CFBoolean;
                    if let Some(v) = v.as_ref() {
                        key = !v.value();
                    }
                }
            }
        }
        let mut sps: Option<Vec<u8>> = None;
        let mut pps: Option<Vec<u8>> = None;
        if key {
            if let Some(fmt) = sbuf.format_description() {
                let get = |index: usize| -> Option<Vec<u8>> {
                    let mut ptr: *const u8 = std::ptr::null();
                    let mut size = 0usize;
                    let mut nal_len = 0i32;
                    let st = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                        &fmt,
                        index,
                        &mut ptr,
                        &mut size,
                        std::ptr::null_mut(),
                        &mut nal_len,
                    );
                    if st != 0 || ptr.is_null() || size == 0 {
                        return None;
                    }
                    if nal_len != 4 && ctx.errors.fetch_add(1, Ordering::Relaxed) == 0 {
                        meetrec_core::warn!("想定外の NAL 長プレフィックス: {nal_len}");
                    }
                    Some(std::slice::from_raw_parts(ptr, size).to_vec())
                };
                sps = get(0);
                pps = get(1);
            }
        }
        ctx.session.push_video_avcc(
            ctx.clock.apply(pts),
            key,
            ctx.width,
            ctx.height,
            sps.as_deref(),
            pps.as_deref(),
            &data,
        );
    }
}

impl VideoEncoder {
    /// `require_hardware` が true ならハードウェアエンコーダ必須で作る。
    pub fn create(
        session: Arc<Session>,
        width: u32,
        height: u32,
        fps: u32,
        settings: &RecordSettings,
        require_hardware: bool,
        clock: Arc<ClockOffset>,
    ) -> Result<VideoEncoder, String> {
        let ctx = Box::into_raw(Box::new(Ctx {
            session,
            width,
            height,
            errors: AtomicU64::new(0),
            clock,
        }));
        // SAFETY: キーは静的な CFString。値は CFBoolean。
        let spec_key = unsafe {
            if require_hardware {
                kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder
            } else {
                kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder
            }
        };
        let yes: &CFType = CFBoolean::new(true).as_ref();
        let spec = CFDictionary::<CFString, CFType>::from_slices(&[spec_key], &[yes]);
        let mut out: *mut VTCompressionSession = std::ptr::null_mut();
        // SAFETY: 出力ポインタと、エンコーダより長生きする ctx を渡す。
        let status = unsafe {
            VTCompressionSession::create(
                None,
                width as i32,
                height as i32,
                kCMVideoCodecType_H264,
                Some(spec.as_opaque()),
                None,
                None,
                Some(output_callback),
                ctx as *mut c_void,
                NonNull::from(&mut out),
            )
        };
        let Some(raw) = NonNull::new(out).filter(|_| status == 0) else {
            // SAFETY: セッションが作られなかったので ctx を参照する者はいない。
            drop(unsafe { Box::from_raw(ctx) });
            return Err(format!(
                "VTCompressionSessionCreate 失敗 (OSStatus {status})"
            ));
        };
        // SAFETY: Create 規約により +1 で返る。
        let vt = unsafe { CFRetained::from_raw(raw) };
        let enc = VideoEncoder {
            vt,
            ctx,
            hardware: require_hardware,
        };

        let set = |key: &CFString, value: &CFType| {
            // SAFETY: 有効なセッションとキー・値。
            let st = unsafe { VTSessionSetProperty(enc.vt.as_ref(), key, Some(value)) };
            if st != 0 {
                meetrec_core::warn!("VT プロパティ {key} を設定できません (OSStatus {st})");
            }
        };
        let yes: &CFType = CFBoolean::new(true).as_ref();
        let no: &CFType = CFBoolean::new(false).as_ref();
        let set_num = |key: &CFString, v: u32| {
            let n = CFNumber::new_i32(v.min(i32::MAX as u32) as i32);
            let n: &CFNumber = &n;
            set(key, n.as_ref());
        };
        // SAFETY: プロパティキーは静的な CFString。
        unsafe {
            set(kVTCompressionPropertyKey_RealTime, yes);
            set(kVTCompressionPropertyKey_AllowFrameReordering, no);
            set(
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_High_AutoLevel.as_ref(),
            );
            set_num(
                kVTCompressionPropertyKey_AverageBitRate,
                settings.video_bitrate_kbps * 1000,
            );
            set_num(kVTCompressionPropertyKey_ExpectedFrameRate, fps);
            // フレームは変化時にしか来ないので、枚数と時間の両方でキーフレーム間隔を縛る。
            set_num(
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                settings.keyframe_seconds * fps,
            );
            set_num(
                kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                settings.keyframe_seconds,
            );
            let st = enc.vt.prepare_to_encode_frames();
            if st != 0 {
                return Err(format!(
                    "VTCompressionSessionPrepareToEncodeFrames 失敗 (OSStatus {st})"
                ));
            }
        }
        Ok(enc)
    }

    /// 1 フレームをエンコードに投入する（非同期。結果は出力コールバックへ）。
    pub fn encode(&self, image: &CVImageBuffer, pts: CMTime) {
        // SAFETY: image は呼び出し中有効で、VT が必要なら自分で retain する。
        let st = unsafe {
            self.vt.encode_frame(
                image,
                pts,
                CMTIME_INVALID,
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if st != 0 {
            // SAFETY: ctx は self が生きている間有効。
            let ctx = unsafe { &*self.ctx };
            if ctx.errors.fetch_add(1, Ordering::Relaxed) == 0 {
                meetrec_core::warn!("VTCompressionSessionEncodeFrame 失敗 (OSStatus {st})");
            }
        }
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        // SAFETY: 残りのフレームを出し切ってから無効化する。invalidate 後は
        // コールバックが呼ばれないので ctx を解放できる。
        unsafe {
            self.vt.complete_frames(CMTIME_INVALID);
            self.vt.invalidate();
            drop(Box::from_raw(self.ctx));
        }
    }
}
