//! Streaming sample-rate conversion (rubato's asynchronous sinc resampler).
//!
//! # Why the asynchronous resampler
//!
//! rubato offers a fixed-ratio FFT resampler and an asynchronous (adjustable-ratio) windowed
//! sinc resampler. We always use the latter because both of DecDRM's uses need to trim the
//! ratio while running: the [`AudioPlayer`](crate::AudioPlayer) tracks the clock drift between
//! the broadcast and the sound card, and a live receiver input may want to correct a sound
//! card's sample-rate offset. Its quality equals the FFT resampler's with the settings below.
//!
//! # Filter design
//!
//! The anti-alias/anti-image filter is a windowed sinc evaluated on a 256× oversampled grid
//! and interpolated between grid points. Its cutoff is set relative to the Nyquist frequency
//! of the *lower* of the two rates (f<sub>s,min</sub>/2), and placed so that the stop band
//! starts at that Nyquist frequency. Measured responses (`tests/resampler.rs`, run
//! `report_quality` with `--ignored --nocapture` to reproduce):
//!
//! | Preset | Taps / window | Flat (±0.02 dB) to | Alias/image level 2.5 % / 8 % past Nyquist | Speed, 44.1→48 kHz stereo |
//! |---|---|---|---|---|
//! | [`High`](ResamplerQuality::High) | 512, Blackman–Harris² | 0.47·f<sub>s,min</sub> | −105 dB / −134 dB | ~60× real time |
//! | [`Balanced`](ResamplerQuality::Balanced) | 256, Blackman–Harris² | 0.45·f<sub>s,min</sub> (up), 0.44 (down) | −64 dB / −141 dB | ~115× |
//! | [`Fast`](ResamplerQuality::Fast) | 64, Blackman | 0.42·f<sub>s,min</sub> | −36 dB / −77 dB | ~400× |
//!
//! For the receiver this means: with `High`, a 20 kHz-wide DRM signal centred in a 48 kHz
//! real IF band, or anything up to ±11.2 kHz in a 24 kHz I/Q recording, passes unchanged,
//! and nothing from beyond Nyquist folds back into it. The noise floor of the processing is
//! about −135 dB (f32 arithmetic).

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Adjustable, Async, FixedAsync, Resampler as _, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};

use crate::{AudioFormat, Error, Result, WORKING_RATE};

/// Fixed input chunk handed to rubato per call, in frames. Smaller chunks mean the ratio can
/// be retuned more often; larger ones amortise per-call overhead. 1024 frames is ~21 ms at
/// 48 kHz.
const CHUNK_FRAMES: usize = 1024;

/// Largest ratio trim supported, as a factor either way (1 %). Clock-drift and sample-rate
/// offset corrections are orders of magnitude smaller.
const MAX_RELATIVE_RATIO: f64 = 1.01;

/// Sinc table oversampling (intermediate points per input sample) for all presets.
const OVERSAMPLING: usize = 256;

/// Most zero frames prepended to align the output to the input (see [`alignment`]).
const MAX_ALIGN_PAD: usize = 512;

/// Speed/quality trade-off of the sinc filter (see the table in the module documentation).
///
/// Cost per output frame is roughly `4 × taps` multiply-adds for cubic interpolation (mono;
/// rubato shares the filter computation between channels) or `2 × taps` for linear.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ResamplerQuality {
    /// 64 taps, Blackman window, linear interpolation. Flat to 0.42·fs, −1.8 dB at
    /// 0.44·fs; fine for meters, spectrum displays and speech.
    Fast,
    /// 256 taps, Blackman–Harris², cubic interpolation. Flat to 0.45·fs; transparent for
    /// audio playback.
    Balanced,
    /// 512 taps, Blackman–Harris², cubic interpolation. Flat to 0.47·fs with alias/image
    /// products ≤ −105 dB. Use for anything feeding the DRM demodulator.
    #[default]
    High,
}

impl ResamplerQuality {
    /// Filter length in input samples (a multiple of 8, as rubato requires).
    fn taps(self) -> usize {
        match self {
            ResamplerQuality::Fast => 64,
            ResamplerQuality::Balanced => 256,
            ResamplerQuality::High => 512,
        }
    }

