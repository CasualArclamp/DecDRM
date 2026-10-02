//! Status indicators ("LEDs") and value formatting for the status strip.
//!
//! Everything here is plain logic without egui drawing, so the colour decisions can be
//! unit-tested. Times are seconds since an arbitrary epoch (`f64`) rather than
//! `Instant`s for the same reason.

use decdrm_core::fac::{Interleaving, MscMode, SdcMode};
use decdrm_core::rx::{RxState, RxStatus};
use decdrm_core::rx::framesync::FrameSyncState;
use decdrm_engine::{AudioStatus, InputStatus, MscStats, Snapshot};
use std::collections::VecDeque;

/// State of one indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Led {
    /// No information (engine not running, feature not active).
    #[default]
    Off,
    Red,
    Yellow,
    Green,
}

impl Led {
    pub fn word(self) -> &'static str {
        match self {
            Self::Off => "inactive",
            Self::Red => "bad",
            Self::Yellow => "partly OK",
            Self::Green => "OK",
        }
    }
}

/// RMS input levels (dBFS) for the input indicator. An OFDM signal has a crest factor
/// of about 10 dB, so an RMS level above −10 dBFS already risks clipped peaks.
pub const LEVEL_SILENT_DBFS: f32 = -90.0;
pub const LEVEL_WEAK_DBFS: f32 = -60.0;
pub const LEVEL_HOT_DBFS: f32 = -10.0;
pub const LEVEL_CLIP_DBFS: f32 = -3.0;

/// Input: level within a sensible range (grey until the first samples arrive).
pub fn input_led(running: bool, input: &InputStatus) -> Led {
    let Some(l) = input.level_dbfs.filter(|_| running) else {
        return Led::Off;
    };
    if !l.is_finite() || l <= LEVEL_SILENT_DBFS || l >= LEVEL_CLIP_DBFS {
        Led::Red
    } else if l <= LEVEL_WEAK_DBFS || l >= LEVEL_HOT_DBFS {
        Led::Yellow
    } else {
        Led::Green
    }
}

/// Time synchronisation: red while the spectrum is searched for a DRM signal, yellow
/// once found but before the robustness mode / symbol timing is known, green after.
pub fn time_sync_led(running: bool, rx: &RxStatus) -> Led {
    if !running {
        Led::Off
    } else if rx.dc_frequency_hz.is_none() {
        Led::Red
    } else if rx.mode.is_none() {
        Led::Yellow
    } else {
        Led::Green
    }
}

/// Frame synchronisation from the time-reference pilots.
pub fn frame_sync_led(running: bool, rx: &RxStatus) -> Led {
    if !running {
        return Led::Off;
    }
    if rx.mode.is_none() {
        return Led::Red;
    }
    match rx.frame_sync {
        FrameSyncState::Searching => Led::Red,
        FrameSyncState::Locked => Led::Green,
        FrameSyncState::Doubtful | FrameSyncState::Corrected => Led::Yellow,
    }
}

/// CRC indicator from the number of good and bad blocks seen in a time window:
/// only good → green, mixed → yellow, only bad or nothing at all → red.
pub fn crc_led(ok: u64, bad: u64) -> Led {
    match (ok > 0, bad > 0) {
        (true, false) => Led::Green,
        (true, true) => Led::Yellow,
        _ => Led::Red,
    }
}

/// One reading of a pair of cumulative good/bad counters.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Reading {
    t: f64,
    ok: u64,
    bad: u64,
}

/// Turns cumulative good/bad counters (e.g. `RxStatus::fac_ok`/`fac_bad`) into the
/// number of good and bad events within the last `window_s` seconds.
#[derive(Debug, Clone)]
pub struct CrcHistory {
    window_s: f64,
    readings: VecDeque<Reading>,
}

impl CrcHistory {
    pub fn new(window_s: f64) -> Self {
        Self {
            window_s,
            readings: VecDeque::new(),
        }
    }

    /// Add a reading taken at time `t` (non-decreasing).
    pub fn push(&mut self, t: f64, ok: u64, bad: u64) {
        if let Some(last) = self.readings.back()
            && (ok < last.ok || bad < last.bad)
        {
            // The counters restarted (new engine): older readings are meaningless.
            self.readings.clear();
        }
        self.readings.push_back(Reading { t, ok, bad });
        // Keep the newest reading at or before the window start as the baseline.
        while self.readings.len() >= 2 && self.readings[1].t <= t - self.window_s {
            self.readings.pop_front();
        }
    }

