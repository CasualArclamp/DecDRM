//! Reception history for the History plot tab: the quality and channel figures of the
//! last five minutes of signal, and the FAC/SDC/MSC/audio error rates per 10 s.
//!
//! The engine samples the figures every half second of input and publishes the recent
//! samples with each snapshot (`Snapshot::recent_metrics`); this appends the ones it
//! has not seen yet. Time is therefore the input position (seconds of signal): a
//! recording decoded faster than real time fills the history completely, just as live
//! reception does. Memory is bounded: at most 601 samples (five minutes at two per
//! second) and 31 bins of counts. Plain logic without egui, so it can be unit-tested;
//! `panels::history` draws it.

use crate::plots::Points;
use decdrm_core::rx::RxState;
use decdrm_engine::{METRICS_INTERVAL_S, MetricsSample, Snapshot};
use std::collections::VecDeque;

/// Signal time kept, seconds.
pub const HISTORY_SECONDS: f64 = 300.0;
/// Width of the error-rate bins, seconds.
pub const BIN_SECONDS: f64 = 10.0;
/// Hard limit on the samples kept (belt and braces; the time window already bounds it).
const MAX_SAMPLES: usize = (HISTORY_SECONDS / METRICS_INTERVAL_S) as usize + 1;
/// A longer pause between two samples (e.g. samples the GUI missed while it was busy)
/// breaks the curves instead of bridging the gap with a straight line.
const GAP_S: f64 = 3.0 * METRICS_INTERVAL_S;

/// A figure followed over time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Snr,
    Mer,
    Wmer,
    Doppler,
    DelaySpread,
    Sro,
}

impl Metric {
    /// The figures in dB (one plot).
    pub const QUALITY: [Metric; 3] = [Self::Snr, Self::Mer, Self::Wmer];
    /// The channel figures (another plot): Hz, ms and Hz.
    pub const CHANNEL: [Metric; 3] = [Self::Doppler, Self::DelaySpread, Self::Sro];

    pub fn label(self) -> &'static str {
        match self {
            Self::Snr => "SNR",
            Self::Mer => "MER",
            Self::Wmer => "WMER",
            Self::Doppler => "Doppler",
            Self::DelaySpread => "Delay spread",
            Self::Sro => "SRO",
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            Self::Snr | Self::Mer | Self::Wmer => "dB",
            Self::Doppler | Self::Sro => "Hz",
            Self::DelaySpread => "ms",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// A channel whose blocks are checked (CRC or decoder verdict).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checked {
    Fac,
    Sdc,
    Msc,
    Audio,
}

impl Checked {
    pub const ALL: [Checked; 4] = [Self::Fac, Self::Sdc, Self::Msc, Self::Audio];

    pub fn label(self) -> &'static str {
        match self {
            Self::Fac => "FAC",
            Self::Sdc => "SDC",
            Self::Msc => "MSC",
            Self::Audio => "Audio",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// One reading of the figures; `None` where unknown (e.g. not synchronised).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Sample {
    t: f64,
    values: [Option<f64>; 6],
}

/// Good and bad blocks counted within one bin of [`BIN_SECONDS`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ErrorBin {
    /// Start of the bin, seconds of signal (a multiple of [`BIN_SECONDS`]).
    pub start: f64,
    /// (good, bad) per [`Checked`] channel.
    pub counts: [(u64, u64); 4],
}

impl ErrorBin {
    /// Percentage of bad blocks, `None` if the channel had none at all in this bin.
    pub fn rate(&self, c: Checked) -> Option<f64> {
        let (ok, bad) = self.counts[c.index()];
        (ok + bad > 0).then(|| 100.0 * bad as f64 / (ok + bad) as f64)
    }