    fn params(self) -> SincInterpolationParameters {
        let (window, interpolation) = match self {
            ResamplerQuality::Fast => (WindowFunction::Blackman, SincInterpolationType::Linear),
            ResamplerQuality::Balanced | ResamplerQuality::High => {
                (WindowFunction::BlackmanHarris2, SincInterpolationType::Cubic)
            }
        };
        SincInterpolationParameters::new(self.taps(), window)
            .oversampling_factor(OVERSAMPLING)
            .interpolation(interpolation)
    }
}

/// Chooses how many zero frames to prepend to the input, and how many output frames to
/// discard, so that output frame `k` lands exactly at input time `k / ratio`.
///
/// rubato's windowed-sinc interpolator is linear-phase with a group delay of
/// `D = taps·r/2 − 1 − r/oversampling` output frames (`r` = output/input ratio; the last two
/// terms come from rubato advancing its read position before the first output and from its
/// sinc-table indexing — measured, and checked by the alignment tests). `D` is generally
/// fractional and whole frames can only be discarded, so we also prepend `P` zero input
/// frames, delaying the output by a further `P·r` frames, with `P` chosen to make
/// `D + P·r` as close to an integer as possible. For the usual audio rates the residual is
/// at most 1/256 of an input sample period (for integer ratios, where `P·r` is whole and
/// cannot absorb the `r/256` term) and often less (0.0026 output frames for 44.1→48 kHz with
/// `P = 72`), at the cost of at most `MAX_ALIGN_PAD` input frames of start-up latency.
///
/// Returns `(pad_input_frames, trim_output_frames)`.
fn alignment(taps: usize, ratio: f64) -> (usize, usize) {
    let delay = taps as f64 * ratio / 2.0 - 1.0 - ratio / OVERSAMPLING as f64;
    let mut best = (0usize, delay, f64::INFINITY);
    for pad in 0..=MAX_ALIGN_PAD {
        let total = delay + pad as f64 * ratio;
        let err = (total - total.round()).abs();
        if err < best.2 - 1e-9 {
            best = (pad, total, err);
        }
        if err < 1e-3 {
            break;
        }
    }
    (best.0, best.1.round().max(0.0) as usize)
}

/// Streaming sample-rate converter for interleaved `f32` audio.
///
/// Feed blocks of any size to [`process`](Self::process); each call returns whatever output
/// is ready. Input is buffered internally until a full 1024-frame chunk is available, so a
/// call may return nothing. At the end of a stream call [`flush`](Self::flush) to get the
/// tail. The output is time-aligned with the input: the filter's group delay is removed, so
/// output frame `k` corresponds to input time `k / ratio` (to within 1/256 of an input
/// sample), and a complete stream of `n` input frames yields `round(n × ratio)` output
/// frames. The price is a start-up latency of about half the filter length plus up to one
/// chunk (≈ 30 ms at 48 kHz for [`High`](ResamplerQuality::High)).
///
/// Chunked and one-shot processing give identical results (for a constant ratio): the result
/// does not depend on how the input is split into blocks.
///
/// ```
/// use decdrm_io::{Resampler, ResamplerQuality};
/// # fn main() -> decdrm_io::Result<()> {
/// let mut rs = Resampler::new(44_100, 48_000, 1, ResamplerQuality::High)?;
/// let mut out = rs.process(&vec![0.0; 44_100]);
/// out.extend(rs.flush());
/// assert_eq!(out.len(), 48_000);
/// # Ok(()) }
/// ```
pub struct Resampler {
    inner: Async<f32>,
    channels: usize,
    in_rate: u32,
    out_rate: u32,
    quality: ResamplerQuality,
    /// Current relative ratio (1.0 = nominal `out_rate / in_rate`).
    ratio_adjust: f64,
    /// Input samples not yet consumed by rubato (interleaved; may end in a partial frame).
    input: Vec<f32>,
    /// One chunk of rubato output.
    scratch: Vec<f32>,
    /// Output frames still to be discarded to compensate the filter delay.
    trim: usize,
    /// Real (non-padding) input samples accepted since the last reset.
    samples_in: u64,
    /// Ideal number of output frames for the real input seen so far.
    expected_out: f64,
    /// Output frames returned since the last reset.
    frames_out: u64,
}

