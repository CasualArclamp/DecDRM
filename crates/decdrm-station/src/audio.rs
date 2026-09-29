//! Audio services: the input (file, sound card or test tone, converted to the encoder's
//! rate and channel count), the encoder (FDK-AAC, libxaac for xHE-AAC, Opus, or EnCodec
//! with the `encodec` feature) and the logical frame of each 400 ms multiplex frame
//! (audio super frame plus text message piece).
//!
//! Timing: one call of [`AudioChain::next_logical_frame`] consumes exactly 400 ms of
//! PCM at the encoder's input rate — five or ten AAC granules of 960 core samples
//! (1920 input samples with SBR), twenty 20 ms Opus frames, 400 ms of xHE-AAC input
//! (1024-, 2048- or 4096-sample frames that do not align with the super frames), or
//! thirty 320-sample EnCodec frames at 24 kHz — and produces one audio super frame
//! (ES 201 980 §5.3.1, §5.4.1).
//!
//! FDK-AAC has an encoder delay: its first calls return no frame. The encoder is primed
//! with silence at start-up until it delivers its first frame, after which every
//! granule yields one frame; a queue absorbs the one-frame offset.
//!
//! xHE-AAC frames run continuously through the super frames, and the super frame
//! builder ([`XheAacFramer`]) must not pad. The encoder is therefore kept two frames
//! ahead of the channel: primed with two frames of silence, it holds the audio up to the
//! end of each super frame plus two frames when that super frame is built, which with its
//! rate control (output at or above the channel rate) always fills the payload.

use crate::config::{AudioInputSettings, Codec, StationConfig};
use crate::error::{Result, StationError};
use crate::plan::AudioPlan;
use decdrm_codecs::{
    AacProfile, CodecError, DrmAacFrame, FdkDrmEncoder, FdkEncoderConfig, OpusDrmEncoder, OpusEncoderConfig,
    XheAacConfig, XheAacEncoder,
};
use decdrm_core::mux::audio::{
    AacSuperFrameFormat, AudioError, AudioFrame, XheAacFramer, build_aac_super_frame, insert_text_message,
};
use decdrm_core::mux::sdc::StreamLengths;
use decdrm_core::mux::text::TextMessageEncoder;
use decdrm_io::{FileReader, InputOptions, InputStream, Resampler, ResamplerQuality};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Frames read from a file per call.
const READ_BLOCK: usize = 4096;

// ---------------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------------

/// Where an audio service's PCM comes from.
///
/// Rust note: a trait object (`Box<dyn AudioSource>`) lets the chain hold any of the
/// source types below; `: Send` lets the whole station move to a worker thread.
pub(crate) trait AudioSource: Send {
    /// Append exactly `frames` frames of interleaved PCM (the encoder's channel count
    /// and rate) to `out`.
    fn read(&mut self, frames: usize, out: &mut Vec<f32>) -> std::result::Result<(), String>;
    /// A non-looping file has ended; silence follows.
    fn finished(&self) -> bool {
        false
    }
    /// The source ends by itself (a non-looping file).
    fn finite(&self) -> bool {
        false
    }
    /// For status displays.
    fn describe(&self) -> String;
    /// Ratio trim applied to follow the input's clock (sound-card input with a
    /// sound-card output), ppm.
    fn drift_ppm(&self) -> Option<f64> {
        None
    }
}

/// Interleaved samples waiting to be consumed.
#[derive(Default)]
struct Fifo {
    buf: Vec<f32>,
    start: usize,
}

impl Fifo {
    fn len(&self) -> usize {
        self.buf.len() - self.start
    }

    /// Move `n` samples (fewer if not available, then zero padded) to `out`.
    fn take(&mut self, n: usize, out: &mut Vec<f32>) {
        let k = n.min(self.len());
        out.extend_from_slice(&self.buf[self.start..self.start + k]);
        out.resize(out.len() + (n - k), 0.0);
        self.start += k;
        // Compact once the consumed head dominates (keeps memory bounded).
        if self.start > self.buf.len() / 2 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
    }
}

/// Mix or duplicate `input` (`in_ch` channels) to `out_ch` channels, with gain.
fn map_channels(input: &[f32], in_ch: usize, out_ch: usize, gain: f32, out: &mut Vec<f32>) {
    for frame in input.chunks_exact(in_ch) {
        if out_ch == 1 {
            out.push(gain * frame.iter().sum::<f32>() / in_ch as f32);
        } else {
            for c in 0..out_ch {
                out.push(gain * frame[c.min(in_ch - 1)]);
            }
        }
    }
}

fn db_to_gain(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

/// A WAV/FLAC file, resampled, optionally looping.
struct FileSource {
    path: PathBuf,
    reader: FileReader,
    looped: bool,
    in_ch: usize,
    out_ch: usize,
    gain: f32,
    resampler: Option<Resampler>,
    fifo: Fifo,
    ended: bool,
    scratch: Vec<f32>,
}

impl FileSource {
    fn open(path: &Path, looped: bool, out_rate: u32, out_ch: usize, gain: f32) -> std::result::Result<Self, String> {
        let reader = FileReader::open(path).map_err(|e| e.to_string())?;
        let fmt = reader.format();
        let resampler = (fmt.sample_rate != out_rate)
            .then(|| Resampler::new(fmt.sample_rate, out_rate, out_ch, ResamplerQuality::Balanced))
            .transpose()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            path: path.to_path_buf(),
            reader,
            looped,
            in_ch: fmt.channels,
            out_ch,
            gain,
            resampler,
            fifo: Fifo::default(),
            ended: false,
            scratch: Vec::new(),
        })
    }
}

