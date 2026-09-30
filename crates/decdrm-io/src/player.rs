//! Programme-audio playback with clock-drift compensation.
//!
//! # The problem
//!
//! Decoded DRM audio arrives at the broadcaster's sample clock (as recovered through the
//! input sound card); the output sound card consumes it at its own clock. Two crystals never
//! agree exactly — 50–200 ppm apart is typical, more for a web SDR feeding a virtual cable —
//! so a plain FIFO between them slowly fills up (latency grows without bound) or runs dry
//! (periodic dropouts). At 100 ppm a 500 ms buffer is exhausted in under 90 minutes.
//!
//! # The fix
//!
//! [`AudioPlayer`] resamples everything to the device rate with an adjustable ratio and runs
//! a slow PI control loop on the queue depth:
//!
//! ```text
//!   decoded PCM ─► Resampler (ratio × (1+u)) ─► channel map ─► ring buffer ─► sound card
//!                        ▲                                          │
//!                        └──── u = −(Kp·e + Ki·∫e dt), |u| ≤ max ◄──┘ e = mean fill − target
//! ```
//!
//! * The queue depth is averaged over time *inside the audio callback* (time-weighted over
//!   each callback), so the bursty arrival of DRM audio (a whole 400 ms super-frame at a
//!   time) averages out; the controller updates once per second of device time and further
//!   low-pass filters the average (τ = 5 s).
//! * `Kp = 0.04 s⁻¹` (25 ms of excess buffering → 1000 ppm), `Ki = Kp²/4` (critically
//!   damped: closed-loop double pole at 0.02 rad/s, a ~50 s time constant). The output is
//!   clamped to ±`max_ppm` (default 1000 ppm, i.e. ±1.7 cents of pitch — inaudible) with
//!   conditional integration against wind-up.
//! * If the queue ever exceeds twice the target (e.g. after a decoder stall), incoming audio
//!   is dropped rather than letting latency grow; if it runs dry, playback pauses with a
//!   short fade and restarts, with a fade-in, once the target depth is buffered again.

use std::time::{Duration, Instant};

use crate::channels::remap_channels_into;
use crate::stream::{deadline_after, nap, OutputOptions, OutputState, OutputStream};
use crate::{check_whole_frames, AudioFormat, Direction, Error, Resampler, ResamplerQuality, Result};

/// Proportional gain: fractional ratio change per second of excess buffering.
const KP: f64 = 0.04;
/// Integral gain (critically damped with `KP`).
const KI: f64 = KP * KP / 4.0;
/// Time constant of the low-pass filter on the measured queue depth, seconds.
const FILL_FILTER_TAU: f64 = 5.0;
/// Controller update period in seconds of device time.
const UPDATE_INTERVAL: f64 = 1.0;
/// How long [`AudioPlayer::push_blocking`] waits for a device that stopped consuming.
const STALL_TIMEOUT: Duration = Duration::from_secs(3);
/// Upper bound for [`PlayerOptions::max_ppm`]; well inside the resampler's ±1 % trim range.
const MAX_PPM_LIMIT: f64 = 5000.0;

/// Options for [`AudioPlayer::open`].
#[derive(Clone, Debug)]
pub struct PlayerOptions {
    /// Output device name (exact, or a unique part) or id; `None` = system default.
    pub device: Option<String>,
    /// Preferred device sample rate; `None` = the device's default (its mix rate).
    pub sample_rate: Option<u32>,
    /// Preferred device channel count; `None` = the device's default.
    pub channels: Option<usize>,
    /// Queue depth the drift loop steers towards. Must comfortably exceed the largest burst
    /// of audio the decoder delivers at once (400 ms for a DRM audio super-frame).
    pub target_latency: Duration,
    /// Enable the drift-compensation loop (live reception). Without it the ratio stays at
    /// nominal and the resampler is bypassed when the rates already match.
    pub drift_compensation: bool,
    /// Clamp for the ratio correction, in ppm (0 to 5000; default 1000).
    pub max_ppm: f64,
    /// Resampler quality. [`Balanced`](ResamplerQuality::Balanced) is transparent for
    /// broadcast audio.
    pub quality: ResamplerQuality,
}

