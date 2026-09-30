//! Snapshot of everything a user interface shows, published by the engine thread.

use crate::session::MscStats;
use crate::source::SourceInfo;
use decdrm_core::fac::ChannelParams;
use decdrm_core::rx::{RxState, RxStatus, Visuals};
use std::collections::VecDeque;

/// Maximum number of log lines kept in the snapshot.
pub const LOG_LINES: usize = 200;
/// Input seconds between two [`MetricsSample`]s.
pub const METRICS_INTERVAL_S: f64 = 0.5;
/// Samples kept in [`Snapshot::recent_metrics`]: 128 s of input, so a UI that fetches a
/// snapshot every 100 ms misses none even while a recording is decoded at 1000× real
/// time.
pub const RECENT_METRICS: usize = 256;

/// The reception figures at one moment of the input, for history plots.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MetricsSample {
    /// Input position, seconds.
    pub t: f64,
    pub state: RxState,
    pub snr_db: Option<f64>,
    pub mer_db: Option<f64>,
    pub wmer_db: Option<f64>,
    pub doppler_hz: f64,
    pub delay_ms: f64,
    /// Sample-rate offset being corrected (Hz at 48 kHz).
    pub sro_hz: f64,
    /// Cumulative (good, bad) counts: FAC blocks, SDC blocks, multiplex frames (see
    /// [`MscStats`]) and audio frames (bad = concealed).
    pub fac: (u64, u64),
    pub sdc: (u64, u64),
    pub msc: (u64, u64),
    pub audio: (u64, u64),
}

/// Input side status.
#[derive(Debug, Clone, Default)]
pub struct InputStatus {
    pub info: SourceInfo,
    pub position_s: f64,
    /// RMS input level in dBFS (`None` before the first samples arrive).
    pub level_dbfs: Option<f32>,
    pub finished: bool,
}

/// One service of the multiplex as the UI lists it.
#[derive(Debug, Clone, Default)]
pub struct ServiceView {
    pub short_id: u8,
    pub service_id: u32,
    pub label: String,
    pub is_audio: bool,
    /// Human-readable coding/application description (one line).
    pub description: String,
    /// Language: the FAC language name, else the SDC ISO 639-2 code (empty if neither).
    pub language: String,
    /// Audio coding of an audio service (SDC type 9).
    pub audio: Option<AudioCodingView>,
    /// Bit rate of the audio stream, bit/s (from the multiplex description).
    pub audio_bitrate: Option<f64>,
    /// Share of the audio stream in the higher protected part A, percent: 0 = equal
    /// error protection (EEP), more = unequal error protection (UEP).
    pub audio_part_a_percent: Option<f64>,
    /// Data applications of the service (SDC type 5), in SDC order.
    pub apps: Vec<AppView>,
    /// Programme type (FAC, audio services; none for "no programme type").
    pub programme_type: Option<String>,
    /// Country (SDC type 12, ISO 3166 code in capitals).
    pub country: Option<String>,
    /// Conditional access (scrambled audio or data, FAC CA flags).
    pub ca: bool,
    /// Whether this receiver can decode the audio: false for a reserved coding (CELP,
    /// HVXC) and for EnCodec in a build without it.
    pub decodable: bool,
}

/// Audio coding of a service (SDC type 9), the facts Dream's service bars show.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioCodingView {
    /// "AAC", "xHE-AAC", "Opus", "EnCodec" or "reserved".
    pub codec: String,
    /// Spectral band replication (AAC; HE-AAC).
    pub sbr: bool,
    /// Parametric stereo (AAC audio mode 01; HE-AAC v2).
    pub parametric_stereo: bool,
    /// Two coded channels (audio mode 10).
    pub stereo: bool,
    /// Signalled sampling rate, Hz: the core coder's for AAC, the output rate for
    /// xHE-AAC.
    pub sample_rate_hz: u32,
    /// Rate of the decoded audio, Hz (AAC with SBR: twice the core rate).
    pub output_rate_hz: u32,
    /// Text messages in the audio stream.
    pub text: bool,
    /// MPEG Surround side information (AAC, xHE-AAC).
    pub surround: bool,
    /// Further codec detail, e.g. the EnCodec bit-rate tier.
    pub detail: Option<String>,
}

/// A data application of a service (SDC type 5).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AppView {
    /// E.g. "MOT Slideshow", "Journaline", "EPG", "TPEG", "application 0x123".
    pub name: String,
    pub user_app_id: u16,
    pub stream_id: u8,
    /// Packet mode (see `packet_id`); otherwise synchronous stream mode.
    pub packet_mode: bool,
    pub packet_id: u8,
    /// Bit rate of the application's stream, bit/s (a stream shared by several
    /// applications counts once for each).
    pub stream_bitrate: Option<f64>,
}

/// Audio decoding status.
#[derive(Debug, Clone, Default)]
pub struct AudioStatus {
    pub codec: String,
    pub frames_ok: u64,
    pub frames_bad: u64,
    pub playing: bool,
    pub buffer_ms: f32,
    pub drift_ppm: f64,
}

/// Smoothed power spectrum of the decoded audio (see `audio_out::AudioAnalyser`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioSpectrum {
    /// Power per bin in dB relative to full scale (a full-scale sine reads 0 dB), from
    /// 0 Hz up to half the sample rate: bin `j` lies at `j · bin_hz`. Empty before the
    /// first analysed block and when no audio was decoded for a while.
    pub db: Vec<f64>,
    /// Bin spacing, Hz (sample rate / FFT length).
    pub bin_hz: f64,
    /// Sample rate of the decoded audio, Hz.
    pub sample_rate: u32,
    /// Channels of the decoded audio (the spectrum is that of their mean).
    pub channels: u8,
}