impl AudioSource for FileSource {
    fn read(&mut self, frames: usize, out: &mut Vec<f32>) -> std::result::Result<(), String> {
        let need = frames * self.out_ch;
        // A looping file that yields nothing twice in a row is empty: stop looping.
        let mut empty_reopens = 0;
        while self.fifo.len() < need && !self.ended {
            let block = match self.reader.read(READ_BLOCK).map_err(|e| e.to_string())? {
                Some(b) => b,
                None if self.looped && empty_reopens == 0 => {
                    self.reader = FileReader::open(&self.path).map_err(|e| e.to_string())?;
                    empty_reopens += 1;
                    continue;
                }
                None => {
                    if let Some(r) = self.resampler.as_mut() {
                        self.fifo.buf.extend(r.flush());
                    }
                    self.ended = true;
                    continue;
                }
            };
            empty_reopens = 0;
            self.scratch.clear();
            map_channels(&block, self.in_ch, self.out_ch, self.gain, &mut self.scratch);
            match self.resampler.as_mut() {
                Some(r) => r.process_into(&self.scratch, &mut self.fifo.buf),
                None => self.fifo.buf.extend_from_slice(&self.scratch),
            }
        }
        self.fifo.take(need, out);
        Ok(())
    }

    fn finished(&self) -> bool {
        self.ended && self.fifo.len() == 0
    }

    fn finite(&self) -> bool {
        !self.looped
    }

    fn describe(&self) -> String {
        format!("{}{}", self.path.display(), if self.looped { " (looping)" } else { "" })
    }
}

/// A sound-card input.
///
/// With a sound-card output as well, two clocks run the station: the output card
/// paces the frames, the input card delivers the audio, and a typical 50–200 ppm
/// difference would slowly fill the capture buffer (dropped input) or drain it
/// (the output starves). [`InputDrift`] then trims the input resampling ratio to hold
/// the capture backlog at a target, like the receiver's playback drift loop.
///
/// The backlog only measures the drift once the output paces the station, that is once
/// the output's queue is full. At start-up the station fills that queue as fast as it
/// can, consuming input faster than it arrives; the first read therefore waits for the
/// target backlog plus the queue plus the read itself, which leaves the target backlog
/// once the queue is full, and the loop starts after the reads that fill it.
struct DeviceSource {
    name: String,
    stream: InputStream,
    in_ch: usize,
    in_rate: u32,
    out_ch: usize,
    out_rate: u32,
    gain: f32,
    resampler: Option<Resampler>,
    fifo: Fifo,
    scratch: Vec<f32>,
    /// Drift compensation (sound-card output).
    drift: Option<InputDrift>,
    /// Capacity of the sound-card output's queue, s (with drift compensation).
    output_queue_s: f64,
    /// The first read has waited for the start-up backlog.
    primed: bool,
    /// Reads left while the station fills the output's queue (the backlog does not
    /// measure the drift yet).
    filling_reads: u32,
}

/// PI loop holding the capture backlog of a sound-card input at a target by trimming
/// the resampling ratio: a growing backlog (input clock fast) makes the resampler
/// consume more input per output frame.
#[derive(Debug, Clone)]
struct InputDrift {
    target_s: f64,
    /// Low-pass filtered backlog, s.
    filtered: Option<f64>,
    integral: f64,
    ppm: f64,
}

impl InputDrift {
    /// Proportional gain: 25 ms of excess backlog → 1000 ppm (as the receiver's player).
    const KP: f64 = 0.04;
    /// Integral gain (critically damped).
    const KI: f64 = Self::KP * Self::KP / 4.0;
    /// Largest trim, ppm (±1.7 cents; sound cards differ by up to ~200 ppm).
    const MAX_PPM: f64 = 1000.0;
    /// Time constant of the backlog filter, s (as the receiver's player). The capture
    /// buffer fills in callback-sized steps (10–20 ms), which the proportional term
    /// would pass on as several hundred ppm of jitter.
    const FILTER_TAU_S: f64 = 5.0;

    fn new(target_s: f64) -> Self {
        Self { target_s, filtered: None, integral: 0.0, ppm: 0.0 }
    }

    /// New ratio trim (ppm) for a backlog of `backlog_s` seconds, `dt` seconds after
    /// the previous update.
    fn update(&mut self, backlog_s: f64, dt: f64) -> f64 {
        let filtered = match self.filtered {
            None => backlog_s,
            Some(f) => f + (dt / Self::FILTER_TAU_S).min(1.0) * (backlog_s - f),
        };
        self.filtered = Some(filtered);
        let e = filtered - self.target_s;
        let limit = Self::MAX_PPM * 1e-6 / Self::KI;
        self.integral = (self.integral + e * dt).clamp(-limit, limit);
        let u = -(Self::KP * e + Self::KI * self.integral);
        self.ppm = (u * 1e6).clamp(-Self::MAX_PPM, Self::MAX_PPM);
        self.ppm
    }
}

/// Capture backlog the drift loop holds, s.
const INPUT_BACKLOG_S: f64 = 0.3;