impl Default for PlayerOptions {
    fn default() -> Self {
        PlayerOptions {
            device: None,
            sample_rate: None,
            channels: None,
            target_latency: Duration::from_millis(500),
            drift_compensation: true,
            max_ppm: 1000.0,
            quality: ResamplerQuality::Balanced,
        }
    }
}

/// Snapshot of the player's state, for display.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayerStatus {
    /// Format the sound card runs at.
    pub device_format: AudioFormat,
    /// Format of the most recently pushed audio, if any.
    pub source_format: Option<AudioFormat>,
    /// Audio currently queued for the sound card.
    pub buffered: Duration,
    /// Target queue depth.
    pub target: Duration,
    /// `buffered / target` (1.0 = on target).
    pub fill: f32,
    /// Ratio correction currently applied, in ppm (positive = producing more samples,
    /// i.e. the sound card runs fast relative to the source).
    pub ppm: f64,
    /// Playing or (re)buffering.
    pub state: OutputState,
    /// Times playback ran dry.
    pub underruns: u64,
    /// Frames discarded because the queue was already too deep.
    pub dropped_frames: u64,
    /// Under/overruns reported by the audio backend.
    pub xruns: u64,
}

/// Plays decoded PCM of any rate and channel count on a sound card, absorbing the clock
/// drift between source and card.
///
/// Create it on (or move it to) the thread that produces audio and call
/// [`push`](Self::push) with each decoded block. Resampling happens on that thread; the
/// sound card's callback only copies from a lock-free ring buffer (see [`crate::stream`]).
///
/// * **Live reception**: [`push`](Self::push) never blocks; the drift loop keeps the queue
///   near [`PlayerOptions::target_latency`].
/// * **File playback** (decoding faster than real time): [`push_blocking`](Self::push_blocking)
///   waits while the queue is above target, which paces the whole pipeline to the sound
///   card; finish with [`drain`](Self::drain).
///
/// ```no_run
/// use decdrm_io::{AudioPlayer, PlayerOptions};
/// # fn decoded_blocks() -> Vec<Vec<f32>> { vec![] }
/// # fn main() -> decdrm_io::Result<()> {
/// let mut player = AudioPlayer::open(PlayerOptions::default())?;
/// for pcm in decoded_blocks() {
///     player.push(&pcm, 48_000, 2)?; // e.g. HE-AAC output, stereo
///     let st = player.status();
///     println!("{:.0} ms buffered, {:+.1} ppm", st.buffered.as_secs_f64() * 1e3, st.ppm);
/// }
/// # Ok(()) }
/// ```
pub struct AudioPlayer {
    output: OutputStream,
    opts: PlayerOptions,
    target_frames: usize,
    high_water_frames: usize,
    resampler: Option<Resampler>,
    source: Option<AudioFormat>,
    drift: DriftController,
    /// Reused scratch buffers (no per-push allocation once warmed up).
    resampled: Vec<f32>,
    mapped: Vec<f32>,
    dropped_frames: u64,
}

impl AudioPlayer {
    /// Opens the output device and starts it (playing silence until audio is pushed).
    pub fn open(opts: PlayerOptions) -> Result<Self> {
        validate(&opts)?;
        let output = OutputStream::open(&OutputOptions {
            device: opts.device.clone(),
            sample_rate: opts.sample_rate,
            channels: opts.channels,
            buffer: opts.target_latency * 2 + Duration::from_secs(1),
            start_threshold: opts.target_latency,
        })?;
        Ok(Self::with_output(output, opts))
    }

    /// Builds a player on an existing output stream (used by `open`, and by tests with a
    /// hand-driven stream).
    pub(crate) fn with_output(output: OutputStream, opts: PlayerOptions) -> Self {
        let dev = output.format();
        let target_frames = (dev.duration_to_frames(opts.target_latency) as usize)
            .clamp(1, output.capacity_frames());
        output.set_start_threshold(target_frames);
        let high_water_frames = (2 * target_frames)
            .max(target_frames + dev.sample_rate as usize / 2)
            .min(output.capacity_frames());
        let drift = DriftController::new(dev.sample_rate, target_frames, opts.max_ppm);
        AudioPlayer {
            output,
            opts,
            target_frames,
            high_water_frames,
            resampler: None,
            source: None,
            drift,
            resampled: Vec::new(),
            mapped: Vec::new(),
            dropped_frames: 0,
        }
    }

