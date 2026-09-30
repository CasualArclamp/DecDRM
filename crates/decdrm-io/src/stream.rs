//! Sound-card capture ([`InputStream`]) and playback ([`OutputStream`]).
//!
//! # Threading model
//!
//! cpal runs our *data callback* on the audio driver's own high-priority thread, every few
//! milliseconds, with a buffer to fill (playback) or drain (capture). If the callback takes
//! too long the driver has nothing to play and you hear a click. So the callback must never
//! wait for anything:
//!
//! * **No `Mutex`.** If the callback tried to lock a mutex that your thread currently holds
//!   (say, while it is being descheduled by the OS), the audio thread would sleep until your
//!   thread runs again — an unbounded delay, and a classic *priority inversion* (a
//!   high-priority thread waiting on a low-priority one).
//! * **No allocation, no I/O, no logging.** All of these may take locks inside the allocator,
//!   the OS or the runtime.
//!
//! Instead each stream owns a **single-producer/single-consumer ring buffer** ([`rtrb`]):
//! a fixed-size array plus two atomic indices. The writer only ever advances the write index
//! and the reader only the read index, so neither can block the other; each side sees
//! "how much is there" with one atomic load. Statistics travel through atomic counters, and
//! backend errors through a second tiny ring buffer.
//!
//! In Rust terms: when a stream is opened, the ring buffer is split into a
//! [`rtrb::Producer`] and a [`rtrb::Consumer`]. One half is *moved* into the callback
//! closure (`move |data, info| ...`), which cpal then owns and runs on its thread; the other
//! half stays in the [`InputStream`]/[`OutputStream`] you hold. Because each half has exactly
//! one owner, the compiler guarantees there is only ever one reader and one writer.
//! Counters shared by both sides live in an [`Arc`] (atomically reference-counted pointer):
//! both sides hold a clone, and the counters are freed when the last clone is dropped.
//!
//! Dropping a stream stops the device first (the `cpal::Stream` field is declared first,
//! and struct fields are dropped in declaration order), then frees the buffers.
//!
//! # Blocking reads and writes
//!
//! Your side may of course wait. [`InputStream::read_blocking`] and
//! [`OutputStream::write_blocking`] sleep for roughly the time the device needs to produce
//! or consume the missing frames, then re-check the ring buffer: simple, and without any
//! signalling from the real-time thread.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, StreamTrait};
// `Sample` provides the `from_sample` conversions (dasp_sample traits re-exported by cpal).
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::device::{choose_config, device_name, find_device, ChosenConfig};
use crate::{check_whole_frames, AudioFormat, Direction, Error, Result};

/// Frames converted per inner step of the output callback (sets the scratch buffer size).
const SCRATCH_FRAMES: usize = 4096;
/// Linear fade-in after (re)starting playback, in frames (~5 ms at 48 kHz).
const FADE_IN_FRAMES: usize = 256;
/// Linear decay from the last played frame to silence on an underrun (~1.3 ms at 48 kHz).
const FADE_OUT_FRAMES: usize = 64;
/// Backend errors kept until the owner polls; further ones are dropped meanwhile.
const ERROR_QUEUE: usize = 32;

/// `now + timeout`, saturating instead of panicking for huge timeouts (e.g. `Duration::MAX`
/// meaning "wait forever").
pub(crate) fn deadline_after(timeout: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(timeout)
        .or_else(|| now.checked_add(Duration::from_secs(u64::from(u32::MAX))))
        .unwrap_or(now)
}

/// How long to sleep while waiting for `frames` frames to be produced or consumed at
/// `rate`: about the time the device needs, kept within 1–20 ms and never past `deadline`.
pub(crate) fn nap(frames: usize, rate: u32, deadline: Instant) -> Duration {
    let secs = frames as f64 / f64::from(rate.max(1));
    Duration::try_from_secs_f64(secs)
        .unwrap_or(Duration::MAX)
        .clamp(Duration::from_millis(1), Duration::from_millis(20))
        .min(deadline.saturating_duration_since(Instant::now()))
}

// ---------------------------------------------------------------------------------------------
// Error plumbing shared by both directions
// ---------------------------------------------------------------------------------------------

/// Health flags written by the cpal error callback.
#[derive(Default)]
pub(crate) struct StreamHealth {
    /// Buffer over/underruns reported by the backend itself.
    pub(crate) xruns: AtomicU64,
    /// Set when the device is gone or the stream was invalidated.
    pub(crate) fatal: AtomicBool,
}

/// Lives inside cpal's error callback.
pub(crate) struct ErrorSink {
    queue: Producer<cpal::Error>,
    health: Arc<StreamHealth>,
}

