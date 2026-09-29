//! Sound-card tests. Only `device_listing_tolerates_zero_devices` runs by default; the rest
//! need hardware and are `#[ignore]`d — run them with
//! `cargo test -p decdrm-io --test soundcard -- --ignored --nocapture --test-threads=1`.

use std::time::{Duration, Instant};

use decdrm_io::{
    list_input_devices, list_output_devices, AudioPlayer, Direction, InputOptions, InputStream,
    OutputState, PlayerOptions,
};

#[test]
fn device_listing_tolerates_zero_devices() {
    for (dir, list) in [(Direction::Input, list_input_devices()), (Direction::Output, list_output_devices())] {
        match list {
            Ok(devices) => {
                println!("{} {dir} device(s)", devices.len());
                assert!(devices.iter().filter(|d| d.is_default).count() <= 1);
                for d in devices {
                    println!("  {}{}", d.name, if d.is_default { " [default]" } else { "" });
                    assert!(!d.name.is_empty());
                    assert_eq!(d.direction, dir);
                    for c in &d.configs {
                        assert!(c.channels > 0 && c.min_sample_rate <= c.max_sample_rate);
                    }
                }
            }
            // A headless machine may have no usable audio host at all.
            Err(e) => println!("{dir} devices unavailable: {e}"),
        }
    }
}

fn tone(freq: f64, rate: u32, frames: usize, amp: f32) -> Vec<f32> {
    (0..frames)
        .map(|n| amp * (2.0 * std::f64::consts::PI * freq * n as f64 / f64::from(rate)).sin() as f32)
        .collect()
}

#[test]
#[ignore = "needs a capture device"]
fn capture_from_default_input() {
    let mut input = InputStream::open(&InputOptions::default()).unwrap();
    let fmt = input.format();
    println!("capturing {fmt} from {}", input.device_name());
    let want = fmt.sample_rate as usize / 2;
    let block = input.read_blocking(want, Duration::from_secs(3)).unwrap();
    assert_eq!(block.len(), want * fmt.channels);
    assert!(block.iter().all(|s| s.is_finite()));
    let st = input.stats();
    println!("{st:?}");
    assert!(st.frames_captured >= want as u64);
    assert!(input.take_errors().is_empty());
}

#[test]
#[ignore = "needs a playback device (plays a quiet 440 Hz tone for 1 s)"]
fn play_tone_on_default_output() {
    let mut player = AudioPlayer::open(PlayerOptions {
        target_latency: Duration::from_millis(200),
        ..Default::default()
    })
    .unwrap();
    println!("playing on {} ({})", player.device_name(), player.device_format());
    let rate = 44_100; // deliberately not the device rate: exercises the resampler
    let t = tone(440.0, rate, rate as usize, 0.05);
    for block in t.chunks(4410) {
        player.push_blocking(block, rate, 1).unwrap();
    }
    assert!(player.drain(Duration::from_secs(3)));
    let st = player.status();
    println!("{st:?}");
    assert_eq!(st.underruns, 0);
    assert_eq!(st.dropped_frames, 0);
}

/// Finds a device whose name contains one of `candidates` (case-insensitive).
fn find(dir: Direction, env: &str, candidates: &[&str]) -> Option<String> {
    if let Ok(name) = std::env::var(env) {
        return Some(name);
    }
    let list = match dir {
        Direction::Input => list_input_devices().ok()?,
        Direction::Output => list_output_devices().ok()?,
    };
    candidates.iter().find_map(|c| {
        let c = c.to_lowercase();
        list.iter().find(|d| d.name.to_lowercase().starts_with(&c)).map(|d| d.name.clone())
    })
}

