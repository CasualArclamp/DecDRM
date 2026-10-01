//! The connection: a thread that holds the WebSocket to the KiwiSDR, sets up its
//! receiver, and fills a FIFO with I/Q samples for [`KiwiStream::read_blocking`].
//!
//! Reconnecting (with backoff, [`KiwiConfig::reconnect_delay`] doubling up to 30 s)
//! only follows a connection that was lost after streaming: a Kiwi that refuses the
//! connection (all channels busy, wrong password, down), closes it cleanly (which is
//! how time limits end a session) or never streamed is not asked again.

use crate::address::KiwiAddress;
use crate::protocol::{self, Agc, KiwiMsg, SndBlock, Tuning};
use std::collections::VecDeque;
use std::fmt;
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::handshake::HandshakeError;
use tungstenite::{Message, WebSocket};

/// Time allowed to open the TCP connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A connection that delivers nothing for this long is taken as lost.
pub const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// At most this much signal is kept for a reader that falls behind (oldest dropped).
pub const MAX_BUFFER_S: f64 = 30.0;
/// Longest wait between reconnection attempts.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
/// A session that streamed this long resets the count of reconnections in a row.
const STABLE_SESSION: Duration = Duration::from_secs(60);

/// What to connect to and how to tune it.
#[derive(Debug, Clone, PartialEq)]
pub struct KiwiConfig {
    pub address: KiwiAddress,
    /// Password of a KiwiSDR whose channels need one (empty: none).
    pub password: String,
    /// Time-limit exemption password (empty: none).
    pub tlimit_password: String,
    /// Frequency to tune, kHz: the DRM frequency (the DC carrier).
    pub freq_khz: f64,
    /// I/Q passband relative to the tuned frequency, Hz.
    pub low_cut_hz: i32,
    pub high_cut_hz: i32,
    pub agc: Agc,
    /// Name in the KiwiSDR's user list.
    pub ident: String,
    /// First wait before reconnecting after a lost connection (doubles each time).
    pub reconnect_delay: Duration,
    /// Give up after this many reconnections in a row.
    pub max_reconnects: u32,
}

impl KiwiConfig {
    /// Tune `address` to `freq_khz` with a ±5 kHz I/Q passband (a 10 kHz DRM channel
    /// around its DC carrier, or a 4.5/5 kHz one above it), the Kiwi's AGC, as "DecDRM".
    pub fn new(address: KiwiAddress, freq_khz: f64) -> Self {
        Self {
            address,
            password: String::new(),
            tlimit_password: String::new(),
            freq_khz,
            low_cut_hz: -5000,
            high_cut_hz: 5000,
            agc: Agc::On,
            ident: "DecDRM".into(),
            reconnect_delay: Duration::from_secs(2),
            max_reconnects: 10,
        }
    }

    fn tuning(&self) -> Tuning {
        Tuning {
            freq_khz: self.freq_khz,
            low_cut_hz: self.low_cut_hz,
            high_cut_hz: self.high_cut_hz,
            agc: self.agc,
            ident: self.ident.clone(),
        }
    }
}

/// Where the connection stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KiwiState {
    #[default]
    Connecting,
    Streaming,
    /// Waiting to reconnect after the connection was lost.
    Reconnecting,
    /// Ended with an error ([`KiwiStatus::error`]).
    Failed,
    /// Ended because it was asked to.
    Stopped,
}

impl fmt::Display for KiwiState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            KiwiState::Connecting => "connecting",
            KiwiState::Streaming => "streaming",
            KiwiState::Reconnecting => "reconnecting",
            KiwiState::Failed => "failed",
            KiwiState::Stopped => "stopped",
        })
    }
}

/// What the connection reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KiwiStatus {
    pub state: KiwiState,
    /// `host:port` of the KiwiSDR in use (after redirections).
    pub address: String,
    pub freq_khz: f64,
    /// Receiver name and location from the Kiwi's configuration.
    pub name: Option<String>,
    pub location: Option<String>,
    /// Firmware version, e.g. "1.826".
    pub version: Option<String>,
    /// Exact sample rate the Kiwi reported, Hz (e.g. 12001.135).
    pub sample_rate: Option<f64>,
    /// S-meter of the passband, dBm.
    pub rssi_dbm: Option<f32>,
    /// Blocks in which the Kiwi's ADC overflowed.
    pub adc_overflows: u64,
    pub reconnects: u32,
    /// I/Q samples delivered so far.
    pub samples: u64,
    /// Samples dropped because the reader fell more than [`MAX_BUFFER_S`] behind.
    pub dropped: u64,
    /// Why it failed, or the last lost connection's reason while reconnecting.
    pub error: Option<String>,
}