impl DeviceSource {
    /// Open `device` for PCM at `out_rate` with `out_ch` channels. `output_queue` is the
    /// capacity of the station's sound-card output, if it has one: the two clocks then
    /// need drift compensation.
    fn open(
        device: &str,
        out_rate: u32,
        out_ch: usize,
        gain: f32,
        output_queue: Option<Duration>,
    ) -> std::result::Result<Self, String> {
        let queue = output_queue.unwrap_or_default();
        let opts = InputOptions {
            device: (!device.eq_ignore_ascii_case("default")).then(|| device.to_string()),
            sample_rate: Some(out_rate),
            channels: Some(out_ch),
            // Room for the start-up backlog (the output queue plus at most 0.7 s) with
            // 2 s to spare.
            buffer: Duration::from_secs(2) + queue,
        };
        let stream = InputStream::open(&opts).map_err(|e| e.to_string())?;
        let fmt = stream.format();
        let compensate = output_queue.is_some();
        // Drift compensation needs a resampler even at equal nominal rates.
        let resampler = (compensate || fmt.sample_rate != out_rate)
            .then(|| Resampler::new(fmt.sample_rate, out_rate, out_ch, ResamplerQuality::Balanced))
            .transpose()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            name: stream.device_name().to_string(),
            in_ch: fmt.channels,
            in_rate: fmt.sample_rate,
            stream,
            out_ch,
            out_rate,
            gain,
            resampler,
            fifo: Fifo::default(),
            scratch: Vec::new(),
            drift: compensate.then(|| InputDrift::new(INPUT_BACKLOG_S)),
            output_queue_s: queue.as_secs_f64(),
            primed: false,
            filling_reads: 0,
        })
    }

    /// Wait until the capture buffer holds the start-up backlog for a first read of
    /// `frames` output frames (see the type's documentation). If the station started
    /// late and more has piled up, the oldest audio is dropped instead.
    fn prime(&mut self, frames: usize) {
        let read_s = frames as f64 / f64::from(self.out_rate);
        let seconds = INPUT_BACKLOG_S + self.output_queue_s + read_s;
        let target = (seconds * f64::from(self.in_rate)) as usize;
        let deadline = std::time::Instant::now() + Duration::from_secs_f64(seconds + 2.0);
        while self.stream.available_frames() < target && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let excess = self.stream.available_frames().saturating_sub(target);
        if excess > 0 {
            self.stream.read(excess);
        }
        self.primed = true;
        // Each read is followed by the frame's write, which blocks once the queue is full.
        self.filling_reads = (self.output_queue_s / read_s - 1e-9).ceil().max(0.0) as u32;
    }

    /// Seconds of input captured but not yet consumed (in the capture buffer and,
    /// converted, in the FIFO).
    fn backlog_s(&self) -> f64 {
        self.stream.available_frames() as f64 / f64::from(self.in_rate)
            + (self.fifo.len() / self.out_ch.max(1)) as f64 / f64::from(self.out_rate)
    }
}

