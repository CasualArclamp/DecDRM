//! The GUI's view of the receiving engine.
//!
//! # Why the GUI polls a snapshot
//!
//! The engine runs all DSP on its own worker thread and owns every piece of receiver
//! state; the GUI never touches that state. About ten times per second the worker
//! publishes a complete [`Snapshot`] into an `Arc<Mutex<Snapshot>>`, and
//! [`Engine::snapshot`] hands the GUI a *clone* of it. A clone rather than a shared
//! reference because in Rust a reference obtained through a `Mutex` lives only as
//! long as the lock guard: drawing a frame while holding the lock would stall the DSP
//! thread, and the borrow checker will not let the reference outlive the guard anyway.
//! Copying a few hundred kilobytes ten times a second is cheap, and afterwards the GUI
//! owns its copy outright — no locks, no lifetimes — so the immediate-mode UI (egui
//! redraws the whole window every frame) simply draws from it.
//!
//! Discrete happenings (log lines, text messages, decoded data objects, the end of the
//! input) arrive through a channel instead ([`Engine::poll_events`]): a snapshot only
//! holds the latest state and would lose items that came and went between two polls.

use crate::data::DataServices;
use crate::indicators::Indicators;
use crate::plots::PlotData;
use decdrm_engine::{Command, Engine, EngineConfig, EngineEvent, InputSpec, ServiceView, Snapshot};
use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often a new snapshot is fetched (the engine publishes at ~10 Hz).
pub const FETCH_INTERVAL: Duration = Duration::from_millis(100);
/// Log lines kept by the GUI (the snapshot itself keeps only the last 200).
pub const LOG_CAPACITY: usize = 5000;
/// Text messages kept for the history list.
pub const TEXT_HISTORY: usize = 20;

/// What selecting service `clicked` changes before the engine confirms it: the
/// highlighted service, and whether the text-message history must be cleared.
///
/// The engine highlights the audio service it decodes, falling back to the chosen
/// service only when no audio service is decoded; choosing a data service therefore
/// changes nothing while an audio service plays (its data is decoded anyway).
/// Mirroring that rule keeps the list from flickering until the next snapshot.
pub fn selection_preview(
    services: &[ServiceView],
    current: Option<u8>,
    clicked: u8,
) -> (Option<u8>, bool) {
    let is_audio = services.iter().any(|s| s.short_id == clicked && s.is_audio);
    let any_audio = services.iter().any(|s| s.is_audio);
    if is_audio {
        // Text messages belong to the decoded audio service.
        (Some(clicked), current != Some(clicked))
    } else if !any_audio {
        (Some(clicked), false)
    } else {
        (current, false)
    }
}

/// Bounded list of log lines.
#[derive(Debug, Clone, Default)]
pub struct LogBuffer {
    lines: VecDeque<String>,
}

