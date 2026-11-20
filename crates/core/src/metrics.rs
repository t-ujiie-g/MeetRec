//! 自己計測（メモリ・CPU）。目標値との照合を診断ログに出す。

use crate::platform::RawUsage;

pub const IDLE_RSS_TARGET: u64 = 30 << 20;
pub const IDLE_CPU_TARGET: f64 = 0.1;
pub const REC_RSS_TARGET: u64 = 150 << 20;
pub const REC_CPU_TARGET: f64 = 10.0;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StateStats {
    pub samples: u64,
    pub rss_last: u64,
    pub rss_peak: u64,
    /// この状態で最初に測った RSS（時間経過で増えていないかの確認用）。
    pub rss_first: u64,
    pub cpu_seconds: f64,
    pub wall_seconds: f64,
    pub cpu_peak_percent: f64,
}

impl StateStats {
    /// 1 コアに対する平均 CPU 使用率（%）。
    pub fn cpu_avg_percent(&self) -> f64 {
        if self.wall_seconds > 0.0 {
            self.cpu_seconds / self.wall_seconds * 100.0
        } else {
            0.0
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Metrics {
    last: Option<(u64, RawUsage, bool)>,
    pub idle: StateStats,
    pub recording: StateStats,
}

impl Metrics {
    /// 計測値を取り込む。直前のサンプルと同じ状態（待機/録画）の区間だけを集計する。
    /// 戻り値は直近区間の CPU%（区間が取れたとき）。
    pub fn sample(&mut self, now_ns: u64, usage: RawUsage, recording: bool) -> Option<f64> {
        let mut interval_cpu = None;
        let stats = if recording {
            &mut self.recording
        } else {
            &mut self.idle
        };
        if let Some((t, prev, was_recording)) = self.last {
            if was_recording == recording && now_ns > t {
                let wall = (now_ns - t) as f64 / 1e9;
                let cpu = usage.cpu_time_ns.saturating_sub(prev.cpu_time_ns) as f64 / 1e9;
                stats.wall_seconds += wall;
                stats.cpu_seconds += cpu;
                let pct = cpu / wall * 100.0;
                stats.cpu_peak_percent = stats.cpu_peak_percent.max(pct);
                interval_cpu = Some(pct);
            }
        }
        if stats.samples == 0 {
            stats.rss_first = usage.rss_bytes;
        }
        stats.samples += 1;
        stats.rss_last = usage.rss_bytes;
        stats.rss_peak = stats.rss_peak.max(usage.rss_bytes);
        self.last = Some((now_ns, usage, recording));
        interval_cpu
    }

    /// 録画開始時に呼ぶ（録画ごとに統計を取り直す）。
    pub fn reset_recording(&mut self) {
        self.recording = StateStats::default();
    }

    pub fn report(&self) -> String {
        fn line(name: &str, s: &StateStats, rss_target: u64, cpu_target: f64) -> String {
            if s.samples == 0 {
                return format!("{name}: 計測なし\n");
            }
            let mb = |b: u64| b as f64 / (1 << 20) as f64;
            let verdict = |ok: bool| if ok { "OK" } else { "超過" };
            format!(
                "{name}: メモリ 現在 {:.1}MB / 最大 {:.1}MB / 初回 {:.1}MB（目標 {:.0}MB 以下: {}）、\
                 CPU 平均 {:.2}% / 最大 {:.2}%（1 コア比、目標 {}% 以下: {}）、計測 {:.0} 秒\n",
                mb(s.rss_last),
                mb(s.rss_peak),
                mb(s.rss_first),
                mb(rss_target),
                verdict(s.rss_peak <= rss_target),
                s.cpu_avg_percent(),
                s.cpu_peak_percent,
                cpu_target,
                verdict(s.cpu_avg_percent() <= cpu_target),
                s.wall_seconds,
            )
        }
        let mut out = line("待機中", &self.idle, IDLE_RSS_TARGET, IDLE_CPU_TARGET);
        out += &line("録画中", &self.recording, REC_RSS_TARGET, REC_CPU_TARGET);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    #[test]
    fn averages_per_state() {
        let mut m = Metrics::default();
        let u = |rss_mb: u64, cpu_ms: u64| RawUsage {
            rss_bytes: rss_mb << 20,
            cpu_time_ns: cpu_ms * 1_000_000,
        };
        assert_eq!(m.sample(0, u(20, 0), false), None);
        let pct = m.sample(60 * S, u(22, 30), false).unwrap();
        assert!((pct - 0.05).abs() < 1e-9);
        // 状態が変わった区間は集計しない。
        assert_eq!(m.sample(70 * S, u(80, 500), true), None);
        let pct = m.sample(80 * S, u(90, 1000), true).unwrap();
        assert!((pct - 5.0).abs() < 1e-9);
        assert_eq!(m.idle.rss_peak, 22 << 20);
        assert_eq!(m.recording.rss_first, 80 << 20);
        assert_eq!(m.recording.rss_peak, 90 << 20);
        assert!((m.idle.cpu_avg_percent() - 0.05).abs() < 1e-9);
        let r = m.report();
        assert!(r.contains("待機中") && r.contains("録画中") && r.contains("OK"));
        m.reset_recording();
        assert_eq!(m.recording.samples, 0);
    }
}
