//! 録画セッション：OS 層から届くエンコード済み映像と生 PCM を受け取り、
//! ミキサ → AAC エンコーダ → fragmented MP4 へ流す。OS 非依存。
//!
//! スレッド構成：OS のキャプチャスレッドが `push_pcm` / `push_video_*` を呼び、
//! セッション内の 1 本のスレッドが 100ms ごとに音声の取り出し・エンコード・
//! フラグメント書き出し（fsync 付き）を行う。すべてのバッファは上限付き。

use std::fs::{self, File};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::audio::{FRAME, FRAME_LEN, Mixer, MixerStats, OUT_RATE};
use crate::h264;
use crate::mp4::{self, FragWriter, TrackKind, TrackSpec, VIDEO_TIMESCALE};
use crate::platform::{NS_PER_SEC, RecordingSummary};
use crate::storage;

pub const SRC_APP: usize = 0;
pub const SRC_MIC: usize = 1;

/// 初期化前に溜めておける映像バイト数の上限。
const PRE_VIDEO_LIMIT: usize = 8 << 20;

pub trait Clock: Send + Sync {
    /// キャプチャのタイムスタンプと同じ単調時計（ns）。
    fn now_ns(&self) -> u64;
}

/// AAC-LC 48kHz ステレオのエンコーダ（OS 提供）。入力は 1024 サンプルの interleaved float。
pub trait AacEncoder {
    /// AudioSpecificConfig。
    fn asc(&self) -> Vec<u8>;
    /// 0 個以上の raw AAC パケット（ADTS ヘッダなし）を返す。
    fn encode(&mut self, pcm: &[f32]) -> Result<Vec<Vec<u8>>, String>;
    fn finish(&mut self) -> Vec<Vec<u8>>;
}

/// セッションスレッド上で呼ばれる（COM 初期化などはこの中で行う）。
pub type EncoderFactory = Box<dyn FnMut() -> Result<Box<dyn AacEncoder>, String> + Send>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// ファイル書き込みに失敗した（ディスク満杯など）。以降のデータは捨てる。
    WriteFailed(String),
    /// 映像が一定時間届かなかったので音声のみで開始した。
    VideoTimeout,
    AudioEncoderFailed(String),
}

pub type EventSink = Box<dyn Fn(SessionEvent) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub path: PathBuf,
    pub marker_dir: PathBuf,
    pub fragment_ms: u32,
    pub audio_bitrate: u32,
    pub separate_tracks: bool,
    /// false なら最初から音声のみ。
    pub expect_video: bool,
    /// 映像の最初のキーフレームを待つ時間。超えたら音声のみで開始する。
    pub video_wait_ms: u64,
    pub mixer_latency_ms: u32,
}