    /// Good and bad events within the window (since the baseline reading).
    pub fn deltas(&self) -> (u64, u64) {
        match (self.readings.front(), self.readings.back()) {
            (Some(a), Some(b)) => (b.ok - a.ok, b.bad - a.bad),
            _ => (0, 0),
        }
    }

    pub fn led(&self) -> Led {
        let (ok, bad) = self.deltas();
        crc_led(ok, bad)
    }
}

/// The indicators of the status strip.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Leds {
    pub input: Led,
    pub time_sync: Led,
    pub frame_sync: Led,
    pub fac: Led,
    pub sdc: Led,
    pub msc: Led,
    pub audio: Led,
}

/// Counter histories behind the CRC indicators plus the current indicator states.
#[derive(Debug, Clone)]
pub struct Indicators {
    fac: CrcHistory,
    sdc: CrcHistory,
    msc: CrcHistory,
    audio: CrcHistory,
    pub leds: Leds,
}

impl Default for Indicators {
    fn default() -> Self {
        Self {
            // A FAC block and a multiplex frame arrive every 400 ms, an SDC block
            // every 1.2 s; each window spans about three of them.
            fac: CrcHistory::new(1.3),
            sdc: CrcHistory::new(2.5),
            msc: CrcHistory::new(1.3),
            audio: CrcHistory::new(1.3),
            leds: Leds::default(),
        }
    }
}

impl Indicators {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Update from a new snapshot taken at time `t`.
    pub fn update(&mut self, t: f64, snap: &Snapshot, running: bool) {
        let rx = &snap.rx;
        self.fac.push(t, rx.fac_ok, rx.fac_bad);
        self.sdc.push(t, rx.sdc_ok, rx.sdc_bad);
        let a = &snap.audio;
        self.audio.push(t, a.frames_ok, a.frames_bad);
        self.msc.push(t, snap.msc.ok, snap.msc.bad);

        let gate = |led: Led| if running { led } else { Led::Off };
        // MDI/RSCI input has no signal: the input light shows whether frames come, the
        // sync lights what the RSCI receiver reports (or the FAC CRCs of plain MDI).
        let (input, time_sync, frame_sync) = match snap.input.mdi.as_ref().filter(|_| running) {
            Some(m) => {
                let sync = match rx.state {
                    RxState::Locked => Led::Green,
                    RxState::Tracking => Led::Yellow,
                    RxState::Acquisition => Led::Red,
                };
                (if m.stats.frames > 0 { Led::Green } else { Led::Yellow }, sync, sync)
            }
            None => (input_led(running, &snap.input), time_sync_led(running, rx), frame_sync_led(running, rx)),
        };
        self.leds = Leds {
            input,
            time_sync,
            frame_sync,
            fac: gate(self.fac.led()),
            sdc: gate(self.sdc.led()),
            msc: msc_led(running, &snap.msc, &self.msc),
            audio: audio_led(running, a, &self.audio),
        };
    }
}

/// MSC: from the engine's per-multiplex-frame verdicts. Grey until a frame with
/// checkable content has been judged (none are while the long interleaver fills, or
/// if no service carries a CRC).
pub fn msc_led(running: bool, msc: &MscStats, history: &CrcHistory) -> Led {
    if !running || msc.ok + msc.bad == 0 {
        Led::Off
    } else {
        history.led()
    }
}

/// Audio decoding: grey until a decoder reports anything, then from the frame CRCs.
pub fn audio_led(running: bool, audio: &AudioStatus, history: &CrcHistory) -> Led {
    if !running || (audio.codec.is_empty() && audio.frames_ok == 0 && audio.frames_bad == 0) {
        Led::Off
    } else {
        history.led()
    }
}

/// `12.3 dB`, or a dash when unknown.
pub fn fmt_db(v: Option<f64>) -> String {
    v.filter(|x| x.is_finite())
        .map_or_else(|| "–".to_string(), |x| format!("{x:.1} dB"))
}