/// Plays a tone into a virtual audio cable and records it from the cable's other end.
/// Silent on the speakers. Override the devices with `DECDRM_LOOPBACK_OUT` (playback side)
/// and `DECDRM_LOOPBACK_IN` (capture side).
#[test]
#[ignore = "needs a virtual audio cable (e.g. VB-Audio CABLE)"]
fn loopback_through_virtual_cable() {
    let out_name = find(Direction::Output, "DECDRM_LOOPBACK_OUT", &["CABLE Input", "CABLE-A Input", "CABLE-B Input"]);
    let in_name = find(Direction::Input, "DECDRM_LOOPBACK_IN", &["CABLE Output", "CABLE-A Output", "CABLE-B Output"]);
    let (Some(out_name), Some(in_name)) = (out_name, in_name) else {
        eprintln!("skipping: no virtual audio cable found");
        return;
    };
    // Pair the ends of the same cable ("CABLE-A Input" <-> "CABLE-A Output").
    println!("loopback: {out_name}  ->  {in_name}");

    let mut input = InputStream::open(&InputOptions {
        device: Some(in_name),
        buffer: Duration::from_secs(10),
        ..Default::default()
    })
    .unwrap();
    let in_fmt = input.format();
    // Make sure nobody else is using the cable right now.
    let idle = input.read_blocking(in_fmt.sample_rate as usize / 4, Duration::from_secs(2)).unwrap();
    let idle_peak = idle.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    if idle_peak > 1e-3 {
        eprintln!("skipping: the cable is carrying audio (peak {idle_peak})");
        return;
    }

    let src_rate = 44_100u32; // exercises resampling and mono -> stereo upmix
    let freq = 1000.0;
    let signal = tone(freq, src_rate, (src_rate as f64 * 1.5) as usize, 0.25);

    let mut captured = Vec::new();
    let started = Instant::now();
    let player_status = std::thread::scope(|s| {
        // The player is `Send`: it is created here and moved into the producer thread.
        let mut player = AudioPlayer::open(PlayerOptions {
            device: Some(out_name.clone()),
            target_latency: Duration::from_millis(200),
            drift_compensation: false,
            ..Default::default()
        })
        .unwrap();
        println!("player device format: {}", player.device_format());
        let producer = s.spawn(move || {
            for block in signal.chunks(2205) {
                player.push_blocking(block, src_rate, 1).unwrap();
            }
            assert!(player.drain(Duration::from_secs(3)));
            player.status()
        });
        // Meanwhile, this thread records.
        while !producer.is_finished() || started.elapsed() < Duration::from_secs(1) {
            captured.extend(input.read_blocking(4800, Duration::from_millis(200)).unwrap());
        }
        captured.extend(input.read_blocking(in_fmt.sample_rate as usize / 2, Duration::from_secs(1)).unwrap());
        producer.join().unwrap()
    });
    println!("player: {player_status:?}");
    println!("input: {:?}", input.stats());
    assert_eq!(player_status.underruns, 0);
    assert_eq!(input.stats().overruns, 0);

    // Analyse the left channel of the recording: find the tone and measure it.
    let left: Vec<f32> = captured.chunks_exact(in_fmt.channels).map(|f| f[0]).collect();
    let rate = f64::from(in_fmt.sample_rate);
    let first = left.iter().position(|s| s.abs() > 0.05).expect("tone not captured");
    let last = left.iter().rposition(|s| s.abs() > 0.05).expect("tone not captured");
    println!(
        "signal captured from {:.3} s to {:.3} s ({:.3} s; tone is 1.5 s)",
        first as f64 / rate,
        last as f64 / rate,
        (last - first) as f64 / rate
    );
    // Anchor the window to the END of the tone: some virtual cables replay a few hundred ms
    // of stale buffer content when a new playback client connects, which would fool an
    // onset detector. Take one second of steady tone ending 0.1 s before the tone stops.
    let end = last.saturating_sub(in_fmt.sample_rate as usize / 10);
    let start = end.checked_sub(in_fmt.sample_rate as usize).expect("tone shorter than 1.1 s");
    let seg = &left[start..end];
    let w = 2.0 * std::f64::consts::PI * freq / rate;
    let (mut ss, mut sc, mut cc, mut xs, mut xc) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (n, &v) in seg.iter().enumerate() {
        let (s, c) = (w * n as f64).sin_cos();
        ss += s * s;
        sc += s * c;
        cc += c * c;
        xs += f64::from(v) * s;
        xc += f64::from(v) * c;
    }
    let det = ss * cc - sc * sc;
    let (a, b) = ((xs * cc - xc * sc) / det, (xc * ss - xs * sc) / det);
    let amp = (a * a + b * b).sqrt();
    let resid = (seg
        .iter()
        .enumerate()
        .map(|(n, &v)| {
            let (s, c) = (w * n as f64).sin_cos();
            (f64::from(v) - (a * s + b * c)).powi(2)
        })
        .sum::<f64>()
        / seg.len() as f64)
        .sqrt();
    let snr = 20.0 * (amp / std::f64::consts::SQRT_2 / resid).log10();
    println!("captured tone: amplitude {amp:.4} (sent 0.25), SNR {snr:.1} dB");
    // Diagnostics: track the tone's phase in 5 ms windows; a jump means samples were lost or
    // inserted somewhere between the player and the recorder.
    let win = in_fmt.sample_rate as usize / 200;
    let mut prev: Option<f64> = None;
    for (k, chunk) in left[first + win * 4..last - win * 4].chunks_exact(win).enumerate() {
        let n0 = first + win * 4 + k * win;
        let (mut xs, mut xc) = (0.0, 0.0);
        for (i, &v) in chunk.iter().enumerate() {
            let (s, c) = (w * (n0 + i) as f64).sin_cos();
            xs += f64::from(v) * s;
            xc += f64::from(v) * c;
        }
        let phase = xc.atan2(xs);
        if let Some(p) = prev {
            let mut d = phase - p;
            while d > std::f64::consts::PI {
                d -= 2.0 * std::f64::consts::PI;
            }
            while d < -std::f64::consts::PI {
                d += 2.0 * std::f64::consts::PI;
            }
            if d.abs() > 0.05 {
                println!(
                    "  phase jump of {:+.1} samples at {:.3} s",
                    d / w,
                    n0 as f64 / rate
                );
            }
        }
        prev = Some(phase);
    }
    // The cable's volume slider scales the level; the waveform must be a clean 1 kHz tone.
    assert!(amp > 0.02, "tone too weak: {amp}");
    assert!(snr > 60.0, "tone distorted or wrong frequency: SNR {snr:.1} dB");
    assert_eq!(player_status.state, OutputState::Buffering);
}
