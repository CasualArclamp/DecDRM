//! Web stream audio input: an internet radio stream (Icecast/SHOUTCAST over HTTP or
//! HTTPS, or a playlist pointing to one) decoded and fed to an audio service.
//!
//! ```text
//!  worker thread                                        station thread
//!  HTTP(S) ─▶ ICY metadata ─▶ framing/Ogg ─▶ decoder ─▶ channels, gain,
//!   ▲ redirects, playlists     │ titles                  resampler (trim) ─▶ FIFO ─▶ read()
//!   └── reconnect with backoff ◀── errors, end of stream          ▲ drift loop (sound card)
//! ```
//!
//! **Threads.** Network and decoding run on a worker thread that converts the audio to
//! the encoder's rate and channel count and appends it to a FIFO; the station's reads
//! only take from the FIFO and never wait long. When the FIFO is full the worker waits
//! (and TCP slows the server down), so a server that sends faster than real time — a
//! file rather than a live stream — loses nothing.
//!
//! **Clock.** A live stream runs on the broadcaster's clock.
//! * With a sound-card output the card paces the station, and the stream must be
//!   followed like a second sound card (the sound-card input's drift loop, reused): a
//!   PI loop trims the worker's resampler to hold the FIFO at a backlog target, 1.5 s
//!   for network jitter (with slower gains than for a sound card). The first read waits
//!   for the target plus the output's queue, as the sound-card input does, and drops the
//!   burst servers send on connecting beyond that. If the FIFO runs dry the source sends
//!   silence and re-buffers to the target; if it runs more than 2 s over the target (a
//!   stall of the station) it skips ahead to the target. A response with a
//!   `Content-Length` is a file, not a live stream: it has no clock to follow and is
//!   neither trimmed nor skipped.
//! * With a file output nothing else keeps time, so the stream paces the station: a
//!   read waits for the audio it needs (up to 3 s while connected, one read's duration
//!   while reconnecting, so that silence then runs in real time).
//!
//! **Robustness.** Connection loss, the end of the stream and undecodable data lead to
//! silence and a reconnection with backoff (1, 2, 4, 8, 15, 30 s; back to 1 s after a
//! connection that lasted a minute). Opening fails with a clear message when the first
//! connection does: a wrong URL, an HTTP error, an unsupported format, or no audio within
//! the open timeout.
//!
//! **Titles.** ICY `StreamTitle`s and the titles in Ogg/FLAC comments are queued with
//! the position of the audio they arrived with, and go on air when the station reads
//! that audio. With `stream_titles` the audio chain then puts the title first in the
//! service's text message cycle (`TextMessageEncoder::set_messages`), followed by the
//! configured messages.
//!
//! **Status.** [`WebStreamStatus`] (in `AudioStatus::web_stream`) and the log lines of
//! `Station::take_log` tell displays what happens.

mod decode;
mod framing;
mod http;
mod icy;
mod ogg;
mod playlist;
#[cfg(test)]
mod tests;

use crate::audio::{AudioSource, ClockFollower, DriftTuning, Fifo, map_channels};
use crate::station::StopHandle;
use decdrm_io::{Resampler, ResamplerQuality};
use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Longest `read()` wait for late audio while connected (file output).
const STALL_WAIT: Duration = Duration::from_secs(3);
/// Backlog beyond the target (sound-card output) at which a live stream skips ahead, s.
const MAX_EXCESS_S: f64 = 2.0;
/// Audio the FIFO holds before the worker waits, with a file output, s.
const FILE_OUTPUT_FIFO_S: f64 = 10.0;
/// Most log lines kept until [`AudioSource::take_log`].
const MAX_LOG: usize = 200;

/// State of a web stream input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WebStreamState {
    /// The first connection is being made.
    #[default]
    Connecting,
    /// Connected; waiting for enough audio (after connecting, or after the buffer ran
    /// dry).
    Buffering,
    /// Audio is flowing.
    Playing,
    /// The connection was lost; waiting to reconnect, or reconnecting.
    Reconnecting,
}