impl ErrorSink {
    fn report(&mut self, err: cpal::Error) {
        match err.kind() {
            cpal::ErrorKind::Xrun => {
                self.health.xruns.fetch_add(1, Relaxed);
            }
            kind => {
                if matches!(
                    kind,
                    cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated
                ) {
                    self.health.fatal.store(true, Relaxed);
                }
                // Drop the error if the queue is full: the owner is not polling anyway.
                let _ = self.queue.push(err);
            }
        }
    }
}

fn error_channel() -> (ErrorSink, Consumer<cpal::Error>, Arc<StreamHealth>) {
    let (queue, rx) = RingBuffer::new(ERROR_QUEUE);
    let health = Arc::new(StreamHealth::default());
    (ErrorSink { queue, health: Arc::clone(&health) }, rx, health)
}

fn drain_errors(rx: &mut Consumer<cpal::Error>) -> Vec<Error> {
    std::iter::from_fn(|| rx.pop().ok()).map(Error::Audio).collect()
}

fn fatal_error(direction: Direction) -> Error {
    Error::Audio(cpal::Error::with_message(
        cpal::ErrorKind::DeviceNotAvailable,
        format!("the {direction} stream stopped (device unplugged or reconfigured?)"),
    ))
}

// ---------------------------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------------------------

/// Options for [`InputStream::open`].
#[derive(Clone, Debug)]
pub struct InputOptions {
    /// Device name (exact, or any unique case-insensitive part of it) or backend id;
    /// `None` = system default input.
    pub device: Option<String>,
    /// Preferred sample rate. The device may not support it; check
    /// [`InputStream::format`] for what you actually got. `None` = the device's default.
    pub sample_rate: Option<u32>,
    /// Preferred channel count (1 for a real IF signal, 2 for I/Q). `None` = device default.
    pub channels: Option<usize>,
    /// Capacity of the ring buffer between the driver and your thread. If your thread falls
    /// behind by more than this, the oldest unread audio is kept and new audio is dropped
    /// (an *overrun*, counted in [`InputStats`]).
    pub buffer: Duration,
}

impl Default for InputOptions {
    fn default() -> Self {
        InputOptions {
            device: None,
            sample_rate: Some(crate::WORKING_RATE),
            channels: None,
            buffer: Duration::from_secs(2),
        }
    }
}

/// Counters describing an input stream's health.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputStats {
    /// Frames delivered by the driver since the stream started.
    pub frames_captured: u64,
    /// Number of callbacks whose data did not (completely) fit into the ring buffer.
    pub overruns: u64,
    /// Frames discarded because the ring buffer was full.
    pub dropped_frames: u64,
    /// Overruns reported by the backend itself (data lost before it reached us).
    pub xruns: u64,
    /// Frames currently waiting to be read.
    pub buffered_frames: usize,
    /// Ring buffer capacity in frames.
    pub capacity_frames: usize,
}

#[derive(Default)]
pub(crate) struct InputShared {
    frames_captured: AtomicU64,
    overruns: AtomicU64,
    dropped_frames: AtomicU64,
}

/// The half of an input stream that runs inside the audio callback.
pub(crate) struct Capturer {
    producer: Producer<f32>,
    shared: Arc<InputShared>,
    channels: usize,
}

impl Capturer {
    /// Converts one callback's worth of device samples to `f32` and queues them.
    /// Real-time safe: no locks, no allocation.
    pub(crate) fn capture<T>(&mut self, data: &[T])
    where
        T: Copy,
        f32: FromSample<T>,
    {
        let ch = self.channels;
        let frames = data.len() / ch;
        let free = self.producer.slots() / ch;
        let mut n = frames.min(free);
        if n > 0 {
            match self.producer.write_chunk_uninit(n * ch) {
                Ok(chunk) => {
                    chunk.fill_from_iter(data[..n * ch].iter().map(|&s| f32::from_sample(s)));
                }
                Err(_) => n = 0,
            }
        }
        if n < frames {
            self.shared.overruns.fetch_add(1, Relaxed);
            self.shared.dropped_frames.fetch_add((frames - n) as u64, Relaxed);
        }
        self.shared.frames_captured.fetch_add(frames as u64, Relaxed);
    }
}

/// A running capture stream delivering interleaved `f32` frames.
///
/// ```no_run
/// use decdrm_io::{InputOptions, InputStream};
/// use std::time::Duration;
/// # fn main() -> decdrm_io::Result<()> {
/// let mut input = InputStream::open(&InputOptions {
///     device: Some("CABLE Output".into()),
///     channels: Some(2),
///     ..Default::default()
/// })?;
/// println!("capturing {} from {}", input.format(), input.device_name());
/// loop {
///     let block = input.read_blocking(4800, Duration::from_secs(1))?;
///     // resample to 48 kHz if needed, then feed the receiver ...
/// #   let _ = block; break;
/// }
/// # Ok(()) }
/// ```
pub struct InputStream {
    // Declared first so it is dropped (device stopped) before the buffers.
    stream: Option<cpal::Stream>,
    consumer: Consumer<f32>,
    shared: Arc<InputShared>,
    health: Arc<StreamHealth>,
    errors: Consumer<cpal::Error>,
    format: AudioFormat,
    device_name: String,
    capacity_frames: usize,
}