    /// Queues decoded audio without blocking (live mode). `pcm` is interleaved with
    /// `channels` channels at `sample_rate` Hz; mono is duplicated to both sides of a stereo
    /// device. Returns the number of device frames queued; audio that would push the queue
    /// beyond twice the target is dropped and counted.
    ///
    /// A change of rate or channel count between calls is handled (the resampler is rebuilt;
    /// the drift estimate is kept).
    pub fn push(&mut self, pcm: &[f32], sample_rate: u32, channels: usize) -> Result<usize> {
        self.prepare(pcm, sample_rate, channels)?;
        if self.opts.drift_compensation {
            self.update_drift();
        }
        self.convert(pcm, channels);
        let ch = self.output.format().channels;
        let frames = self.mapped.len() / ch;
        let room = self.high_water_frames.saturating_sub(self.output.buffered_frames());
        let n = frames.min(room);
        let pushed = self.output.push_frames(&self.mapped[..n * ch]);
        self.dropped_frames += (frames - pushed) as u64;
        Ok(pushed)
    }

    /// Queues decoded audio, first waiting while more than the target latency is queued
    /// (file-playback mode: this paces the producer to the sound card). Drift compensation
    /// is not applied in this mode — the producer follows the card's clock.
    ///
    /// Fails if the output device is lost or stops consuming for several seconds.
    pub fn push_blocking(&mut self, pcm: &[f32], sample_rate: u32, channels: usize) -> Result<()> {
        self.prepare(pcm, sample_rate, channels)?;
        self.convert(pcm, channels);
        let ch = self.output.format().channels;
        let rate = self.output.format().sample_rate;
        let mut done = 0;
        let mut watchdog = Watchdog::new(self.output.stats().frames_played);
        loop {
            // Wait until the queue has drained to the target and there is room.
            loop {
                let queued = self.output.buffered_frames();
                let want = (self.mapped.len() - done) / ch;
                if queued <= self.target_frames && self.output.free_frames() > 0 {
                    break;
                }
                self.check_alive(&mut watchdog)?;
                let excess = queued.saturating_sub(self.target_frames).max(want.min(1024));
                std::thread::sleep(nap(excess, rate, deadline_after(Duration::from_millis(20))));
            }
            done += self.output.push_frames(&self.mapped[done..]) * ch;
            if done >= self.mapped.len() {
                return Ok(());
            }
        }
    }

