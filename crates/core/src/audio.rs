//! 音声ミキサ：タイムスタンプ付きの PCM（相手音声・マイク）を 48kHz ステレオの
//! 共通タイムラインに並べ、1024 サンプル単位で取り出す。
//!
//! - 各ソースは任意のサンプルレート／チャンネル数で届く（デバイス切替で途中変更可）。
//! - タイムスタンプとのずれが小さいうちは再サンプル比を ±0.5% の範囲で微調整して
//!   追従し（クロックドリフト対策）、100ms を超えたら位置を合わせ直す（切替・欠落対策）。
//! - バッファは固定長（ソースごと約 3 秒）。あふれた分は捨て、メモリは増えない。

use crate::platform::NS_PER_SEC;

pub const OUT_RATE: u32 = 48_000;
pub const FRAME: usize = 1024;
/// 1 フレームの interleaved ステレオ float の長さ。
pub const FRAME_LEN: usize = FRAME * 2;
/// AAC-LC 48kHz ステレオの AudioSpecificConfig。OS のエンコーダから取得できないときの既定値で、
/// `OUT_RATE` とチャンネル数（2）に対応する。
pub const DEFAULT_ASC: [u8; 2] = [0x11, 0x90];

const CAP: usize = OUT_RATE as usize * 3;
const HARD_RESYNC: f64 = OUT_RATE as f64 * 0.1;
const MAX_ADJ: f64 = 0.005;

fn pos_of(ns: u64) -> u64 {
    (ns as u128 * OUT_RATE as u128 / NS_PER_SEC as u128) as u64
}

struct Source {
    /// 絶対位置 p のサンプルは `ring[(p % CAP) * 2..]`。未書き込み領域は常に 0。
    ring: Vec<f32>,
    /// 次に書く出力位置（小数部は再サンプラの位相に含める）。
    next_pos: Option<u64>,
    in_rate: u32,
    in_channels: u16,
    prev: [f32; 2],
    phase: f64,
    err_avg: f64,
    resyncs: u64,
    dropped: u64,
}

