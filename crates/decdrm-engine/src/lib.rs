//! DecDRM engine: runs a receiving [`session::Session`] on a worker thread fed from a
//! file or sound card, and publishes [`Snapshot`]s and [`EngineEvent`]s for the CLI
//! and GUI.
//!
//! Threading model: the worker owns all DSP state. The UI side only holds an
//! `Arc<Mutex<Snapshot>>` (replaced wholesale ~10×/s, so the lock is held for a
//! clone) and channel endpoints for commands and events. `crossbeam_channel` is used
//! because its channels can be polled without blocking from a GUI frame loop.

pub mod audio_out;
pub mod session;
pub mod snapshot;
pub mod source;

pub use decdrm_core::rx::{InputFormat, RealChannel, ReceiverConfig};
pub use decdrm_data;
pub use session::{Session, SessionEvent};
pub use snapshot::{AudioStatus, InputStatus, ServiceView, Snapshot};
pub use source::{InputSpec, Source, SourceInfo};

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Engine configuration.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub input: InputSpec,
    /// Receiver settings; `channels` is overwritten with the source's channel count.
    pub receiver: ReceiverConfig,
    /// Play decoded audio on a sound card.
    pub play_audio: bool,
    pub output_device: Option<String>,
    /// Write decoded audio to this WAV/FLAC file.
    pub record_audio: Option<std::path::PathBuf>,
    /// Directory for slideshow images, websites, EPG and raw data.
    pub data_dir: Option<std::path::PathBuf>,
}

impl EngineConfig {
    pub fn file(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            input: InputSpec::File { path: path.into(), realtime: false },
            receiver: ReceiverConfig::default(),
            play_audio: false,
            output_device: None,
            record_audio: None,
            data_dir: None,
        }
    }
}

/// Commands to the worker.
#[derive(Debug, Clone)]
pub enum Command {
    Restart,
    SelectService(u8),
    Stop,
}

/// Things the worker reports besides the snapshot.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    Log(String),
    /// A complete text message of the selected audio service.
    Text(String),
    /// Output of a data service decoder (slideshow images, Journaline pages, EPG,
    /// website files, raw data). `short_id` is the service it belongs to. Feed these
    /// into the GUI models in `decdrm_data::{slideshow, journaline, website}`.
    Data { short_id: u8, event: decdrm_data::DataEvent },
    /// The worker finished (end of file, stop, or error).
    Stopped { error: Option<String> },
}

/// Handle to a running engine. Dropping it stops the worker.
pub struct Engine {
    cmd: Sender<Command>,
    events: Receiver<EngineEvent>,
    shared: Arc<Mutex<Snapshot>>,
    handle: Option<JoinHandle<()>>,
}

impl Engine {
    /// Start the worker thread. Source errors are reported through
    /// [`EngineEvent::Stopped`] and [`Snapshot::error`].
    pub fn start(cfg: EngineConfig) -> Self {
        let (cmd_tx, cmd_rx) = unbounded();
        let (ev_tx, ev_rx) = unbounded();
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        let worker_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("decdrm-engine".into())
            .spawn(move || {
                let err = match worker(cfg, &cmd_rx, &ev_tx, &worker_shared) {
                    Ok(()) => None,
                    Err(e) => Some(format!("{e:#}")),
                };
                if let Ok(mut s) = worker_shared.lock() {
                    s.stopped = true;
                    s.error = err.clone();
                }
                let _ = ev_tx.send(EngineEvent::Stopped { error: err });
            })
            .expect("spawn engine thread");
        Self { cmd: cmd_tx, events: ev_rx, shared, handle: Some(handle) }
    }

    /// Latest snapshot (a clone).
    pub fn snapshot(&self) -> Snapshot {
        self.shared.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Non-blocking: all events queued since the last call.
    pub fn poll_events(&self) -> Vec<EngineEvent> {
        self.events.try_iter().collect()
    }

    /// Blocking receive with timeout (for the CLI).
    pub fn recv_event(&self, timeout: Duration) -> Option<EngineEvent> {
        self.events.recv_timeout(timeout).ok()
    }

    pub fn command(&self, c: Command) {
        let _ = self.cmd.send(c);
    }

    /// Wait for the worker to finish (e.g. end of file).
    pub fn join(mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = self.cmd.send(Command::Stop);
            let _ = h.join();
        }
    }
}

