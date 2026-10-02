//! Recording the received audio through the engine's commands, end to end: a station
//! writes a few seconds of signal, the engine receives it in real time (as a live input
//! arrives), and `Command::StartRecording` / `Command::StopRecording` record the
//! decoded audio while it plays.

use decdrm_engine::{Command, Engine, EngineConfig, EngineEvent, InputSpec, Snapshot};
use decdrm_io::FileReader;
use decdrm_station::{Station, StationConfig};
use std::time::{Duration, Instant};

/// Collect the engine's log until `done` holds for a snapshot or `limit` passes.
fn run_until(engine: &Engine, log: &mut Vec<String>, limit: Duration, done: impl Fn(&Snapshot) -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(EngineEvent::Log(l)) = engine.recv_event(Duration::from_millis(50)) {
            log.push(l);
        }
        if done(&engine.snapshot()) {
            return true;
        }
    }
    false
}

#[test]
fn recording_the_received_audio() {
    let dir = tempfile::tempdir().unwrap();
    let toml = r#"
        [channel]
        mode = "B"
        occupancy = 3
        [output]
        file = "signal.wav"
        [[service]]
        label = "Recorder Test"
        id = 0xD0D0C7
        [service.audio]
        codec = "aac"
        core_rate = 24000
        input = { tone_hz = 1000.0 }
    "#;
    let mut cfg = StationConfig::from_toml_str(toml).unwrap();
    cfg.base_dir = Some(dir.path().to_path_buf());
    let mut station = Station::new(cfg).unwrap_or_else(|e| panic!("{e}"));
    station.run_frames(15).unwrap();
    station.finish().unwrap();

    let input = InputSpec::File { path: dir.path().join("signal.wav"), realtime: true };
    let engine = Engine::start(EngineConfig { input, ..EngineConfig::default() });
    let rec = dir.path().join("received.wav");
    // Before the signal is found: the file opens with the first audio.
    engine.command(Command::StartRecording(rec.clone()));
    let mut log = Vec::new();
    let recorded = |s: &Snapshot| s.audio.recording.as_ref().is_some_and(|r| r.seconds >= 1.5);
    assert!(run_until(&engine, &mut log, Duration::from_secs(30), recorded), "{log:#?}");
    engine.command(Command::StopRecording);
    let stopped = |s: &Snapshot| s.audio.recording.as_ref().is_some_and(|r| !r.active);
    assert!(run_until(&engine, &mut log, Duration::from_secs(10), stopped), "{log:#?}");
    let r = engine.snapshot().audio.recording.expect("the recording's final state");
    // The receiver goes on after the recording has stopped.
    let ended = |s: &Snapshot| s.stopped;
    assert!(run_until(&engine, &mut log, Duration::from_secs(30), ended), "{log:#?}");
    let last = engine.snapshot();
    drop(engine);
    for l in &log {
        println!("{l}");
    }

    assert_eq!(r.files, std::slice::from_ref(&rec));
    assert_eq!(r.error, None);
    assert!(log.iter().any(|l| l == &format!("recording the audio to {}", rec.display())), "{log:#?}");
    assert!(log.iter().any(|l| l.starts_with("recording stopped: ") && l.ends_with("s of audio in received.wav")), "{log:#?}");
    assert_eq!(last.audio.recording.as_ref().map(|r| r.seconds), Some(r.seconds), "nothing recorded after the stop");
    assert!(last.audio.frames_ok as f64 * 1024.0 / 24_000.0 > r.seconds + 0.5, "decoding went on");

    let mut reader = FileReader::open(&rec).unwrap();
    assert_eq!((reader.format().sample_rate, reader.format().channels), (24_000, 1));
    let frames = reader.total_frames().unwrap();
    assert!((frames as f64 / 24_000.0 - r.seconds).abs() < 1e-6, "{frames} frames for {} s", r.seconds);
    let mut audio = Vec::new();
    while let Some(block) = reader.read(24_000).unwrap() {
        audio.extend(block);
    }
    // The 1 kHz tone: about 2000 zero crossings a second, at the tone's level.
    let crossings = audio.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
    let per_second = crossings as f64 / r.seconds;
    assert!((1900.0..2100.0).contains(&per_second), "{per_second:.0} zero crossings per second");
    let rms = (audio.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / audio.len() as f64).sqrt();
    assert!(rms > 0.05, "rms {rms}");
}