impl fmt::Display for WebStreamState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WebStreamState::Connecting => "connecting",
            WebStreamState::Buffering => "buffering",
            WebStreamState::Playing => "playing",
            WebStreamState::Reconnecting => "reconnecting",
        })
    }
}

/// Status of a web stream input, for displays (`AudioStatus::web_stream`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WebStreamStatus {
    /// The configured URL.
    pub url: String,
    /// The URL that delivers the audio (after playlists and redirects).
    pub stream_url: Option<String>,
    pub state: WebStreamState,
    /// The coding, e.g. `MP3`, `HE-AAC v2`, `Ogg Opus`.
    pub codec: Option<String>,
    /// Bit rate: as the server or the stream declares it, else measured, bit/s.
    pub bitrate: Option<u32>,
    /// Sampling rate of the decoded stream, Hz.
    pub sample_rate: Option<u32>,
    /// Channels of the decoded stream.
    pub channels: Option<u16>,
    /// The station's name (`icy-name`).
    pub station_name: Option<String>,
    /// The genre (`icy-genre`).
    pub genre: Option<String>,
    /// The title of what is being sent now ("now playing").
    pub title: Option<String>,
    /// Decoded audio waiting to be sent, s.
    pub buffer_s: f64,
    /// The backlog the clock-following holds (sound-card output); 0 with a file output.
    pub buffer_target_s: f64,
    /// Connections re-established after a loss.
    pub reconnects: u64,
    /// Times the audio ran out while playing (silence was sent).
    pub underruns: u64,
    /// Frames that could not be decoded (replaced by silence or skipped).
    pub decode_errors: u64,
    /// The last problem (connection error, unsupported data), if any.
    pub last_error: Option<String>,
}

impl WebStreamStatus {
    /// The stream, e.g. `"Radio X", MP3 128 kbit/s, 44.1 kHz stereo` (what is known).
    pub fn stream(&self) -> String {
        let mut parts = Vec::new();
        if let Some(name) = &self.station_name {
            parts.push(format!("\"{name}\""));
        }
        let mut coding = self.codec.clone().unwrap_or_default();
        if let Some(b) = self.bitrate {
            coding += &format!(" {:.0} kbit/s", f64::from(b) / 1000.0);
        }
        if !coding.trim().is_empty() {
            parts.push(coding.trim().to_string());
        }
        if let Some(r) = self.sample_rate {
            let khz = if r.is_multiple_of(1000) { format!("{}", r / 1000) } else { format!("{:.1}", f64::from(r) / 1000.0) };
            let ch = match self.channels {
                Some(1) => " mono",
                Some(2) => " stereo",
                _ => "",
            };
            parts.push(format!("{khz} kHz{ch}"));
        }
        parts.join(", ")
    }

    /// One line for status displays, e.g. `playing: "Radio X", MP3 128 kbit/s, 44.1 kHz
    /// stereo, 1.5 s buffered`.
    pub fn summary(&self) -> String {
        let mut parts = vec![self.stream()];
        if matches!(self.state, WebStreamState::Playing | WebStreamState::Buffering) {
            parts.push(format!("{:.1} s buffered", self.buffer_s));
        }
        if let (WebStreamState::Reconnecting, Some(e)) = (self.state, &self.last_error) {
            parts.push(e.clone());
        }
        parts.retain(|p| !p.is_empty());
        if parts.is_empty() { self.state.to_string() } else { format!("{}: {}", self.state, parts.join(", ")) }
    }
}

/// Settings of a web stream input.
#[derive(Clone)]
pub(crate) struct WebStreamOptions {
    pub url: String,
    /// The encoder's rate and channel count.
    pub out_rate: u32,
    pub out_channels: usize,
    pub gain: f32,
    /// Capacity of the sound-card output's queue when the station plays to a sound card
    /// (the stream's clock is then followed); `None`: the stream paces the station.
    pub output_queue: Option<Duration>,
    /// The station's stop flag (ends waits in `open` and `read`).
    pub stop: StopHandle,
    pub connect_timeout: Duration,
    /// Longest wait for a response or more data.
    pub io_timeout: Duration,
    /// Longest wait in `open` for the first audio.
    pub open_timeout: Duration,
    /// Waits before reconnecting; the last one repeats.
    pub backoff: Vec<Duration>,
    /// TLS configuration (tests: their own root certificate); default: the system's.
    pub tls: Option<Arc<rustls::ClientConfig>>,
    /// Backlog target with a sound-card output, s.
    pub buffer_s: f64,
}