impl Resampler {
    /// Creates a converter from `in_rate` to `out_rate` Hz for `channels` interleaved channels.
    pub fn new(
        in_rate: u32,
        out_rate: u32,
        channels: usize,
        quality: ResamplerQuality,
    ) -> Result<Self> {
        AudioFormat::new(in_rate, channels).validate()?;
        AudioFormat::new(out_rate, channels).validate()?;
        let ratio = f64::from(out_rate) / f64::from(in_rate);
        let inner = Async::<f32>::new_sinc(
            ratio,
            MAX_RELATIVE_RATIO,
            &quality.params(),
            CHUNK_FRAMES,
            channels,
            FixedAsync::Input,
        )
        .map_err(|e| Error::Resampler(e.to_string()))?;
        let scratch = vec![0.0; inner.output_frames_max() * channels];
        let mut rs = Resampler {
            inner,
            channels,
            in_rate,
            out_rate,
            quality,
            ratio_adjust: 1.0,
            input: Vec::with_capacity((2 * CHUNK_FRAMES + MAX_ALIGN_PAD) * channels),
            scratch,
            trim: 0,
            samples_in: 0,
            expected_out: 0.0,
            frames_out: 0,
        };
        rs.start();
        Ok(rs)
    }

    /// Prepares a fresh stream: alignment padding and trim, counters.
    fn start(&mut self) {
        let (pad, trim) = alignment(self.quality.taps(), self.ratio());
        self.input.clear();
        self.input.resize(pad * self.channels, 0.0);
        self.trim = trim;
        self.samples_in = 0;
        self.expected_out = 0.0;
        self.frames_out = 0;
    }

    /// Converts anything to the receiver's 48 kHz [`WORKING_RATE`] at
    /// [`High`](ResamplerQuality::High) quality.
    ///
    /// See also [`To48k`], which skips the work entirely for 48 kHz input.
    pub fn to_48k(in_rate: u32, channels: usize) -> Result<Self> {
        Resampler::new(in_rate, WORKING_RATE, channels, ResamplerQuality::High)
    }