impl LogBuffer {
    pub fn push(&mut self, line: impl Into<String>) {
        self.lines.push_back(line.into());
        while self.lines.len() > LOG_CAPACITY {
            self.lines.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn get(&self, i: usize) -> Option<&str> {
        self.lines.get(i).map(String::as_str)
    }

    pub fn clear(&mut self) {
        self.lines.clear();
    }

    /// All lines joined with newlines (for the clipboard).
    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Append a text message unless it repeats the newest one (stations repeat their
/// messages continuously), keeping at most [`TEXT_HISTORY`].
pub fn push_text(history: &mut VecDeque<String>, text: String) {
    if history.back() != Some(&text) {
        history.push_back(text);
        while history.len() > TEXT_HISTORY {
            history.pop_front();
        }
    }
}

/// Engine handle plus everything the GUI derives from it.
pub struct RxSession {
    engine: Option<Engine>,
    /// A stop command was sent; waiting for the worker to report `Stopped`.
    stopping: bool,
    /// Latest snapshot (a copy owned by the GUI, see the module docs).
    pub snap: Snapshot,
    /// Plot data prepared from `snap`.
    pub plots: PlotData,
    pub indicators: Indicators,
    pub data: DataServices,
    pub log: LogBuffer,
    /// Recent text messages of the selected audio service, oldest first.
    pub texts: VecDeque<String>,
    /// Description of the current source (for the title / log).
    pub source_label: String,
    /// Live sound-card input (slideshow trigger times use the wall clock).
    live: bool,
    epoch: Instant,
    last_fetch: Option<Instant>,
}

impl Default for RxSession {
    fn default() -> Self {
        Self {
            engine: None,
            stopping: false,
            snap: Snapshot::default(),
            plots: PlotData::default(),
            indicators: Indicators::default(),
            data: DataServices::default(),
            log: LogBuffer::default(),
            texts: VecDeque::new(),
            source_label: String::new(),
            live: false,
            epoch: Instant::now(),
            last_fetch: None,
        }
    }
}

impl RxSession {
    /// The engine is running (it may already have been asked to stop).
    pub fn is_running(&self) -> bool {
        self.engine.is_some()
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping
    }

    /// Start a new engine, replacing a running one.
    pub fn start(&mut self, cfg: EngineConfig, label: String) {
        self.shutdown();
        self.live = matches!(cfg.input, InputSpec::Device { .. });
        self.snap = Snapshot::default();
        self.plots = PlotData::default();
        self.indicators.clear();
        self.data.clear();
        self.texts.clear();
        self.log.push(format!("── start: {label}"));
        self.source_label = label;
        self.engine = Some(Engine::start(cfg));
        self.last_fetch = None;
    }

    /// Ask the worker to stop; the handle is released when it reports `Stopped`.
    pub fn stop(&mut self) {
        if let Some(e) = &self.engine {
            e.command(Command::Stop);
            self.stopping = true;
        }
    }

    /// Restart signal acquisition (keeps the source open).
    pub fn restart(&mut self) {
        if let Some(e) = &self.engine {
            e.command(Command::Restart);
        }
    }

    /// Ask the engine to decode / show service `short_id`, and show the expected
    /// result right away (see [`selection_preview`]); the next snapshot confirms it.
    pub fn select_service(&mut self, short_id: u8) {
        if let Some(e) = &self.engine {
            e.command(Command::SelectService(short_id));
            let (highlight, clear_texts) =
                selection_preview(&self.snap.services, self.snap.selected_service, short_id);
            if clear_texts {
                self.texts.clear();
            }
            self.snap.selected_service = highlight;
        }
    }

    /// Stop and join the worker right away (used before starting a new one and on
    /// exit). Dropping an [`Engine`] sends `Stop` and waits for its thread.
    pub fn shutdown(&mut self) {
        if let Some(e) = self.engine.take() {
            drop(e);
            self.log.push("── stopped");
        }
        self.stopping = false;
    }

    /// Drain engine events and, at most every [`FETCH_INTERVAL`], fetch a new snapshot.
    /// Returns `true` if anything changed (the caller then repaints).
    pub fn poll(&mut self, now: Instant) -> bool {
        let Some(engine) = &self.engine else {
            return false;
        };
        let events = engine.poll_events();
        let mut changed = !events.is_empty();
        let mut finished = false;
        let now_unix = self.live.then(unix_now);
        for ev in events {
            match ev {
                EngineEvent::Log(line) => self.log.push(line),
                EngineEvent::Text(text) => push_text(&mut self.texts, text),
                EngineEvent::Data { short_id, event } => {
                    self.data.apply(short_id, &event, now_unix)
                }
                EngineEvent::Stopped { error } => {
                    if let Some(e) = error {
                        self.log.push(format!("error: {e}"));
                    }
                    finished = true;
                }
            }
        }
        if let Some(t) = now_unix {
            self.data.tick(t);
        }
        let due = self
            .last_fetch
            .is_none_or(|t| now.duration_since(t) >= FETCH_INTERVAL);
        if due || finished {
            let snap = engine.snapshot();
            // `seq` counts the engine's publications: the same number means the same
            // content, so the plot data need not be prepared again.
            if snap.seq != self.snap.seq {
                self.plots = PlotData::from_snapshot(&snap);
            }
            self.snap = snap;
            self.last_fetch = Some(now);
            changed = true;
            // The CRC indicators work on time windows, so they are updated on every
            // fetch, new content or not.
            let t = now.duration_since(self.epoch).as_secs_f64();
            let running = !finished && !self.snap.stopped;
            self.indicators.update(t, &self.snap, running);
        }
        if finished {
            // The worker has exited, so dropping the handle joins it immediately. The
            // final snapshot (plots, counters, error) stays on screen.
            self.engine = None;
            self.stopping = false;
            self.log.push("── finished");
        }
        changed
    }
}

/// Seconds since 1970 (UTC), from the system clock.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_is_bounded() {
        let mut log = LogBuffer::default();
        for i in 0..LOG_CAPACITY + 10 {
            log.push(format!("line {i}"));
        }
        assert_eq!(log.len(), LOG_CAPACITY);
        assert_eq!(log.get(0), Some("line 10"));
        assert!(log.text().ends_with(&format!("line {}", LOG_CAPACITY + 9)));
        log.clear();
        assert_eq!(log.len(), 0);
    }

    #[test]
    fn text_history_skips_repeats() {
        let mut h = VecDeque::new();
        push_text(&mut h, "a".into());
        push_text(&mut h, "a".into());
        push_text(&mut h, "b".into());
        push_text(&mut h, "a".into());
        assert_eq!(h, ["a", "b", "a"]);
        for i in 0..TEXT_HISTORY * 2 {
            push_text(&mut h, i.to_string());
        }
        assert_eq!(h.len(), TEXT_HISTORY);
    }

    #[test]
    fn idle_session_does_nothing() {
        let mut s = RxSession::default();
        assert!(!s.is_running());
        assert!(!s.poll(Instant::now()));
        s.stop();
        s.restart();
        s.select_service(1);
        assert!(!s.is_stopping());
        assert_eq!(s.snap.selected_service, None, "no engine, no selection");
    }

    #[test]
    fn selection_follows_the_engine_rule() {
        let service = |short_id, is_audio| ServiceView {
            short_id,
            is_audio,
            ..Default::default()
        };
        let mixed = [service(0, true), service(1, true), service(2, false)];
        // Another audio service: highlight it and drop the old service's texts.
        assert_eq!(selection_preview(&mixed, Some(0), 1), (Some(1), true));
        assert_eq!(selection_preview(&mixed, Some(0), 0), (Some(0), false));
        // A data service while audio is decoded: the audio service stays highlighted.
        assert_eq!(selection_preview(&mixed, Some(0), 2), (Some(0), false));
        // Data-only multiplex: the chosen data service is highlighted.
        let data_only = [service(0, false), service(1, false)];
        assert_eq!(selection_preview(&data_only, Some(0), 1), (Some(1), false));
    }

    /// End-to-end with a missing file: the engine reports the error and stops, and the
    /// session releases the handle and keeps the error in the log.
    #[test]
    fn failed_start_is_reported() {
        let mut s = RxSession::default();
        let cfg = EngineConfig::file("this/file/does/not/exist.flac");
        s.start(cfg, "missing".into());
        assert!(s.is_running());
        let t0 = Instant::now();
        while s.is_running() && t0.elapsed() < Duration::from_secs(10) {
            s.poll(Instant::now());
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!s.is_running(), "engine should stop on its own");
        assert!(s.snap.error.is_some());
        let log = s.log.text();
        assert!(log.contains("error:"), "{log}");
    }
}