    /// Flushes the resampler and plays out everything queued, waiting up to `timeout`.
    /// Returns `true` if all audio was played. Use at the end of a file.
    pub fn drain(&mut self, timeout: Duration) -> bool {
        if let Some(rs) = &mut self.resampler {
            let tail = rs.flush();
            let in_ch = rs.channels();
            self.mapped.clear();
            remap_channels_into(&tail, in_ch, self.output.format().channels, &mut self.mapped);
            let deadline = deadline_after(timeout);
            let ch = self.output.format().channels;
            let mut done = 0;
            while done < self.mapped.len() && Instant::now() < deadline {
                let pushed = self.output.push_frames(&self.mapped[done..]);
                done += pushed * ch;
                if pushed == 0 {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            return self.output.drain(deadline.saturating_duration_since(Instant::now()));
        }
        self.output.drain(timeout)
    }

    /// Current state for display.
    pub fn status(&self) -> PlayerStatus {
        let dev = self.output.format();
        let stats = self.output.stats();
        let buffered = dev.frames_to_duration(stats.buffered_frames as u64);
        PlayerStatus {
            device_format: dev,
            source_format: self.source,
            buffered,
            target: self.opts.target_latency,
            fill: stats.buffered_frames as f32 / self.target_frames.max(1) as f32,
            ppm: self.ppm(),
            state: stats.state,
            underruns: stats.underruns,
            dropped_frames: self.dropped_frames,
            xruns: stats.xruns,
        }
    }

    /// Audio currently queued.
    pub fn buffered(&self) -> Duration {
        self.output.buffered()
    }

    /// Ratio correction currently applied, in ppm.
    pub fn ppm(&self) -> f64 {
        self.resampler.as_ref().map_or(0.0, |rs| rs.ratio_adjust_ppm())
    }

    /// Set the playback volume (linear gain, see [`OutputStream::set_volume`]); it takes
    /// effect at once, not after the queued audio.
    pub fn set_volume(&self, gain: f32) {
        self.output.set_volume(gain);
    }

    /// The playback volume (linear gain).
    pub fn volume(&self) -> f32 {
        self.output.volume()
    }

    /// The sound card's format.
    pub fn device_format(&self) -> AudioFormat {
        self.output.format()
    }

    /// Name of the output device.
    pub fn device_name(&self) -> &str {
        self.output.device_name()
    }

    /// Backend errors reported since the last call (device lost, ...).
    pub fn take_errors(&mut self) -> Vec<Error> {
        self.output.take_errors()
    }

    /// Validates the block and (re)builds the resampler when the source format changes.
    fn prepare(&mut self, pcm: &[f32], sample_rate: u32, channels: usize) -> Result<()> {
        let src = AudioFormat::new(sample_rate, channels);
        src.validate()?;
        check_whole_frames(pcm, channels)?;
        if self.source == Some(src) {
            return Ok(());
        }
        let dev = self.output.format();
        let dev_rate = dev.sample_rate;
        let keep_ppm = self.ppm();
        // Play out what the old resampler still holds (its filter delay and partial chunk)
        // instead of dropping it at the format change.
        if let Some(mut old) = self.resampler.take() {
            let tail = old.flush();
            self.mapped.clear();
            remap_channels_into(&tail, old.channels(), dev.channels, &mut self.mapped);
            let pushed = self.output.push_frames(&self.mapped);
            self.dropped_frames += (self.mapped.len() / dev.channels - pushed) as u64;
        }
        self.resampler = if sample_rate != dev_rate || self.opts.drift_compensation {
            let mut rs = Resampler::new(sample_rate, dev_rate, channels, self.opts.quality)?;
            rs.set_ratio_adjust_ppm(keep_ppm)?;
            Some(rs)
        } else {
            None
        };
        self.source = Some(src);
        Ok(())
    }

    /// Resamples `pcm` and maps it to the device's channels into `self.mapped`.
    fn convert(&mut self, pcm: &[f32], channels: usize) {
        let dev_ch = self.output.format().channels;
        // Disjoint field borrows: `src` may borrow `self.resampled` while `self.mapped` is
        // borrowed mutably — allowed because they are different fields.
        let src: &[f32] = match &mut self.resampler {
            Some(rs) => {
                self.resampled.clear();
                rs.process_into(pcm, &mut self.resampled);
                &self.resampled
            }
            None => pcm,
        };
        self.mapped.clear();
        remap_channels_into(src, channels, dev_ch, &mut self.mapped);
    }

    fn update_drift(&mut self) {
        let sh = self.output.shared();
        use std::sync::atomic::Ordering::Relaxed;
        let update = self.drift.update(
            sh.frames_rendered.load(Relaxed),
            sh.fill_integral.load(Relaxed),
            sh.playing.load(Relaxed),
            sh.underruns.load(Relaxed),
        );
        if let (Some(ppm), Some(rs)) = (update, &mut self.resampler) {
            // `ppm` is clamped to ±max_ppm ≤ 5000 ppm, inside the resampler's ±1 % range, so
            // this cannot fail.
            let _ = rs.set_ratio_adjust_ppm(ppm);
        }
    }

    fn check_alive(&self, watchdog: &mut Watchdog) -> Result<()> {
        if self.output.is_dead() {
            return Err(Error::Audio(cpal::Error::with_message(
                cpal::ErrorKind::DeviceNotAvailable,
                format!("the {} device stopped", Direction::Output),
            )));
        }
        if watchdog.stalled(self.output.stats().frames_played) {
            return Err(Error::Audio(cpal::Error::with_message(
                cpal::ErrorKind::StreamInvalidated,
                format!(
                    "the {} device stopped consuming audio for {} s",
                    Direction::Output,
                    STALL_TIMEOUT.as_secs()
                ),
            )));
        }
        Ok(())
    }
}

impl std::fmt::Debug for AudioPlayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioPlayer").field("status", &self.status()).finish()
    }
}