impl AudioSource for DeviceSource {
    fn read(&mut self, frames: usize, out: &mut Vec<f32>) -> std::result::Result<(), String> {
        if self.drift.is_some() && !self.primed {
            self.prime(frames);
        }
        let need = frames * self.out_ch;
        while self.fifo.len() < need {
            let block = self.stream.read_blocking(1024, Duration::from_secs(2)).map_err(|e| e.to_string())?;
            if block.is_empty() {
                return Err(format!("sound card \"{}\" delivered no audio for 2 s", self.name));
            }
            self.scratch.clear();
            map_channels(&block, self.in_ch, self.out_ch, self.gain, &mut self.scratch);
            match self.resampler.as_mut() {
                Some(r) => r.process_into(&self.scratch, &mut self.fifo.buf),
                None => self.fifo.buf.extend_from_slice(&self.scratch),
            }
        }
        self.fifo.take(need, out);
        let backlog = self.backlog_s();
        if self.filling_reads > 0 {
            self.filling_reads -= 1;
        } else if let (Some(drift), Some(r)) = (self.drift.as_mut(), self.resampler.as_mut()) {
            let ppm = drift.update(backlog, frames as f64 / f64::from(self.out_rate));
            r.set_ratio_adjust_ppm(ppm).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("sound card \"{}\"", self.name)
    }

    fn drift_ppm(&self) -> Option<f64> {
        self.drift.as_ref().map(|d| d.ppm)
    }
}

/// A sine test tone.
struct ToneSource {
    freq: f64,
    phase: f64,
    step: f64,
    amp: f32,
    ch: usize,
}

impl AudioSource for ToneSource {
    fn read(&mut self, frames: usize, out: &mut Vec<f32>) -> std::result::Result<(), String> {
        out.reserve(frames * self.ch);
        for _ in 0..frames {
            let v = self.amp * self.phase.sin() as f32;
            self.phase = (self.phase + self.step) % std::f64::consts::TAU;
            for _ in 0..self.ch {
                out.push(v);
            }
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("{} Hz test tone", self.freq)
    }
}

/// Open the source described by `input` for an encoder taking `rate` Hz, `ch` channels.
fn open_source(cfg: &StationConfig, input: &AudioInputSettings, rate: u32, ch: usize) -> std::result::Result<Box<dyn AudioSource>, String> {
    let gain = db_to_gain(input.gain_db);
    if let Some(f) = &input.file {
        return Ok(Box::new(FileSource::open(&cfg.resolve(f), input.looped, rate, ch, gain)?));
    }
    if let Some(d) = &input.device {
        // Two sound cards (input and output) need drift compensation; with file output
        // the input's clock paces the station.
        let output_queue = crate::output::device_queue(&cfg.output);
        return Ok(Box::new(DeviceSource::open(d, rate, ch, gain, output_queue)?));
    }
    let freq = input.tone_hz.ok_or("the audio input needs `file`, `device` or `tone_hz`")?;
    Ok(Box::new(ToneSource {
        freq,
        phase: 0.0,
        step: std::f64::consts::TAU * freq / f64::from(rate),
        amp: db_to_gain(input.level_dbfs),
        ch,
    }))
}

// ---------------------------------------------------------------------------------
// Encoders
// ---------------------------------------------------------------------------------

/// FDK-AAC encoder producing AAC audio super frames.
struct AacEncoder {
    enc: FdkDrmEncoder,
    fmt: AacSuperFrameFormat,
    frames: usize,
    /// Samples per encoder call (frame length × channels).
    granule: usize,
    /// Encoded frames not yet sent; `None` = the encoder failed on that granule.
    queue: VecDeque<Option<DrmAacFrame>>,
    silence: Vec<f32>,
    /// Frames replaced by empty ones (encoder errors and super-frame overflows).
    dropped: u64,
    encoder_errors: u64,
}

impl AacEncoder {
    fn new(plan: &AudioPlan, stream: StreamLengths) -> std::result::Result<Self, decdrm_codecs::CodecError> {
        let profile = match plan.codec {
            Codec::HeAac => AacProfile::HeAac,
            Codec::HeAacV2 => AacProfile::HeAacV2,
            _ => AacProfile::Lc,
        };
        let cfg = FdkEncoderConfig {
            stereo: plan.stereo && plan.codec != Codec::HeAacV2,
            ..FdkEncoderConfig::new(profile, plan.core_rate, plan.encoder_bitrate)
        };
        let granule = cfg.frame_len() * cfg.input_channels();
        let fdk = FdkDrmEncoder::new(cfg)?;
        // FDK silently raises rates below its minimum for the configuration; the
        // frames would then overflow the stream. (Validation normally prevents this
        // with its measured minimum payloads.)
        let effective = fdk.effective_bitrate();
        if f64::from(effective) > 1.02 * f64::from(plan.encoder_bitrate) {
            return Err(decdrm_codecs::CodecError::InvalidConfig(format!(
                "FDK encodes this configuration at no less than {:.1} kbit/s, the stream allows {:.1} kbit/s",
                f64::from(effective) / 1000.0,
                f64::from(plan.encoder_bitrate) / 1000.0
            )));
        }
        let mut enc = Self {
            enc: fdk,
            fmt: AacSuperFrameFormat::aac(plan.frames_per_super_frame, stream),
            frames: plan.frames_per_super_frame,
            granule,
            queue: VecDeque::new(),
            silence: vec![0.0; granule],
            dropped: 0,
            encoder_errors: 0,
        };
        // Prime the encoder's delay line (see the module docs).
        for _ in 0..16 {
            enc.encode_silence();
            if !enc.queue.is_empty() {
                break;
            }
        }
        Ok(enc)
    }

    fn push(&mut self, r: std::result::Result<Option<DrmAacFrame>, decdrm_codecs::CodecError>) {
        match r {
            Ok(Some(f)) => self.queue.push_back(Some(f)),
            Ok(None) => {}
            // A failed DRM re-packing (e.g. too many HCR segments): the frame is lost,
            // the encoder itself carries on.
            Err(_) => {
                self.encoder_errors += 1;
                self.queue.push_back(None);
            }
        }
    }

    fn encode_silence(&mut self) {
        let r = self.enc.encode(&self.silence);
        self.push(r);
    }