impl WebStreamOptions {
    /// Defaults for `url` feeding an encoder at `out_rate` Hz with `out_channels`.
    pub fn new(url: &str, out_rate: u32, out_channels: usize) -> Self {
        WebStreamOptions {
            url: url.to_string(),
            out_rate,
            out_channels,
            gain: 1.0,
            output_queue: None,
            stop: StopHandle::default(),
            connect_timeout: Duration::from_secs(10),
            io_timeout: Duration::from_secs(15),
            open_timeout: Duration::from_secs(20),
            backoff: [1, 2, 4, 8, 15, 30].map(Duration::from_secs).to_vec(),
            tls: None,
            buffer_s: DriftTuning::NETWORK.target_s,
        }
    }

    /// Samples the FIFO holds before the worker waits: room for the start-up backlog
    /// and the excess a live stream may skip (sound-card output), or 10 s.
    fn fifo_capacity(&self) -> usize {
        let seconds = match self.output_queue {
            Some(q) => self.buffer_s + q.as_secs_f64() + 0.4 + MAX_EXCESS_S + 1.0,
            None => FILE_OUTPUT_FIFO_S,
        };
        (seconds * f64::from(self.out_rate)) as usize * self.out_channels
    }
}

/// Check a web stream URL without connecting (for validation).
pub(crate) fn check_url(url: &str) -> Result<(), String> {
    http::Url::parse(url).map(|_| ())
}

/// State shared by the worker and the source.
struct Shared {
    inner: Mutex<Inner>,
    changed: Condvar,
    /// Ask the worker to stop (also aborts its network reads).
    stop: Arc<AtomicBool>,
    /// Ratio trim for the worker's resampler, ppm (`f64` bits).
    trim_ppm: AtomicU64,
}

struct Inner {
    /// Audio at the encoder's rate and channel count.
    fifo: Fifo,
    /// Samples ever appended to / removed from the FIFO (positions for the titles).
    written: u64,
    consumed: u64,
    /// Title changes waiting for the audio they arrived with to be read.
    titles: VecDeque<(u64, Option<String>)>,
    /// The worker's part of the status (its `title` is the title on air).
    status: WebStreamStatus,
    /// The current response is a file (it has a `Content-Length`), not a live stream.
    on_demand: bool,
    log: Vec<String>,
    /// The first connection's outcome: audio arrived, or why not.
    first: Option<Result<(), String>>,
    /// The worker has ended.
    done: bool,
}

impl Inner {
    fn log(&mut self, line: String) {
        if self.log.len() >= MAX_LOG {
            self.log.remove(0);
        }
        self.log.push(format!("web stream: {line}"));
    }

    /// Move `n` samples to `out` (zero padded if fewer are there); returns how many
    /// were there.
    fn take(&mut self, n: usize, out: &mut Vec<f32>) -> usize {
        let k = n.min(self.fifo.len());
        self.fifo.take(n, out);
        self.consumed += k as u64;
        self.titles_on_air();
        k
    }

    /// Drop the oldest `n` samples.
    fn discard(&mut self, n: usize) {
        let k = n.min(self.fifo.len());
        self.fifo.discard(k);
        self.consumed += k as u64;
        self.titles_on_air();
    }

    /// Titles whose audio has now been read go on air.
    fn titles_on_air(&mut self) {
        while self.titles.front().is_some_and(|(at, _)| *at <= self.consumed) {
            let (_, title) = self.titles.pop_front().expect("checked");
            if title != self.status.title {
                if let Some(t) = &title {
                    self.log(format!("title: {t}"));
                }
                self.status.title = title;
            }
        }
    }
}