/// Why the stream ended for good.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct KiwiError(pub String);

struct Inner {
    /// Interleaved I/Q at the Kiwi's sample rate.
    fifo: VecDeque<f32>,
    status: KiwiStatus,
    log: Vec<String>,
}

struct Shared {
    inner: Mutex<Inner>,
    data: Condvar,
    stop: AtomicBool,
    /// A handle of the current TCP connection, to unblock a read when stopping.
    socket: Mutex<Option<TcpStream>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock cannot leave the FIFO inconsistent in a way
        // that matters (it only drops samples), so recover from poisoning.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn log(&self, line: String) {
        self.lock().log.push(line);
    }

    fn set_state(&self, state: KiwiState) {
        self.lock().status.state = state;
        self.data.notify_all();
    }

    fn end(&self, state: KiwiState, error: Option<String>) {
        let mut i = self.lock();
        i.status.state = state;
        if let Some(e) = error {
            i.log.push(format!("KiwiSDR: {e}"));
            i.status.error = Some(e);
        }
        drop(i);
        self.data.notify_all();
    }

    fn push(&self, block: &SndBlock) {
        let mut i = self.lock();
        i.fifo.extend(block.samples.iter().copied());
        let cap = (2.0 * MAX_BUFFER_S * i.status.sample_rate.unwrap_or(12_000.0)) as usize;
        if i.fifo.len() > cap {
            let excess = (i.fifo.len() - cap) & !1;
            i.fifo.drain(..excess);
            i.status.dropped += (excess / 2) as u64;
        }
        i.status.rssi_dbm = Some(block.rssi_dbm);
        i.status.adc_overflows += u64::from(block.adc_overflow());
        i.status.samples += (block.samples.len() / 2) as u64;
        drop(i);
        self.data.notify_all();
    }
}