    fn super_frame(&mut self, pcm: &[f32], len: usize) -> std::result::Result<Vec<u8>, AudioError> {
        for g in pcm.chunks_exact(self.granule) {
            let r = self.enc.encode(g);
            self.push(r);
        }
        // Should not happen after priming; keeps the frame count exact regardless.
        for _ in 0..2 * self.frames {
            if self.queue.len() >= self.frames {
                break;
            }
            self.encode_silence();
        }
        while self.queue.len() < self.frames {
            self.queue.push_back(None);
        }
        let frames: Vec<Option<DrmAacFrame>> = self.queue.drain(..self.frames).collect();
        let (sf, dropped) = pack_aac(&frames, &self.fmt, len)?;
        self.dropped += dropped + frames.iter().filter(|f| f.is_none()).count() as u64;
        Ok(sf)
    }
}

/// Build an AAC audio super frame of `len` bytes. The frames' minimum lengths must
/// fit into the payload: if they do not, the largest frames are replaced by empty
/// ones (the receiver conceals them). The last frame's length is implicit and cannot be
/// zero, so a lost last frame is sent as one zero byte (its CRC fails). Spare bytes are
/// spread over the frames as padding between core and SBR data
/// ([`DrmAacFrame::to_bytes_padded`]). Returns the super frame and the number of
/// frames dropped for lack of space.
fn pack_aac(frames: &[Option<DrmAacFrame>], fmt: &AacSuperFrameFormat, len: usize) -> std::result::Result<(Vec<u8>, u64), AudioError> {
    let n = frames.len();
    let last = n - 1;
    let payload = fmt.payload_len(len).ok_or(AudioError::TooShort(len))?;
    let mut keep: Vec<bool> = frames.iter().map(Option::is_some).collect();
    let mut sizes: Vec<usize> = frames.iter().map(|f| f.as_ref().map_or(0, DrmAacFrame::min_len)).collect();
    let mut dropped = 0;
    loop {
        let total = sizes.iter().sum::<usize>() + usize::from(!keep[last]);
        if total <= payload {
            break;
        }
        let victim = (0..last).filter(|&i| keep[i]).max_by_key(|&i| sizes[i]).or(keep[last].then_some(last));
        match victim {
            Some(i) => {
                keep[i] = false;
                sizes[i] = 0;
                dropped += 1;
            }
            None => break,
        }
    }
    let slack = payload.saturating_sub(sizes.iter().sum::<usize>() + usize::from(!keep[last]));
    let kept: Vec<usize> = (0..n).filter(|&i| keep[i]).collect();
    if kept.is_empty() {
        sizes[last] = payload;
    } else {
        for (k, &i) in kept.iter().enumerate() {
            sizes[i] += slack / kept.len() + usize::from(k < slack % kept.len());
        }
        if !keep[last] {
            sizes[last] = 1;
        }
    }
    let audio_frames: Vec<AudioFrame> = (0..n)
        .map(|i| match (&frames[i], keep[i]) {
            (Some(f), true) => AudioFrame::with_crc(f.to_bytes_padded(sizes[i]).expect("size ≥ min_len"), f.crc()),
            _ => AudioFrame::with_crc(vec![0; sizes[i]], 0),
        })
        .collect();
    Ok((build_aac_super_frame(&audio_frames, fmt, len)?, dropped))
}

/// Opus encoder producing Dream-style Opus super frames.
struct OpusEncoder {
    enc: OpusDrmEncoder,
    fmt: AacSuperFrameFormat,
    granule: usize,
    encoder_errors: u64,
}

impl OpusEncoder {
    fn new(plan: &AudioPlan, stream: StreamLengths) -> std::result::Result<Self, decdrm_codecs::CodecError> {
        let enc = OpusDrmEncoder::new(OpusEncoderConfig::new(plan.input_channels, plan.opus_packet_bytes))?;
        Ok(Self {
            enc,
            fmt: AacSuperFrameFormat::opus(stream),
            granule: decdrm_codecs::OPUS_FRAME_LEN * plan.input_channels,
            encoder_errors: 0,
        })
    }

    fn super_frame(&mut self, pcm: &[f32], len: usize) -> std::result::Result<Vec<u8>, AudioError> {
        let frames: Vec<AudioFrame> = pcm
            .chunks_exact(self.granule)
            .map(|g| match self.enc.encode(g) {
                Ok(f) => AudioFrame::with_crc(f.data, f.crc),
                Err(_) => {
                    // An empty packet is concealed by the receiver.
                    self.encoder_errors += 1;
                    AudioFrame::with_crc(Vec::new(), 0)
                }
            })
            .collect();
        build_aac_super_frame(&frames, &self.fmt, len)
    }
}

/// xHE-AAC encoder (libxaac) and super frame builder, two frames ahead of the channel
/// (see the module docs).
struct XheEncoder {
    enc: XheAacEncoder,
    config: XheAacConfig,
    /// The Static Config SDC type 9 announces; every encoder must produce it.
    static_config: Vec<u8>,
    framer: XheAacFramer,
    /// One access unit's worth of interleaved silence.
    silence: Vec<f32>,
    /// libxaac failures (the encoder is then replaced).
    encoder_errors: u64,
    /// Frames lost to encoder failures or replaced by silence to fill a payload.
    dropped: u64,
}

impl XheEncoder {
    /// Frames the encoder runs ahead of the channel.
    const LEAD_FRAMES: usize = 2;
    /// Frames of silence added at most to fill one payload before giving up (the lead
    /// makes even one unnecessary; each frame adds at least an average frame's bytes).
    const MAX_FILL_FRAMES: usize = 32;

    fn new(plan: &AudioPlan) -> std::result::Result<Self, CodecError> {
        let config = plan
            .xhe
            .clone()
            .ok_or_else(|| CodecError::InvalidConfig("xHE-AAC service without an encoder configuration".into()))?;
        let static_config = plan.params.codec_config.clone();
        let enc = Self::open(&config, &static_config)?;
        let silence = vec![0.0; enc.frame_len() * usize::from(enc.channels())];
        let mut e =
            Self { enc, config, static_config, framer: XheAacFramer::new(), silence, encoder_errors: 0, dropped: 0 };
        e.prime()?;
        Ok(e)
    }

    /// An encoder for `config` that produces `static_config` (the plan's SDC type 9).
    fn open(config: &XheAacConfig, static_config: &[u8]) -> std::result::Result<XheAacEncoder, CodecError> {
        let enc = XheAacEncoder::new(config.clone())?;
        if enc.static_config() != static_config {
            return Err(CodecError::InvalidConfig(
                "libxaac produced another xHE-AAC Static Config than SDC type 9 announces".into(),
            ));
        }
        Ok(enc)
    }