/// Seconds as `m:ss.s`.
pub fn fmt_time(s: f64) -> String {
    // Round once to tenths, then split, so 59.96 s becomes "1:00.0" and not "0:60.0".
    let tenths = (s.max(0.0) * 10.0).round() as u64;
    format!(
        "{}:{:02}.{}",
        tenths / 600,
        (tenths % 600) / 10,
        tenths % 10
    )
}

/// Fraction of bad items, as a percentage string.
pub fn fmt_error_rate(ok: u64, bad: u64) -> String {
    let total = ok + bad;
    if total == 0 {
        "–".into()
    } else {
        format!("{:.1} %", 100.0 * bad as f64 / total as f64)
    }
}

pub fn fmt_msc_mode(m: MscMode) -> &'static str {
    match m {
        MscMode::Qam64Sm => "64-QAM",
        MscMode::Qam64HmSym => "64-QAM HMsym",
        MscMode::Qam64HmMix => "64-QAM HMmix",
        MscMode::Qam16Sm => "16-QAM",
    }
}

pub fn fmt_sdc_mode(m: SdcMode) -> &'static str {
    match m {
        SdcMode::Qam16 => "16-QAM",
        SdcMode::Qam4 => "4-QAM",
    }
}

/// Interleaving depth as a word, and its duration for a tooltip.
pub fn fmt_interleaving(i: Interleaving) -> (&'static str, &'static str) {
    match i {
        Interleaving::Long => ("long", "long interleaving: 2 s (5 multiplex frames)"),
        Interleaving::Short => ("short", "short interleaving: 400 ms (1 multiplex frame)"),
    }
}