impl InputStream {
    /// Opens and starts a capture stream.
    ///
    /// The device's native sample type (16/24/32-bit integer or float) is converted to
    /// normalised `f32` in the callback. If the preferred rate or channel count is not
    /// supported, the closest supported configuration is used — check [`format`](Self::format).
    pub fn open(opts: &InputOptions) -> Result<Self> {
        let device = find_device(Direction::Input, opts.device.as_deref())?;
        let chosen = choose_config(&device, Direction::Input, opts.sample_rate, opts.channels)?;
        let format = chosen.format();
        let capacity = format.duration_to_frames(opts.buffer).max(4096) as usize;
        let (mut this, capturer, sink) = Self::parts(format, capacity);
        let stream = build_input(&device, chosen, capturer, sink)?;
        stream.play()?;
        this.stream = Some(stream);
        this.device_name = device_name(&device);
        Ok(this)
    }

    /// Builds the stream object and the callback half without touching any device.
    pub(crate) fn parts(format: AudioFormat, capacity_frames: usize) -> (Self, Capturer, ErrorSink) {
        let ch = format.channels.max(1);
        let (producer, consumer) = RingBuffer::new(capacity_frames * ch);
        let shared = Arc::new(InputShared::default());
        let (sink, errors, health) = error_channel();
        let capturer = Capturer { producer, shared: Arc::clone(&shared), channels: ch };
        let stream = InputStream {
            stream: None,
            consumer,
            shared,
            health,
            errors,
            format,
            device_name: String::new(),
            capacity_frames,
        };
        (stream, capturer, sink)
    }

    /// The format actually delivered (may differ from the requested one).
    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// Name of the device being captured.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Frames ready to be read without waiting.
    pub fn available_frames(&self) -> usize {
        self.consumer.slots() / self.format.channels
    }

    /// Returns up to `max_frames` frames that are already buffered (possibly none).
    pub fn read(&mut self, max_frames: usize) -> Vec<f32> {
        let mut out = Vec::new();
        self.read_into(&mut out, max_frames);
        out
    }

    /// Appends up to `max_frames` buffered frames to `out`; returns the number of frames.
    pub fn read_into(&mut self, out: &mut Vec<f32>, max_frames: usize) -> usize {
        let ch = self.format.channels;
        let n = self.available_frames().min(max_frames);
        if n == 0 {
            return 0;
        }
        match self.consumer.read_chunk(n * ch) {
            Ok(chunk) => {
                let (a, b) = chunk.as_slices();
                out.extend_from_slice(a);
                out.extend_from_slice(b);
                chunk.commit_all();
                n
            }
            Err(_) => 0,
        }
    }

    /// Waits until `frames` frames are available (or `timeout` expires) and returns them.
    ///
    /// On timeout, returns whatever is available (possibly nothing). Returns an error if the
    /// device has gone away and no more data will come. Requests larger than the ring buffer
    /// are capped to its capacity.
    pub fn read_blocking(&mut self, frames: usize, timeout: Duration) -> Result<Vec<f32>> {
        let frames = frames.min(self.capacity_frames);
        let deadline = deadline_after(timeout);
        loop {
            let avail = self.available_frames();
            if avail >= frames {
                return Ok(self.read(frames));
            }
            if avail == 0 && self.health.fatal.load(Relaxed) {
                return Err(fatal_error(Direction::Input));
            }
            if Instant::now() >= deadline {
                return Ok(self.read(frames));
            }
            std::thread::sleep(nap(frames - avail, self.format.sample_rate, deadline));
        }
    }

    /// Current counters.
    pub fn stats(&self) -> InputStats {
        InputStats {
            frames_captured: self.shared.frames_captured.load(Relaxed),
            overruns: self.shared.overruns.load(Relaxed),
            dropped_frames: self.shared.dropped_frames.load(Relaxed),
            xruns: self.health.xruns.load(Relaxed),
            buffered_frames: self.available_frames(),
            capacity_frames: self.capacity_frames,
        }
    }

    /// Backend errors reported since the last call (device lost, reconfigured, ...).
    /// Over/underruns are not errors; they are counted in [`stats`](Self::stats).
    pub fn take_errors(&mut self) -> Vec<Error> {
        drain_errors(&mut self.errors)
    }

    /// `true` once the backend reported that the device is gone.
    pub fn is_dead(&self) -> bool {
        self.health.fatal.load(Relaxed)
    }

