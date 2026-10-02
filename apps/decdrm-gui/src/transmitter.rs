//! The GUI's transmitter: a [`Station`] running on its own worker thread.
//!
//! The pattern is the receiver's (see `receiver.rs` for why the GUI works on published
//! copies): the worker owns the station; about ten times per second it publishes a
//! [`TxSnapshot`] into an `Arc<Mutex<_>>`, and the GUI clones it. The stop request
//! (and a request to load the Journaline page files again) goes to the worker through
//! a channel — the stop request also through the station's [`StopHandle`],
//! which also cuts short a wait for room on a slow or stalled sound card — and the
//! worker's log lines and end (with the error, if any) come back through another. The receiver and the transmitter are
//! independent, so both can run at once (e.g. a loopback through a virtual cable).
//!
//! Pacing: a sound-card output blocks the worker while the card's buffer is full, so
//! the station runs in real time; a file-only transmission runs as fast as possible
//! (bounded by the duration limit or the end of the inputs, see
//! [`crate::tx_config::start_check`]).

use crate::plots::SpectrumPlot;
use crate::spectrum::SpectrumAverager;
use crate::tx_config::signal_band;
use decdrm_station::{MultiplexPlan, Station, StationConfig, StationStatus, StopHandle};
use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often the worker publishes, and the GUI fetches, a snapshot.
pub const PUBLISH_INTERVAL: Duration = Duration::from_millis(100);
/// Messages kept for the Transmitter tab.
const MESSAGES: usize = 50;

/// What the transmitter tab shows, published by the worker.
#[derive(Debug, Clone, Default)]
pub struct TxSnapshot {
    /// Publish counter.
    pub seq: u64,
    /// The station's own status (frames, levels, SDC, services).
    pub status: StationStatus,
    /// Averaged spectrum of the output in dB, bins from −24 to +24 kHz.
    pub spectrum_db: Vec<f64>,
    /// One output channel (real IF); else I/Q.
    pub real_output: bool,
    /// DC carrier and occupied band in that spectrum, Hz.
    pub dc_hz: Option<f64>,
    pub band_hz: Option<(f64, f64)>,
    /// Frames to transmit (the duration limit), if any.
    pub frames_limit: Option<u64>,
    /// The station has been created (outputs open).
    pub started: bool,
}

enum Cmd {
    Stop,
    /// Load the Journaline page files again now ([`Station::reload_journaline`]).
    ReloadJournaline,
}

enum TxEvent {
    Log(String),
    Stopped { error: Option<String> },
}

/// Handle to the worker thread. Dropping it stops the station and waits for it.
struct TxWorker {
    cmd: Sender<Cmd>,
    events: Receiver<TxEvent>,
    shared: Arc<Mutex<TxSnapshot>>,
    /// The station's stop handle, once the worker has created the station.
    stop: Arc<Mutex<Option<StopHandle>>>,
    handle: Option<JoinHandle<()>>,
}

impl TxWorker {
    fn start(cfg: StationConfig, plan: MultiplexPlan, frames: Option<u64>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ev_tx, ev_rx) = mpsc::channel();
        let first = TxSnapshot {
            frames_limit: frames,
            ..TxSnapshot::default()
        };
        let shared = Arc::new(Mutex::new(first.clone()));
        let worker_shared = Arc::clone(&shared);
        let stop = Arc::new(Mutex::new(None));
        let worker_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("decdrm-station".into())
            .spawn(move || {
                // Rust note: `catch_unwind` turns a panic in the station (a bug) into an
                // error message instead of a silently dead thread. `AssertUnwindSafe`
                // is our promise that nothing half-updated is used afterwards: the
                // station is dropped with the closure.
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    run(
                        cfg,
                        plan,
                        first,
                        &cmd_rx,
                        &ev_tx,
                        &worker_shared,
                        &worker_stop,
                    )
                }));
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(e),
                    Err(panic) => Some(format!(
                        "the transmitter stopped unexpectedly: {}",
                        panic_message(panic.as_ref())
                    )),
                };
                let _ = ev_tx.send(TxEvent::Stopped { error });
            })
            .expect("spawn the transmitter thread");
        Self {
            cmd: cmd_tx,
            events: ev_rx,
            shared,
            stop,
            handle: Some(handle),
        }
    }

    fn snapshot(&self) -> TxSnapshot {
        self.shared.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Ask the worker to stop: through the channel (checked between frames) and the
    /// station's stop handle (which ends a wait on the sound card at once).
    fn request_stop(&self) {
        let _ = self.cmd.send(Cmd::Stop);
        if let Ok(slot) = self.stop.lock()
            && let Some(h) = slot.as_ref()
        {
            h.stop();
        }
    }
}