    /// Resamples a block of interleaved input and returns the output that became available.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::new();
        self.process_into(input, &mut out);
        out
    }

    /// Like [`process`](Self::process) but appends to `out`, avoiding a fresh allocation per
    /// call once `out` has grown.
    pub fn process_into(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let before = self.samples_in / self.channels as u64;
        self.samples_in += input.len() as u64;
        let after = self.samples_in / self.channels as u64;
        self.expected_out += (after - before) as f64 * self.ratio();
        self.input.extend_from_slice(input);
        self.run(out);
    }

    /// Ends the stream: pads with silence to push out the samples still inside the filter,
    /// returns them, and resets the converter so it can start a new, unrelated stream (the
    /// ratio adjustment is kept).
    pub fn flush(&mut self) -> Vec<f32> {
        let ch = self.channels;
        let mut out = Vec::new();
        // Drop a trailing partial frame, then pad with zeros until the output catches up
        // with the real input. The loop bound only guards against a logic error.
        self.input.truncate(self.input.len() / ch * ch);
        let target = self.expected_out.round() as u64;
        let mut guard = 0;
        while self.frames_out < target && guard < 64 {
            let need = self.inner.input_frames_next();
            let have = self.input.len() / ch;
            if have < need {
                self.input.resize(need * ch, 0.0);
            }
            self.run(&mut out);
            guard += 1;
        }
        if self.frames_out > target {
            let excess = (self.frames_out - target) as usize;
            out.truncate(out.len().saturating_sub(excess * ch));
        }
        self.reset();
        out
    }

    /// Discards all buffered state (pending input, filter memory) without producing output.
    pub fn reset(&mut self) {
        self.inner.reset();
        // rubato's `reset` restores its original ratio; re-apply our trim.
        if self.ratio_adjust != 1.0 {
            let _ = self.inner.set_resample_ratio_relative(self.ratio_adjust, false);
        }
        self.start();
    }

    /// Trims the conversion ratio by `factor` relative to nominal: `1.0` is exactly
    /// `out_rate / in_rate`; `1.0001` produces 100 ppm *more* output samples per input sample
    /// (use it when the consumer's clock runs fast relative to the producer's).
    ///
    /// The change is ramped smoothly over the next chunk (no clicks). The factor must lie in
    /// `[1/1.01, 1.01]`.
    pub fn set_ratio_adjust(&mut self, factor: f64) -> Result<()> {
        if !factor.is_finite() || !(1.0 / MAX_RELATIVE_RATIO..=MAX_RELATIVE_RATIO).contains(&factor) {
            return Err(Error::invalid(format!(
                "ratio adjustment {factor} outside [{:.4}, {MAX_RELATIVE_RATIO}]",
                1.0 / MAX_RELATIVE_RATIO
            )));
        }
        self.inner
            .set_resample_ratio_relative(factor, true)
            .map_err(|e| Error::Resampler(e.to_string()))?;
        self.ratio_adjust = factor;
        Ok(())
    }

    /// [`set_ratio_adjust`](Self::set_ratio_adjust) in parts per million
    /// (`+100.0` = 100 ppm more output samples).
    pub fn set_ratio_adjust_ppm(&mut self, ppm: f64) -> Result<()> {
        self.set_ratio_adjust(1.0 + ppm * 1e-6)
    }

    /// Current ratio trim as a factor (see [`set_ratio_adjust`](Self::set_ratio_adjust)).
    pub fn ratio_adjust(&self) -> f64 {
        self.ratio_adjust
    }

    /// Current ratio trim in ppm.
    pub fn ratio_adjust_ppm(&self) -> f64 {
        (self.ratio_adjust - 1.0) * 1e6
    }

    /// Effective conversion ratio (output frames per input frame), including the trim.
    pub fn ratio(&self) -> f64 {
        f64::from(self.out_rate) / f64::from(self.in_rate) * self.ratio_adjust
    }

    /// Input sample rate in Hz.
    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    /// Nominal output sample rate in Hz.
    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }

    /// Number of interleaved channels.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Quality preset this converter was built with.
    pub fn quality(&self) -> ResamplerQuality {
        self.quality
    }

    /// Input frames buffered but not yet converted (less than one 1024-frame chunk, plus the
    /// alignment padding right after construction or a reset).
    pub fn pending_input_frames(&self) -> usize {
        self.input.len() / self.channels
    }

    /// Feeds every complete chunk in `self.input` through rubato.
    fn run(&mut self, out: &mut Vec<f32>) {
        let ch = self.channels;
        let mut offset = 0; // in samples
        loop {
            let need = self.inner.input_frames_next();
            let avail = (self.input.len() - offset) / ch;
            if avail < need {
                break;
            }
            let next_out = self.inner.output_frames_next().max(self.inner.output_frames_max());
            if self.scratch.len() < next_out * ch {
                self.scratch.resize(next_out * ch, 0.0);
            }
            let out_frames = self.scratch.len() / ch;
            let result = match (
                InterleavedSlice::new(&self.input[offset..], ch, avail),
                InterleavedSlice::new_mut(&mut self.scratch[..], ch, out_frames),
            ) {
                (Ok(inp), Ok(mut outp)) => self.inner.process_into_buffer(&inp, &mut outp, None),
                // The slices are sized from `avail`/`out_frames` above, so construction
                // cannot fail; treat it like a processing error if it ever does.
                _ => Err(rubato::ResampleError::InsufficientInputBufferSize {
                    expected: need,
                    actual: avail,
                }),
            };
            match result {
                Ok((nin, nout)) => {
                    offset += nin * ch;
                    let skip = self.trim.min(nout);
                    self.trim -= skip;
                    out.extend_from_slice(&self.scratch[skip * ch..nout * ch]);
                    self.frames_out += (nout - skip) as u64;
                }
                Err(e) => {
                    // Unreachable with the buffer sizing above. Never panic in release
                    // builds: drop the offending input and carry on.
                    debug_assert!(false, "rubato process failed: {e}");
                    offset = self.input.len() / ch * ch;
                    break;
                }
            }
        }
        self.input.drain(..offset);
    }
}

impl std::fmt::Debug for Resampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resampler")
            .field("in_rate", &self.in_rate)
            .field("out_rate", &self.out_rate)
            .field("channels", &self.channels)
            .field("quality", &self.quality)
            .field("ratio_adjust", &self.ratio_adjust)
            .finish_non_exhaustive()
    }
}