/// Broadcast time and date from the SDC (type 8), minute resolution. Per ES 201 980
/// §6.4.3.9 it is sent in the first SDC block on or after each minute's edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BroadcastTime {
    /// UTC as seconds since the Unix epoch (a whole minute).
    pub unix_s: i64,
    /// Local time offset signalled with it, minutes (positive: ahead of UTC).
    pub local_offset_min: Option<i32>,
}

impl BroadcastTime {
    /// From the SDC time and date entity.
    pub fn from_sdc(t: &decdrm_core::mux::sdc::TimeAndDate) -> Self {
        // MJD 40587 is 1970-01-01.
        let days = i64::from(t.mjd) - 40_587;
        Self {
            unix_s: days * 86_400 + i64::from(t.hour) * 3600 + i64::from(t.minute) * 60,
            local_offset_min: t.local_offset.and_then(|o| o.minutes()),
        }
    }
}

/// Everything a UI needs, cheap enough to clone ~10 times per second.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Publish counter: increments with every published snapshot.
    pub seq: u64,
    pub rx: RxStatus,
    /// Channel parameters of the latest FAC (modulations, interleaving, occupancy).
    pub channel: Option<ChannelParams>,
    /// Multiplex frames decoded, and how many passed their content checks.
    pub msc: MscStats,
    pub visuals: Visuals,
    pub input: InputStatus,
    pub services: Vec<ServiceView>,
    /// The audio service being decoded; in a data-only multiplex the chosen (or
    /// first) data service.
    pub selected_service: Option<u8>,
    /// Latest complete text message of the selected audio service.
    pub text: Option<String>,
    pub audio: AudioStatus,
    /// Spectrum of the decoded audio (empty while none is decoded).
    pub audio_spectrum: AudioSpectrum,
    /// Broadcast time and date from the SDC (type 8), formatted.
    pub time_utc: Option<String>,
    /// The same as a number.
    pub time: Option<BroadcastTime>,
    /// Alternative frequencies, schedules and regions from the SDC, one line each.
    pub afs: Vec<String>,
    /// The figures every [`METRICS_INTERVAL_S`] of input, the last [`RECENT_METRICS`]
    /// of them, oldest first. A UI keeping a longer history appends the samples newer
    /// than the last one it has, so it gets every sample whatever the decoding speed.
    pub recent_metrics: VecDeque<MetricsSample>,
    pub log: VecDeque<String>,
    /// The worker stopped (end of file, error or stop command).
    pub stopped: bool,
    pub error: Option<String>,
}

impl Snapshot {
    pub fn push_log(&mut self, line: impl Into<String>) {
        self.log.push_back(line.into());
        while self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }

    /// Record `sample` if [`METRICS_INTERVAL_S`] of input has passed since the last one
    /// (the first is always taken); keeps the last [`RECENT_METRICS`].
    pub fn push_metrics(&mut self, sample: MetricsSample) {
        // A little tolerance: positions are sums of chunk durations.
        if self.recent_metrics.back().is_some_and(|last| sample.t - last.t < METRICS_INTERVAL_S - 1e-6) {
            return;
        }
        self.recent_metrics.push_back(sample);
        while self.recent_metrics.len() > RECENT_METRICS {
            self.recent_metrics.pop_front();
        }
    }
}

impl MetricsSample {
    /// The figures of `rx` and the counters, at input position `t`.
    pub fn new(t: f64, rx: &RxStatus, msc: &MscStats, audio_ok: u64, audio_bad: u64) -> Self {
        Self {
            t,
            state: rx.state,
            snr_db: rx.snr_db,
            mer_db: rx.mer_db,
            wmer_db: rx.wmer_db,
            doppler_hz: rx.doppler_hz,
            delay_ms: rx.delay_ms,
            sro_hz: rx.sro_hz,
            fac: (rx.fac_ok, rx.fac_bad),
            sdc: (rx.sdc_ok, rx.sdc_bad),
            msc: (msc.ok, msc.bad),
            audio: (audio_ok, audio_bad),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdrm_core::mux::sdc::{LocalTimeOffset, TimeAndDate};

    #[test]
    fn metrics_are_sampled_and_bounded() {
        let mut snap = Snapshot::default();
        let sample = |t: f64| MetricsSample { t, ..Default::default() };
        // A chunk every 50 ms of input: one sample per half second is kept.
        for i in 0..=20 {
            snap.push_metrics(sample(f64::from(i) * 0.05));
        }
        let times: Vec<f64> = snap.recent_metrics.iter().map(|s| s.t).collect();
        assert_eq!(times.len(), 3, "{times:?}");
        assert!((times[1] - 0.5).abs() < 1e-9 && (times[2] - 1.0).abs() < 1e-9, "{times:?}");
        for i in 0..1000 {
            snap.push_metrics(sample(2.0 + f64::from(i) * METRICS_INTERVAL_S));
        }
        assert_eq!(snap.recent_metrics.len(), RECENT_METRICS);
        assert_eq!(snap.recent_metrics.back().unwrap().t, 2.0 + 999.0 * METRICS_INTERVAL_S);
    }

    #[test]
    fn broadcast_time_from_sdc() {
        let mut t = TimeAndDate::from_utc(2018, 9, 10, 12, 21);
        assert_eq!(BroadcastTime::from_sdc(&t), BroadcastTime { unix_s: 1_536_582_060, local_offset_min: None });
        t.local_offset = Some(LocalTimeOffset::from_minutes(330));
        assert_eq!(BroadcastTime::from_sdc(&t).local_offset_min, Some(330));
    }
}