fn validate(opts: &PlayerOptions) -> Result<()> {
    if opts.target_latency < Duration::from_millis(10) || opts.target_latency > Duration::from_secs(10) {
        return Err(Error::invalid("target_latency must be between 10 ms and 10 s"));
    }
    if !(0.0..=MAX_PPM_LIMIT).contains(&opts.max_ppm) {
        return Err(Error::invalid(format!("max_ppm must be between 0 and {MAX_PPM_LIMIT}")));
    }
    Ok(())
}

/// Detects a device that has stopped consuming (frames_played not advancing).
struct Watchdog {
    last_frames: u64,
    since: Instant,
}

impl Watchdog {
    fn new(frames: u64) -> Self {
        Watchdog { last_frames: frames, since: Instant::now() }
    }

    fn stalled(&mut self, frames: u64) -> bool {
        if frames != self.last_frames {
            self.last_frames = frames;
            self.since = Instant::now();
            false
        } else {
            self.since.elapsed() > STALL_TIMEOUT
        }
    }
}

/// PI controller from time-averaged queue depth to a resampling-ratio correction.
#[derive(Debug)]
pub(crate) struct DriftController {
    rate: f64,
    target: f64,
    max: f64,
    interval: u64,
    /// `(frames_rendered, fill_integral)` at the start of the current measurement window.
    base: Option<(u64, u64)>,
    last_underruns: u64,
    filtered: Option<f64>,
    /// ∫ e dt in s².
    integral: f64,
    ppm: f64,
}

impl DriftController {
    pub(crate) fn new(rate: u32, target_frames: usize, max_ppm: f64) -> Self {
        DriftController {
            rate: f64::from(rate.max(1)),
            target: target_frames as f64,
            max: max_ppm * 1e-6,
            interval: (f64::from(rate) * UPDATE_INTERVAL) as u64,
            base: None,
            last_underruns: 0,
            filtered: None,
            integral: 0.0,
            ppm: 0.0,
        }
    }

