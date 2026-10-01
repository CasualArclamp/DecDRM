//! Receiving through a KiwiSDR, end to end: a station's I/Q signal, brought down to a
//! Kiwi's 12 kHz and 16-bit samples, is served by the stand-in KiwiSDR
//! (`decdrm_kiwi::mock`), and the engine — with a `InputSpec::Kiwi` input — decodes it.

use decdrm_engine::decdrm_kiwi::mock::{MockConfig, MockEnd, MockKiwi, MockSession};
use decdrm_engine::decdrm_kiwi::{KiwiAddress, KiwiConfig, KiwiState};
use decdrm_engine::{Command, Engine, EngineConfig, EngineEvent, InputSpec, Snapshot};
use decdrm_io::{FileReader, Resampler, ResamplerQuality};
use decdrm_station::{Station, StationConfig};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 20 s of a mode B, 10 kHz, 64-QAM station (HE-AAC tone with a text message) as I/Q
/// at the Kiwi's 12 kHz, 16-bit.
fn kiwi_signal() -> Vec<(i16, i16)> {
    let dir = tempfile::tempdir().unwrap();
    let toml = r#"
        [channel]
        mode = "B"
        occupancy = 3
        msc_mode = "64-QAM"
        [output]
        file = "kiwi.wav"
        format = "iq"
        [time]
        start = "2026-10-01T06:00:00Z"
        [[service]]
        label = "Kiwi Test"
        id = 0xD0D0C1
        [service.audio]
        codec = "he-aac"
        core_rate = 12000
        text = ["Hello KiwiSDR"]
        input = { tone_hz = 700.0 }
    "#;
    let mut cfg = StationConfig::from_toml_str(toml).unwrap();
    cfg.base_dir = Some(dir.path().to_path_buf());
    let mut station = Station::new(cfg).unwrap_or_else(|e| panic!("{e}"));
    station.run_frames(50).unwrap();
    station.finish().unwrap();

    let mut reader = FileReader::open(dir.path().join("kiwi.wav")).unwrap();
    let mut iq48 = Vec::new();
    while let Some(v) = reader.read(48_000).unwrap() {
        iq48.extend(v);
    }
    let mut down = Resampler::new(48_000, 12_000, 2, ResamplerQuality::High).unwrap();
    let mut iq12 = down.process(&iq48);
    iq12.extend(down.flush());
    let q = |v: f32| (v * 32767.0).round().clamp(-32768.0, 32767.0) as i16;
    iq12.as_chunks::<2>().0.iter().map(|&[i, qv]| (q(i), q(qv))).collect()
}

/// Collect the engine's log until `done` holds for a snapshot or `limit` passes.
fn run_until(engine: &Engine, log: &mut Vec<String>, limit: Duration, done: impl Fn(&Snapshot) -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(EngineEvent::Log(l)) = engine.recv_event(Duration::from_millis(100)) {
            log.push(l);
        }
        if done(&engine.snapshot()) {
            return true;
        }
    }
    false
}

