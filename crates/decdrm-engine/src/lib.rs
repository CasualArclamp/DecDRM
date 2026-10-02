//! DecDRM engine: runs a receiving [`session::Session`] on a worker thread fed from a
//! file or sound card, and publishes [`Snapshot`]s and [`EngineEvent`]s for the CLI
//! and GUI.
//!
//! Threading model: the worker owns all DSP state. The UI side only holds an
//! `Arc<Mutex<Snapshot>>` (replaced wholesale ~10×/s, so the lock is held for a
//! clone) and channel endpoints for commands and events. `crossbeam_channel` is used
//! because its channels can be polled without blocking from a GUI frame loop.

pub mod afs;
pub mod audio_out;
pub mod data_store;
pub mod logger;
pub mod session;
pub mod snapshot;
pub mod source;

pub use decdrm_core::rx::{InputFormat, RealChannel, ReceiverConfig};
pub use decdrm_data;
pub use decdrm_kiwi;
pub use decdrm_mdi;
pub use logger::{LogConfig, LogFormat};
pub use session::{MscStats, Session, SessionEvent};
pub use snapshot::{
    AppView, AudioCodingView, AudioSpectrum, AudioStatus, BroadcastTime, DiversityView, InputStatus, METRICS_INTERVAL_S,
    MdiStatus, MetricsSample, RECENT_METRICS, RecordingStatus, RemoteControlStatus, ServiceView, Snapshot,
};
pub use source::{Input, InputSpec, MdiSpec, Source, SourceInfo};

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Seconds of input without decoded audio after which the published audio spectrum is
/// blanked (signal lost, or a data service chosen), so a stale spectrum is not shown.
/// Audio arrives in bursts, one per 400 ms multiplex frame.
pub const AUDIO_SPECTRUM_HOLD_S: f64 = 2.0;

/// Linear playback gain of a volume setting in percent (0–100): a squared law, so a
/// slider's travel matches loudness better than a linear gain (50 % ≈ −12 dB).
pub fn volume_gain(percent: f32) -> f32 {
    let p = if percent.is_finite() { percent.clamp(0.0, 100.0) / 100.0 } else { 1.0 };
    p * p
}

/// Engine configuration.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub input: InputSpec,
    /// Receiver settings; `channels` is overwritten with the source's channel count.
    pub receiver: ReceiverConfig,
    /// Play decoded audio on a sound card.
    pub play_audio: bool,
    pub output_device: Option<String>,
    /// Playback volume, a linear gain (1.0 = as decoded); see [`Command::SetVolume`].
    pub volume: f32,
    /// Record the decoded audio to this WAV/FLAC file from the start (see
    /// [`Command::StartRecording`]).
    pub record_audio: Option<std::path::PathBuf>,
    /// Directory for slideshow images, websites, EPG and raw data.
    pub data_dir: Option<std::path::PathBuf>,
    /// Reception log (metrics rows as CSV or JSON Lines, events in JSON Lines).
    pub log: Option<LogConfig>,
    /// Shortest time between two published snapshots: 100 ms by default (enough for a
    /// status line), 1/60 s for the GUI's plots, which then follow the channel symbol
    /// by symbol. Live input is read in pieces no longer than this.
    pub publish_interval: Duration,
    /// Remote control: accept RCI commands (TS 102 349) here — tune, select a service.
    pub rci_listen: Option<decdrm_mdi::net::UdpOrigin>,
    /// Start with the RF monitor on (see [`Command::SetMonitor`]).
    pub monitor: bool,
}

impl Default for EngineConfig {
    /// The default sound-card input, no outputs.
    fn default() -> Self {
        Self {
            input: InputSpec::Device { name: None, channels: None },
            receiver: ReceiverConfig::default(),
            play_audio: false,
            output_device: None,
            volume: 1.0,
            record_audio: None,
            data_dir: None,
            log: None,
            publish_interval: Duration::from_millis(100),
            rci_listen: None,
            monitor: false,
        }
    }
}

impl EngineConfig {
    pub fn file(path: impl Into<std::path::PathBuf>) -> Self {
        Self { input: InputSpec::File { path: path.into(), realtime: false }, ..Self::default() }
    }
}