    /// Feeds the renderer's counters; returns a new correction in ppm once per update
    /// interval while playing.
    pub(crate) fn update(
        &mut self,
        frames_rendered: u64,
        fill_integral: u64,
        playing: bool,
        underruns: u64,
    ) -> Option<f64> {
        if !playing || underruns != self.last_underruns {
            // (Re)buffering: the queue depth is meaningless until playback resumes. Restart
            // the measurement but keep the integrator — it holds the learned clock offset.
            self.last_underruns = underruns;
            self.base = None;
            self.filtered = None;
            return None;
        }
        let Some((r0, i0)) = self.base else {
            self.base = Some((frames_rendered, fill_integral));
            return None;
        };
        let dn = frames_rendered.saturating_sub(r0);
        if dn < self.interval.max(1) {
            return None;
        }
        self.base = Some((frames_rendered, fill_integral));
        let mean_fill = fill_integral.saturating_sub(i0) as f64 / dn as f64;
        let dt = dn as f64 / self.rate;
        let filtered = match self.filtered {
            None => mean_fill,
            Some(f) => f + (dt / FILL_FILTER_TAU).min(1.0) * (mean_fill - f),
        };
        self.filtered = Some(filtered);

        // Positive error = too much queued = the source runs fast relative to the card:
        // produce fewer samples (negative correction).
        let e = (filtered - self.target) / self.rate;
        let p = KP * e;
        let candidate = self.integral + e * dt;
        let u_now = -(p + KI * self.integral);
        let u_new = -(p + KI * candidate);
        // Conditional integration: never integrate further into saturation.
        if u_new.abs() <= self.max || u_new.abs() < u_now.abs() {
            self.integral = candidate;
        }
        let limit = if KI > 0.0 { self.max / KI } else { 0.0 };
        self.integral = self.integral.clamp(-limit, limit);
        let u = (-(p + KI * self.integral)).clamp(-self.max, self.max);
        self.ppm = u * 1e6;
        Some(self.ppm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::OutputStream;

    /// Pure controller model: queue depth integrates the rate mismatch.
    #[test]
    fn controller_converges_on_a_constant_offset() {
        let rate = 48_000u32;
        let target = 24_000usize;
        let mut c = DriftController::new(rate, target, 1000.0);
        let delta = 250e-6; // card consumes 250 ppm faster than the source produces
        let mut fill = target as f64 + 4800.0; // start 100 ms too full
        let (mut rendered, mut integral, mut u) = (0u64, 0u64, 0.0f64);
        for _ in 0..4000 {
            // 0.1 s steps
            let n = 4800u64;
            integral += (fill.max(0.0) as u64) * n;
            rendered += n;
            fill += (u - delta) * n as f64;
            if let Some(ppm) = c.update(rendered, integral, true, 0) {
                u = ppm * 1e-6;
            }
        }
        assert!((u * 1e6 - 250.0).abs() < 5.0, "ppm = {}", u * 1e6);
        assert!((fill - target as f64).abs() < 48.0, "fill error {} frames", fill - target as f64);
    }

    #[test]
    fn controller_waits_while_buffering() {
        let mut c = DriftController::new(48_000, 1000, 1000.0);
        assert_eq!(c.update(0, 0, false, 0), None);
        assert_eq!(c.update(100_000, 0, false, 0), None);
        assert_eq!(c.update(100_000, 0, true, 0), None); // starts a window
        assert!(c.update(150_000, 50_000_000, true, 0).is_some());
        assert_eq!(c.update(200_000, 0, true, 1), None); // underrun resets the window
    }

    /// End-to-end: a hand-driven output stream consuming at a clock offset `delta`, fed with
    /// 400 ms bursts (like DRM audio super-frames) through the real resampler.
    fn simulate(delta_ppm: f64, seconds: f64) -> (PlayerStatus, f64, u64) {
        let dev = AudioFormat::new(48_000, 2);
        let opts = PlayerOptions { quality: ResamplerQuality::Fast, ..Default::default() };
        let cap = dev.duration_to_frames(opts.target_latency * 2 + Duration::from_secs(1)) as usize;
        let (out, mut renderer, _sink) = OutputStream::parts(dev, cap, 0);
        let mut player = AudioPlayer::with_output(out, opts);

        let src_rate = 48_000u32;
        let burst = 19_200usize; // 400 ms
        let tone: Vec<f32> = (0..burst)
            .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48_000.0).sin())
            .collect();
        let step = 0.01;
        let consume_rate = f64::from(dev.sample_rate) * (1.0 + delta_ppm * 1e-6);
        let (mut produced, mut rendered) = (0u64, 0u64);
        let mut buf = Vec::new();
        let steps = (seconds / step) as usize;
        let mut fill_sum = 0.0;
        let mut fill_n = 0u64;
        let mut underruns_late = 0;
        for k in 0..steps {
            let t = k as f64 * step;
            while produced as f64 / f64::from(src_rate) <= t {
                player.push(&tone, src_rate, 1).unwrap();
                produced += burst as u64;
            }
            let due = ((t + step) * consume_rate) as u64;
            let n = (due - rendered) as usize;
            buf.resize(n * 2, 0.0);
            renderer.render(&mut buf);
            rendered = due;
            if t > seconds - 100.0 {
                fill_sum += player.buffered().as_secs_f64();
                fill_n += 1;
            }
            if t > 10.0 {
                underruns_late = player.status().underruns;
            }
        }
        (player.status(), fill_sum / fill_n as f64, underruns_late)
    }

    #[test]
    fn player_absorbs_clock_drift() {
        for delta in [300.0, -500.0] {
            let (st, mean_fill, underruns) = simulate(delta, 700.0);
            assert!((st.ppm - delta).abs() < 30.0, "delta {delta}: ppm {}", st.ppm);
            // The sawtooth of 400 ms bursts averages to the target.
            assert!((mean_fill - 0.5).abs() < 0.03, "delta {delta}: mean fill {mean_fill}");
            assert_eq!(underruns, 0, "delta {delta}");
            assert_eq!(st.dropped_frames, 0, "delta {delta}");
        }
    }

    #[test]
    fn push_validates_input() {
        let dev = AudioFormat::new(48_000, 2);
        let (out, _r, _s) = OutputStream::parts(dev, 48_000, 0);
        let mut p = AudioPlayer::with_output(out, PlayerOptions::default());
        assert!(p.push(&[0.0; 3], 48_000, 2).is_err());
        assert!(p.push(&[0.0; 4], 0, 2).is_err());
        assert!(p.push(&[0.0; 4], 48_000, 0).is_err());
        assert!(p.push(&[0.0; 4], 24_000, 2).is_ok());
        assert_eq!(p.status().source_format, Some(AudioFormat::new(24_000, 2)));
    }
}