    /// Encode the lead: frames of silence.
    fn prime(&mut self) -> std::result::Result<(), CodecError> {
        for _ in 0..Self::LEAD_FRAMES {
            let silence = std::mem::take(&mut self.silence);
            let r = self.encode(&silence);
            self.silence = silence;
            r?;
        }
        Ok(())
    }

    fn encode(&mut self, pcm: &[f32]) -> std::result::Result<(), CodecError> {
        for au in self.enc.encode(pcm)? {
            self.framer.push_access_unit(&au.data, au.bit_reservoir_level);
        }
        Ok(())
    }

    /// Encode `pcm`; if libxaac fails (fatal for its instance), count it and continue with
    /// a new encoder, primed again (the audio of the failed call is lost). Fails only if
    /// no new encoder can be made.
    fn encode_or_restart(&mut self, pcm: &[f32]) -> std::result::Result<(), CodecError> {
        if self.encode(pcm).is_ok() {
            return Ok(());
        }
        self.encoder_errors += 1;
        self.dropped += (pcm.len() / self.silence.len().max(1)) as u64;
        self.enc = Self::open(&self.config, &self.static_config)?;
        self.prime()
    }

    /// The super frame of `len` bytes after encoding `pcm` (400 ms of input).
    fn super_frame(&mut self, pcm: &[f32], len: usize, service: &str) -> Result<Vec<u8>> {
        let codec_error = |source| StationError::Codec { service: service.to_string(), source };
        self.encode_or_restart(pcm).map_err(codec_error)?;
        // The lead fills the payload; should it not, add silence rather than pad the
        // payload (which would break the frame in progress).
        for _ in 0..Self::MAX_FILL_FRAMES {
            if self.framer.ready(len) {
                break;
            }
            self.dropped += 1;
            let silence = std::mem::take(&mut self.silence);
            let r = self.encode_or_restart(&silence);
            self.silence = silence;
            r.map_err(codec_error)?;
        }
        Ok(self.framer.next_super_frame(len)?)
    }
}

/// EnCodec encoder producing DecDRM's EnCodec super frames (the `encodec` feature).
#[cfg(feature = "encodec")]
struct EncodecEncoder {
    enc: decdrm_encodec::EncodecDrmEncoder,
    encoder_errors: u64,
}

#[cfg(feature = "encodec")]
impl EncodecEncoder {
    fn super_frame(&mut self, pcm: &[f32], len: usize) -> Vec<u8> {
        match self.enc.super_frame(pcm, len) {
            Ok(sf) => sf,
            Err(_) => {
                // Zeros fail every CRC of the super frame, so the receiver conceals it.
                self.encoder_errors += 1;
                vec![0; len]
            }
        }
    }
}

/// The EnCodec encoder of `plan`, with the encoder half of the model from the default
/// location (`decdrm_encodec::weights`).
#[cfg(feature = "encodec")]
fn open_encodec(plan: &AudioPlan) -> std::result::Result<Encoder, decdrm_codecs::CodecError> {
    let config = plan
        .encodec
        .ok_or_else(|| decdrm_codecs::CodecError::InvalidConfig("EnCodec service without a configuration".into()))?;
    decdrm_encodec::EncodecDrmEncoder::open(config)
        .map(|enc| Encoder::Encodec(EncodecEncoder { enc, encoder_errors: 0 }))
        .map_err(|e| decdrm_codecs::CodecError::Unsupported(e.to_string()))
}

#[cfg(not(feature = "encodec"))]
fn open_encodec(_plan: &AudioPlan) -> std::result::Result<Encoder, decdrm_codecs::CodecError> {
    Err(decdrm_codecs::CodecError::Unsupported("EnCodec is not built in (build with `--features encodec`)".into()))
}

enum Encoder {
    Aac(AacEncoder),
    Xhe(Box<XheEncoder>),
    Opus(OpusEncoder),
    #[cfg(feature = "encodec")]
    Encodec(EncodecEncoder),
}

// ---------------------------------------------------------------------------------
// Chain
// ---------------------------------------------------------------------------------

/// Counters and levels of an audio service, updated every frame.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AudioCounters {
    /// RMS level of the last 400 ms of input, dBFS.
    pub input_rms_dbfs: f32,
    /// Peak level of the last 400 ms of input, dBFS.
    pub input_peak_dbfs: f32,
    /// Audio super frames produced.
    pub super_frames: u64,
    /// Audio frames sent empty (encoder errors, or too large for the super frame).
    pub frames_dropped: u64,
    /// Encoder errors.
    pub encoder_errors: u64,
    /// Ratio trim following a sound-card input's clock, ppm (sound-card input and
    /// output only).
    pub input_drift_ppm: Option<f64>,
}

/// Input → encoder → logical frames of one audio service.
pub(crate) struct AudioChain {
    pub plan: AudioPlan,
    source: Box<dyn AudioSource>,
    encoder: Encoder,
    text: Option<TextMessageEncoder>,
    stream_len: usize,
    pcm: Vec<f32>,
    pub counters: AudioCounters,
}

fn to_db(v: f32) -> f32 {
    20.0 * v.max(1e-10).log10()
}

impl AudioChain {
    /// Open the input and create the encoder of service `service` (for error texts).
    pub fn new(cfg: &StationConfig, service: usize, plan: &AudioPlan, stream: StreamLengths) -> Result<Self> {
        let name = format!("service {service} (\"{}\")", cfg.services[service].label);
        let settings = cfg.services[service].audio.as_ref().expect("audio plan has audio settings");
        let source = open_source(cfg, &settings.input, plan.input_rate, plan.input_channels)
            .map_err(|message| StationError::Input { service: name.clone(), message })?;
        let encoder = match plan.codec {
            Codec::Opus => OpusEncoder::new(plan, stream).map(Encoder::Opus),
            Codec::XheAac => XheEncoder::new(plan).map(|e| Encoder::Xhe(Box::new(e))),
            Codec::Encodec => open_encodec(plan),
            _ => AacEncoder::new(plan, stream).map(Encoder::Aac),
        }
        .map_err(|source| StationError::Codec { service: name, source })?;
        let text = plan.text.then(|| {
            let mut t = TextMessageEncoder::new();
            for m in settings.text.iter().filter(|m| !m.is_empty()) {
                t.add_message(m);
            }
            t
        });
        Ok(Self {
            plan: plan.clone(),
            source,
            encoder,
            text,
            stream_len: stream.total(),
            pcm: Vec::new(),
            counters: AudioCounters::default(),
        })
    }