/// Commands to the worker.
#[derive(Debug, Clone)]
pub enum Command {
    Restart,
    SelectService(u8),
    /// Change the playback volume (linear gain); it applies at once, not after the
    /// audio already queued on the sound card.
    SetVolume(f32),
    /// Retune a KiwiSDR input to this frequency, kHz, on the open connection; the
    /// receiver starts afresh (another station). An RSCI input sends it to its receiver
    /// by RCI. Other inputs ignore it.
    Tune(f64),
    /// Record the decoded audio (what is played, before the volume) to this WAV file,
    /// or FLAC with a `.flac` name, ending a recording in progress. The station's own
    /// sample rate and channels, 16-bit; a change of format carries on in `name-2.wav`,
    /// …. [`AudioStatus::recording`] shows it.
    StartRecording(std::path::PathBuf),
    /// End the recording, completing its file.
    StopRecording,
    /// The RF monitor: play the receiver's input instead of the decoded audio (`true`),
    /// or the decoded audio again — to hear the signal itself. The input plays as it
    /// comes in: I/Q with I on the left and Q on the right, a mono signal on both sides
    /// (in diversity reception the first input). Decoding goes on, and a recording
    /// keeps the decoded audio. [`AudioStatus::monitor`] shows it.
    SetMonitor(bool),
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
                    // A new state, so a new number (`snapshot_if_newer` goes by it).
                    s.seq += 1;
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