    /// Pauses capture (if the backend supports it).
    pub fn pause(&self) -> Result<()> {
        if let Some(s) = &self.stream {
            s.pause()?;
        }
        Ok(())
    }

    /// Resumes capture after [`pause`](Self::pause).
    pub fn play(&self) -> Result<()> {
        if let Some(s) = &self.stream {
            s.play()?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for InputStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputStream")
            .field("device", &self.device_name)
            .field("format", &self.format)
            .field("stats", &self.stats())
            .finish()
    }
}

fn build_input(
    device: &cpal::Device,
    chosen: ChosenConfig,
    capturer: Capturer,
    sink: ErrorSink,
) -> Result<cpal::Stream> {
    let cfg = chosen.config;
    match chosen.sample_format {
        SampleFormat::F32 => build_input_typed::<f32>(device, cfg, capturer, sink),
        SampleFormat::F64 => build_input_typed::<f64>(device, cfg, capturer, sink),
        SampleFormat::I8 => build_input_typed::<i8>(device, cfg, capturer, sink),
        SampleFormat::I16 => build_input_typed::<i16>(device, cfg, capturer, sink),
        SampleFormat::I24 => build_input_typed::<cpal::I24>(device, cfg, capturer, sink),
        SampleFormat::I32 => build_input_typed::<i32>(device, cfg, capturer, sink),
        SampleFormat::U8 => build_input_typed::<u8>(device, cfg, capturer, sink),
        SampleFormat::U16 => build_input_typed::<u16>(device, cfg, capturer, sink),
        SampleFormat::U32 => build_input_typed::<u32>(device, cfg, capturer, sink),
        other => Err(Error::NoUsableConfig(format!("{} (sample type {other})", device_name(device)))),
    }
}

fn build_input_typed<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut capturer: Capturer,
    mut sink: ErrorSink,
) -> Result<cpal::Stream>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    // `move` closures take ownership of `capturer` and `sink`; cpal keeps the closures alive
    // (and runs them on its audio thread) for as long as the returned `Stream` exists.
    Ok(device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| capturer.capture(data),
        move |err| sink.report(err),
        None,
    )?)
}

// ---------------------------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------------------------

/// Options for [`OutputStream::open`].
#[derive(Clone, Debug)]
pub struct OutputOptions {
    /// Device name (exact, or any unique case-insensitive part of it) or backend id;
    /// `None` = system default output.
    pub device: Option<String>,
    /// Preferred sample rate; `None` = the device's default (usually its mix rate).
    pub sample_rate: Option<u32>,
    /// Preferred channel count; `None` = the device's default.
    pub channels: Option<usize>,
    /// Ring buffer capacity (maximum queued audio).
    pub buffer: Duration,
    /// Audio that must be queued before playback starts, and again after an underrun
    /// (prevents stuttering when data trickles in).
    pub start_threshold: Duration,
}

impl Default for OutputOptions {
    fn default() -> Self {
        OutputOptions {
            device: None,
            sample_rate: None,
            channels: None,
            buffer: Duration::from_secs(1),
            start_threshold: Duration::from_millis(50),
        }
    }
}

/// Whether an output stream is currently playing queued audio or waiting for enough audio
/// to (re)start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OutputState {
    /// Playing silence until the start threshold is reached (initially, and after an
    /// underrun).
    Buffering,
    /// Playing queued audio.
    Playing,
}

/// Counters describing an output stream's health.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputStats {
    /// Frames handed to the device so far (audio and silence): the device clock.
    pub frames_played: u64,
    /// Times playback ran out of queued audio.
    pub underruns: u64,
    /// Frames of silence inserted by those underruns (in the callback that ran dry).
    pub underrun_frames: u64,
    /// Underruns reported by the backend itself.
    pub xruns: u64,
    /// Frames currently queued.
    pub buffered_frames: usize,
    /// Ring buffer capacity in frames.
    pub capacity_frames: usize,
    /// Playing or buffering.
    pub state: OutputState,
}

pub(crate) struct OutputShared {
    /// Frames rendered (the device clock).
    pub(crate) frames_rendered: AtomicU64,
    /// Σ over callbacks of `queued_frames × callback_frames`: dividing a difference of this by
    /// the matching difference of `frames_rendered` gives the time-averaged queue depth.
    pub(crate) fill_integral: AtomicU64,
    pub(crate) underruns: AtomicU64,
    pub(crate) underrun_frames: AtomicU64,
    pub(crate) playing: AtomicBool,
    pub(crate) start_threshold: AtomicUsize,
    /// When set, running dry is expected (end of stream) and not counted as an underrun.
    pub(crate) draining: AtomicBool,
    /// Playback volume: a linear gain as `f32` bits (1.0 = unchanged).
    pub(crate) volume: AtomicU32,
}

