//! The RF monitor through a real (virtual) sound card: the engine plays an I/Q file's
//! samples instead of the decoded audio, I on the left and Q on the right. Never run
//! automatically: it plays to `DECDRM_MONITOR_OUT` (default "CABLE-A Input", a VB-Audio
//! virtual cable) and records `DECDRM_MONITOR_IN` (default "CABLE-A Output", the
//! cable's other end), so nothing is heard.
//!
//! ```text
//! cargo test --release -p decdrm-engine --test monitor -- --ignored --nocapture
//! ```

use decdrm_core::rx::{InputFormat, ReceiverConfig};
use decdrm_engine::{Engine, EngineConfig, EngineEvent, InputSpec};
use decdrm_io::{AudioFormat, Container, Encoding, FileWriter, InputOptions, InputStream};
use std::f32::consts::TAU;
use std::time::{Duration, Instant};

/// Frequency with the most power between `lo` and `hi` Hz (10 Hz steps).
fn dominant(x: &[f32], rate: f32, lo: f32, hi: f32) -> f32 {
    let power = |f: f32| {
        let (mut c, mut s) = (0.0f64, 0.0f64);
        for (i, &v) in x.iter().enumerate() {
            let ph = TAU * f * i as f32 / rate;
            c += f64::from(v * ph.cos());
            s += f64::from(v * ph.sin());
        }
        c * c + s * s
    };
    let mut best = (lo, 0.0);
    let mut f = lo;
    while f <= hi {
        let p = power(f);
        if p > best.1 {
            best = (f, p);
        }
        f += 10.0;
    }
    best.0
}

#[test]
#[ignore = "plays to a sound card: DECDRM_MONITOR_OUT / DECDRM_MONITOR_IN (default CABLE-A Input / Output)"]
fn monitor_plays_the_iq_input() {
    let out = std::env::var("DECDRM_MONITOR_OUT").unwrap_or_else(|_| "CABLE-A Input".into());
    let inp = std::env::var("DECDRM_MONITOR_IN").unwrap_or_else(|_| "CABLE-A Output".into());
    // 5 s of I/Q at 48 kHz: 1 kHz on I, 3 kHz on Q (no DRM signal: nothing decodes).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("iq.wav");
    let mut w = FileWriter::create(&path, AudioFormat::new(48_000, 2), Container::Wav, Encoding::Float32).unwrap();
    let frames: Vec<f32> = (0..5 * 48_000)
        .flat_map(|i| {
            let t = i as f32 / 48_000.0;
            [0.3 * (TAU * 1000.0 * t).sin(), 0.3 * (TAU * 3000.0 * t).sin()]
        })
        .collect();
    w.write(&frames).unwrap();
    w.finalize().unwrap();

    // Record the cable's other end while the engine plays.
    let opts = InputOptions { device: Some(inp.clone()), sample_rate: None, channels: None, buffer: Duration::from_secs(2) };
    let mut capture = InputStream::open(&opts).unwrap();
    let fmt = capture.format();
    let recorder = std::thread::spawn(move || {
        let mut got = Vec::new();
        let until = Instant::now() + Duration::from_secs(7);
        while Instant::now() < until {
            got.extend(capture.read_blocking(4096, Duration::from_secs(2)).unwrap());
        }
        got
    });

    let cfg = EngineConfig {
        input: InputSpec::File { path: path.clone(), realtime: true },
        receiver: ReceiverConfig { input: InputFormat::Iq { swap: false }, ..ReceiverConfig::default() },
        play_audio: true,
        output_device: Some(out.clone()),
        monitor: true,
        ..EngineConfig::default()
    };
    let engine = Engine::start(cfg);
    let started = Instant::now();
    loop {
        match engine.recv_event(Duration::from_secs(20)) {
            Some(EngineEvent::Stopped { error }) => {
                assert!(error.is_none(), "{error:?}");
                break;
            }
            Some(_) => {}
            None => panic!("no end of input within 20 s"),
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    assert!(engine.snapshot().audio.monitor, "the snapshot shows the monitor");
    let got = recorder.join().unwrap();
    println!("played 5 s of I/Q through {out} in {elapsed:.1} s; recorded {} frames of {fmt} from {inp}", got.len() / fmt.channels);
    assert!(elapsed > 4.0, "the sound card paces the file ({elapsed:.1} s)");

    // The loudest second of the recording: I on the left, Q on the right.
    let ch = fmt.channels;
    assert!(ch >= 2, "{inp} delivers {ch} channel(s)");
    let rate = fmt.sample_rate as usize;
    let side = |c: usize, from: usize| -> Vec<f32> { got.chunks_exact(ch).skip(from).take(rate).map(|f| f[c]).collect() };
    let rms = |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt();
    let from = (0..got.len() / ch / rate).map(|s| s * rate).max_by(|&a, &b| rms(&side(0, a)).total_cmp(&rms(&side(0, b)))).unwrap();
    let (left, right) = (side(0, from), side(1, from));
    let (fl, fr) = (dominant(&left, rate as f32, 500.0, 4000.0), dominant(&right, rate as f32, 500.0, 4000.0));
    println!("left {fl} Hz at {:.1} dBFS, right {fr} Hz at {:.1} dBFS", 20.0 * rms(&left).log10(), 20.0 * rms(&right).log10());
    assert_eq!((fl, fr), (1000.0, 3000.0), "I left, Q right");
    assert!(rms(&left) > 0.1 && rms(&right) > 0.1, "played at the input's level");
}