    /// The latest snapshot if it is not the one numbered `seq` (see [`Snapshot::seq`]):
    /// a caller polling every frame copies only new ones.
    pub fn snapshot_if_newer(&self, seq: u64) -> Option<Snapshot> {
        self.shared.lock().ok().filter(|s| s.seq != seq).map(|s| s.clone())
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
    let realtime = cfg.input.is_realtime();
    // The receiver configuration of an input (the channels it has; a KiwiSDR is I/Q).
    let receiver_for = |spec: &InputSpec, info: &SourceInfo| {
        let mut rcfg = cfg.receiver.clone();
        rcfg.channels = info.channels;
        if spec.is_iq() {
            rcfg.input = InputFormat::Iq { swap: false };
        }
        rcfg
    };
    let mut session = match &cfg.input {
        InputSpec::Diversity(specs) => Session::new_diversity([
            receiver_for(&specs[0], source.branch_info(0)),
            receiver_for(&specs[1], source.branch_info(1)),
        ]),
        InputSpec::Mdi(_) => Session::new_mdi(),
        spec => Session::new(receiver_for(spec, &info)),
    };
    // File playback paces the decoder to the sound card; live input relies on the
    // player's drift compensation.
    let mut audio = audio_out::AudioOut::new(cfg.play_audio, cfg.output_device.clone(), info.is_file, cfg.record_audio.clone())?;
    audio.set_monitor(cfg.monitor);
    audio.set_volume(cfg.volume);
    let mut saver = cfg.data_dir.clone().map(data_store::DataStore::new);
    let mut logger = cfg.log.as_ref().map(logger::Logger::create).transpose()?;
    let log = |line: String, snap: &mut Snapshot| {
        let _ = ev_tx.send(EngineEvent::Log(line.clone()));
        snap.push_log(line);
    };

    let mut snap = Snapshot::default();
    snap.input.info = info.clone();
    if cfg.input.is_mdi() {
        log(format!("input: {}", info.name), &mut snap);
    } else if cfg.input.is_iq() && !info.is_file {
        log(format!("input: {} (I/Q)", info.name), &mut snap);
    } else {
        log(format!("input: {} ({} Hz, {} ch)", info.name, info.sample_rate, info.channels), &mut snap);
    }

    // Remote control: RCI commands become engine commands.
    let mut remote = match &cfg.rci_listen {
        Some(o) => match decdrm_mdi::rci::RciListener::bind(o) {
            Ok(l) => {
                let at = l.local_addr().map_or_else(|_| o.to_string(), |a| a.to_string());
                log(format!("remote control (RCI): listening on {at}"), &mut snap);
                snap.remote = Some(RemoteControlStatus { listen: at, ..RemoteControlStatus::default() });
                Some(l)
            }
            Err(e) => {
                log(format!("remote control (RCI): cannot listen on {o}: {e}"), &mut snap);
                None
            }
        },
        None => None,
    };
    let mut last_remote_poll = Instant::now();

    let started = Instant::now();
    let mut last_publish = Instant::now() - Duration::from_secs(1);
    // Input position of the latest decoded audio (for blanking a stale audio spectrum).
    let mut last_audio_s: Option<f64> = None;
    // 50 ms pieces, shorter when snapshots are published more often (live input then
    // reaches the plots at that rate).
    let chunk_s = cfg.publish_interval.as_secs_f64().clamp(0.005, 0.05);
    let chunk_frames = ((f64::from(info.sample_rate) * chunk_s) as usize).max(64);
    loop {
        // Commands from the remote control, at most every 50 ms (a recording is decoded
        // in many short steps).
        let mut remote_commands = Vec::new();
        if let Some(l) = remote.as_mut()
            && last_remote_poll.elapsed() >= Duration::from_millis(50)
        {
            last_remote_poll = Instant::now();
            for (rci, from) in l.poll(Duration::from_millis(1)).unwrap_or_default() {
                let what = rci.describe();
                let command = match rci {
                    decdrm_mdi::RciCommand::Frequency(hz) => Some(Command::Tune(f64::from(hz) / 1000.0)),
                    decdrm_mdi::RciCommand::Service(id) => Some(Command::SelectService(id)),
                    decdrm_mdi::RciCommand::Demodulation(m) if m.starts_with("drm") => None,
                    _ => {
                        log(format!("remote control from {from}: {what}: not supported"), &mut snap);
                        continue;
                    }
                };
                log(format!("remote control from {from}: {what}"), &mut snap);
                if let Some(r) = snap.remote.as_mut() {
                    r.commands += 1;
                    r.last = Some(what);
                }
                remote_commands.extend(command);
            }
        }
        for c in cmd_rx.try_iter().chain(remote_commands) {
            match c {
                Command::Stop => {
                    if let Some(line) = finish_audio(&mut audio)? {
                        log(line, &mut snap);
                    }
                    if let Some(l) = logger.as_mut() {
                        l.finish(source.position_s(), &session, &snap)?;
                    }
                    snap.audio_spectrum = fresh_audio_spectrum(&audio, last_audio_s, source.position_s());
                    publish(shared, &mut snap, &mut session, &source, &audio, true);
                    return Ok(());
                }
                Command::Restart => {
                    session.restart();
                    log("receiver restarted".into(), &mut snap);
                }
                Command::SelectService(id) => {
                    session.select_service(id);
                    snap.selected_service = session.selected_service();
                    // An RSCI receiver decodes the service too (its audio status).
                    source.send_rci(&[decdrm_mdi::RciCommand::Service(id)]);
                }
                Command::SetVolume(gain) => audio.set_volume(gain),
                Command::SetMonitor(on) => {
                    if on != audio.monitoring() {
                        audio.set_monitor(on);
                        log(if on { "RF monitor on: the input plays".into() } else { "RF monitor off".into() }, &mut snap);
                    }
                }
                Command::StartRecording(path) => match audio.start_recording(path.clone()) {
                    Ok(()) => log(format!("recording the audio to {}", path.display()), &mut snap),
                    Err(e) => log(format!("{e:#}"), &mut snap),
                },
                Command::StopRecording => {
                    let stopped = audio.stop_recording();
                    if let Some(r) = audio.recording() {
                        log(format!("recording stopped: {}", r.describe()), &mut snap);
                    }
                    if let Err(e) = stopped {
                        log(format!("recording: {e:#}"), &mut snap);
                    }
                }
                Command::Tune(freq_khz) => {
                    if source.tune(freq_khz) {
                        session.new_station();
                        // Nothing of the previous station stays on show.
                        snap.services = session.service_views();
                        snap.selected_service = session.selected_service();
                        snap.text = None;
                        last_audio_s = None;
                        log(format!("tuning to {freq_khz:.3} kHz"), &mut snap);
                    } else {
                        log("only a KiwiSDR input or an RSCI receiver with an RCI address can be retuned".into(), &mut snap);
                    }
                }
            }
        }

        let read = source.read_input(chunk_frames);
        for line in source.take_log() {
            log(line, &mut snap);
        }
        let read = match read {
            Ok(r) => r,
            Err(e) => {
                // The input ended with an error (e.g. the KiwiSDR closed the connection):
                // publish the final state first, so the last snapshot is not up to a
                // publish interval behind, then report it.
                if let Some(line) = finish_audio(&mut audio)? {
                    log(line, &mut snap);
                }
                if let Some(l) = logger.as_mut() {
                    l.finish(source.position_s(), &session, &snap)?;
                }
                snap.audio_spectrum = fresh_audio_spectrum(&audio, last_audio_s, source.position_s());
                publish(shared, &mut snap, &mut session, &source, &audio, true);
                return Err(e);
            }
        };
        // The frames of every branch (one, or two for diversity reception); at the end
        // of the input, what diversity reception still holds back.
        let (events, ended) = match read {
            Some(Input::Samples(branches)) => {
                if let Some(frames) = branches.iter().find(|f| !f.is_empty()) {
                    let rms = (frames.iter().map(|v| v * v).sum::<f32>() / frames.len() as f32).sqrt();
                    snap.input.level_dbfs = Some(20.0 * rms.max(1e-9).log10());
                }
                if let Some(frames) = branches.first().filter(|f| !f.is_empty())
                    && let Err(e) = audio.push_monitor(frames, session.input_channels(0))
                {
                    log(format!("audio output error: {e:#}"), &mut snap);
                }
                let mut events = Vec::new();
                for (b, frames) in branches.iter().enumerate().filter(|(_, f)| !f.is_empty()) {
                    events.extend(session.push_branch(b, frames));
                }
                (events, false)
            }
            Some(Input::Mdi(frames)) => (frames.iter().flat_map(|f| session.push_mdi(f)).collect(), false),
            None => (session.flush(), true),
        };
        let t = source.position_s();
        for ev in events {
            if let Some(l) = logger.as_mut() {
                log_event(l, t, &ev)?;
            }
            match ev {
                SessionEvent::Log(l) => log(l, &mut snap),
                SessionEvent::Audio(pcm) => {
                    last_audio_s = Some(t);
                    if let Err(e) = audio.push(&pcm.samples, pcm.sample_rate, usize::from(pcm.channels)) {
                        log(format!("audio output error: {e:#}"), &mut snap);
                    }
                }
                SessionEvent::Text(t) => {
                    if let Some(t) = &t {
                        let _ = ev_tx.send(EngineEvent::Text(t.clone()));
                    }
                    snap.text = t;
                }
                SessionEvent::Data { short_id, event } => {
                    if let Some(s) = saver.as_mut()
                        && let Some(line) = s.save(short_id, &event)
                    {
                        log(line, &mut snap);
                    }
                    let _ = ev_tx.send(EngineEvent::Data { short_id, event });
                }
                SessionEvent::ServicesChanged => {
                    snap.services = session.service_views();
                    snap.selected_service = session.selected_service();
                }
            }
        }
        if ended {
            if let Some(line) = finish_audio(&mut audio)? {
                log(line, &mut snap);
            }
            if let Some(l) = logger.as_mut() {
                l.finish(source.position_s(), &session, &snap)?;
            }
            log(format!("end of input after {:.1} s", source.position_s()), &mut snap);
            snap.audio_spectrum = fresh_audio_spectrum(&audio, last_audio_s, source.position_s());
            publish(shared, &mut snap, &mut session, &source, &audio, true);
            return Ok(());
        }

        let st = &session.audio_stats;
        snap.push_metrics(MetricsSample::new(t, session.status(), &session.msc_stats, st.frames_ok, st.frames_concealed));
        if let Some(l) = logger.as_mut() {
            l.tick(t, &session, &snap)?;
        }
        if last_publish.elapsed() >= cfg.publish_interval {
            snap.audio_spectrum = fresh_audio_spectrum(&audio, last_audio_s, t);
            publish(shared, &mut snap, &mut session, &source, &audio, false);
            last_publish = Instant::now();
        }
        if realtime && !audio.is_playing() && !cfg.input.is_mdi() {
            // Pace to wall-clock time (with playback, the sound card paces us; an MDI
            // recording paces itself).
            let ahead = source.position_s() - started.elapsed().as_secs_f64();
            if ahead > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(ahead.min(0.2)));
            }
        }
    }
}