    /// The stream bytes of the next multiplex frame.
    pub fn next_logical_frame(&mut self, service_name: &str) -> Result<Vec<u8>> {
        // 400 ms at the encoder's input rate.
        let frames = self.plan.input_rate as usize * 2 / 5;
        self.pcm.clear();
        self.source
            .read(frames, &mut self.pcm)
            .map_err(|message| StationError::Input { service: service_name.to_string(), message })?;
        let (sum, peak) = self.pcm.iter().fold((0.0f64, 0.0f32), |(s, p), &v| (s + f64::from(v * v), p.max(v.abs())));
        self.counters.input_rms_dbfs = to_db((sum / self.pcm.len().max(1) as f64).sqrt() as f32);
        self.counters.input_peak_dbfs = to_db(peak);
        self.counters.input_drift_ppm = self.source.drift_ppm();
        let len = self.plan.super_frame_len;
        let mut lf = match &mut self.encoder {
            Encoder::Aac(e) => {
                let sf = e.super_frame(&self.pcm, len)?;
                self.counters.frames_dropped = e.dropped;
                self.counters.encoder_errors = e.encoder_errors;
                sf
            }
            Encoder::Xhe(e) => {
                let sf = e.super_frame(&self.pcm, len, service_name)?;
                self.counters.frames_dropped = e.dropped;
                self.counters.encoder_errors = e.encoder_errors;
                sf
            }
            Encoder::Opus(e) => {
                let sf = e.super_frame(&self.pcm, len)?;
                self.counters.frames_dropped = e.encoder_errors;
                self.counters.encoder_errors = e.encoder_errors;
                sf
            }
            #[cfg(feature = "encodec")]
            Encoder::Encodec(e) => {
                let sf = e.super_frame(&self.pcm, len);
                self.counters.frames_dropped = e.encoder_errors * decdrm_encodec::FRAMES_PER_SUPER_FRAME as u64;
                self.counters.encoder_errors = e.encoder_errors;
                sf
            }
        };
        if let Some(t) = self.text.as_mut() {
            lf.resize(self.stream_len, 0);
            insert_text_message(&mut lf, t.next_piece());
        }
        debug_assert_eq!(lf.len(), self.stream_len);
        self.counters.super_frames += 1;
        Ok(lf)
    }

    /// Whether the input is a non-looping file that has ended.
    pub fn input_finished(&self) -> bool {
        self.source.finished()
    }

    /// Whether the input ends by itself.
    pub fn input_finite(&self) -> bool {
        self.source.finite()
    }

    /// Description of the input.
    pub fn input_description(&self) -> String {
        self.source.describe()
    }
}

#[cfg(test)]
mod tests {
    use super::InputDrift;

    /// Input clock `offset` fast (fraction), station consuming 400 ms per step, the
    /// backlog measured with ±10 ms of jitter (the capture buffer fills in steps).
    /// Returns the mean backlog, the mean trim and the trim's largest deviation from
    /// its mean over the last 400 s of 1200 s.
    fn simulate(offset: f64) -> (f64, f64, f64) {
        let mut drift = InputDrift::new(0.3);
        let (mut backlog, dt) = (0.3, 0.4);
        let mut ppm = 0.0;
        let mut seed = 1u32;
        let mut tail = Vec::new();
        for step in 0..3000 {
            // Arrived: dt·(1 + offset); consumed: dt / (1 + trim) of input.
            backlog += dt * (1.0 + offset) - dt / (1.0 + ppm * 1e-6);
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let jitter = (f64::from(seed >> 8) / f64::from(1u32 << 24) - 0.5) * 0.02;
            ppm = drift.update(backlog + jitter, dt);
            if step >= 2000 {
                tail.push((backlog, ppm));
            }
        }
        let n = tail.len() as f64;
        let mean_backlog = tail.iter().map(|t| t.0).sum::<f64>() / n;
        let mean_ppm = tail.iter().map(|t| t.1).sum::<f64>() / n;
        let ripple = tail.iter().map(|t| (t.1 - mean_ppm).abs()).fold(0.0, f64::max);
        (mean_backlog, mean_ppm, ripple)
    }