impl Shared {
    /// Rust note: a poisoned mutex (a panic while it was held) still holds usable data
    /// here; `into_inner` takes it instead of propagating the panic.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn log(&self, line: String) {
        self.lock().log(line);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Wait for a change (or `timeout`) with the lock held by `guard`.
    fn wait<'a>(&self, guard: MutexGuard<'a, Inner>, timeout: Duration) -> MutexGuard<'a, Inner> {
        match self.changed.wait_timeout(guard, timeout) {
            Ok((g, _)) => g,
            Err(poisoned) => poisoned.into_inner().0,
        }
    }
}

/// A web stream as the audio input of a service (see the module docs).
pub(crate) struct WebStreamSource {
    url: String,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    out_ch: usize,
    out_rate: u32,
    stop: StopHandle,
    /// Sound-card output: the clock follower.
    clock: Option<ClockFollower>,
    /// The first read has waited for the start-up backlog.
    primed: bool,
    /// Sound-card output: playing (else buffering, sending silence).
    playing: bool,
    /// File output: the last read came up short while connected.
    stalled: bool,
    underruns: u64,
}

impl WebStreamSource {
    /// Start the worker and wait until the stream delivers audio (or fails).
    pub fn open(opts: WebStreamOptions) -> Result<Self, String> {
        let url = http::Url::parse(&opts.url)?;
        let mut source = Self::new(&opts);
        let worker = {
            let shared = source.shared.clone();
            let opts = opts.clone();
            std::thread::Builder::new()
                .name("decdrm-webstream".into())
                .spawn(move || Worker::new(shared, opts, url).run())
                .map_err(|e| format!("cannot start the web stream thread: {e}"))?
        };
        source.worker = Some(worker);
        let deadline = Instant::now() + opts.open_timeout;
        let shared = source.shared.clone();
        let mut inner = shared.lock();
        let outcome = loop {
            if let Some(first) = inner.first.clone() {
                break first;
            }
            let now = Instant::now();
            if source.stop.is_stopped() {
                break Err("stopped while connecting".to_string());
            }
            if now >= deadline {
                break Err(format!("no audio from {} within {} s", source.url, opts.open_timeout.as_secs_f64()));
            }
            inner = shared.wait(inner, (deadline - now).min(Duration::from_millis(100)));
        };
        drop(inner);
        match outcome {
            Ok(()) => Ok(source),
            Err(e) => {
                source.shutdown();
                Err(e)
            }
        }
    }