impl Default for OutputShared {
    fn default() -> Self {
        Self {
            frames_rendered: AtomicU64::new(0),
            fill_integral: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            underrun_frames: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            start_threshold: AtomicUsize::new(0),
            draining: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
        }
    }
}

/// The half of an output stream that runs inside the audio callback.
pub(crate) struct Renderer {
    consumer: Consumer<f32>,
    shared: Arc<OutputShared>,
    channels: usize,
    capacity_frames: usize,
    playing: bool,
    fade_in_left: usize,
    fade_out_left: usize,
    last_frame: Vec<f32>,
    /// Volume applied at the end of the previous callback (ramped to the new setting).
    gain: f32,
}

impl Renderer {
    /// Fills `out` (interleaved `f32`) from the ring buffer. Real-time safe: no locks, no
    /// allocation (`last_frame` is allocated once, at construction).
    pub(crate) fn render(&mut self, out: &mut [f32]) {
        let ch = self.channels;
        let frames = out.len() / ch;
        // A trailing partial frame (never produced by cpal) is silenced.
        out[frames * ch..].fill(0.0);
        let out = &mut out[..frames * ch];

        let avail = self.consumer.slots() / ch;
        self.shared.fill_integral.fetch_add((avail as u64) * (frames as u64), Relaxed);

        if !self.playing {
            let threshold =
                self.shared.start_threshold.load(Relaxed).clamp(1, self.capacity_frames.max(1));
            let draining = self.shared.draining.load(Relaxed);
            if avail >= threshold || (draining && avail > 0) {
                self.playing = true;
                self.fade_in_left = FADE_IN_FRAMES;
                self.fade_out_left = 0;
                self.shared.playing.store(true, Relaxed);
            }
        }

        let mut n = 0;
        if self.playing {
            n = frames.min(avail);
            if n > 0 {
                match self.consumer.read_chunk(n * ch) {
                    Ok(chunk) => {
                        let (a, b) = chunk.as_slices();
                        out[..a.len()].copy_from_slice(a);
                        out[a.len()..a.len() + b.len()].copy_from_slice(b);
                        chunk.commit_all();
                    }
                    Err(_) => n = 0,
                }
            }
            // Fade in after a (re)start so playback does not begin with a step.
            let mut f = 0;
            while self.fade_in_left > 0 && f < n {
                let g = 1.0 - self.fade_in_left as f32 / (FADE_IN_FRAMES as f32 + 1.0);
                out[f * ch..(f + 1) * ch].iter_mut().for_each(|s| *s *= g);
                self.fade_in_left -= 1;
                f += 1;
            }
            if n > 0 {
                self.last_frame.copy_from_slice(&out[(n - 1) * ch..n * ch]);
            }
            if n < frames {
                // Ran dry: go back to buffering, and decay from the last sample instead of
                // jumping to zero (a step would be heard as a click).
                self.playing = false;
                self.shared.playing.store(false, Relaxed);
                self.fade_out_left = FADE_OUT_FRAMES;
                if !self.shared.draining.load(Relaxed) {
                    self.shared.underruns.fetch_add(1, Relaxed);
                    self.shared.underrun_frames.fetch_add((frames - n) as u64, Relaxed);
                }
            }
        }

        for frame in out[n * ch..].chunks_exact_mut(ch) {
            if self.fade_out_left > 0 {
                let g = self.fade_out_left as f32 / (FADE_OUT_FRAMES as f32 + 1.0);
                for (o, &l) in frame.iter_mut().zip(&self.last_frame) {
                    *o = l * g;
                }
                self.fade_out_left -= 1;
            } else {
                frame.fill(0.0);
            }
        }

        // Volume, ramped linearly over this callback when it changes (a step would be
        // heard as a click or "zipper" noise).
        let target = f32::from_bits(self.shared.volume.load(Relaxed));
        if target != 1.0 || self.gain != 1.0 {
            let start = self.gain;
            let step = (target - start) / frames.max(1) as f32;
            for (i, frame) in out.chunks_exact_mut(ch).enumerate() {
                let g = if step == 0.0 { target } else { start + step * (i + 1) as f32 };
                frame.iter_mut().for_each(|s| *s *= g);
            }
            self.gain = target;
        }
        self.shared.frames_rendered.fetch_add(frames as u64, Relaxed);
    }
}

/// A running playback stream fed with interleaved `f32` frames.
///
/// This is the raw building block: what you queue is played at the device's rate, with no
/// resampling. For decoded programme audio use [`AudioPlayer`](crate::AudioPlayer), which
/// adds rate conversion, channel mapping and clock-drift compensation on top.
pub struct OutputStream {
    // Declared first so it is dropped (device stopped) before the buffers.
    stream: Option<cpal::Stream>,
    producer: Producer<f32>,
    shared: Arc<OutputShared>,
    health: Arc<StreamHealth>,
    errors: Consumer<cpal::Error>,
    format: AudioFormat,
    device_name: String,
    capacity_frames: usize,
}