/// A retune keeps the connection, tells the Kiwi, and starts the receiver afresh; the
/// stand-in serves the same station on every frequency, so it is found again.
#[test]
fn retuning_keeps_the_connection_and_starts_afresh() {
    let kiwi = MockKiwi::start(MockConfig {
        iq: Arc::new(kiwi_signal()),
        // In real time, as a Kiwi sends, so the signal goes on while the retune settles.
        paced: true,
        sessions: vec![MockSession::Stream { blocks: None, end: MockEnd::Wait }],
        ..MockConfig::default()
    })
    .unwrap();
    let input = InputSpec::Kiwi(KiwiConfig::new(KiwiAddress::parse(&kiwi.address()).unwrap(), 6140.0));
    let engine = Engine::start(EngineConfig { input, ..EngineConfig::default() });
    let mut log = Vec::new();
    let decoding = |s: &Snapshot| s.audio.frames_ok >= 10 && s.services.iter().any(|v| v.label == "Kiwi Test");
    assert!(run_until(&engine, &mut log, Duration::from_secs(60), decoding), "{log:#?}");

    engine.command(Command::Tune(7325.0));
    // At once: nothing of the old station on show, the new frequency in the status.
    let fresh = |s: &Snapshot| {
        s.services.is_empty() && s.audio.frames_ok == 0 && s.input.kiwi.as_ref().is_some_and(|k| k.freq_khz == 7325.0)
    };
    assert!(run_until(&engine, &mut log, Duration::from_secs(10), fresh), "{log:#?}");
    assert!(run_until(&engine, &mut log, Duration::from_secs(60), decoding), "{log:#?}");
    let snap = engine.snapshot();
    drop(engine);
    for l in &log {
        println!("{l}");
    }
    assert!(snap.input.info.name.ends_with("at 7325.000 kHz"), "{}", snap.input.info.name);
    let at = |text: &str| log.iter().position(|l| l.contains(text)).unwrap_or_else(|| panic!("no {text:?} in {log:#?}"));
    let tuned = at("tuning to 7325.000 kHz");
    assert!(at("KiwiSDR: retuned to 7325.000 kHz") > tuned);
    assert!(log[tuned..].iter().any(|l| l.contains("signal found")), "{log:#?}");
    assert!(kiwi.commands().iter().any(|c| c == "SET mod=iq low_cut=-5000 high_cut=5000 freq=7325.000"));
    assert_eq!(kiwi.connections(), 1, "retuned on the same connection");
}

/// Diversity reception through two stand-in KiwiSDRs serving the same station with
/// independent noise: both connect, the frames are combined, and audio decodes.
#[test]
fn diversity_through_two_kiwisdrs() {
    let clean = kiwi_signal();
    let blocks = clean.len() / 512;
    // Each Kiwi's own noise, ~20 dB below the signal.
    let noisy = |seed: u64| -> Vec<(i16, i16)> {
        let mut rng = decdrm_core::channel::Rng::new(seed);
        let rms = (clean.iter().map(|&(i, q)| f64::from(i).powi(2) + f64::from(q).powi(2)).sum::<f64>() / clean.len() as f64).sqrt();
        let sigma = rms * 0.1 / std::f64::consts::SQRT_2;
        let q = |v: f64| v.round().clamp(-32768.0, 32767.0) as i16;
        clean.iter().map(|&(i, qd)| (q(f64::from(i) + sigma * rng.gaussian()), q(f64::from(qd) + sigma * rng.gaussian()))).collect()
    };
    let start = |iq: Vec<(i16, i16)>, name: &str| {
        MockKiwi::start(MockConfig {
            iq: Arc::new(iq),
            name: Some(name.into()),
            sessions: vec![MockSession::Stream { blocks: Some(blocks), end: MockEnd::Close }],
            ..MockConfig::default()
        })
        .unwrap()
    };
    let (a, b) = (start(noisy(1), "Kiwi A"), start(noisy(2), "Kiwi B"));
    let spec = |k: &MockKiwi| InputSpec::Kiwi(KiwiConfig::new(KiwiAddress::parse(&k.address()).unwrap(), 6140.0));
    let input = InputSpec::Diversity(Box::new([spec(&a), spec(&b)]));
    let engine = Engine::start(EngineConfig { input, ..EngineConfig::default() });
    let started = Instant::now();
    let (mut log, mut error) = (Vec::new(), None);
    while started.elapsed() < Duration::from_secs(90) {
        match engine.recv_event(Duration::from_millis(100)) {
            Some(EngineEvent::Log(l)) => log.push(l),
            Some(EngineEvent::Stopped { error: e }) => {
                error = e;
                break;
            }
            _ => {}
        }
    }
    let snap = engine.snapshot();
    for l in &log {
        println!("{l}");
    }
    let d = snap.diversity.clone().expect("diversity view");
    println!("audio {} ok / {} concealed; {:?}; ended: {error:?}", snap.audio.frames_ok, snap.audio.frames_bad, d.stats);
    // Both stand-ins close after the signal: both branches end, with their reasons.
    assert!(error.as_deref().is_some_and(|e| e.contains("closed the connection")), "{error:?}");
    assert!(log.iter().any(|l| l.starts_with("KiwiSDR 1:")) && log.iter().any(|l| l.starts_with("KiwiSDR 2:")), "{log:?}");
    assert!(log.iter().any(|l| l.contains("branch 2: signal found")), "{log:?}");
    let (k1, k2) = (snap.input.kiwi.expect("first Kiwi"), snap.input.kiwi2.expect("second Kiwi"));
    assert_eq!((k1.name.as_deref(), k2.name.as_deref()), (Some("Kiwi A"), Some("Kiwi B")));
    assert!(d.stats.combined >= 30, "{:?}", d.stats);
    assert!(d.branches.iter().all(|s| s.snr_db.is_some_and(|x| x > 10.0)), "{:?}", d.branches.map(|s| s.snr_db));
    assert!(snap.audio.frames_ok >= 150 && snap.audio.frames_bad <= 2, "{} ok, {} concealed", snap.audio.frames_ok, snap.audio.frames_bad);
    assert_eq!(snap.services.first().map(|s| s.label.as_str()), Some("Kiwi Test"));
}