    /// The source without its worker (`open` starts it; tests feed the FIFO).
    fn new(opts: &WebStreamOptions) -> Self {
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                fifo: Fifo::default(),
                written: 0,
                consumed: 0,
                titles: VecDeque::new(),
                status: WebStreamStatus { url: opts.url.clone(), ..Default::default() },
                on_demand: false,
                log: Vec::new(),
                first: None,
                done: false,
            }),
            changed: Condvar::new(),
            stop: Arc::new(AtomicBool::new(false)),
            trim_ppm: AtomicU64::new(0f64.to_bits()),
        });
        let clock = opts.output_queue.map(|q| ClockFollower::new(DriftTuning { target_s: opts.buffer_s, ..DriftTuning::NETWORK }, q));
        WebStreamSource {
            url: opts.url.clone(),
            shared,
            worker: None,
            out_ch: opts.out_channels,
            out_rate: opts.out_rate,
            stop: opts.stop.clone(),
            clock,
            primed: false,
            playing: false,
            stalled: false,
            underruns: 0,
        }
    }

    /// Stop the worker and wait (briefly) for it to end.
    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.changed.notify_all();
        let Some(worker) = self.worker.take() else { return };
        // The worker notices within ~0.1 s (its socket reads poll the flag); a name
        // lookup that hangs is abandoned rather than waited for.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut inner = self.shared.lock();
        while !inner.done && Instant::now() < deadline {
            inner = self.shared.wait(inner, Duration::from_millis(50));
        }
        let done = inner.done;
        drop(inner);
        if done {
            let _ = worker.join();
        }
    }

    fn samples_per_s(&self) -> f64 {
        f64::from(self.out_rate) * self.out_ch as f64
    }

    /// Whole frames in `samples`.
    fn frames(&self, samples: usize) -> usize {
        samples / self.out_ch * self.out_ch
    }

    /// The stream paces the station (file output): wait for the audio.
    fn read_paced(&mut self, need: usize, read_s: f64, out: &mut Vec<f32>) {
        let shared = self.shared.clone();
        let mut inner = shared.lock();
        let start = Instant::now();
        while inner.fifo.len() < need && !self.stop.is_stopped() && !inner.done {
            let limit = if inner.status.state == WebStreamState::Playing { STALL_WAIT } else { Duration::from_secs_f64(read_s) };
            let elapsed = start.elapsed();
            if elapsed >= limit {
                break;
            }
            inner = shared.wait(inner, (limit - elapsed).min(Duration::from_millis(100)));
        }
        let have = inner.take(need, out);
        // A stall is counted and logged once, when it begins.
        let stalled = have < need && inner.status.state == WebStreamState::Playing;
        if stalled && !self.stalled {
            self.underruns += 1;
            inner.log("the stream stalled: silence until it delivers again".to_string());
        }
        self.stalled = stalled;
        drop(inner);
        shared.changed.notify_all();
    }

    /// A sound card paces the station: follow the stream's clock.
    fn read_following(&mut self, need: usize, read_s: f64, out: &mut Vec<f32>) {
        let shared = self.shared.clone();
        let per_s = self.samples_per_s();
        let mut inner = shared.lock();
        let Some(target) = self.clock.as_ref().map(ClockFollower::target_s) else { return };
        if !self.primed {
            // The start-up backlog (see `DeviceSource`): target + output queue + this read.
            let want_s = self.clock.as_ref().map_or(0.0, |c| c.start_backlog_s(read_s));
            let want = self.frames((want_s * per_s).round() as usize);
            let deadline = Instant::now() + Duration::from_secs_f64(want_s + 5.0);
            while inner.fifo.len() < want && !self.stop.is_stopped() && !inner.done && Instant::now() < deadline {
                inner = shared.wait(inner, Duration::from_millis(50));
            }
            if !inner.on_demand {
                // The connection burst beyond the start-up backlog.
                let excess = self.frames(inner.fifo.len().saturating_sub(want));
                inner.discard(excess);
            }
            self.primed = true;
            if let Some(clock) = self.clock.as_mut() {
                clock.started(read_s);
            }
            self.playing = inner.fifo.len() >= need;
        }
        let backlog = |inner: &Inner| inner.fifo.len() as f64 / per_s;
        if !self.playing && backlog(&inner) >= target {
            self.playing = true;
            if let Some(clock) = self.clock.as_mut() {
                clock.resume();
            }
            let level = backlog(&inner);
            inner.log(format!("{level:.1} s buffered, playing"));
        }
        if self.playing {
            let have = inner.take(need, out);
            if have < need {
                self.playing = false;
                self.underruns += 1;
                let missing = (need - have) as f64 / per_s;
                inner.log(format!("the buffer ran dry ({missing:.2} s missing); re-buffering"));
            }
        } else {
            out.resize(out.len() + need, 0.0);
        }
        let excess_s = backlog(&inner) - target;
        if excess_s > MAX_EXCESS_S && !inner.on_demand {
            inner.discard(self.frames((excess_s * per_s).round() as usize));
            inner.log(format!("skipped {excess_s:.1} s of buffered audio to stay {target:.1} s behind the stream"));
        }
        if self.playing {
            // A file has no clock: no trim.
            let measured = if inner.on_demand { None } else { Some(backlog(&inner)) };
            if let (Some(level), Some(clock)) = (measured, self.clock.as_mut())
                && let Some(ppm) = clock.update(level, read_s)
            {
                shared.trim_ppm.store(ppm.to_bits(), Ordering::Relaxed);
            }
        }
        drop(inner);
        shared.changed.notify_all();
    }

    /// The status (worker part plus the buffer and clock).
    pub fn status(&self) -> WebStreamStatus {
        let inner = self.shared.lock();
        let mut s = inner.status.clone();
        s.buffer_s = inner.fifo.len() as f64 / self.samples_per_s();
        s.underruns = self.underruns;
        if let Some(clock) = &self.clock {
            s.buffer_target_s = clock.target_s();
            if s.state == WebStreamState::Playing && self.primed && !self.playing {
                s.state = WebStreamState::Buffering;
            }
        }
        s
    }
}