/// Brings any input rate to the 48 kHz [`WORKING_RATE`] the receiver runs at.
///
/// A thin wrapper around an optional [`Resampler`] ([`High`](ResamplerQuality::High) quality)
/// that is a zero-cost pass-through when the input already runs at 48 kHz.
///
/// ```no_run
/// use decdrm_io::{FileReader, To48k};
/// # fn main() -> decdrm_io::Result<()> {
/// let mut reader = FileReader::open("samples/FMGold_xHE_ModeB_9khz.flac")?; // 44.1 kHz
/// let fmt = reader.format();
/// let mut conv = To48k::new(fmt.sample_rate, fmt.channels)?;
/// while let Some(block) = reader.read(4410)? {
///     let at_48k = conv.process(&block);
///     // feed `at_48k` to the receiver ...
/// #   let _ = at_48k;
/// }
/// let tail = conv.flush();
/// # let _ = tail;
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct To48k {
    inner: Option<Resampler>,
    channels: usize,
}

impl To48k {
    /// Creates a converter for `channels`-channel input at `in_rate` Hz.
    pub fn new(in_rate: u32, channels: usize) -> Result<Self> {
        AudioFormat::new(in_rate, channels).validate()?;
        let inner = if in_rate == WORKING_RATE {
            None
        } else {
            Some(Resampler::to_48k(in_rate, channels)?)
        };
        Ok(To48k { inner, channels })
    }

    /// Converts a block (see [`Resampler::process`]); a copy of the input when passing through.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        match &mut self.inner {
            Some(rs) => rs.process(input),
            None => input.to_vec(),
        }
    }

    /// Returns the tail at the end of the stream (see [`Resampler::flush`]).
    pub fn flush(&mut self) -> Vec<f32> {
        match &mut self.inner {
            Some(rs) => rs.flush(),
            None => Vec::new(),
        }
    }

    /// `true` if the input is already at 48 kHz and no resampling happens.
    pub fn is_passthrough(&self) -> bool {
        self.inner.is_none()
    }

    /// Number of interleaved channels.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The underlying resampler, e.g. to apply a sample-rate-offset correction with
    /// [`Resampler::set_ratio_adjust`]. `None` in pass-through mode.
    pub fn resampler_mut(&mut self) -> Option<&mut Resampler> {
        self.inner.as_mut()
    }
}

/// One-shot conversion of a complete in-memory signal (process + flush).
pub fn resample(
    input: &[f32],
    channels: usize,
    in_rate: u32,
    out_rate: u32,
    quality: ResamplerQuality,
) -> Result<Vec<f32>> {
    crate::check_whole_frames(input, channels)?;
    let mut rs = Resampler::new(in_rate, out_rate, channels, quality)?;
    let mut out = rs.process(input);
    out.extend(rs.flush());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Residual misalignment, in input samples, left by `alignment`.
    fn residual_in_samples(taps: usize, ratio: f64) -> f64 {
        let (pad, trim) = alignment(taps, ratio);
        let delay = taps as f64 * ratio / 2.0 - 1.0 - ratio / OVERSAMPLING as f64;
        (delay + pad as f64 * ratio - trim as f64) / ratio
    }

    #[test]
    fn alignment_residual_is_tiny_for_common_rates() {
        let rates = [8000u32, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 96_000];
        for q in [ResamplerQuality::Fast, ResamplerQuality::Balanced, ResamplerQuality::High] {
            for &from in &rates {
                for to in [44_100u32, 48_000] {
                    let r = f64::from(to) / f64::from(from);
                    let res = residual_in_samples(q.taps(), r);
                    assert!(
                        res.abs() <= 1.0 / OVERSAMPLING as f64 + 1e-9,
                        "{q:?} {from}->{to}: residual {res} input samples"
                    );
                    let (pad, _) = alignment(q.taps(), r);
                    assert!(pad <= MAX_ALIGN_PAD);
                }
            }
        }
        // 44.1 -> 48 kHz (r = 160/147): the best pad (72 frames) leaves 0.00255 of an
        // output frame.
        let r = 48_000.0 / 44_100.0;
        assert_eq!(alignment(512, r).0, 72);
        assert!((residual_in_samples(512, r) * r).abs() < 0.003);
    }
}
