//! Reads the user's DRM recordings in `<workspace>/samples`. Skipped (with a message, not a
//! failure) when the directory or an individual file is absent, since the recordings are not
//! committed.

use std::path::{Path, PathBuf};

use decdrm_io::{AudioFormat, FileReader, To48k};

fn samples_dir() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples");
    if dir.is_dir() {
        Some(dir)
    } else {
        eprintln!("skipping: {} not found", dir.display());
        None
    }
}

struct Expect {
    name: &'static str,
    rate: u32,
    channels: usize,
    bits: u32,
    /// Duration in seconds as stated by the recording (checked to ±5 ms).
    seconds: f64,
}

const EXPECTED: &[Expect] = &[
    Expect { name: "DW_ModeB_10kHz.flac", rate: 48_000, channels: 1, bits: 16, seconds: 44.068 },
    Expect { name: "FMGold_xHE_ModeB_9khz.flac", rate: 44_100, channels: 1, bits: 16, seconds: 64.728 },
    Expect {
        name: "Test_Mode_B_10kHz_IQ_Pos_26dB_SNR.flac",
        rate: 24_000,
        channels: 2,
        bits: 8,
        seconds: 67.219,
    },
    Expect { name: "Opus_Codec_Test_Mode_B_10kHz.flac", rate: 48_000, channels: 1, bits: 8, seconds: 231.322 },
];

/// Decodes a whole file block by block; returns (frames, peak, rms).
fn decode_all(reader: &mut FileReader, block: usize) -> (u64, f32, f64) {
    let ch = reader.format().channels;
    let (mut frames, mut peak, mut energy) = (0u64, 0.0f32, 0.0f64);
    while let Some(b) = reader.read(block).expect("decode error") {
        assert_eq!(b.len() % ch, 0);
        frames += (b.len() / ch) as u64;
        for &s in &b {
            assert!((-1.0..1.0).contains(&s), "sample {s} outside [-1, 1)");
            peak = peak.max(s.abs());
            energy += f64::from(s) * f64::from(s);
        }
    }
    (frames, peak, (energy / (frames.max(1) as f64 * ch as f64)).sqrt())
}

#[test]
fn recordings_have_expected_format_and_length() {
    let Some(dir) = samples_dir() else { return };
    for e in EXPECTED {
        let path = dir.join(e.name);
        if !path.is_file() {
            eprintln!("skipping {}: not present", e.name);
            continue;
        }
        let mut r = FileReader::open(&path).unwrap();
        assert_eq!(r.format(), AudioFormat::new(e.rate, e.channels), "{}", e.name);
        assert_eq!(r.bits_per_sample(), Some(e.bits), "{}", e.name);
        assert_eq!(r.container(), "flac");
        let total = r.total_frames().expect("FLAC states its length");
        let secs = r.duration().unwrap().as_secs_f64();
        assert!((secs - e.seconds).abs() < 0.005, "{}: {secs} s", e.name);

        let (frames, peak, rms) = decode_all(&mut r, 4800);
        assert_eq!(frames, total, "{}: decoded frame count", e.name);
        assert_eq!(r.decode_errors(), 0, "{}", e.name);
        // A real signal: not silent, not clipped to nothing.
        assert!(rms > 1e-3 && peak > 0.01, "{}: rms {rms}, peak {peak}", e.name);
        println!("{}: {} ({} bit), {frames} frames = {secs:.3} s, rms {rms:.3}", e.name, r.format(), e.bits);
    }
}

#[test]
fn resampling_recordings_to_48k() {
    let Some(dir) = samples_dir() else { return };
    for name in ["FMGold_xHE_ModeB_9khz.flac", "Test_Mode_B_10kHz_IQ_Pos_26dB_SNR.flac"] {
        let path = dir.join(name);
        if !path.is_file() {
            eprintln!("skipping {name}: not present");
            continue;
        }
        let mut r = FileReader::open(&path).unwrap();
        let fmt = r.format();
        let total = r.total_frames().unwrap();
        let mut conv = To48k::new(fmt.sample_rate, fmt.channels).unwrap();
        assert!(!conv.is_passthrough());
        let mut out_samples = 0usize;
        while let Some(block) = r.read(4096).unwrap() {
            let y = conv.process(&block);
            assert!(y.iter().all(|s| s.is_finite()));
            out_samples += y.len();
        }
        out_samples += conv.flush().len();
        let expected = (total as f64 * 48_000.0 / f64::from(fmt.sample_rate)).round() as usize;
        assert_eq!(out_samples, expected * fmt.channels, "{name}");
    }
}

/// Every recording present decodes completely, with the length its header states.
#[test]
fn all_recordings_decode() {
    let Some(dir) = samples_dir() else { return };
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case("flac") || x.eq_ignore_ascii_case("wav"))
        })
        .collect();
    entries.sort();
    for path in entries {
        let mut r = FileReader::open(&path).unwrap_or_else(|e| panic!("{e}"));
        let total = r.total_frames();
        let (frames, _, _) = decode_all(&mut r, 48_000);
        if let Some(total) = total {
            assert_eq!(frames, total, "{}", path.display());
        }
        println!(
            "{}: {} {}-bit, {:.1} s, {} decode errors",
            path.file_name().unwrap().to_string_lossy(),
            r.format(),
            r.bits_per_sample().unwrap_or(0),
            frames as f64 / f64::from(r.format().sample_rate),
            r.decode_errors()
        );
    }
}
