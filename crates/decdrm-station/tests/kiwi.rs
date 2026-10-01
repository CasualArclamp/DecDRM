//! Receiving through a KiwiSDR, end to end: a station's I/Q signal, brought down to a
//! Kiwi's 12 kHz and 16-bit samples, is served by the stand-in KiwiSDR
//! (`decdrm_kiwi::mock`), and the engine — with a `InputSpec::Kiwi` input — decodes it.

use decdrm_engine::decdrm_kiwi::mock::{MockConfig, MockEnd, MockKiwi, MockSession};
use decdrm_engine::decdrm_kiwi::{KiwiAddress, KiwiConfig, KiwiState};
use decdrm_engine::{Engine, EngineConfig, EngineEvent, InputSpec};
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
    iq12.chunks_exact(2).map(|p| (q(p[0]), q(p[1]))).collect()
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