impl OutputStream {
    /// Opens and starts a playback stream. It plays silence until
    /// [`OutputOptions::start_threshold`] worth of audio has been queued.
    pub fn open(opts: &OutputOptions) -> Result<Self> {
        let device = find_device(Direction::Output, opts.device.as_deref())?;
        let chosen = choose_config(&device, Direction::Output, opts.sample_rate, opts.channels)?;
        let format = chosen.format();
        let capacity = format.duration_to_frames(opts.buffer).max(4096) as usize;
        let threshold = format.duration_to_frames(opts.start_threshold) as usize;
        let (mut this, renderer, sink) = Self::parts(format, capacity, threshold);
        let stream = build_output(&device, chosen, renderer, sink)?;
        stream.play()?;
        this.stream = Some(stream);
        this.device_name = device_name(&device);
        Ok(this)
    }

    /// Builds the stream object and the callback half without touching any device (used by
    /// `open`, and by tests that drive the renderer by hand).
    pub(crate) fn parts(
        format: AudioFormat,
        capacity_frames: usize,
        start_threshold_frames: usize,
    ) -> (Self, Renderer, ErrorSink) {
        let ch = format.channels.max(1);
        let (producer, consumer) = RingBuffer::new(capacity_frames * ch);
        let shared = Arc::new(OutputShared::default());
        shared.start_threshold.store(start_threshold_frames, Relaxed);
        let (sink, errors, health) = error_channel();
        let renderer = Renderer {
            consumer,
            shared: Arc::clone(&shared),
            channels: ch,
            capacity_frames,
            playing: false,
            fade_in_left: 0,
            fade_out_left: 0,
            last_frame: vec![0.0; ch],
            gain: 1.0,
        };
        let stream = OutputStream {
            stream: None,
            producer,
            shared,
            health,
            errors,
            format,
            device_name: String::new(),
            capacity_frames,
        };
        (stream, renderer, sink)
    }

    /// Set the playback volume: a linear gain (0 = silent, 1 = unchanged, at most 4)
    /// applied as the samples leave for the device, so it takes effect within one
    /// callback; changes are ramped over a callback to avoid clicks.
    pub fn set_volume(&self, gain: f32) {
        let gain = if gain.is_finite() { gain.clamp(0.0, 4.0) } else { 1.0 };
        self.shared.volume.store(gain.to_bits(), Relaxed);
    }