/// Tooltip for the MSC indicator with the engine's frame counts.
pub fn msc_help(m: &MscStats) -> String {
    format!(
        "multiplex frames whose audio frames and data packets passed their CRCs \
         ({} ok, {} bad, {} decoded in all)",
        m.ok, m.bad, m.frames
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdrm_core::params::RobustnessMode;

    fn input(level: Option<f32>) -> InputStatus {
        InputStatus {
            position_s: 1.0,
            level_dbfs: level,
            ..Default::default()
        }
    }

    #[test]
    fn input_levels() {
        assert_eq!(input_led(false, &input(Some(-30.0))), Led::Off);
        assert_eq!(input_led(true, &input(None)), Led::Off, "nothing read yet");
        assert_eq!(input_led(true, &input(Some(-30.0))), Led::Green);
        assert_eq!(input_led(true, &input(Some(-8.0))), Led::Yellow);
        assert_eq!(input_led(true, &input(Some(-1.0))), Led::Red);
        assert_eq!(input_led(true, &input(Some(-70.0))), Led::Yellow);
        assert_eq!(input_led(true, &input(Some(-180.0))), Led::Red);
        assert_eq!(input_led(true, &input(Some(f32::NAN))), Led::Red);
    }

    #[test]
    fn sync_leds() {
        let mut rx = RxStatus::default();
        assert_eq!(time_sync_led(false, &rx), Led::Off);
        assert_eq!(time_sync_led(true, &rx), Led::Red);
        assert_eq!(frame_sync_led(true, &rx), Led::Red);
        rx.dc_frequency_hz = Some(12_000.0);
        assert_eq!(time_sync_led(true, &rx), Led::Yellow);
        rx.mode = Some(RobustnessMode::B);
        assert_eq!(time_sync_led(true, &rx), Led::Green);
        assert_eq!(
            frame_sync_led(true, &rx),
            Led::Red,
            "frame sync still searching"
        );
        rx.frame_sync = FrameSyncState::Doubtful;
        assert_eq!(frame_sync_led(true, &rx), Led::Yellow);
        rx.frame_sync = FrameSyncState::Locked;
        assert_eq!(frame_sync_led(true, &rx), Led::Green);
        assert_eq!(frame_sync_led(false, &rx), Led::Off);
    }

    #[test]
    fn crc_colours() {
        assert_eq!(crc_led(3, 0), Led::Green);
        assert_eq!(crc_led(3, 1), Led::Yellow);
        assert_eq!(crc_led(0, 2), Led::Red);
        assert_eq!(
            crc_led(0, 0),
            Led::Red,
            "nothing decoded while running is bad"
        );
    }

    #[test]
    fn crc_history_window() {
        let mut h = CrcHistory::new(1.0);
        h.push(0.0, 0, 0);
        assert_eq!(h.deltas(), (0, 0));
        h.push(0.4, 1, 0);
        h.push(0.8, 2, 0);
        assert_eq!(h.led(), Led::Green);
        h.push(1.2, 2, 1);
        assert_eq!(h.deltas(), (2, 1), "baseline is the reading at t=0.0");
        assert_eq!(h.led(), Led::Yellow);
        // The good blocks age out of the window (baseline now the reading at 0.8 s);
        // only the bad one remains.
        h.push(1.9, 2, 1);
        assert_eq!(h.deltas(), (0, 1));
        assert_eq!(h.led(), Led::Red);
        // The bad block (between 0.8 and 1.2 s) has aged out too.
        h.push(2.3, 2, 1);
        assert_eq!(h.deltas(), (0, 0));
        h.push(3.5, 2, 1);
        assert_eq!(h.deltas(), (0, 0));
        // A counter reset (new engine) starts over.
        h.push(3.6, 0, 0);
        h.push(3.7, 1, 0);
        assert_eq!(h.deltas(), (1, 0));
    }

    #[test]
    fn indicator_update_gates_on_running() {
        let mut ind = Indicators::default();
        let mut snap = Snapshot::default();
        snap.rx.fac_ok = 5;
        ind.update(0.0, &snap, true);
        snap.rx.fac_ok = 7;
        ind.update(0.5, &snap, true);
        assert_eq!(ind.leds.fac, Led::Green);
        assert_eq!(ind.leds.sdc, Led::Red, "no SDC yet");
        assert_eq!(ind.leds.msc, Led::Off, "no MSC information at all");
        assert_eq!(ind.leds.audio, Led::Off, "no audio decoder yet");
        ind.update(0.6, &snap, false);
        assert_eq!(ind.leds, Leds::default(), "everything off when stopped");
    }

    #[test]
    fn msc_and_audio_from_counters() {
        let mut ind = Indicators::default();
        let mut snap = Snapshot::default();
        snap.audio.codec = "AAC".into();
        // Frames decoded while the interleaver fills are not judged: MSC stays grey.
        snap.msc = MscStats {
            frames: 4,
            ok: 0,
            bad: 0,
        };
        ind.update(0.0, &snap, true);
        assert_eq!(ind.leds.audio, Led::Red, "decoder present but no frames");
        assert_eq!(ind.leds.msc, Led::Off);
        snap.audio.frames_ok = 10;
        snap.msc = MscStats {
            frames: 6,
            ok: 1,
            bad: 1,
        };
        ind.update(0.4, &snap, true);
        assert_eq!(ind.leds.audio, Led::Green);
        assert_eq!(ind.leds.msc, Led::Yellow, "one good and one bad frame");
        snap.msc = MscStats {
            frames: 7,
            ok: 2,
            bad: 1,
        };
        ind.update(2.0, &snap, true);
        assert_eq!(ind.leds.msc, Led::Green, "the bad frame aged out");
    }

    #[test]
    fn formatting() {
        assert_eq!(fmt_db(Some(12.345)), "12.3 dB");
        assert_eq!(fmt_db(None), "–");
        assert_eq!(fmt_db(Some(f64::NAN)), "–");
        assert_eq!(fmt_time(75.25), "1:15.3");
        assert_eq!(fmt_time(5.0), "0:05.0");
        assert_eq!(fmt_time(59.96), "1:00.0");
        assert_eq!(fmt_error_rate(0, 0), "–");
        assert_eq!(fmt_error_rate(3, 1), "25.0 %");
        assert_eq!(fmt_msc_mode(MscMode::Qam64HmMix), "64-QAM HMmix");
        assert_eq!(fmt_msc_mode(MscMode::Qam16Sm), "16-QAM");
        assert_eq!(fmt_sdc_mode(SdcMode::Qam4), "4-QAM");
        assert_eq!(fmt_interleaving(Interleaving::Long).0, "long");
        let help = msc_help(&MscStats {
            frames: 10,
            ok: 7,
            bad: 1,
        });
        assert!(help.contains("7 ok, 1 bad, 10 decoded"), "{help}");
    }
}
