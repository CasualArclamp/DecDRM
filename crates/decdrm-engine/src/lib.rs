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
pub use logger::{LogConfig, LogFormat};
pub use session::{MscStats, Session, SessionEvent};
pub use snapshot::{
    AppView, AudioCodingView, AudioSpectrum, AudioStatus, BroadcastTime, InputStatus, METRICS_INTERVAL_S, MetricsSample,
    RECENT_METRICS, ServiceView, Snapshot,
};
pub use source::{InputSpec, Source, SourceInfo};

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
    /// Write decoded audio to this WAV/FLAC file.
    pub record_audio: Option<std::path::PathBuf>,
    /// Directory for slideshow images, websites, EPG and raw data.
    pub data_dir: Option<std::path::PathBuf>,
    /// Reception log (metrics rows as CSV or JSON Lines, events in JSON Lines).
    pub log: Option<LogConfig>,
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
    /// receiver starts afresh (another station). Other inputs ignore it.
    Tune(f64),
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
    if cfg.input.is_iq() {
        rcfg.input = InputFormat::Iq { swap: false };
    }
    let mut session = Session::new(rcfg);
    // File playback paces the decoder to the sound card; live input relies on the
    // player's drift compensation.
    let mut audio = audio_out::AudioOut::new(cfg.play_audio, cfg.output_device.clone(), info.is_file, cfg.record_audio.clone())?;
    audio.set_volume(cfg.volume);
    let mut saver = cfg.data_dir.clone().map(data_store::DataStore::new);
    let mut logger = cfg.log.as_ref().map(logger::Logger::create).transpose()?;
    let log = |line: String, snap: &mut Snapshot| {
        let _ = ev_tx.send(EngineEvent::Log(line.clone()));
        snap.push_log(line);
    };

    let mut snap = Snapshot::default();
    snap.input.info = info.clone();
    if cfg.input.is_iq() && !info.is_file {
        log(format!("input: {} (I/Q)", info.name), &mut snap);
    } else {
        log(format!("input: {} ({} Hz, {} ch)", info.name, info.sample_rate, info.channels), &mut snap);
    }

    let started = Instant::now();
    let mut last_publish = Instant::now() - Duration::from_secs(1);
    // Input position of the latest decoded audio (for blanking a stale audio spectrum).
    let mut last_audio_s: Option<f64> = None;
    let chunk_frames = (info.sample_rate as usize / 20).max(256); // 50 ms
    loop {
        for c in cmd_rx.try_iter() {
            match c {
                Command::Stop => {
                    audio.finish()?;
                    if let Some(l) = logger.as_mut() {
                        l.finish(source.position_s(), &session, &snap)?;
                    }
                    snap.audio_spectrum = fresh_audio_spectrum(&audio, last_audio_s, source.position_s());
                    publish(shared, &mut snap, &session, &source, &audio, true);
                    return Ok(());
                }
                Command::Restart => {
                    session.restart();
                    log("receiver restarted".into(), &mut snap);
                }
                Command::SelectService(id) => {
                    session.select_service(id);
                    snap.selected_service = session.selected_service();
                }
                Command::SetVolume(gain) => audio.set_volume(gain),
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
                        log("only a KiwiSDR input can be retuned".into(), &mut snap);
                    }
                }
            }
        }

        let read = source.read(chunk_frames);
        for line in source.take_log() {
            log(line, &mut snap);
        }
        let Some(frames) = read? else {
            audio.finish()?;
            if let Some(l) = logger.as_mut() {
                l.finish(source.position_s(), &session, &snap)?;
            }
            log(format!("end of input after {:.1} s", source.position_s()), &mut snap);
            snap.audio_spectrum = fresh_audio_spectrum(&audio, last_audio_s, source.position_s());
            publish(shared, &mut snap, &session, &source, &audio, true);
            return Ok(());
        };
        if !frames.is_empty() {
            let rms = (frames.iter().map(|v| v * v).sum::<f32>() / frames.len() as f32).sqrt();
            snap.input.level_dbfs = Some(20.0 * rms.max(1e-9).log10());
        }
        let t = source.position_s();
        for ev in session.push(&frames) {
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

        let st = &session.audio_stats;
        snap.push_metrics(MetricsSample::new(t, session.status(), &session.msc_stats, st.frames_ok, st.frames_concealed));
        if let Some(l) = logger.as_mut() {
            l.tick(t, &session, &snap)?;
        }
        if last_publish.elapsed() >= Duration::from_millis(100) {
            snap.audio_spectrum = fresh_audio_spectrum(&audio, last_audio_s, t);
            publish(shared, &mut snap, &session, &source, &audio, false);
            last_publish = Instant::now();
        }
        if realtime && !audio.is_playing() {
            // Pace to wall-clock time (with playback, the sound card paces us).
            let ahead = source.position_s() - started.elapsed().as_secs_f64();
            if ahead > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(ahead.min(0.2)));
            }
        }
    }
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
    session: &Session,
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
    let st = &session.audio_stats;
    snap.audio.codec = st.codec.clone();
    snap.audio.frames_ok = st.frames_ok;
    snap.audio.frames_bad = st.frames_concealed;
    snap.audio.playing = audio.is_playing();
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