impl Drop for WebStreamSource {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl AudioSource for WebStreamSource {
    fn read(&mut self, frames: usize, out: &mut Vec<f32>) -> std::result::Result<(), String> {
        let need = frames * self.out_ch;
        let read_s = frames as f64 / f64::from(self.out_rate);
        if self.clock.is_some() {
            self.read_following(need, read_s, out);
        } else {
            self.read_paced(need, read_s, out);
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("{} ({})", self.url, self.status().summary())
    }

    fn drift_ppm(&self) -> Option<f64> {
        self.clock.as_ref().map(ClockFollower::ppm)
    }

    fn title(&self) -> Option<String> {
        self.shared.lock().status.title.clone()
    }

    fn web_stream(&self) -> Option<WebStreamStatus> {
        Some(self.status())
    }

    fn take_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.shared.lock().log)
    }
}

// ---------------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------------

/// How a connection ended.
enum Outcome {
    /// The server ended the stream.
    Ended,
    Failed(String),
    Stopped,
}

/// The worker thread: connections, decoding, conversion to the encoder's format.
struct Worker {
    shared: Arc<Shared>,
    opts: WebStreamOptions,
    url: http::Url,
    /// Resample even at equal rates (the trim follows the stream's clock).
    follow_clock: bool,
    capacity: usize,
    resampler: Option<(u32, Resampler)>,
    applied_ppm: f64,
    mapped: Vec<f32>,
    resampled: Vec<f32>,
}

impl Worker {
    fn new(shared: Arc<Shared>, opts: WebStreamOptions, url: http::Url) -> Self {
        Worker {
            follow_clock: opts.output_queue.is_some(),
            capacity: opts.fifo_capacity(),
            shared,
            opts,
            url,
            resampler: None,
            applied_ppm: 0.0,
            mapped: Vec::new(),
            resampled: Vec::new(),
        }
    }

    fn run(mut self) {
        let mut attempt = 0usize;
        while !self.shared.stopped() {
            {
                let mut inner = self.shared.lock();
                inner.status.state = if inner.first.is_none() { WebStreamState::Connecting } else { WebStreamState::Reconnecting };
            }
            self.shared.changed.notify_all();
            let started = Instant::now();
            let outcome = self.connection();
            if self.shared.stopped() {
                break;
            }
            let (message, ended) = match outcome {
                Outcome::Stopped => break,
                Outcome::Ended => ("the server ended the stream".to_string(), true),
                Outcome::Failed(e) => (e, false),
            };
            {
                let mut inner = self.shared.lock();
                inner.status.state = WebStreamState::Reconnecting;
                inner.status.last_error = Some(message.clone());
                if inner.first.is_none() {
                    // The first connection brought no audio: `open` reports why.
                    let why = if ended { "the server ended the stream before any audio arrived".to_string() } else { message };
                    inner.first = Some(Err(why));
                    break;
                }
            }
            if started.elapsed() >= Duration::from_secs(60) {
                attempt = 0;
            }
            let wait = self.opts.backoff.get(attempt).or(self.opts.backoff.last()).copied().unwrap_or(Duration::from_secs(1));
            attempt += 1;
            self.shared.log(format!("{message}; reconnecting in {:.1} s", wait.as_secs_f64()));
            let until = Instant::now() + wait;
            while Instant::now() < until && !self.shared.stopped() {
                std::thread::sleep((until - Instant::now()).min(Duration::from_millis(50)));
            }
        }
        self.shared.lock().done = true;
        self.shared.changed.notify_all();
    }