impl Drop for TxWorker {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            self.request_stop();
            let _ = h.join();
        }
    }
}

/// Text of a panic payload (a `&str` or `String` for `panic!("…")`).
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// The worker: create the station, transmit until stopped / the duration is reached /
/// the inputs have ended, then finish it (completing the file).
fn run(
    cfg: StationConfig,
    plan: MultiplexPlan,
    mut snap: TxSnapshot,
    cmd: &Receiver<Cmd>,
    events: &Sender<TxEvent>,
    shared: &Mutex<TxSnapshot>,
    stop: &Mutex<Option<StopHandle>>,
) -> Result<(), String> {
    let log = |line: String| {
        let _ = events.send(TxEvent::Log(line));
    };
    // The stop handle exists before the station, so Stop also ends the wait of
    // creating it (a web stream input connecting, up to 20 s). The GUI has just
    // validated `cfg` and shown this plan; no need to check twice.
    let handle = StopHandle::default();
    if let Ok(mut slot) = stop.lock() {
        *slot = Some(handle.clone());
    }
    let mut station = Station::with_plan_and_stop(cfg, plan, handle).map_err(|e| e.to_string())?;
    for line in station.take_log() {
        log(line);
    }
    let plan = station.plan();
    let (dc, band) = signal_band(plan.layout, plan.output.format);
    let channels = station.output_channels();
    snap.real_output = channels == 1;
    snap.dc_hz = Some(dc);
    snap.band_hz = Some(band);
    snap.started = true;
    if let Some(dev) = &station.status().device {
        log(format!("sound card: {dev}"));
    }
    let frames = snap.frames_limit;
    let mut spectrum = SpectrumAverager::new();
    let mut last_publish: Option<Instant> = None;
    publish(shared, &mut snap, station.status(), &spectrum);

    let reason = loop {
        match cmd.try_recv() {
            Ok(Cmd::Stop) | Err(TryRecvError::Disconnected) => break "stopped",
            // What changed goes to the station's log, taken after the frame.
            Ok(Cmd::ReloadJournaline) => {
                station.reload_journaline();
            }
            Err(TryRecvError::Empty) => {}
        }
        match frames {
            Some(n) if station.status().frames >= n => break "duration reached",
            None if station.inputs_finished() => break "all inputs have ended",
            _ => {}
        }
        let due = last_publish.is_none_or(|t| t.elapsed() >= PUBLISH_INTERVAL);
        // `transmit_frame` returns a slice borrowed from the station; it is used inside
        // `map`, so the result below holds no borrow and `finish` may consume the
        // station. (The spectrum only needs the frames that are published.)
        let sent = station.transmit_frame().map(|samples| {
            if due {
                spectrum.push(samples, channels);
            }
        });
        // The inputs' own log: a web stream's connections, titles, reconnections.
        for line in station.take_log() {
            log(line);
        }
        if let Err(e) = sent {
            // Still complete the output file, so what was sent is readable.
            let _ = station.finish();
            return Err(e.to_string());
        }
        if due {
            // A modulator's channel comes with the MDI: the band marks follow it.
            let plan = station.plan();
            let (dc, band) = signal_band(plan.layout, plan.output.format);
            snap.dc_hz = Some(dc);
            snap.band_hz = Some(band);
            publish(shared, &mut snap, station.status(), &spectrum);
            last_publish = Some(Instant::now());
        }
    };
    let status = station.finish().map_err(|e| e.to_string())?;
    log(format!("{reason} after {:.1} s of signal", status.seconds));
    publish(shared, &mut snap, &status, &spectrum);
    Ok(())
}