/// Let queued audio play out and complete a recording in progress; returns the log
/// line for that recording.
fn finish_audio(audio: &mut audio_out::AudioOut) -> Result<Option<String>> {
    let recording = audio.recording().is_some_and(|r| r.active);
    audio.finish()?;
    Ok(audio.recording().filter(|_| recording).map(|r| format!("recording saved: {}", r.describe())))
}

/// The audio spectrum to publish at input position `now_s`: blank unless audio was
/// decoded within the last [`AUDIO_SPECTRUM_HOLD_S`] seconds of input.
fn fresh_audio_spectrum(audio: &audio_out::AudioOut, last_audio_s: Option<f64>, now_s: f64) -> AudioSpectrum {
    if last_audio_s.is_some_and(|t| now_s - t <= AUDIO_SPECTRUM_HOLD_S) {
        audio.spectrum()
    } else {
        AudioSpectrum::default()
    }
}

/// Events worth a line in the JSON Lines log.
fn log_event(l: &mut logger::Logger, t: f64, ev: &SessionEvent) -> Result<()> {
    use decdrm_data::DataEvent;
    match ev {
        SessionEvent::Log(m) => l.event(t, "log", &[("message", m.clone())]),
        SessionEvent::Text(Some(m)) => l.event(t, "text", &[("text", m.clone())]),
        SessionEvent::Data { short_id, event } => {
            let obj = |kind: &str, name: &str, size: usize| {
                vec![("service", short_id.to_string()), ("object", kind.to_string()), ("name", name.to_string()), ("bytes", size.to_string())]
            };
            match event {
                DataEvent::SlideShowImage { name, data, .. } => l.event(t, "data", &obj("slide", name, data.len())),
                DataEvent::WebsiteFile { path, data, .. } => l.event(t, "data", &obj("website_file", path, data.len())),
                DataEvent::Epg { name, xml } => l.event(t, "data", &obj("epg", name, xml.len())),
                _ => Ok(()),
            }
        }
        _ => Ok(()),
    }
}