    /// One connection: resolve playlists, detect the format, decode until the stream
    /// ends or fails.
    fn connection(&mut self) -> Outcome {
        let req = http::Request {
            stop: self.shared.stop.clone(),
            tls: self.opts.tls.clone(),
            icy_metadata: true,
            connect_timeout: self.opts.connect_timeout,
            io_timeout: self.opts.io_timeout,
        };
        let mut target = self.url.clone();
        for depth in 0.. {
            if depth > 3 {
                return Outcome::Failed("playlists nested more than three deep".into());
            }
            let resp = match http::get(&target, &req) {
                Ok(r) => r,
                Err(http::HttpError::Stopped) => return Outcome::Stopped,
                Err(e) => return Outcome::Failed(e.to_string()),
            };
            if resp.url != target {
                self.shared.log(format!("redirected to {}", resp.url));
            }
            let header = |name: &str| resp.header(name).map(str::to_string);
            let content_type = header("content-type");
            let metaint = header("icy-metaint").and_then(|v| v.trim().parse().ok());
            let (name, genre, bitrate) = (header("icy-name"), header("icy-genre"), declared_bitrate(&resp));
            let on_demand = resp.header("content-length").is_some();
            let url = resp.url.clone();
            let mut input = icy::StreamInput::new(resp.body, metaint);
            let class = match input.peek(16 * 1024, |h| decode::sniff(h).is_some()) {
                Ok(head) => decode::classify(content_type.as_deref(), &url.path, head),
                Err(e) if http::is_stopped(&e) => return Outcome::Stopped,
                Err(e) => return Outcome::Failed(format!("{url}: {}", http::describe_io(&e))),
            };
            match class {
                decode::Class::Playlist => {
                    let text = match input.read_text(256 * 1024) {
                        Ok(t) => t,
                        Err(e) => return Outcome::Failed(format!("{url}: {}", http::describe_io(&e))),
                    };
                    let entry = match playlist::entries(&text) {
                        Ok(list) => list[0].clone(),
                        Err(e) => return Outcome::Failed(format!("{url}: {e}")),
                    };
                    target = match url.join(&entry) {
                        Ok(u) => u,
                        Err(e) => return Outcome::Failed(format!("{url}: playlist entry {e}")),
                    };
                    self.shared.log(format!("playlist {url} lists {target}"));
                }
                decode::Class::Rejected(why) => return Outcome::Failed(format!("{url}: {why}")),
                decode::Class::Audio(format) => {
                    {
                        let mut inner = self.shared.lock();
                        inner.on_demand = on_demand;
                        let s = &mut inner.status;
                        s.stream_url = Some(url.to_string());
                        s.station_name = name.filter(|n| !n.trim().is_empty());
                        s.genre = genre.filter(|g| !g.trim().is_empty());
                        s.bitrate = bitrate;
                        s.state = WebStreamState::Buffering;
                    }
                    if on_demand {
                        self.shared.log(format!("{url} is a file (it has a length), not a live stream"));
                    }
                    return self.decode(input, decode::open(format), bitrate);
                }
            }
        }
        unreachable!("the loop returns")
    }

    /// Decode a connected stream until it ends or fails.
    fn decode(&mut self, mut input: icy::StreamInput, mut decoder: Box<dyn decode::StreamDecoder>, declared: Option<u32>) -> Outcome {
        let mut decoded_s = 0.0;
        let mut announced = false;
        let mut reported_errors = 0;
        loop {
            if self.shared.stopped() {
                return Outcome::Stopped;
            }
            let block = match decoder.next(&mut input) {
                Ok(Some(b)) => b,
                Ok(None) => return Outcome::Ended,
                Err(decode::DecodeError::Io(e)) if http::is_stopped(&e) => return Outcome::Stopped,
                Err(decode::DecodeError::Io(e)) => return Outcome::Failed(format!("connection lost: {}", http::describe_io(&e))),
                Err(decode::DecodeError::Fatal(e)) => return Outcome::Failed(e),
            };
            if block.channels == 0 || block.rate == 0 || block.samples.is_empty() {
                continue;
            }
            decoded_s += (block.samples.len() / block.channels) as f64 / f64::from(block.rate);
            // Titles found while reading this block apply from its start.
            let titles: Vec<Option<String>> = [input.take_title(), decoder.take_title()]
                .into_iter()
                .flatten()
                .map(|t| Some(t.trim().to_string()).filter(|t| !t.is_empty()))
                .collect();
            {
                let mut inner = self.shared.lock();
                let s = &mut inner.status;
                s.codec = Some(decoder.codec());
                s.sample_rate = Some(block.rate);
                s.channels = Some(decoder.channels().unwrap_or(block.channels as u16));
                s.decode_errors = decoder.errors();
                s.bitrate = declared.or(decoder.nominal_bitrate()).or_else(|| {
                    (decoded_s >= 2.0).then(|| (input.bytes as f64 * 8.0 / decoded_s).round() as u32)
                });
                if !announced {
                    announced = true;
                    s.state = WebStreamState::Playing;
                    s.last_error = None;
                    let what = if inner.first.is_some() {
                        inner.status.reconnects += 1;
                        "reconnected to"
                    } else {
                        "connected to"
                    };
                    let s = &inner.status;
                    let line = format!("{what} {}: {}", s.stream_url.as_deref().unwrap_or_default(), s.stream());
                    inner.log(line);
                }
                let errors = inner.status.decode_errors;
                if errors >= reported_errors + 50 {
                    inner.log(format!("{errors} undecodable frames so far"));
                    reported_errors = errors;
                }
            }
            if !self.push(&block, titles) {
                return Outcome::Stopped;
            }
        }
    }