fn publish(
    shared: &Mutex<TxSnapshot>,
    snap: &mut TxSnapshot,
    status: &StationStatus,
    spectrum: &SpectrumAverager,
) {
    snap.seq += 1;
    snap.status = status.clone();
    snap.spectrum_db = spectrum.db();
    if let Ok(mut s) = shared.lock() {
        *s = snap.clone();
    }
}

/// Transmitter handle plus what the GUI derives from it.
#[derive(Default)]
pub struct TxSession {
    worker: Option<TxWorker>,
    stopping: bool,
    /// Latest snapshot (the final one stays after the end).
    pub snap: TxSnapshot,
    /// Spectrum of the output, ready to draw.
    pub spectrum: SpectrumPlot,
    /// Log lines of the transmitter, oldest first.
    pub messages: VecDeque<String>,
    /// Why the last transmission failed.
    pub error: Option<String>,
    /// Description of what is being transmitted.
    pub label: String,
    started_at: Option<Instant>,
    ended_at: Option<Instant>,
    last_fetch: Option<Instant>,
}

impl TxSession {
    pub fn is_running(&self) -> bool {
        self.worker.is_some()
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping
    }

    /// Wall-clock time since the start (frozen at the end).
    pub fn elapsed(&self) -> Option<Duration> {
        let start = self.started_at?;
        Some(
            self.ended_at
                .unwrap_or_else(Instant::now)
                .duration_since(start),
        )
    }

    /// Start transmitting `cfg` with its `plan` (from `cfg.validate()`), for at most
    /// `frames` frames.
    pub fn start(
        &mut self,
        cfg: StationConfig,
        plan: MultiplexPlan,
        frames: Option<u64>,
        label: String,
    ) {
        self.shutdown();
        self.error = None;
        self.push_message(format!("── transmit: {label}"));
        self.label = label;
        let worker = TxWorker::start(cfg, plan, frames);
        self.snap = worker.snapshot();
        self.spectrum = SpectrumPlot::default();
        self.worker = Some(worker);
        self.started_at = Some(Instant::now());
        self.ended_at = None;
        self.last_fetch = None;
    }

    /// Ask the worker to stop; it finishes the station (completing the file, playing
    /// out the sound card's buffer) and then reports its end.
    pub fn stop(&mut self) {
        if let Some(w) = &self.worker {
            w.request_stop();
            self.stopping = true;
        }
    }

    /// Load the Journaline page files again now (the station also does so by itself
    /// when a page file changes); what changed appears in the messages.
    pub fn reload_journaline(&self) {
        if let Some(w) = &self.worker {
            let _ = w.cmd.send(Cmd::ReloadJournaline);
        }
    }

    /// Stop and wait for the worker (before a new start, and on exit).
    pub fn shutdown(&mut self) {
        if let Some(w) = self.worker.take() {
            drop(w);
            self.ended_at = Some(Instant::now());
        }
        self.stopping = false;
    }

    fn push_message(&mut self, line: String) {
        self.messages.push_back(line);
        while self.messages.len() > MESSAGES {
            self.messages.pop_front();
        }
    }