fn publish(
    shared: &Arc<Mutex<Snapshot>>,
    snap: &mut Snapshot,
    session: &mut Session,
    source: &Source,
    audio: &audio_out::AudioOut,
    finished: bool,
) {
    snap.seq += 1;
    snap.rx = session.status().clone();
    snap.channel = session.ensemble().channel().copied();
    snap.msc = session.msc_stats;
    snap.visuals = session.visuals();
    snap.input.info = source.info().clone();
    snap.input.position_s = source.position_s();
    snap.input.finished = finished;
    snap.input.kiwi = source.kiwi_status();
    snap.input.kiwi2 = source.kiwi_status_of(1);
    snap.input.mdi = source.mdi_status();
    snap.diversity = session.diversity();
    let st = &session.audio_stats;
    snap.audio.codec = st.codec.clone();
    snap.audio.frames_ok = st.frames_ok;
    snap.audio.frames_bad = st.frames_concealed;
    snap.audio.playing = audio.is_playing();
    snap.audio.recording = audio.recording();
    snap.audio.monitor = audio.monitoring();
    if let Some((buffered, ppm)) = audio.status() {
        snap.audio.buffer_ms = buffered.as_secs_f32() * 1000.0;
        snap.audio.drift_ppm = ppm;
    }
    snap.afs = afs::describe(session.ensemble().alternative_frequencies(), session.ensemble().time());
    snap.time = session.ensemble().time().map(BroadcastTime::from_sdc);
    snap.time_utc = session.ensemble().time().map(|t| {
        let (y, m, d) = t.date();
        format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", t.hour, t.minute)
    });
    if let Ok(mut s) = shared.lock() {
        *s = snap.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_follows_a_squared_law() {
        assert_eq!(volume_gain(100.0), 1.0);
        assert_eq!(volume_gain(0.0), 0.0);
        assert!((20.0 * volume_gain(50.0).log10() + 12.04).abs() < 0.01, "50 % is -12 dB");
        assert_eq!(volume_gain(150.0), 1.0);
        assert_eq!(volume_gain(f32::NAN), 1.0);
    }

    #[test]
    fn stale_audio_spectrum_is_blanked() {
        let mut audio = audio_out::AudioOut::new(false, None, false, None).unwrap();
        audio.push(&vec![0.25; audio_out::AUDIO_FFT_LEN], 48_000, 1).unwrap();
        assert!(fresh_audio_spectrum(&audio, None, 1.0).db.is_empty(), "no audio decoded yet");
        let fresh = fresh_audio_spectrum(&audio, Some(10.0), 10.0 + AUDIO_SPECTRUM_HOLD_S);
        assert_eq!(fresh.db.len(), audio_out::AUDIO_FFT_LEN / 2 + 1);
        assert!(fresh_audio_spectrum(&audio, Some(10.0), 10.1 + AUDIO_SPECTRUM_HOLD_S).db.is_empty(), "stale");
    }
}