    /// Convert a block to the encoder's channels and rate and append it to the FIFO,
    /// waiting while the FIFO is full. False if the worker was stopped meanwhile.
    fn push(&mut self, block: &decode::Decoded, titles: Vec<Option<String>>) -> bool {
        self.mapped.clear();
        map_channels(&block.samples, block.channels, self.opts.out_channels, self.opts.gain, &mut self.mapped);
        let resample = self.follow_clock || block.rate != self.opts.out_rate;
        let data: &[f32] = if resample {
            if self.resampler.as_ref().is_none_or(|(rate, _)| *rate != block.rate) {
                match Resampler::new(block.rate, self.opts.out_rate, self.opts.out_channels, ResamplerQuality::Balanced) {
                    Ok(r) => {
                        self.resampler = Some((block.rate, r));
                        self.applied_ppm = 0.0;
                    }
                    Err(e) => {
                        self.shared.log(format!("cannot resample {} Hz: {e}", block.rate));
                        return true;
                    }
                }
            }
            let (_, r) = self.resampler.as_mut().expect("created above");
            let ppm = f64::from_bits(self.shared.trim_ppm.load(Ordering::Relaxed));
            if ppm != self.applied_ppm && r.set_ratio_adjust_ppm(ppm).is_ok() {
                self.applied_ppm = ppm;
            }
            self.resampled.clear();
            r.process_into(&self.mapped, &mut self.resampled);
            &self.resampled
        } else {
            &self.mapped
        };
        let mut inner = self.shared.lock();
        while inner.fifo.len() >= self.capacity && !self.shared.stopped() {
            inner = self.shared.wait(inner, Duration::from_millis(50));
        }
        if self.shared.stopped() {
            return false;
        }
        let at = inner.written;
        inner.titles.extend(titles.into_iter().map(|t| (at, t)));
        inner.fifo.buf.extend_from_slice(data);
        inner.written += data.len() as u64;
        if inner.first.is_none() {
            inner.first = Some(Ok(()));
        }
        drop(inner);
        self.shared.changed.notify_all();
        true
    }
}

/// The bit rate the server declares: `icy-br` (kbit/s, sometimes "128,128") or
/// `ice-audio-info` (`ice-bitrate=128;…` or `bitrate=128;…`).
fn declared_bitrate(resp: &http::Response) -> Option<u32> {
    let kbps = |v: &str| v.split(',').next().and_then(|n| n.trim().parse::<u32>().ok()).filter(|&k| k > 0 && k < 10_000);
    if let Some(k) = resp.header("icy-br").and_then(kbps) {
        return Some(k * 1000);
    }
    let info = resp.header("ice-audio-info")?;
    info.split(';')
        .find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            matches!(k.trim(), "ice-bitrate" | "bitrate").then(|| kbps(v)).flatten()
        })
        .map(|k| k * 1000)
}