    pub fn count(&self, c: Checked) -> (u64, u64) {
        self.counts[c.index()]
    }
}

/// The figures of a sample, or nothing while no signal is being received.
fn values_of(s: &MetricsSample) -> [Option<f64>; 6] {
    if s.state == RxState::Acquisition {
        return [None; 6];
    }
    let finite = |v: f64| v.is_finite().then_some(v);
    [
        s.snr_db.and_then(finite),
        s.mer_db.and_then(finite),
        s.wmer_db.and_then(finite),
        finite(s.doppler_hz),
        finite(s.delay_ms),
        finite(s.sro_hz),
    ]
}

/// The cumulative (good, bad) counters of a sample, per [`Checked`] channel.
fn counters_of(s: &MetricsSample) -> [(u64, u64); 4] {
    [s.fac, s.sdc, s.msc, s.audio]
}

#[derive(Debug, Clone, Default)]
pub struct History {
    /// Oldest first.
    samples: VecDeque<Sample>,
    bins: VecDeque<ErrorBin>,
    /// The counters of the previous sample, to take differences.
    counters: Option<[(u64, u64); 4]>,
    /// Input position of the newest sample.
    latest: Option<f64>,
}

impl History {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Input position of the newest sample: the "now" of the plots.
    pub fn latest(&self) -> Option<f64> {
        self.latest
    }

    pub fn samples(&self) -> usize {
        self.samples.len()
    }

    /// Take the samples of a snapshot that are newer than the newest one kept (the
    /// history is cleared when a new engine run starts).
    pub fn push(&mut self, snap: &Snapshot) {
        for s in &snap.recent_metrics {
            if self.latest.is_none_or(|l| s.t > l) {
                self.add(s);
            }
        }
    }

    fn add(&mut self, s: &MetricsSample) {
        if !s.t.is_finite() {
            return;
        }
        self.latest = Some(s.t);
        self.count_errors(s.t, counters_of(s));
        self.samples.push_back(Sample {
            t: s.t,
            values: values_of(s),
        });
        self.trim(s.t);
    }

    /// Add the blocks counted since the previous sample to the bin of time `t`.
    fn count_errors(&mut self, t: f64, now: [(u64, u64); 4]) {
        let Some(prev) = self.counters.replace(now) else {
            return; // the first reading is only the baseline
        };
        let start = (t / BIN_SECONDS).floor() * BIN_SECONDS;
        if self.bins.back().is_none_or(|b| b.start < start) {
            self.bins.push_back(ErrorBin {
                start,
                ..ErrorBin::default()
            });
        }
        let Some(bin) = self.bins.back_mut() else {
            return;
        };
        for ((count, (ok, bad)), (prev_ok, prev_bad)) in bin.counts.iter_mut().zip(now).zip(prev) {
            // A counter that went back (receiver restarted) gives no difference this once.
            if ok >= prev_ok && bad >= prev_bad {
                count.0 += ok - prev_ok;
                count.1 += bad - prev_bad;
            }
        }
    }

    fn trim(&mut self, t: f64) {
        let oldest = t - HISTORY_SECONDS;
        while self.samples.front().is_some_and(|s| s.t < oldest) || self.samples.len() > MAX_SAMPLES
        {
            self.samples.pop_front();
        }
        while self
            .bins
            .front()
            .is_some_and(|b| b.start + BIN_SECONDS <= oldest)
        {
            self.bins.pop_front();
        }
    }

    /// Signal time covered by the samples, seconds.
    pub fn span(&self) -> f64 {
        match (self.samples.front(), self.latest) {
            (Some(first), Some(latest)) => (latest - first.t).max(0.0),
            _ => 0.0,
        }
    }

    /// Width of the time axis, seconds: the covered span rounded up to whole minutes,
    /// at least one and at most five, so a short recording is not squeezed into a
    /// corner.
    pub fn window(&self) -> f64 {
        ((self.span() / 60.0).ceil() * 60.0).clamp(60.0, HISTORY_SECONDS)
    }

    /// The curve of `m` as runs of consecutive known values, x = seconds relative to
    /// the newest sample (≤ 0). An unknown value or a pause between samples ends a
    /// run, so the plot shows a gap.
    pub fn segments(&self, m: Metric) -> Vec<Points> {
        let Some(latest) = self.latest else {
            return Vec::new();
        };
        let mut out: Vec<Points> = Vec::new();
        let mut run: Points = Vec::new();
        let mut last_t = f64::NEG_INFINITY;
        for s in &self.samples {
            if s.t - last_t > GAP_S && !run.is_empty() {
                out.push(std::mem::take(&mut run));
            }
            last_t = s.t;
            match s.values[m.index()] {
                Some(v) => run.push([s.t - latest, v]),
                None if !run.is_empty() => out.push(std::mem::take(&mut run)),
                None => {}
            }
        }
        if !run.is_empty() {
            out.push(run);
        }
        out
    }