fn worker(
    cfg: EngineConfig,
    cmd_rx: &Receiver<Command>,
    ev_tx: &Sender<EngineEvent>,
    shared: &Arc<Mutex<Snapshot>>,
) -> Result<()> {
    let mut source = Source::open(&cfg.input)?;
    let info = source.info().clone();
    let realtime = matches!(cfg.input, InputSpec::File { realtime: true, .. });
    let mut rcfg = cfg.receiver.clone();
    rcfg.channels = info.channels;
    let mut session = Session::new(rcfg);
    let log = |line: String, snap: &mut Snapshot| {
        let _ = ev_tx.send(EngineEvent::Log(line.clone()));
        snap.push_log(line);
    };

    let mut snap = Snapshot::default();
    snap.input.info = info.clone();
    log(
        format!("input: {} ({} Hz, {} ch)", info.name, info.sample_rate, info.channels),
        &mut snap,
    );

    let started = Instant::now();
    let mut last_publish = Instant::now() - Duration::from_secs(1);
    let chunk_frames = (info.sample_rate as usize / 20).max(256); // 50 ms
    loop {
        // Commands.
        for c in cmd_rx.try_iter() {
            match c {
                Command::Stop => {
                    publish(shared, &mut snap, &session, &source, true);
                    return Ok(());
                }
                Command::Restart => {
                    session.restart();
                    log("receiver restarted".into(), &mut snap);
                }
                Command::SelectService(id) => snap.selected_service = Some(id),
            }
        }

        let Some(frames) = source.read(chunk_frames)? else {
            publish(shared, &mut snap, &session, &source, true);
            log(format!("end of input after {:.1} s", source.position_s()), &mut snap);
            publish(shared, &mut snap, &session, &source, true);
            return Ok(());
        };
        if !frames.is_empty() {
            let rms = (frames.iter().map(|v| v * v).sum::<f32>() / frames.len() as f32).sqrt();
            snap.input.level_dbfs = 20.0 * rms.max(1e-9).log10();
        }
        for ev in session.push(&frames) {
            match ev {
                SessionEvent::Log(l) => log(l, &mut snap),
                SessionEvent::Fac(fac) => update_services_from_fac(&mut snap, &fac),
                SessionEvent::Sdc(_) | SessionEvent::Msc(_) => {}
            }
        }

        if last_publish.elapsed() >= Duration::from_millis(100) {
            publish(shared, &mut snap, &session, &source, false);
            last_publish = Instant::now();
        }
        if realtime {
            // Pace to wall-clock time.
            let ahead = source.position_s() - started.elapsed().as_secs_f64();
            if ahead > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(ahead.min(0.2)));
            }
        }
    }
}

fn publish(shared: &Arc<Mutex<Snapshot>>, snap: &mut Snapshot, session: &Session, source: &Source, finished: bool) {
    snap.rx = session.status().clone();
    snap.visuals = session.visuals();
    snap.input.position_s = source.position_s();
    snap.input.finished = finished;
    if let Ok(mut s) = shared.lock() {
        *s = snap.clone();
    }
}

/// Until the SDC parser is wired in, list services from the FAC alone.
fn update_services_from_fac(snap: &mut Snapshot, fac: &decdrm_core::fac::Fac) {
    let s = &fac.service;
    let view = ServiceView {
        short_id: s.short_id,
        service_id: s.service_id,
        label: String::new(),
        is_audio: !s.is_data,
        description: session::describe_fac_service(fac),
        language: decdrm_core::fac::LANGUAGES.get(s.language as usize).copied().unwrap_or("").to_string(),
    };
    match snap.services.iter_mut().find(|v| v.short_id == s.short_id) {
        Some(v) => {
            let label = std::mem::take(&mut v.label);
            *v = view;
            v.label = label;
        }
        None => {
            snap.services.push(view);
            snap.services.sort_by_key(|v| v.short_id);
        }
    }
    if snap.selected_service.is_none() && !s.is_data {
        snap.selected_service = Some(s.short_id);
    }
}