impl Source {
    fn new() -> Source {
        Source {
            ring: vec![0.0; CAP * 2],
            next_pos: None,
            in_rate: 0,
            in_channels: 0,
            prev: [0.0; 2],
            phase: 0.0,
            err_avg: 0.0,
            resyncs: 0,
            dropped: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MixerStats {
    pub resyncs: u64,
    pub dropped_samples: u64,
    pub frames_out: u64,
}

pub struct Mixer {
    sources: Vec<Source>,
    /// ここまで出力済み（サンプル位置）。
    emitted: u64,
    latency: u64,
    frames_out: u64,
    scratch: Vec<[f32; 2]>,
}

impl Mixer {
    /// `latency_ms`：到着の遅れを待つ時間。これより遅れて届いた音は捨てられる。
    pub fn new(sources: usize, latency_ms: u32) -> Mixer {
        Mixer {
            sources: (0..sources).map(|_| Source::new()).collect(),
            emitted: 0,
            latency: OUT_RATE as u64 * latency_ms as u64 / 1000,
            frames_out: 0,
            scratch: Vec::new(),
        }
    }

    pub fn stats(&self) -> MixerStats {
        MixerStats {
            resyncs: self.sources.iter().map(|s| s.resyncs).sum(),
            dropped_samples: self.sources.iter().map(|s| s.dropped).sum(),
            frames_out: self.frames_out,
        }
    }

    /// PCM を投入する。`pts_ns` は先頭サンプルの時刻（録画開始からの経過）、
    /// `data` は interleaved float。
    pub fn push(&mut self, source: usize, pts_ns: u64, rate: u32, channels: u16, data: &[f32]) {
        let ch = channels as usize;
        // 1 フレームに満たない入力（チャンネル数より短い）も捨てる。
        if rate == 0 || ch == 0 || data.len() < ch {
            return;
        }
        let emitted = self.emitted;
        let s = &mut self.sources[source];
        let n = data.len() / ch;
        let ts_pos = pos_of(pts_ns) as f64;

        let format_changed = s.in_rate != rate || s.in_channels != channels;
        let err = s.next_pos.map(|p| ts_pos - p as f64);
        let mut adj = 0.0;
        match err {
            Some(e) if !format_changed && e.abs() <= HARD_RESYNC => {
                s.err_avg += (e - s.err_avg) * 0.1;
                adj = (s.err_avg / OUT_RATE as f64 * 0.5).clamp(-MAX_ADJ, MAX_ADJ);
            }
            _ => {
                if s.next_pos.is_some() {
                    s.resyncs += 1;
                }
                s.next_pos = Some(ts_pos as u64);
                s.in_rate = rate;
                s.in_channels = channels;
                s.prev = [0.0; 2];
                s.phase = 0.0;
                s.err_avg = 0.0;
            }
        }

        // 遅れている（err > 0）なら出力を多めに作る＝ステップを小さくする。
        let step = rate as f64 / OUT_RATE as f64 * (1.0 - adj);
        let frame = |i: usize| -> [f32; 2] {
            let l = data[i * ch];
            let r = if ch >= 2 { data[i * ch + 1] } else { l };
            [l, r]
        };
        let out = &mut self.scratch;
        out.clear();
        let mut phase = s.phase;
        while phase + 1.0 < n as f64 {
            let i = phase.floor();
            let frac = (phase - i) as f32;
            let a = if i < 0.0 { s.prev } else { frame(i as usize) };
            let b = frame((i + 1.0) as usize);
            out.push([a[0] + (b[0] - a[0]) * frac, a[1] + (b[1] - a[1]) * frac]);
            phase += step;
        }
        s.phase = phase - n as f64;
        s.prev = frame(n - 1);

        let mut pos = s.next_pos.unwrap();
        for v in out.iter() {
            if pos < emitted || pos >= emitted + CAP as u64 {
                s.dropped += 1;
            } else {
                let at = (pos % CAP as u64) as usize * 2;
                s.ring[at] = v[0];
                s.ring[at + 1] = v[1];
            }
            pos += 1;
        }
        s.next_pos = Some(pos);
    }

    /// `now_ns`（録画開始からの経過）から遅延ぶん手前まで、1024 サンプル単位で取り出す。
    /// `f(mix, per_source)`：ミックス済みフレームと、ソースごとのフレーム。
    /// `flush` が true なら遅延を待たずに now まで出す（終了時）。
    /// 1 回の呼び出しで出すのは最大 `max_frames`（長時間止まった後の一気出し防止）。
    pub fn pull(
        &mut self,
        now_ns: u64,
        flush: bool,
        max_frames: usize,
        mut f: impl FnMut(&[f32], &[Vec<f32>]),
    ) {
        let now = pos_of(now_ns);
        let target = if flush {
            now
        } else {
            now.saturating_sub(self.latency)
        };
        let mut mix = vec![0.0f32; FRAME_LEN];
        let mut per: Vec<Vec<f32>> = self
            .sources
            .iter()
            .map(|_| vec![0.0f32; FRAME_LEN])
            .collect();
        let mut done = 0;
        while self.emitted + FRAME as u64 <= target && done < max_frames {
            mix.iter_mut().for_each(|x| *x = 0.0);
            for (s, out) in self.sources.iter_mut().zip(per.iter_mut()) {
                for i in 0..FRAME {
                    let at = ((self.emitted + i as u64) % CAP as u64) as usize * 2;
                    out[i * 2] = s.ring[at];
                    out[i * 2 + 1] = s.ring[at + 1];
                    s.ring[at] = 0.0;
                    s.ring[at + 1] = 0.0;
                }
                for (m, v) in mix.iter_mut().zip(out.iter()) {
                    *m += *v;
                }
            }
            for m in mix.iter_mut() {
                *m = m.clamp(-1.0, 1.0);
            }
            self.emitted += FRAME as u64;
            self.frames_out += 1;
            done += 1;
            f(&mix, &per);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    fn sine(rate: u32, freq: f64, start_sample: u64, n: usize, ch: usize) -> Vec<f32> {
        let mut v = Vec::with_capacity(n * ch);
        for i in 0..n {
            let t = (start_sample + i as u64) as f64 / rate as f64;
            let x = (t * freq * std::f64::consts::TAU).sin() as f32 * 0.5;
            for _ in 0..ch {
                v.push(x);
            }
        }
        v
    }

    fn collect(m: &mut Mixer, now_ns: u64, flush: bool) -> (Vec<f32>, Vec<Vec<f32>>) {
        let mut mix = Vec::new();
        let mut per: Vec<Vec<f32>> = vec![Vec::new(); m.sources.len()];
        m.pull(now_ns, flush, usize::MAX, |a, p| {
            mix.extend_from_slice(a);
            for (dst, src) in per.iter_mut().zip(p) {
                dst.extend_from_slice(src);
            }
        });
        (mix, per)
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    #[test]
    fn passthrough_48k_stereo_is_placed_by_timestamp() {
        let mut m = Mixer::new(1, 0);
        // 100ms 地点から 10ms チャンクを 50 個（0.5 秒）。
        for k in 0..50u64 {
            let data = sine(48_000, 440.0, k * 480, 480, 2);
            m.push(0, 100 * MS + k * 10 * MS, 48_000, 2, &data);
        }
        let (mix, _) = collect(&mut m, 700 * MS, true);
        // 先頭 100ms（4800 サンプル）は無音。
        assert!(rms(&mix[..4800 * 2]) < 1e-6);
        // その後は振幅 0.5 の正弦波（RMS ≒ 0.354）。
        let body = &mix[5000 * 2..28_000 * 2];
        assert!((rms(body) - 0.3535).abs() < 0.01, "{}", rms(body));
        // 終了後は無音。
        assert!(rms(&mix[29_000 * 2..]) < 1e-6);
    }

    #[test]
    fn resamples_44k1_mono_without_drift() {
        let mut m = Mixer::new(1, 0);
        let mut sample = 0u64;
        // 441 サンプル = 10ms を 300 個（3 秒）。逐次取り出す。
        let mut total = Vec::new();
        for k in 0..300u64 {
            let data = sine(44_100, 1000.0, sample, 441, 1);
            m.push(0, k * 10 * MS, 44_100, 1, &data);
            sample += 441;
            let (mix, _) = collect(&mut m, k * 10 * MS, false);
            total.extend(mix);
        }
        let (mix, _) = collect(&mut m, 3000 * MS, true);
        total.extend(mix);
        assert_eq!(m.stats().resyncs, 0);
        assert_eq!(m.stats().dropped_samples, 0);
        assert!((rms(&total[4800..total.len() - 9600]) - 0.3535).abs() < 0.01);
        // 周波数が保たれている：ゼロ交差数 ≒ 2 * 1000Hz * 秒数。
        let left: Vec<f32> = total.iter().step_by(2).copied().collect();
        let crossings = left
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        let secs = left.len() as f64 / 48_000.0;
        assert!(
            (crossings as f64 - 2000.0 * secs).abs() < 20.0,
            "{crossings} / {secs}"
        );
    }

    #[test]
    fn mixes_two_sources_and_keeps_them_separate() {
        let mut m = Mixer::new(2, 0);
        m.push(0, 0, 48_000, 2, &vec![0.25f32; 4800 * 2]);
        m.push(1, 0, 16_000, 1, &vec![0.5f32; 1600]);
        let (mix, per) = collect(&mut m, 100 * MS, true);
        assert_eq!(mix.len(), 4 * FRAME_LEN);
        assert!((mix[2000] - 0.75).abs() < 1e-3);
        assert!((per[0][2000] - 0.25).abs() < 1e-3);
        assert!((per[1][2000] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn clipping_is_bounded() {
        let mut m = Mixer::new(2, 0);
        m.push(0, 0, 48_000, 1, &vec![0.9f32; 2048]);
        m.push(1, 0, 48_000, 1, &vec![0.9f32; 2048]);
        let (mix, _) = collect(&mut m, 40 * MS, true);
        assert!(mix.iter().all(|v| v.abs() <= 1.0));
    }

    #[test]
    fn gap_and_device_switch_resync() {
        let mut m = Mixer::new(1, 0);
        m.push(0, 0, 48_000, 2, &vec![0.5f32; 4800 * 2]); // 0〜100ms
        // デバイス切替：400ms 欠落し、レートも変わる。
        m.push(0, 500 * MS, 44_100, 2, &vec![0.25f32; 4410 * 2]); // 500〜600ms
        assert_eq!(m.stats().resyncs, 1);
        let (mix, _) = collect(&mut m, 700 * MS, true);
        let at = |ms: usize| mix[ms * 48 * 2];
        assert!((at(50) - 0.5).abs() < 1e-3);
        assert_eq!(at(300), 0.0);
        assert!((at(550) - 0.25).abs() < 1e-3);
        assert_eq!(at(650), 0.0);
    }

    #[test]
    fn late_data_is_dropped_not_misplaced() {
        let mut m = Mixer::new(1, 0);
        let _ = collect(&mut m, 1000 * MS, true);
        m.push(0, 100 * MS, 48_000, 2, &vec![0.5f32; 480 * 2]);
        assert_eq!(m.stats().dropped_samples, 479);
        let (mix, _) = collect(&mut m, 2000 * MS, true);
        assert!(mix.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn overflow_is_dropped_and_memory_is_fixed() {
        let mut m = Mixer::new(1, 0);
        // 取り出さずに 10 秒ぶん投入。
        for k in 0..1000u64 {
            m.push(0, k * 10 * MS, 48_000, 2, &vec![0.1f32; 480 * 2]);
        }
        assert!(m.stats().dropped_samples > 0);
        assert_eq!(m.sources[0].ring.len(), CAP * 2);
    }

    #[test]
    fn clock_drift_is_absorbed_without_resync() {
        // デバイスのクロックが 300ppm 速い：10ms ごとに 480 サンプル来るが、
        // タイムスタンプは 300ppm ずつ早く進む。
        let mut m = Mixer::new(1, 200);
        let mut t = 0f64;
        let mut out = 0usize;
        for _ in 0..(100 * 120) {
            m.push(0, t as u64, 48_000, 2, &vec![0.3f32; 480 * 2]);
            t += 10.0 * MS as f64 * (1.0 - 300e-6);
            m.pull(t as u64, false, usize::MAX, |_, _| out += 1);
        }
        assert_eq!(m.stats().resyncs, 0, "ドリフトは微調整で吸収する");
        assert!(out > 5000);
    }

    #[test]
    fn latency_and_max_frames() {
        let mut m = Mixer::new(1, 250);
        let mut n = 0;
        m.pull(250 * MS, false, 100, |_, _| n += 1);
        assert_eq!(n, 0);
        m.pull(1250 * MS, false, 10, |_, _| n += 1);
        assert_eq!(n, 10);
        m.pull(1250 * MS, false, 1000, |_, _| n += 1);
        assert_eq!(n, 48_000 / FRAME);
    }

    #[test]
    fn input_shorter_than_one_frame_is_ignored() {
        let mut m = Mixer::new(1, 0);
        // ステレオなのに 1 サンプルしかない（1 フレームに満たない）。
        m.push(0, 0, 48_000, 2, &[0.5]);
        m.push(0, 0, 48_000, 2, &[]);
        assert_eq!(m.stats(), MixerStats::default());
    }
}