impl SessionConfig {
    pub fn new(path: PathBuf, marker_dir: PathBuf) -> SessionConfig {
        SessionConfig {
            path,
            marker_dir,
            fragment_ms: 2000,
            audio_bitrate: 96_000,
            separate_tracks: false,
            expect_video: true,
            video_wait_ms: 5000,
            mixer_latency_ms: 250,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionStats {
    pub video_frames: u64,
    pub audio_packets: u64,
    pub bytes: u64,
    pub mixer: MixerStats,
    pub audio_only: bool,
}

struct VideoCfg {
    width: u16,
    height: u16,
    sps: Vec<u8>,
    pps: Vec<u8>,
}

struct Mux {
    file: Option<File>,
    writer: Option<FragWriter<File>>,
    has_video: bool,
    video_off: bool,
    video_cfg: Option<VideoCfg>,
    /// 初期化後に SPS/PPS が変わった場合、以降のキーフレームの前に付ける。
    inband: Option<Vec<u8>>,
    pre_video: Vec<(u64, bool, Vec<u8>)>,
    pre_video_bytes: usize,
    pre_audio: Vec<(usize, Vec<u8>)>,
    audio_ascs: Option<Vec<Vec<u8>>>,
    last_flush_ms: u64,
    video_frames: u64,
    audio_packets: u64,
    failed: bool,
}

struct Shared {
    cfg: SessionConfig,
    clock: Arc<dyn Clock>,
    t0: u64,
    mixer: Mutex<Mixer>,
    mux: Mutex<Mux>,
    stop: Mutex<bool>,
    wake: Condvar,
    events: EventSink,
}

impl Shared {
    fn elapsed_ns(&self) -> u64 {
        self.clock.now_ns().saturating_sub(self.t0)
    }

    fn fail(&self, mux: &mut Mux, e: std::io::Error) {
        if !mux.failed {
            mux.failed = true;
            crate::warn!("録画ファイルへの書き込みに失敗: {e}");
            (self.events)(SessionEvent::WriteFailed(e.to_string()));
        }
    }

    /// 書き出しを開始できる状態なら moov を書く。
    fn try_init(&self, mux: &mut Mux, now_ms: u64) {
        if mux.writer.is_some() || mux.failed {
            return;
        }
        let Some(ascs) = mux.audio_ascs.clone() else {
            return;
        };
        let video_ready = mux.video_cfg.is_some() && mux.pre_video.iter().any(|v| v.1);
        if !mux.video_off && !video_ready {
            if now_ms < self.cfg.video_wait_ms {
                return;
            }
            mux.video_off = true;
            if self.cfg.expect_video {
                (self.events)(SessionEvent::VideoTimeout);
            }
        }
        let mut tracks = Vec::new();
        mux.has_video = !mux.video_off;
        if mux.has_video {
            let c = mux.video_cfg.as_ref().unwrap();
            tracks.push(TrackSpec {
                kind: TrackKind::Video {
                    width: c.width,
                    height: c.height,
                    sps: c.sps.clone(),
                    pps: c.pps.clone(),
                },
                timescale: VIDEO_TIMESCALE,
                enabled: true,
            });
        }
        for (i, asc) in ascs.iter().enumerate() {
            tracks.push(TrackSpec {
                kind: TrackKind::Audio {
                    sample_rate: OUT_RATE,
                    channels: 2,
                    asc: asc.clone(),
                    bitrate: self.cfg.audio_bitrate,
                },
                timescale: OUT_RATE,
                enabled: i == 0,
            });
        }
        if tracks.is_empty() {
            return;
        }
        let Some(file) = mux.file.take() else { return };
        match FragWriter::new(file, tracks) {
            Ok(w) => mux.writer = Some(w),
            Err(e) => return self.fail(mux, e),
        }
        // 溜めていたサンプルを流す。映像は最初のキーフレームから。
        let pre_video = std::mem::take(&mut mux.pre_video);
        mux.pre_video_bytes = 0;
        if mux.has_video {
            let first_key = pre_video.iter().position(|v| v.1).unwrap_or(0);
            for (pts, key, data) in pre_video.into_iter().skip(first_key) {
                self.write_video(mux, pts, key, &data);
            }
        }
        for (track, data) in std::mem::take(&mut mux.pre_audio) {
            self.write_audio(mux, track, &data);
        }
        mux.last_flush_ms = now_ms;
    }

    fn write_video(&self, mux: &mut Mux, pts: u64, key: bool, data: &[u8]) {
        let Some(w) = mux.writer.as_mut() else { return };
        let res = match (&mux.inband, key) {
            (Some(prefix), true) => {
                let mut v = prefix.clone();
                v.extend_from_slice(data);
                w.write_timed(0, pts, key, &v)
            }
            _ => w.write_timed(0, pts, key, data),
        };
        mux.video_frames += 1;
        if let Err(e) = res {
            self.fail(mux, e);
        }
    }

    fn write_audio(&self, mux: &mut Mux, track: usize, data: &[u8]) {
        let offset = usize::from(mux.has_video);
        if let Some(w) = mux.writer.as_mut() {
            w.write_fixed(track + offset, FRAME as u32, data);
            if track == 0 {
                mux.audio_packets += 1;
            }
        }
    }

    fn video(
        &self,
        pts_ns: u64,
        key: bool,
        size: (u32, u32),
        sps: Option<&[u8]>,
        pps: Option<&[u8]>,
        avcc: &[u8],
    ) {
        let rel = pts_ns.saturating_sub(self.t0);
        let pts = (rel as u128 * VIDEO_TIMESCALE as u128 / NS_PER_SEC as u128) as u64;
        let mut mux = self.mux.lock().unwrap();
        if mux.video_off || mux.failed {
            return;
        }
        if let (Some(sps), Some(pps)) = (sps, pps) {
            let changed = mux
                .video_cfg
                .as_ref()
                .is_none_or(|c| c.sps != sps || c.pps != pps);
            if changed {
                if mux.writer.is_some() {
                    crate::warn!("映像のパラメータセットが録画中に変わりました（in-band で継続）");
                    let mut p = Vec::new();
                    for nal in [sps, pps] {
                        p.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                        p.extend_from_slice(nal);
                    }
                    mux.inband = Some(p);
                }
                mux.video_cfg = Some(VideoCfg {
                    width: size.0.min(u16::MAX as u32) as u16,
                    height: size.1.min(u16::MAX as u32) as u16,
                    sps: sps.to_vec(),
                    pps: pps.to_vec(),
                });
            }
        }
        if avcc.is_empty() {
            return;
        }
        if mux.writer.is_some() {
            self.write_video(&mut mux, pts, key, avcc);
        } else {
            if mux.pre_video_bytes + avcc.len() > PRE_VIDEO_LIMIT {
                mux.pre_video.clear();
                mux.pre_video_bytes = 0;
            }
            mux.pre_video_bytes += avcc.len();
            mux.pre_video.push((pts, key, avcc.to_vec()));
            let now_ms = self.elapsed_ns() / 1_000_000;
            self.try_init(&mut mux, now_ms);
        }
    }

    fn finalize(&self) -> RecordingSummary {
        let end_ms = self.elapsed_ns() / 1_000_000;
        let mut mux = self.mux.lock().unwrap();
        let mut summary = RecordingSummary {
            path: self.cfg.path.clone(),
            video_frames: mux.video_frames,
            audio_only: !mux.has_video,
            ..Default::default()
        };
        let mut fragments = 0;
        match mux.writer.take() {
            Some(w) if !mux.failed => match w.finish(end_ms) {
                Ok((file, fin)) => {
                    let _ = file.sync_all();
                    summary.duration_ms = fin.duration_ms;
                    summary.bytes = fin.bytes;
                    fragments = fin.fragments;
                }
                Err(e) => self.fail(&mut mux, e),
            },
            Some(w) => drop(w),
            None => {}
        }
        mux.file = None;
        if mux.failed {
            // 書き込み失敗後：最後の完全なフラグメントまでを生かす。
            if let Ok(r) = mp4::repair_file(&self.cfg.path) {
                summary.duration_ms = r.duration_ms;
                summary.bytes = r.valid_len;
                fragments = r.fragments;
            }
        }
        if fragments == 0 {
            let _ = fs::remove_file(&self.cfg.path);
            summary.empty = true;
        }
        storage::remove_marker(&self.cfg.marker_dir, &self.cfg.path);
        summary
    }
}

/// セッションスレッドの本体（テストでは手動で `step` を呼ぶ）。
struct Ticker {
    shared: Arc<Shared>,
    encoders: Vec<Box<dyn AacEncoder>>,
    packets: Vec<(usize, Vec<u8>)>,
    frames: Vec<f32>,
}

impl Ticker {
    fn new(shared: Arc<Shared>, mut factory: EncoderFactory) -> Ticker {
        let want = if shared.cfg.separate_tracks { 3 } else { 1 };
        let mut encoders = Vec::new();
        for _ in 0..want {
            match factory() {
                Ok(e) => encoders.push(e),
                Err(e) => {
                    crate::warn!("AAC エンコーダを作成できません: {e}");
                    (shared.events)(SessionEvent::AudioEncoderFailed(e));
                    encoders.clear();
                    break;
                }
            }
        }
        shared.mux.lock().unwrap().audio_ascs = Some(encoders.iter().map(|e| e.asc()).collect());
        Ticker {
            shared,
            encoders,
            packets: Vec::new(),
            frames: Vec::new(),
        }
    }

    fn encode(&mut self, track: usize, pcm_at: std::ops::Range<usize>) {
        if let Some(enc) = self.encoders.get_mut(track) {
            match enc.encode(&self.frames[pcm_at]) {
                Ok(p) => self.packets.extend(p.into_iter().map(|d| (track, d))),
                Err(e) => {
                    crate::warn!("AAC エンコード失敗: {e}");
                    (self.shared.events)(SessionEvent::AudioEncoderFailed(e));
                    self.encoders.clear();
                }
            }
        }
    }

    fn step(&mut self, stopping: bool) {
        let shared = self.shared.clone();
        let now_ns = shared.elapsed_ns();
        let now_ms = now_ns / 1_000_000;
        let n_enc = self.encoders.len();

        // ミキサのロックは PCM を写す間だけ。
        self.frames.clear();
        let frames = &mut self.frames;
        shared
            .mixer
            .lock()
            .unwrap()
            .pull(now_ns, stopping, 256, |mix, per| {
                frames.extend_from_slice(mix);
                if n_enc >= 3 {
                    frames.extend_from_slice(&per[SRC_APP]);
                    frames.extend_from_slice(&per[SRC_MIC]);
                }
            });
        let per_frame = FRAME_LEN * if n_enc >= 3 { 3 } else { 1 };
        let count = self.frames.len() / per_frame;
        for i in 0..count {
            let base = i * per_frame;
            for track in 0..n_enc.min(3) {
                let at = base + track * FRAME_LEN;
                self.encode(track, at..at + FRAME_LEN);
            }
        }
        if stopping {
            for (track, enc) in self.encoders.iter_mut().enumerate() {
                self.packets
                    .extend(enc.finish().into_iter().map(|d| (track, d)));
            }
        }

        let mut mux = shared.mux.lock().unwrap();
        for (track, data) in self.packets.drain(..) {
            if mux.writer.is_some() {
                shared.write_audio(&mut mux, track, &data);
            } else if mux.pre_audio.len() < 4096 {
                mux.pre_audio.push((track, data));
            }
        }
        shared.try_init(&mut mux, now_ms);
        if mux.failed {
            return;
        }
        let due = now_ms.saturating_sub(mux.last_flush_ms) >= shared.cfg.fragment_ms as u64;
        if due
            && !stopping
            && let Some(w) = mux.writer.as_mut()
        {
            // sync_data まで行い、電源断でも直前のフラグメントまで残す。
            let res = w.flush(now_ms).and_then(|_| w.get_mut().sync_data());
            mux.last_flush_ms = now_ms;
            if let Err(e) = res {
                shared.fail(&mut mux, e);
            }
        }
    }
}

pub struct Session {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    /// ファイルと進行中マーカーを作り、セッションスレッドを起動する。
    pub fn start(
        cfg: SessionConfig,
        clock: Arc<dyn Clock>,
        factory: EncoderFactory,
        events: EventSink,
    ) -> Result<Session, String> {
        let shared = Self::prepare(cfg, clock, events)?;
        let s2 = shared.clone();
        let thread = std::thread::Builder::new()
            .name("meetrec-session".into())
            .spawn(move || {
                let mut ticker = Ticker::new(s2.clone(), factory);
                loop {
                    let guard = s2.stop.lock().unwrap();
                    let (guard, _) = s2
                        .wake
                        .wait_timeout_while(guard, Duration::from_millis(100), |stop| !*stop)
                        .unwrap();
                    let stopping = *guard;
                    drop(guard);
                    ticker.step(stopping);
                    if stopping {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Session {
            shared,
            thread: Some(thread),
        })
    }

    fn prepare(
        cfg: SessionConfig,
        clock: Arc<dyn Clock>,
        events: EventSink,
    ) -> Result<Arc<Shared>, String> {
        if let Some(dir) = cfg.path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("保存先を作成できません: {e}"))?;
        }
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(&cfg.path)
            .map_err(|e| format!("録画ファイルを作成できません: {e}"))?;
        if let Err(e) = storage::write_marker(&cfg.marker_dir, &cfg.path) {
            crate::warn!("進行中マーカーを書けません: {e}");
        }
        let t0 = clock.now_ns();
        let mixer = Mixer::new(2, cfg.mixer_latency_ms);
        let mux = Mux {
            file: Some(file),
            writer: None,
            has_video: false,
            video_off: !cfg.expect_video,
            video_cfg: None,
            inband: None,
            pre_video: Vec::new(),
            pre_video_bytes: 0,
            pre_audio: Vec::new(),
            audio_ascs: None,
            last_flush_ms: 0,
            video_frames: 0,
            audio_packets: 0,
            failed: false,
        };
        Ok(Arc::new(Shared {
            cfg,
            clock,
            t0,
            mixer: Mutex::new(mixer),
            mux: Mutex::new(mux),
            stop: Mutex::new(false),
            wake: Condvar::new(),
            events,
        }))
    }

    /// 生 PCM を投入する（`source` は `SRC_APP` / `SRC_MIC`、`pts_ns` はセッション時計の絶対値）。
    pub fn push_pcm(&self, source: usize, pts_ns: u64, rate: u32, channels: u16, data: &[f32]) {
        let Some(rel) = pts_ns.checked_sub(self.shared.t0) else {
            return;
        };
        self.shared
            .mixer
            .lock()
            .unwrap()
            .push(source, rel, rate, channels, data);
    }

    /// エンコーダ出力（Annex B の 1 アクセスユニット）を投入する。
    pub fn push_video_annexb(&self, pts_ns: u64, width: u32, height: u32, data: &[u8]) {
        let au = h264::annexb_to_avcc(data);
        self.shared.video(
            pts_ns,
            au.key,
            (width, height),
            au.sps.as_deref(),
            au.pps.as_deref(),
            &au.avcc,
        );
    }

    /// エンコーダ出力（4 バイト長プレフィックスの AVCC）を投入する。
    /// `sps`/`pps` はキーフレーム時にフォーマット記述から取り出して渡す。
    #[allow(clippy::too_many_arguments)]
    pub fn push_video_avcc(
        &self,
        pts_ns: u64,
        key: bool,
        width: u32,
        height: u32,
        sps: Option<&[u8]>,
        pps: Option<&[u8]>,
        data: &[u8],
    ) {
        self.shared
            .video(pts_ns, key, (width, height), sps, pps, data);
    }

    /// 映像が取れないと分かった時点で呼ぶ（待たずに音声のみで開始する）。
    pub fn video_unavailable(&self) {
        let mut mux = self.shared.mux.lock().unwrap();
        if mux.writer.is_none() {
            mux.video_off = true;
            mux.pre_video.clear();
        }
    }

    pub fn stats(&self) -> SessionStats {
        let mixer = self.shared.mixer.lock().unwrap().stats();
        let mux = self.shared.mux.lock().unwrap();
        SessionStats {
            video_frames: mux.video_frames,
            audio_packets: mux.audio_packets,
            bytes: mux.writer.as_ref().map_or(0, |w| w.bytes_written()),
            mixer,
            audio_only: mux.writer.is_some() && !mux.has_video,
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.shared.cfg.path
    }

    /// 残りを書き出してファイルを確定する。
    pub fn finish(mut self) -> RecordingSummary {
        self.stop_thread();
        self.shared.finalize()
    }

    fn stop_thread(&mut self) {
        *self.shared.stop.lock().unwrap() = true;
        self.shared.wake.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // finish を呼ばずに捨てられた場合もスレッドは止める（ファイルは修復対象として残る）。
        self.stop_thread();
    }
}

/// fps 上限のためのフレーム間引き。
#[derive(Debug, Clone)]
pub struct FrameGate {
    min_interval_ns: u64,
    last: Option<u64>,
}

impl FrameGate {
    pub fn new(fps: u32) -> FrameGate {
        // 配信タイミングの揺れで本来のフレームを落とさないよう 10% の余裕を持たせる。
        FrameGate {
            min_interval_ns: NS_PER_SEC * 9 / 10 / fps.max(1) as u64,
            last: None,
        }
    }

    pub fn accept(&mut self, pts_ns: u64) -> bool {
        match self.last {
            Some(last) if pts_ns < last + self.min_interval_ns => false,
            _ => {
                self.last = Some(pts_ns);
                true
            }
        }
    }
}

/// ソフトウェアエンコードにフォールバックしたときの解像度上限（長辺）。両 OS で揃える（DECISIONS D23）。
pub const SOFTWARE_MAX_LONG_SIDE: u32 = 1280;

/// 出力キャンバス（エンコード解像度）を決める。長辺を `max_long` 以下に縮小し、偶数に丸める。
/// 小さいウィンドウ（ミニウィンドウ等）から始まった場合は、後で大きなウィンドウに
/// 切り替わっても粗くならないよう 1280x720 を使う。
pub fn canvas_size(width: u32, height: u32, max_long: u32) -> (u32, u32) {
    let (w, h) = (width.max(1) as u64, height.max(1) as u64);
    let long = w.max(h);
    if long < 960 {
        let c = (1280.min(max_long), 720.min(max_long * 9 / 16));
        return (c.0 & !1, c.1 & !1);
    }
    let max_long = max_long as u64;
    let (w, h) = if long > max_long {
        (w * max_long / long, h * max_long / long)
    } else {
        (w, h)
    };
    (((w as u32) & !1).max(2), ((h as u32) & !1).max(2))
}

/// アスペクト比を保ってキャンバス中央に収める矩形 (x, y, w, h)。
pub fn fit_rect(src_w: u32, src_h: u32, canvas_w: u32, canvas_h: u32) -> (u32, u32, u32, u32) {
    let (sw, sh) = (src_w.max(1) as u64, src_h.max(1) as u64);
    let (cw, ch) = (canvas_w as u64, canvas_h as u64);
    let (w, h) = if sw * ch >= sh * cw {
        (cw, (sh * cw / sw).max(1))
    } else {
        ((sw * ch / sh).max(1), ch)
    };
    let (w, h) = ((w & !1).max(2).min(cw), (h & !1).max(2).min(ch));
    (
        (((cw - w) / 2) & !1) as u32,
        (((ch - h) / 2) & !1) as u32,
        w as u32,
        h as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::tests::top_boxes;
    use crate::testutil::temp_dir;
    use std::sync::atomic::{AtomicU64, Ordering};

    const MS: u64 = 1_000_000;

    struct FakeClock(AtomicU64);
    impl Clock for FakeClock {
        fn now_ns(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    struct FakeAac;
    impl AacEncoder for FakeAac {
        fn asc(&self) -> Vec<u8> {
            crate::audio::DEFAULT_ASC.to_vec()
        }
        fn encode(&mut self, pcm: &[f32]) -> Result<Vec<Vec<u8>>, String> {
            assert_eq!(pcm.len(), FRAME_LEN);
            Ok(vec![vec![0x21; 64]])
        }
        fn finish(&mut self) -> Vec<Vec<u8>> {
            Vec::new()
        }
    }

    struct Rig {
        clock: Arc<FakeClock>,
        shared: Arc<Shared>,
        ticker: Ticker,
        events: Arc<Mutex<Vec<SessionEvent>>>,
        path: PathBuf,
        marker: PathBuf,
    }

    const T0: u64 = 5_000 * MS;
    const SPS: [u8; 5] = [0x67, 66, 0, 31, 1];
    const PPS: [u8; 2] = [0x68, 2];

    fn rig(name: &str, tweak: impl FnOnce(&mut SessionConfig)) -> Rig {
        let dir = temp_dir(name);
        let path = dir.join("2026-11-20_1430_Zoom.mp4");
        let mut cfg = SessionConfig::new(path.clone(), dir.join("inprogress"));
        cfg.mixer_latency_ms = 0;
        tweak(&mut cfg);
        let clock = Arc::new(FakeClock(AtomicU64::new(T0)));
        let events = Arc::new(Mutex::new(Vec::new()));
        let ev2 = events.clone();
        let shared = Session::prepare(
            cfg,
            clock.clone(),
            Box::new(move |e| ev2.lock().unwrap().push(e)),
        )
        .unwrap();
        let ticker = Ticker::new(
            shared.clone(),
            Box::new(|| Ok(Box::new(FakeAac) as Box<dyn AacEncoder>)),
        );
        let marker = storage::marker_path(&dir.join("inprogress"), &path);
        Rig {
            clock,
            shared,
            ticker,
            events,
            path,
            marker,
        }
    }

    impl Rig {
        fn session(&self) -> Session {
            Session {
                shared: self.shared.clone(),
                thread: None,
            }
        }
        /// 100ms 進めて 1 ステップ。`video` なら 1 フレーム投入。
        fn advance(&mut self, video: Option<bool>) {
            let now = self.clock.0.fetch_add(100 * MS, Ordering::SeqCst) + 100 * MS;
            let s = self.session();
            s.push_pcm(SRC_APP, now - 100 * MS, 48_000, 2, &vec![0.1; 4800 * 2]);
            s.push_pcm(SRC_MIC, now - 100 * MS, 16_000, 1, &vec![0.1; 1600]);
            if let Some(key) = video {
                let (sps, pps) = if key {
                    (Some(&SPS[..]), Some(&PPS[..]))
                } else {
                    (None, None)
                };
                s.push_video_avcc(
                    now,
                    key,
                    1280,
                    720,
                    sps,
                    pps,
                    &[0, 0, 0, 1, if key { 0x65 } else { 0x41 }],
                );
            }
            self.ticker.step(false);
        }
        fn finish(mut self) -> RecordingSummary {
            self.ticker.step(true);
            self.shared.finalize()
        }
    }

    #[test]
    fn records_video_and_audio() {
        let mut r = rig("session-av", |_| {});
        assert!(r.marker.exists());
        for i in 0..100 {
            r.advance(Some(i % 40 == 0));
        }
        let stats = r.session().stats();
        assert_eq!(stats.video_frames, 100);
        assert!(!stats.audio_only);
        let (path, marker) = (r.path.clone(), r.marker.clone());
        let summary = r.finish();
        assert!(!summary.empty && !summary.audio_only);
        assert!(
            (9_900..=10_100).contains(&summary.duration_ms),
            "{}",
            summary.duration_ms
        );
        assert!(!marker.exists());
        let buf = fs::read(&path).unwrap();
        assert_eq!(buf.len() as u64, summary.bytes);
        let boxes = top_boxes(&buf);
        assert_eq!(boxes.iter().filter(|b| *b == "moof").count(), 5);
        assert_eq!(boxes.last().unwrap(), "mfra");
        let rep = mp4::repair_file(&path).unwrap();
        assert_eq!(rep.truncated_bytes, 0);
        assert!(!rep.duration_patched);
    }

    #[test]
    fn crash_leaves_playable_file_and_marker() {
        let mut r = rig("session-crash", |_| {});
        for i in 0..55 {
            r.advance(Some(i == 0));
        }
        // finish せずに「強制終了」。ここまでに 2 フラグメント（4 秒）が fsync 済み。
        let (path, marker) = (r.path.clone(), r.marker.clone());
        drop(r);
        assert!(marker.exists());
        let out = storage::recover(marker.parent().unwrap());
        match &out[..] {
            [
                storage::RecoveryOutcome::Repaired {
                    path: p,
                    duration_ms,
                },
            ] => {
                assert_eq!(p, &path);
                assert!((3_900..=4_300).contains(duration_ms), "{duration_ms}");
            }
            other => panic!("{other:?}"),
        }
        assert!(!marker.exists());
        assert!(storage::recover(marker.parent().unwrap()).is_empty());
    }

    #[test]
    fn waits_for_first_keyframe() {
        let mut r = rig("session-key", |_| {});
        r.advance(Some(false)); // キーフレーム前の P フレームは捨てられる
        r.advance(Some(false));
        assert!(r.shared.mux.lock().unwrap().writer.is_none());
        r.advance(Some(true));
        assert!(r.shared.mux.lock().unwrap().writer.is_some());
        assert_eq!(r.session().stats().video_frames, 1);
        // 初期化前の音声（0.3 秒ぶん）も失われていない。
        assert!(r.session().stats().audio_packets >= 13);
    }

    #[test]
    fn falls_back_to_audio_only_after_timeout() {
        let mut r = rig("session-timeout", |_| {});
        for _ in 0..49 {
            r.advance(None);
        }
        assert!(r.events.lock().unwrap().is_empty());
        for _ in 0..30 {
            r.advance(None);
        }
        assert_eq!(*r.events.lock().unwrap(), vec![SessionEvent::VideoTimeout]);
        assert!(r.session().stats().audio_only);
        // 後から映像が来ても無視する。
        r.advance(Some(true));
        let summary = r.finish();
        assert!(summary.audio_only && !summary.empty);
        assert_eq!(summary.video_frames, 0);
        assert!(
            (7_900..=8_100).contains(&summary.duration_ms),
            "{}",
            summary.duration_ms
        );
    }

    #[test]
    fn audio_only_from_start() {
        let mut r = rig("session-audio", |c| c.expect_video = false);
        for _ in 0..30 {
            r.advance(None);
        }
        assert!(r.events.lock().unwrap().is_empty());
        let summary = r.finish();
        assert!(summary.audio_only && !summary.empty);
    }

    #[test]
    fn separate_tracks_writes_three_audio_tracks() {
        let mut r = rig("session-sep", |c| c.separate_tracks = true);
        for i in 0..30 {
            r.advance(Some(i == 0));
        }
        let path = r.path.clone();
        r.finish();
        let buf = fs::read(&path).unwrap();
        let traks = buf.windows(4).filter(|w| *w == b"trak").count();
        assert_eq!(traks, 4);
    }

    #[test]
    fn empty_recording_is_deleted() {
        let r = rig("session-empty", |_| {});
        let (path, marker) = (r.path.clone(), r.marker.clone());
        let summary = r.finish();
        assert!(summary.empty);
        assert!(!path.exists() && !marker.exists());
    }

    #[test]
    fn static_screen_writes_no_video_but_keeps_duration() {
        let mut r = rig("session-static", |_| {});
        r.advance(Some(true));
        for _ in 0..99 {
            r.advance(None); // 画面が変化しない 10 秒
        }
        let summary = r.finish();
        assert_eq!(summary.video_frames, 1);
        assert!((9_900..=10_100).contains(&summary.duration_ms));
    }

    #[test]
    fn real_thread_start_and_finish() {
        let dir = temp_dir("session-thread");
        let cfg = SessionConfig::new(dir.join("rec.mp4"), dir.join("inprogress"));
        struct Real(std::time::Instant);
        impl Clock for Real {
            fn now_ns(&self) -> u64 {
                self.0.elapsed().as_nanos() as u64 + 1_000_000_000
            }
        }
        let clock = Arc::new(Real(std::time::Instant::now()));
        let s = Session::start(
            cfg,
            clock.clone(),
            Box::new(|| Ok(Box::new(FakeAac) as Box<dyn AacEncoder>)),
            Box::new(|_| {}),
        )
        .unwrap();
        s.push_video_avcc(
            clock.now_ns(),
            true,
            640,
            360,
            Some(&SPS),
            Some(&PPS),
            &[0, 0, 0, 1, 0x65],
        );
        std::thread::sleep(Duration::from_millis(450));
        let summary = s.finish();
        assert!(!summary.empty);
        assert_eq!(summary.video_frames, 1);
        assert!(summary.duration_ms >= 400);
    }

    #[test]
    fn frame_gate_limits_fps() {
        let mut g = FrameGate::new(10);
        let accepted = (0..600).filter(|i| g.accept(i * 16_666_667)).count(); // 60fps を 10 秒
        assert!((95..=112).contains(&accepted), "{accepted}");
        let mut g = FrameGate::new(10);
        assert_eq!((0..50).filter(|i| g.accept(i * 100 * MS)).count(), 50);
    }

    #[test]
    fn canvas_and_fit() {
        assert_eq!(canvas_size(1920, 1080, 1920), (1920, 1080));
        assert_eq!(canvas_size(3840, 2160, 1920), (1920, 1080));
        assert_eq!(canvas_size(2560, 1600, 1920), (1920, 1200));
        assert_eq!(canvas_size(1281, 721, 1920), (1280, 720));
        assert_eq!(canvas_size(1080, 1920, 1920), (1080, 1920));
        assert_eq!(canvas_size(240, 135, 1920), (1280, 720));
        assert_eq!(fit_rect(1920, 1080, 1280, 720), (0, 0, 1280, 720));
        assert_eq!(fit_rect(1000, 1000, 1280, 720), (280, 0, 720, 720));
        assert_eq!(fit_rect(2000, 500, 1280, 720), (0, 200, 1280, 320));
    }
}