    /// Error rates of channel `c` as points (x: the middle of the time the bin covers,
    /// relative to the newest sample, so the newest bin, still filling, stays at or
    /// before now; y: percent), for the bins in which it had blocks.
    pub fn error_points(&self, c: Checked) -> Points {
        let Some(latest) = self.latest else {
            return Vec::new();
        };
        self.bins
            .iter()
            .filter_map(|b| {
                let end = (b.start + BIN_SECONDS).min(latest);
                Some([(b.start + end) / 2.0 - latest, b.rate(c)?])
            })
            .collect()
    }

    /// The bin at `x` seconds relative to the newest sample.
    pub fn bin_at(&self, x: f64) -> Option<&ErrorBin> {
        let t = self.latest? + x;
        self.bins
            .iter()
            .find(|b| (b.start..b.start + BIN_SECONDS).contains(&t))
    }

    #[cfg(test)]
    fn bins(&self) -> impl Iterator<Item = &ErrorBin> {
        self.bins.iter()
    }
}

/// Seconds relative to now as `−m:ss` (`0` for now).
pub fn fmt_ago(x: f64) -> String {
    let s = (-x).round().max(0.0) as u64;
    if s == 0 {
        "0".into()
    } else {
        format!("−{}:{:02}", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sample at input position `t` with the given SNR (none: not synchronised) and
    /// FAC and audio counters.
    fn sample(t: f64, snr: Option<f64>, fac: (u64, u64), audio: (u64, u64)) -> MetricsSample {
        MetricsSample {
            t,
            state: if snr.is_some() {
                RxState::Locked
            } else {
                RxState::Acquisition
            },
            snr_db: snr,
            mer_db: snr.map(|v| v - 1.0),
            doppler_hz: 0.5,
            delay_ms: 1.25,
            sro_hz: -2.0,
            fac,
            audio,
            ..MetricsSample::default()
        }
    }

    /// Feed samples through snapshots the way the engine publishes them.
    fn feed(h: &mut History, samples: &[MetricsSample]) {
        let mut snap = Snapshot::default();
        for s in samples {
            snap.push_metrics(*s);
            h.push(&snap);
        }
    }

    #[test]
    fn samples_are_merged_once_and_bounded() {
        let mut h = History::default();
        assert!(h.is_empty());
        // Ten minutes of samples; each snapshot repeats the recent ones.
        let all: Vec<MetricsSample> = (0..1200)
            .map(|i| {
                sample(
                    f64::from(i) * 0.5,
                    Some(20.0),
                    (u64::from(i as u32), 0),
                    (0, 0),
                )
            })
            .collect();
        feed(&mut h, &all);
        assert_eq!(h.samples(), MAX_SAMPLES, "five minutes at two per second");
        assert!((h.span() - HISTORY_SECONDS).abs() < 1e-9, "{}", h.span());
        assert_eq!(h.window(), HISTORY_SECONDS);
        assert!(h.bins().count() <= 31);
        let snr = h.segments(Metric::Snr);
        assert_eq!(snr.len(), 1);
        assert_eq!(snr[0].first().unwrap()[0], -HISTORY_SECONDS);
        assert_eq!(
            snr[0].last().unwrap()[0],
            0.0,
            "x is relative to the newest"
        );
        assert_eq!(h.latest(), Some(599.5));
        // A snapshot with nothing new changes nothing.
        let before = h.samples();
        let mut snap = Snapshot::default();
        snap.push_metrics(all[1199]);
        h.push(&snap);
        assert_eq!(h.samples(), before);
    }

    #[test]
    fn unknown_values_and_pauses_split_the_curve() {
        let mut h = History::default();
        let figures = [Some(20.0), Some(21.0), None, None, Some(19.0)];
        let samples: Vec<MetricsSample> = figures
            .into_iter()
            .enumerate()
            .map(|(i, snr)| sample(i as f64 * 0.5, snr, (0, 0), (0, 0)))
            .collect();
        feed(&mut h, &samples);
        let snr = h.segments(Metric::Snr);
        assert_eq!(
            snr,
            vec![vec![[-2.0, 20.0], [-1.5, 21.0]], vec![[0.0, 19.0]]]
        );
        assert_eq!(h.segments(Metric::Mer)[0][0], [-2.0, 19.0]);
        // The channel figures are only recorded while a signal is received.
        let doppler = h.segments(Metric::Doppler);
        assert_eq!(
            doppler,
            vec![vec![[-2.0, 0.5], [-1.5, 0.5]], vec![[0.0, 0.5]]]
        );
        assert_eq!(h.window(), 60.0, "at least a minute");
        // Samples the GUI missed: the curve is broken, not bridged.
        feed(&mut h, &[sample(10.0, Some(18.0), (0, 0), (0, 0))]);
        let snr = h.segments(Metric::Snr);
        assert_eq!(snr.len(), 3);
        assert_eq!(snr[2], vec![[0.0, 18.0]]);
    }

    #[test]
    fn error_rates_per_bin() {
        let mut h = History::default();
        // Baseline at 5 s, then 10 FAC blocks with 2 bad and 20 audio frames with 5 bad
        // by 9 s (bin 0 … 10 s), then only good ones up to 15 s (bin 10 … 20 s).
        feed(
            &mut h,
            &[
                sample(5.0, Some(20.0), (100, 10), (50, 0)),
                sample(9.0, Some(20.0), (108, 12), (65, 5)),
                sample(15.0, Some(20.0), (118, 12), (85, 5)),
            ],
        );
        let bins: Vec<&ErrorBin> = h.bins().collect();
        assert_eq!(bins.len(), 2);
        assert_eq!(bins[0].start, 0.0);
        assert_eq!(bins[0].count(Checked::Fac), (8, 2));
        assert_eq!(bins[0].rate(Checked::Fac), Some(20.0));
        assert_eq!(bins[0].rate(Checked::Audio), Some(25.0));
        assert_eq!(bins[1].rate(Checked::Fac), Some(0.0));
        assert_eq!(bins[0].rate(Checked::Sdc), None, "no SDC blocks at all");
        // Points in the middle of the time each bin covers, relative to the newest
        // sample (15 s): 5 s, and 12.5 s for the bin still filling.
        assert_eq!(
            h.error_points(Checked::Fac),
            vec![[-10.0, 20.0], [-2.5, 0.0]]
        );
        assert!(h.error_points(Checked::Msc).is_empty());
        assert_eq!(h.bin_at(-12.0).map(|b| b.start), Some(0.0));
        assert_eq!(h.bin_at(-1.0).map(|b| b.start), Some(10.0));
        assert_eq!(h.bin_at(-20.0), None);
    }

    #[test]
    fn counter_resets() {
        let mut h = History::default();
        feed(
            &mut h,
            &[
                sample(1.0, Some(20.0), (10, 0), (0, 0)),
                // The receiver restarted: its counters start again at zero.
                sample(2.0, Some(20.0), (1, 1), (0, 0)),
                sample(3.0, Some(20.0), (3, 1), (0, 0)),
            ],
        );
        let bins: Vec<&ErrorBin> = h.bins().collect();
        assert_eq!(
            bins[0].count(Checked::Fac),
            (2, 0),
            "no difference across the reset"
        );
        h.clear();
        assert!(h.is_empty() && h.bins().count() == 0 && h.latest().is_none());
    }

    #[test]
    fn relative_time_labels() {
        assert_eq!(fmt_ago(0.0), "0");
        assert_eq!(fmt_ago(-0.2), "0");
        assert_eq!(fmt_ago(-90.0), "−1:30");
        assert_eq!(fmt_ago(-300.0), "−5:00");
    }
}