/// A running KiwiSDR connection. Dropping it stops the connection (without waiting).
pub struct KiwiStream {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl KiwiStream {
    /// Connect in the background; samples arrive once the Kiwi streams.
    pub fn start(cfg: KiwiConfig) -> Self {
        let status = KiwiStatus { address: cfg.address.to_string(), freq_khz: cfg.freq_khz, ..KiwiStatus::default() };
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner { fifo: VecDeque::new(), status, log: Vec::new() }),
            data: Condvar::new(),
            stop: AtomicBool::new(false),
            socket: Mutex::new(None),
        });
        let worker = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("decdrm-kiwi".into())
            .spawn(move || run(&cfg, &worker))
            .expect("spawn KiwiSDR thread");
        Self { shared, handle: Some(handle) }
    }

    pub fn status(&self) -> KiwiStatus {
        self.shared.lock().status.clone()
    }

    /// The Kiwi's exact sample rate, once it has reported it.
    pub fn sample_rate(&self) -> Option<f64> {
        self.shared.lock().status.sample_rate
    }

    /// Connection events since the last call (for the receiver's log).
    pub fn take_log(&self) -> Vec<String> {
        std::mem::take(&mut self.shared.lock().log)
    }

    /// Up to `max_frames` I/Q frames (interleaved I, Q at [`Self::sample_rate`]),
    /// waiting up to `timeout` for the first. An empty vector means nothing arrived in
    /// time; an error, that the stream has ended and everything was read.
    pub fn read_blocking(&self, max_frames: usize, timeout: Duration) -> Result<Vec<f32>, KiwiError> {
        let deadline = Instant::now() + timeout;
        let mut i = self.shared.lock();
        loop {
            if !i.fifo.is_empty() {
                let n = (2 * max_frames.max(1)).min(i.fifo.len()) & !1;
                return Ok(i.fifo.drain(..n).collect());
            }
            match i.status.state {
                KiwiState::Failed => return Err(KiwiError(i.status.error.clone().unwrap_or_else(|| "KiwiSDR connection failed".into()))),
                KiwiState::Stopped => return Err(KiwiError("KiwiSDR connection stopped".into())),
                _ => {}
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(Vec::new());
            }
            i = self.shared.data.wait_timeout(i, deadline - now).unwrap_or_else(|p| p.into_inner()).0;
        }
    }

    /// Ask the connection to end: it closes the WebSocket and the thread exits soon
    /// (at once when waiting for data; a connection attempt in progress finishes first).
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(s) = self.shared.socket.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            let _ = s.shutdown(Shutdown::Both);
        }
        self.shared.data.notify_all();
    }

    /// Stop and wait for the thread (tests, orderly shutdown).
    pub fn stop_and_join(mut self) {
        self.stop();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for KiwiStream {
    fn drop(&mut self) {
        self.stop();
    }
}

/// How one connection ended.
enum Outcome {
    Stopped,
    /// Go to another address (HTTP redirection or the Kiwi's `redirect`).
    Redirect(String),
    /// Refused or unusable: do not try again.
    Fatal(String),
    /// Closed by the Kiwi: do not try again.
    Closed(String),
    /// Lost (network error, timeout); `streamed` if samples had arrived.
    Lost { error: String, streamed: bool },
}

fn run(cfg: &KiwiConfig, shared: &Shared) {
    let mut address = cfg.address.clone();
    let mut redirects = 0;
    let mut losses = 0u32;
    let mut ever_streamed = false;
    loop {
        if shared.stopped() {
            shared.end(KiwiState::Stopped, None);
            return;
        }
        shared.set_state(if ever_streamed { KiwiState::Reconnecting } else { KiwiState::Connecting });
        shared.log(format!("KiwiSDR: connecting to {address}"));
        let started = Instant::now();
        match session(&address, cfg, shared) {
            Outcome::Stopped => {
                shared.end(KiwiState::Stopped, None);
                return;
            }
            Outcome::Redirect(target) => match KiwiAddress::parse(&target) {
                Ok(next) if redirects < 3 => {
                    shared.log(format!("KiwiSDR: redirected to {next}"));
                    shared.lock().status.address = next.to_string();
                    address = next;
                    redirects += 1;
                }
                Ok(_) => return shared.end(KiwiState::Failed, Some("too many redirections".into())),
                Err(e) => return shared.end(KiwiState::Failed, Some(format!("redirected to an unusable address \"{target}\": {e}"))),
            },
            Outcome::Fatal(e) | Outcome::Closed(e) => return shared.end(KiwiState::Failed, Some(e)),
            Outcome::Lost { error, streamed } => {
                ever_streamed |= streamed;
                if !ever_streamed {
                    return shared.end(KiwiState::Failed, Some(error));
                }
                if streamed && started.elapsed() >= STABLE_SESSION {
                    losses = 0;
                }
                losses += 1;
                if losses > cfg.max_reconnects {
                    return shared.end(KiwiState::Failed, Some(format!("{error}; gave up after {} reconnection attempts", cfg.max_reconnects)));
                }
                let delay = cfg.reconnect_delay.saturating_mul(1 << (losses - 1).min(16)).min(MAX_RECONNECT_DELAY);
                {
                    let mut i = shared.lock();
                    i.status.reconnects += 1;
                    i.status.state = KiwiState::Reconnecting;
                    i.status.error = Some(error.clone());
                    i.log.push(format!("KiwiSDR: connection lost ({error}); reconnecting in {:.0} s", delay.as_secs_f64()));
                }
                let until = Instant::now() + delay;
                while Instant::now() < until && !shared.stopped() {
                    std::thread::sleep(Duration::from_millis(50).min(until.saturating_duration_since(Instant::now())));
                }
            }
        }
    }
}

/// The value the reference client puts in the WebSocket path.
fn timestamp() -> u32 {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    (secs.wrapping_add(u64::from(std::process::id())) & 0xFFFF_FFFF) as u32
}

fn connect(address: &KiwiAddress) -> Result<TcpStream, String> {
    let addrs: Vec<_> = (address.host.as_str(), address.port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot find {}: {e}", address.host))?
        .collect();
    let mut last = format!("no address for {}", address.host);
    for a in addrs {
        match TcpStream::connect_timeout(&a, CONNECT_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => last = format!("cannot connect to {address}: {e}"),
        }
    }
    Err(last)
}

fn session(address: &KiwiAddress, cfg: &KiwiConfig, shared: &Shared) -> Outcome {
    let stream = match connect(address) {
        Ok(s) => s,
        Err(error) => return if shared.stopped() { Outcome::Stopped } else { Outcome::Lost { error, streamed: false } },
    };
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
    if let Ok(handle) = stream.try_clone() {
        *shared.socket.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
    }
    if shared.stopped() {
        return Outcome::Stopped;
    }
    let url = format!("ws://{address}/{}/SND", timestamp());
    let ws = match tungstenite::client(url.as_str(), stream) {
        Ok((ws, _)) => ws,
        Err(HandshakeError::Failure(tungstenite::Error::Http(response))) => {
            let code = response.status().as_u16();
            let location = response.headers().get("location").and_then(|v| v.to_str().ok());
            return match (code, location) {
                (301 | 302 | 307 | 308, Some(l)) => Outcome::Redirect(l.to_string()),
                (404, _) if address.is_proxied() => Outcome::Fatal("this KiwiSDR is not online (the kiwisdr.com proxy has no connection to it)".into()),
                _ => Outcome::Fatal(format!("{address} answered HTTP {code} instead of opening a WebSocket: is it a KiwiSDR?")),
            };
        }
        Err(e) => {
            return if shared.stopped() {
                Outcome::Stopped
            } else {
                Outcome::Lost { error: format!("WebSocket handshake with {address} failed: {}", describe(&e.to_string())), streamed: false }
            };
        }
    };
    let mut conn = Connection { ws, streamed: false };
    conn.run(cfg, shared)
}

/// Shorter wording for the common I/O errors.
fn describe(e: &str) -> String {
    let l = e.to_ascii_lowercase();
    if l.contains("timed out") || l.contains("10060") || l.contains("would block") || l.contains("10035") {
        format!("no data for {} s", READ_TIMEOUT.as_secs())
    } else {
        e.to_string()
    }
}

/// One open WebSocket to a Kiwi.
struct Connection {
    ws: WebSocket<TcpStream>,
    streamed: bool,
}

impl Connection {
    fn lost(&self, shared: &Shared, e: &tungstenite::Error) -> Outcome {
        if shared.stopped() {
            return Outcome::Stopped;
        }
        match e {
            tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
                Outcome::Closed("the KiwiSDR closed the connection (its time limit, or the owner's choice)".into())
            }
            _ => Outcome::Lost { error: describe(&e.to_string()), streamed: self.streamed },
        }
    }

    fn send(&mut self, shared: &Shared, text: &str) -> Result<(), Outcome> {
        self.ws.send(Message::text(text)).map_err(|e| self.lost(shared, &e))
    }

    fn run(&mut self, cfg: &KiwiConfig, shared: &Shared) -> Outcome {
        match self.exchange(cfg, shared) {
            Ok(never) => match never {},
            Err(outcome) => {
                if matches!(outcome, Outcome::Stopped) {
                    let _ = self.ws.close(None);
                    let _ = self.ws.flush();
                }
                outcome
            }
        }
    }

    /// The message loop; it only returns how it ended (`Ok` is uninhabited).
    fn exchange(&mut self, cfg: &KiwiConfig, shared: &Shared) -> Result<std::convert::Infallible, Outcome> {
        self.send(shared, &protocol::auth(&cfg.password, &cfg.tlimit_password))?;
        let tuning = cfg.tuning();
        let mut freq_offset = 0.0;
        let mut set_up = false;
        let mut first_block = true;
        let mut version: (Option<u32>, Option<u32>) = (None, None);
        let mut last_keepalive = Instant::now();
        loop {
            if shared.stopped() {
                return Err(Outcome::Stopped);
            }
            let message = self.ws.read().map_err(|e| self.lost(shared, &e))?;
            let data: &[u8] = match &message {
                Message::Binary(b) => b,
                Message::Text(t) => t.as_bytes(),
                Message::Close(frame) => {
                    let why = frame.as_ref().map(|f| f.reason.to_string()).filter(|r| !r.is_empty());
                    return Err(Outcome::Closed(match why {
                        Some(r) => format!("the KiwiSDR closed the connection: {r}"),
                        None => "the KiwiSDR closed the connection (its time limit, or the owner's choice)".into(),
                    }));
                }
                _ => continue,
            };
            let Some((tag, body)) = protocol::split_tag(data) else { continue };
            match tag {
                "MSG" => {
                    let text = String::from_utf8_lossy(body).into_owned();
                    for (name, value) in protocol::parse_msg(&text) {
                        match protocol::interpret(name, value) {
                            KiwiMsg::AudioRate(r) => self.send(shared, &protocol::ar_ok(r))?,
                            KiwiMsg::SampleRate(rate) => {
                                shared.lock().status.sample_rate = Some(rate);
                                if !set_up {
                                    for command in protocol::setup(&tuning, freq_offset) {
                                        self.send(shared, &command)?;
                                    }
                                    set_up = true;
                                    last_keepalive = Instant::now();
                                    shared.log(format!(
                                        "KiwiSDR: tuned to {:.3} kHz, I/Q {:+.1} … {:+.1} kHz at {rate:.3} Hz",
                                        cfg.freq_khz,
                                        f64::from(cfg.low_cut_hz) / 1e3,
                                        f64::from(cfg.high_cut_hz) / 1e3
                                    ));
                                }
                            }
                            KiwiMsg::TooBusy(n) => {
                                return Err(Outcome::Fatal(format!("all {n} channels of this KiwiSDR are in use; try again later or choose another KiwiSDR")));
                            }
                            KiwiMsg::BadPassword(code) => {
                                return Err(Outcome::Fatal(format!("the KiwiSDR refused the connection: {}", protocol::bad_password_text(code))));
                            }
                            KiwiMsg::Down => return Err(Outcome::Fatal("the KiwiSDR is down at the moment".into())),
                            KiwiMsg::Redirect(url) => return Err(Outcome::Redirect(url)),
                            KiwiMsg::VersionMajor(v) => version.0 = Some(v),
                            KiwiMsg::VersionMinor(v) => version.1 = Some(v),
                            KiwiMsg::FreqOffset(f) => freq_offset = f,
                            KiwiMsg::Config { name, location } => {
                                let line = match (&name, &location) {
                                    (Some(n), Some(l)) => Some(format!("KiwiSDR: \"{n}\", {l}")),
                                    (Some(n), None) => Some(format!("KiwiSDR: \"{n}\"")),
                                    (None, Some(l)) => Some(format!("KiwiSDR: {l}")),
                                    (None, None) => None,
                                };
                                let mut i = shared.lock();
                                if name.is_some() {
                                    i.status.name = name;
                                }
                                if location.is_some() {
                                    i.status.location = location;
                                }
                                i.log.extend(line);
                            }
                            KiwiMsg::Other => {}
                        }
                        if let (Some(maj), Some(min)) = version {
                            shared.lock().status.version = Some(format!("{maj}.{min}"));
                            version = (None, None);
                        }
                    }
                }
                "SND" => match protocol::parse_snd(body) {
                    Ok(block) if !block.is_iq() => {
                        return Err(Outcome::Fatal("the KiwiSDR sends audio instead of I/Q".into()));
                    }
                    // The first block is what the channel's previous user left (kiwiclient).
                    Ok(_) if first_block => first_block = false,
                    Ok(block) => {
                        shared.push(&block);
                        if !self.streamed {
                            self.streamed = true;
                            shared.set_state(KiwiState::Streaming);
                            shared.log("KiwiSDR: streaming".into());
                        }
                    }
                    Err(protocol::SndError::Compressed) => {
                        return Err(Outcome::Fatal("the KiwiSDR sends compressed audio instead of I/Q".into()));
                    }
                    Err(protocol::SndError::Truncated) => {}
                },
                _ => {}
            }
            if set_up && last_keepalive.elapsed() >= Duration::from_secs(1) {
                self.send(shared, protocol::KEEPALIVE)?;
                last_keepalive = Instant::now();
            }
        }
    }
}