    /// The playback volume (linear gain).
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.shared.volume.load(Relaxed))
    }

    /// The device's format: queue audio at this rate and channel count.
    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// Name of the device playing.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Queues as many whole frames of `samples` as fit and returns how many frames were
    /// taken; the rest is dropped. Never blocks. `samples` must hold whole frames.
    pub fn write(&mut self, samples: &[f32]) -> Result<usize> {
        check_whole_frames(samples, self.format.channels)?;
        Ok(self.push_frames(samples))
    }

    /// Queues all of `samples`, waiting for room as the device plays. Returns the number of
    /// frames queued, which is less than requested only if `timeout` expired.
    pub fn write_blocking(&mut self, samples: &[f32], timeout: Duration) -> Result<usize> {
        check_whole_frames(samples, self.format.channels)?;
        let ch = self.format.channels;
        let deadline = deadline_after(timeout);
        let mut done = 0;
        while done < samples.len() {
            done += self.push_frames(&samples[done..]) * ch;
            if done == samples.len() {
                break;
            }
            if self.health.fatal.load(Relaxed) {
                return Err(fatal_error(Direction::Output));
            }
            if Instant::now() >= deadline {
                break;
            }
            let missing = ((samples.len() - done) / ch).saturating_sub(self.free_frames());
            std::thread::sleep(nap(missing.max(1), self.format.sample_rate, deadline));
        }
        Ok(done / ch)
    }

    /// Frames currently queued (not yet played).
    pub fn buffered_frames(&self) -> usize {
        self.capacity_frames - self.free_frames()
    }

    /// Queued audio as a duration.
    pub fn buffered(&self) -> Duration {
        self.format.frames_to_duration(self.buffered_frames() as u64)
    }

    /// Frames that can be queued right now without dropping.
    pub fn free_frames(&self) -> usize {
        self.producer.slots() / self.format.channels
    }

    /// Ring buffer capacity in frames.
    pub fn capacity_frames(&self) -> usize {
        self.capacity_frames
    }

    /// Changes how much audio must be queued before playback (re)starts.
    pub fn set_start_threshold(&self, frames: usize) {
        self.shared.start_threshold.store(frames, Relaxed);
    }

    /// Playing or buffering.
    pub fn state(&self) -> OutputState {
        if self.shared.playing.load(Relaxed) {
            OutputState::Playing
        } else {
            OutputState::Buffering
        }
    }

    /// Current counters.
    pub fn stats(&self) -> OutputStats {
        OutputStats {
            frames_played: self.shared.frames_rendered.load(Relaxed),
            underruns: self.shared.underruns.load(Relaxed),
            underrun_frames: self.shared.underrun_frames.load(Relaxed),
            xruns: self.health.xruns.load(Relaxed),
            buffered_frames: self.buffered_frames(),
            capacity_frames: self.capacity_frames,
            state: self.state(),
        }
    }

    /// Plays out everything queued (even if it is less than the start threshold), waiting
    /// up to `timeout`. Returns `true` if the queue emptied. Running dry at the end is not
    /// counted as an underrun.
    pub fn drain(&mut self, timeout: Duration) -> bool {
        let deadline = deadline_after(timeout);
        self.shared.draining.store(true, Relaxed);
        let drained = loop {
            let queued = self.buffered_frames();
            if queued == 0 {
                break true;
            }
            if self.health.fatal.load(Relaxed) || self.stream.is_none() {
                break false;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(nap(queued, self.format.sample_rate, deadline));
        };
        if drained {
            // Let the device play the last callback's worth before the caller stops it.
            let rest = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(Duration::from_millis(50).min(rest));
        }
        self.shared.draining.store(false, Relaxed);
        drained
    }

    /// Backend errors reported since the last call.
    pub fn take_errors(&mut self) -> Vec<Error> {
        drain_errors(&mut self.errors)
    }

    /// `true` once the backend reported that the device is gone.
    pub fn is_dead(&self) -> bool {
        self.health.fatal.load(Relaxed)
    }

    /// Pauses playback (if the backend supports it).
    pub fn pause(&self) -> Result<()> {
        if let Some(s) = &self.stream {
            s.pause()?;
        }
        Ok(())
    }

    /// Resumes playback after [`pause`](Self::pause).
    pub fn play(&self) -> Result<()> {
        if let Some(s) = &self.stream {
            s.play()?;
        }
        Ok(())
    }

    pub(crate) fn shared(&self) -> &OutputShared {
        &self.shared
    }

    /// Pushes whole frames without blocking; returns frames pushed.
    pub(crate) fn push_frames(&mut self, samples: &[f32]) -> usize {
        let ch = self.format.channels;
        let n = (samples.len() / ch).min(self.free_frames());
        if n == 0 {
            return 0;
        }
        // Copies into the free region of the ring and publishes it with one atomic store.
        match self.producer.write_chunk_uninit(n * ch) {
            Ok(chunk) => chunk.fill_from_iter(samples[..n * ch].iter().copied()) / ch,
            Err(_) => 0,
        }
    }

}

impl std::fmt::Debug for OutputStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputStream")
            .field("device", &self.device_name)
            .field("format", &self.format)
            .field("stats", &self.stats())
            .finish()
    }
}

fn build_output(
    device: &cpal::Device,
    chosen: ChosenConfig,
    renderer: Renderer,
    sink: ErrorSink,
) -> Result<cpal::Stream> {
    let cfg = chosen.config;
    match chosen.sample_format {
        SampleFormat::F32 => build_output_typed::<f32>(device, cfg, renderer, sink),
        SampleFormat::F64 => build_output_typed::<f64>(device, cfg, renderer, sink),
        SampleFormat::I8 => build_output_typed::<i8>(device, cfg, renderer, sink),
        SampleFormat::I16 => build_output_typed::<i16>(device, cfg, renderer, sink),
        SampleFormat::I24 => build_output_typed::<cpal::I24>(device, cfg, renderer, sink),
        SampleFormat::I32 => build_output_typed::<i32>(device, cfg, renderer, sink),
        SampleFormat::U8 => build_output_typed::<u8>(device, cfg, renderer, sink),
        SampleFormat::U16 => build_output_typed::<u16>(device, cfg, renderer, sink),
        SampleFormat::U32 => build_output_typed::<u32>(device, cfg, renderer, sink),
        other => Err(Error::NoUsableConfig(format!("{} (sample type {other})", device_name(device)))),
    }
}