    /// Drain the worker's messages and fetch its snapshot at most every
    /// [`PUBLISH_INTERVAL`]. Returns `true` if anything changed.
    pub fn poll(&mut self, now: Instant) -> bool {
        let Some(w) = &self.worker else { return false };
        let mut changed = false;
        let mut stopped = None;
        for ev in w.events.try_iter() {
            changed = true;
            match ev {
                TxEvent::Log(line) => self.messages.push_back(line),
                TxEvent::Stopped { error } => stopped = Some(error),
            }
        }
        while self.messages.len() > MESSAGES {
            self.messages.pop_front();
        }
        let due = self
            .last_fetch
            .is_none_or(|t| now.duration_since(t) >= PUBLISH_INTERVAL);
        if due || stopped.is_some() {
            let snap = w.snapshot();
            if snap.seq != self.snap.seq {
                self.spectrum = SpectrumPlot::new(
                    &snap.spectrum_db,
                    0.0,
                    48_000.0,
                    snap.real_output,
                    snap.dc_hz,
                    snap.band_hz,
                );
                changed = true;
            }
            self.snap = snap;
            self.last_fetch = Some(now);
        }
        if let Some(error) = stopped {
            // The worker has ended, so dropping the handle joins it at once.
            self.worker = None;
            self.stopping = false;
            self.ended_at = Some(now);
            match error {
                Some(e) => {
                    self.push_message(format!("error: {e}"));
                    self.error = Some(e);
                }
                None => self.push_message("── finished".into()),
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::TxOutput;
    use crate::tx_config::{EXAMPLE_STATION, Overrides, check, frames_for, materialize_example};

    fn wait_until_stopped(tx: &mut TxSession) {
        let t0 = Instant::now();
        while tx.is_running() && t0.elapsed() < Duration::from_secs(60) {
            tx.poll(Instant::now());
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!tx.is_running(), "the transmitter should stop on its own");
    }

    #[test]
    fn idle_session_does_nothing() {
        let mut tx = TxSession::default();
        assert!(!tx.is_running() && tx.elapsed().is_none());
        assert!(!tx.poll(Instant::now()));
        tx.stop();
        assert!(!tx.is_stopping());
    }

    #[test]
    fn panic_messages() {
        assert_eq!(panic_message(&"boom"), "boom");
        assert_eq!(panic_message(&String::from("bang")), "bang");
        assert_eq!(panic_message(&42_u8), "unknown panic");
    }

    /// End to end: two seconds of the example station into a WAV file, through the
    /// worker thread, as fast as possible.
    #[test]
    fn transmits_the_example_to_a_file() {
        let dir = std::env::temp_dir().join(format!("decdrm-gui-tx-run-{}", std::process::id()));
        materialize_example(&dir).unwrap();
        let wav = dir.join("out.wav");
        let ov = Overrides {
            output: TxOutput::File,
            file: Some(wav.clone()),
            device: None,
        };
        let (cfg, plan) = check(EXAMPLE_STATION, &dir, &ov).unwrap();
        let mut tx = TxSession::default();
        tx.start(cfg, plan, frames_for(2.0), "test".into());
        assert!(tx.is_running());
        wait_until_stopped(&mut tx);
        assert_eq!(tx.error, None, "{:?}", tx.messages);
        assert!(tx.snap.started);
        assert_eq!(tx.snap.status.frames, 5);
        assert_eq!(tx.snap.status.output_file.as_deref(), Some(wav.as_path()));
        assert!(!tx.snap.spectrum_db.is_empty());
        assert_eq!(tx.snap.status.services.len(), 2);
        // 2 s of 48 kHz 16-bit mono plus the channel filter's tail and the header.
        let bytes = std::fs::metadata(&wav).unwrap().len();
        assert!(bytes > 2 * 48_000 * 2, "{bytes} bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Stop ends a long transmission early and still completes the file.
    #[test]
    fn stop_ends_a_transmission() {
        let dir = std::env::temp_dir().join(format!("decdrm-gui-tx-stop-{}", std::process::id()));
        materialize_example(&dir).unwrap();
        let wav = dir.join("stopped.wav");
        let ov = Overrides {
            output: TxOutput::File,
            file: Some(wav.clone()),
            device: None,
        };
        let (cfg, plan) = check(EXAMPLE_STATION, &dir, &ov).unwrap();
        let mut tx = TxSession::default();
        let limit = frames_for(3600.0);
        tx.start(cfg, plan, limit, "long".into());
        tx.stop();
        assert!(tx.is_stopping());
        wait_until_stopped(&mut tx);
        assert_eq!(tx.error, None, "{:?}", tx.messages);
        assert!(tx.snap.status.frames < limit.unwrap(), "stopped early");
        assert!(
            tx.messages.iter().any(|m| m.starts_with("stopped")),
            "{:?}",
            tx.messages
        );
        assert!(
            wav.exists(),
            "the file is completed, not left behind half-written"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Journaline page file edited while transmitting: the Update button's request
    /// loads it, and the change shows in the messages and the status.
    #[test]
    fn journaline_update_while_transmitting() {
        let dir = std::env::temp_dir().join(format!("decdrm-gui-tx-jl-{}", std::process::id()));
        materialize_example(&dir).unwrap();
        let ov = Overrides {
            output: TxOutput::File,
            file: Some(dir.join("jl.wav")),
            device: None,
        };
        let (cfg, plan) = check(EXAMPLE_STATION, &dir, &ov).unwrap();
        let mut tx = TxSession::default();
        tx.start(cfg, plan, frames_for(3600.0), "journaline".into());
        // Edited once the station has loaded the page file.
        let t0 = Instant::now();
        while !tx.snap.started && t0.elapsed() < Duration::from_secs(60) {
            tx.poll(Instant::now());
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(tx.snap.started, "{:?}", tx.messages);
        let pages = dir.join("journaline.toml");
        let text = std::fs::read_to_string(&pages).unwrap();
        std::fs::write(&pages, text.replace("\"Headlines\"", "\"Top stories\"")).unwrap();
        tx.reload_journaline();
        let journaline = |tx: &TxSession| {
            tx.snap
                .status
                .services
                .iter()
                .flat_map(|s| &s.apps)
                .find_map(|a| a.journaline.clone())
        };
        let t0 = Instant::now();
        while journaline(&tx).is_none_or(|j| j.updates == 0)
            && t0.elapsed() < Duration::from_secs(60)
        {
            tx.poll(Instant::now());
            std::thread::sleep(Duration::from_millis(20));
        }
        tx.stop();
        wait_until_stopped(&mut tx);
        assert_eq!(tx.error, None, "{:?}", tx.messages);
        let j = journaline(&tx).expect("a Journaline application");
        assert_eq!(
            (j.updates, j.pages, j.error),
            (1, 4, None),
            "{:?}",
            tx.messages
        );
        assert!(
            tx.messages
                .iter()
                .any(|m| m.contains("Journaline page file reloaded: pages 0, 1 changed")),
            "{:?}",
            tx.messages
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A station that cannot be created reports its error and ends: here the Journaline
    /// page file disappears between the check and the start.
    #[test]
    fn errors_are_reported() {
        let dir = std::env::temp_dir().join(format!("decdrm-gui-tx-err-{}", std::process::id()));
        materialize_example(&dir).unwrap();
        let (mut cfg, plan) = check(EXAMPLE_STATION, &dir, &Overrides::default()).unwrap();
        cfg.output.file = Some(dir.join("never.wav"));
        std::fs::remove_file(dir.join("journaline.toml")).unwrap();
        let mut tx = TxSession::default();
        tx.start(cfg, plan, frames_for(1.0), "broken".into());
        wait_until_stopped(&mut tx);
        let e = tx.error.clone().unwrap_or_default();
        assert!(e.contains("journaline"), "{e}");
        assert!(
            !dir.join("never.wav").exists(),
            "no output file for a bad configuration"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