#[test]
fn decodes_a_station_through_a_kiwisdr() {
    let iq = kiwi_signal();
    let blocks = iq.len() / 512;
    println!("serving {:.1} s of I/Q in {blocks} blocks", iq.len() as f64 / 12_000.0);
    let kiwi = MockKiwi::start(MockConfig {
        iq: Arc::new(iq),
        // A real Kiwi's rate: the engine resamples from 12001 Hz, the receiver tracks
        // the remaining 83 ppm.
        sample_rate: 12_001.135,
        name: Some("Mock Kiwi".into()),
        sessions: vec![MockSession::Stream { blocks: Some(blocks), end: MockEnd::Close }],
        ..MockConfig::default()
    })
    .unwrap();

    let input = InputSpec::Kiwi(KiwiConfig::new(KiwiAddress::parse(&kiwi.address()).unwrap(), 6140.0));
    // The receiver's format setting says "real": a Kiwi input is I/Q regardless.
    let engine = Engine::start(EngineConfig { input, ..EngineConfig::default() });
    let started = Instant::now();
    let (mut log, mut texts, mut error) = (Vec::new(), Vec::new(), None);
    let mut kiwi_status = None;
    while started.elapsed() < Duration::from_secs(60) {
        match engine.recv_event(Duration::from_millis(100)) {
            Some(EngineEvent::Log(l)) => log.push(l),
            Some(EngineEvent::Text(t)) => texts.push(t),
            Some(EngineEvent::Stopped { error: e }) => {
                error = e;
                break;
            }
            _ => {}
        }
        if let Some(k) = engine.snapshot().input.kiwi {
            kiwi_status = Some(k);
        }
    }
    let snap = engine.snapshot();
    for l in &log {
        println!("{l}");
    }
    println!(
        "{:.1} s of signal; audio {} ok / {} concealed; texts {texts:?}; ended: {error:?}",
        snap.input.position_s, snap.audio.frames_ok, snap.audio.frames_bad
    );
    // The stand-in closes the connection after the signal, as a Kiwi's time limit does.
    assert!(error.as_deref().is_some_and(|e| e.contains("closed the connection")), "{error:?}");
    assert!(log.iter().any(|l| l.contains("KiwiSDR: streaming")), "{log:?}");
    assert!(log.iter().any(|l| l.contains("\"Mock Kiwi\"")), "{log:?}");
    let k = kiwi_status.expect("Kiwi status in the snapshot");
    assert_eq!(k.sample_rate, Some(12_001.135));
    assert!(matches!(k.state, KiwiState::Streaming | KiwiState::Failed));
    assert_eq!(snap.input.info.sample_rate, 12_001);
    assert!(snap.input.position_s > 19.0, "{} s", snap.input.position_s);
    // HE-AAC with a 12 kHz core: 5 frames per 400 ms; acquisition takes the first seconds.
    assert!(snap.audio.frames_ok >= 150, "{} audio frames", snap.audio.frames_ok);
    assert!(snap.audio.frames_bad <= 2, "{} concealed", snap.audio.frames_bad);
    assert!(texts.iter().any(|t| t == "Hello KiwiSDR"), "{texts:?}");
    let label = snap.services.first().map(|s| s.label.clone());
    assert_eq!(label.as_deref(), Some("Kiwi Test"));
}