fn build_output_typed<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut renderer: Renderer,
    mut sink: ErrorSink,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    // Allocated here, before the stream starts, and moved into the callback: the callback
    // itself never allocates.
    let mut scratch = vec![0.0f32; SCRATCH_FRAMES * renderer.channels];
    Ok(device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            for chunk in data.chunks_mut(scratch.len()) {
                let s = &mut scratch[..chunk.len()];
                renderer.render(s);
                for (d, &x) in chunk.iter_mut().zip(s.iter()) {
                    *d = T::from_sample(x);
                }
            }
        },
        move |err| sink.report(err),
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_converts_and_counts_overruns() {
        let (mut input, mut cap, _sink) = InputStream::parts(AudioFormat::new(48_000, 2), 4);
        cap.capture::<i16>(&[16384, -16384, 0, 32767]);
        assert_eq!(input.available_frames(), 2);
        // 3 more frames but only 2 free: one frame dropped.
        cap.capture::<i16>(&[1, 2, 3, 4, 5, 6]);
        let st = input.stats();
        assert_eq!((st.overruns, st.dropped_frames, st.frames_captured), (1, 1, 5));
        let got = input.read(10);
        assert_eq!(got.len(), 8);
        assert_eq!(&got[..3], &[0.5, -0.5, 0.0]);
        assert!(input.read(10).is_empty());
    }

    #[test]
    fn renderer_buffers_plays_and_fades() {
        let fmt = AudioFormat::new(48_000, 1);
        let (mut out, mut r, _sink) = OutputStream::parts(fmt, 10_000, 1000);
        let mut buf = vec![1.0f32; 480];

        // Below the start threshold: silence, no underrun counted.
        assert_eq!(out.write(&vec![1.0; 600]).unwrap(), 600);
        r.render(&mut buf);
        assert!(buf.iter().all(|&s| s == 0.0));
        assert_eq!(out.state(), OutputState::Buffering);

        // Reach the threshold: plays with a fade-in, then full scale.
        out.write(&vec![1.0; 600]).unwrap();
        r.render(&mut buf);
        assert_eq!(out.state(), OutputState::Playing);
        assert!(buf[0] < 0.01);
        assert!((buf[FADE_IN_FRAMES + 1] - 1.0).abs() < 1e-6);
        for w in buf[..FADE_IN_FRAMES].windows(2) {
            assert!(w[1] >= w[0]);
        }
        r.render(&mut buf); // 1200 - 960 = 240 frames left
        assert_eq!(out.buffered_frames(), 240);

        // Run dry: 240 frames of audio, then a short decay to silence, one underrun.
        r.render(&mut buf);
        assert_eq!(out.stats().underruns, 1);
        assert_eq!(out.stats().underrun_frames, 240);
        assert_eq!(out.state(), OutputState::Buffering);
        assert!((buf[239] - 1.0).abs() < 1e-6);
        assert!(buf[240] < 1.0 && buf[240] > 0.9);
        assert!(buf[240 + FADE_OUT_FRAMES..].iter().all(|&s| s == 0.0));
        assert_eq!(out.stats().frames_played, 4 * 480);
    }

    #[test]
    fn volume_is_ramped_then_applied() {
        let fmt = AudioFormat::new(48_000, 1);
        let (mut out, mut r, _sink) = OutputStream::parts(fmt, 10_000, 0);
        out.write(&vec![1.0; 1000]).unwrap();
        let mut first = vec![0.0f32; FADE_IN_FRAMES + 44];
        r.render(&mut first);
        assert!(first[FADE_IN_FRAMES..].iter().all(|&s| s == 1.0), "unity by default, after the fade-in");
        let mut buf = vec![0.0f32; 100];
        assert_eq!(out.volume(), 1.0);
        out.set_volume(0.25);
        r.render(&mut buf);
        assert!((buf[0] - (1.0 - 0.75 / 100.0)).abs() < 1e-6, "ramp starts near the old gain: {}", buf[0]);
        assert!(buf.windows(2).all(|w| w[1] < w[0]), "falls smoothly");
        assert!((buf[99] - 0.25).abs() < 1e-6);
        r.render(&mut buf);
        assert!(buf.iter().all(|&s| (s - 0.25).abs() < 1e-6), "then flat at the new gain");
        out.set_volume(f32::NAN);
        assert_eq!(out.volume(), 1.0, "invalid values mean unity");
        out.set_volume(9.0);
        assert_eq!(out.volume(), 4.0);
    }

    #[test]
    fn write_rejects_partial_frames() {
        let (mut out, _r, _sink) = OutputStream::parts(AudioFormat::new(48_000, 2), 100, 0);
        assert!(out.write(&[0.0; 3]).is_err());
        assert_eq!(out.write(&[0.0; 400]).unwrap(), 100);
        assert_eq!(out.free_frames(), 0);
    }

    #[test]
    fn errors_are_queued_and_classified() {
        let (mut input, _cap, mut sink) = InputStream::parts(AudioFormat::new(8000, 1), 16);
        sink.report(cpal::Error::new(cpal::ErrorKind::Xrun));
        sink.report(cpal::Error::new(cpal::ErrorKind::DeviceNotAvailable));
        assert_eq!(input.stats().xruns, 1);
        assert!(input.is_dead());
        assert_eq!(input.take_errors().len(), 1);
        assert!(input.read_blocking(8, Duration::from_millis(10)).is_err());
    }
}