    #[test]
    fn input_drift_loop_follows_the_clock() {
        for offset_ppm in [-200.0, -50.0, 0.0, 100.0, 300.0] {
            let (backlog, ppm, ripple) = simulate(offset_ppm * 1e-6);
            assert!((backlog - 0.3).abs() < 0.002, "{offset_ppm} ppm: backlog {backlog}");
            assert!((ppm + offset_ppm).abs() < 5.0, "{offset_ppm} ppm: trim {ppm}");
            // Unfiltered, the ±10 ms would give ±400 ppm.
            assert!(ripple < 200.0, "{offset_ppm} ppm: trim ripple {ripple}");
        }
    }

    use super::*;

    #[test]
    fn channel_mapping() {
        let mut out = Vec::new();
        map_channels(&[1.0, 0.0, 0.5, 0.5], 2, 1, 1.0, &mut out);
        assert_eq!(out, [0.5, 0.5]);
        out.clear();
        map_channels(&[0.25, -0.25], 1, 2, 2.0, &mut out);
        assert_eq!(out, [0.5, 0.5, -0.5, -0.5]);
        out.clear();
        map_channels(&[1.0, 2.0, 3.0], 3, 2, 1.0, &mut out);
        assert_eq!(out, [1.0, 2.0]);
    }

    #[test]
    fn fifo_pads_and_compacts() {
        let mut f = Fifo::default();
        f.buf.extend([1.0, 2.0, 3.0]);
        let mut out = Vec::new();
        f.take(2, &mut out);
        f.take(3, &mut out);
        assert_eq!(out, [1.0, 2.0, 3.0, 0.0, 0.0]);
        assert_eq!(f.len(), 0);
        assert!(f.buf.is_empty());
    }

    /// A file input is resampled to the encoder rate; without looping it ends in
    /// silence, with looping it restarts.
    #[test]
    fn file_source_end_and_loop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.wav");
        let fmt = decdrm_io::AudioFormat::new(16_000, 1);
        let mut w = decdrm_io::FileWriter::create(&path, fmt, decdrm_io::Container::Wav, decdrm_io::Encoding::Float32).unwrap();
        w.write(&vec![0.5; 8000]).unwrap(); // 0.5 s
        w.finalize().unwrap();
        // 0.5 s at 16 kHz → 12 000 frames at 24 kHz, stereo.
        let mut once = FileSource::open(&path, false, 24_000, 2, 1.0).unwrap();
        let mut out = Vec::new();
        once.read(9600, &mut out).unwrap();
        assert!(!once.finished());
        once.read(9600, &mut out).unwrap();
        assert!(once.finished() && once.finite());
        assert_eq!(out.len(), 2 * 19_200);
        let loud = out.iter().filter(|v| (**v - 0.5).abs() < 0.01).count();
        // Allow for the resampler's ramps at the start and the end.
        assert!((loud as i64 - 24_000).abs() < 1000, "{loud} samples of the file");
        assert!(out[out.len() - 100..].iter().all(|&v| v == 0.0), "silence after the end");
        // Looping: still sound after three passes.
        let mut looped = FileSource::open(&path, true, 24_000, 1, 1.0).unwrap();
        let mut out = Vec::new();
        for _ in 0..4 {
            looped.read(9600, &mut out).unwrap();
        }
        assert!(!looped.finished() && !looped.finite());
        assert!(out[out.len() - 1000..].iter().all(|v| (v - 0.5).abs() < 0.05));
    }

    /// Frames too large for the payload are dropped (largest first); the rest is padded.
    #[test]
    fn aac_packing_overflow_and_padding() {
        let cfg = FdkEncoderConfig::new(AacProfile::HeAac, 12_000, 12_000);
        let mut enc = FdkDrmEncoder::new(cfg.clone()).unwrap();
        let pcm: Vec<f32> =
            (0..cfg.frame_len()).map(|i| 0.3 * (i as f32 * 0.07).sin() + 0.01 * ((i * 7919) % 13) as f32).collect();
        let mut frames = Vec::new();
        while frames.len() < 5 {
            if let Some(f) = enc.encode(&pcm).unwrap() {
                frames.push(Some(f));
            }
        }
        let sizes: Vec<usize> = frames.iter().map(|f| f.as_ref().unwrap().min_len()).collect();
        let stream = StreamLengths { part_a: 0, part_b: 1000 };
        let fmt = AacSuperFrameFormat::aac(5, stream);
        let overhead = fmt.header_bytes() + 5;
        // Plenty of room: nothing dropped, every frame padded.
        let (sf, dropped) = pack_aac(&frames, &fmt, 1000).unwrap();
        assert_eq!((sf.len(), dropped), (1000, 0));
        // One byte too few: exactly one frame is dropped.
        let tight = overhead + sizes.iter().sum::<usize>() - 1;
        let (sf, dropped) = pack_aac(&frames, &fmt, tight).unwrap();
        assert_eq!((sf.len(), dropped), (tight, 1));
        // A lost last frame keeps one byte.
        frames[4] = None;
        let (sf, dropped) = pack_aac(&frames, &fmt, 1000).unwrap();
        assert_eq!((sf.len(), dropped), (1000, 0));
        let parsed = decdrm_core::mux::audio::parse_aac_super_frame(&sf, &fmt).unwrap();
        assert_eq!(parsed[4].data.len(), 1);
    }
}
