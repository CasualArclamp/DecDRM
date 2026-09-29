//! WAV/FLAC writer → reader round trips and reader behaviour.

use std::path::Path;

use decdrm_io::{AudioFormat, Container, Encoding, Error, FileReader, FileWriter};

/// Deterministic test signal whose values are exactly representable at `bits` bits
/// (so integer round trips can be checked bit-exactly). Includes both full-scale extremes.
fn exact_signal(frames: usize, channels: usize, bits: u32) -> Vec<f32> {
    let full = (1i64 << (bits - 1)) as f64;
    let mut state = 0x1234_5678_u32;
    let mut v = Vec::with_capacity(frames * channels);
    for n in 0..frames * channels {
        // xorshift32 noise mixed with a sine, quantised to the target bit depth.
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let noise = (state as f64 / u32::MAX as f64) * 2.0 - 1.0;
        let x = 0.6 * (n as f64 * 0.01).sin() + 0.3 * noise;
        let q = (x * full).round().clamp(-full, full - 1.0);
        v.push((q / full) as f32);
    }
    if v.len() >= 2 {
        v[0] = -1.0;
        v[1] = ((full - 1.0) / full) as f32;
    }
    v
}

fn write_file(path: &Path, fmt: AudioFormat, c: Container, e: Encoding, data: &[f32], block: usize) {
    let mut w = FileWriter::create(path, fmt, c, e).unwrap();
    for chunk in data.chunks(block * fmt.channels) {
        w.write(chunk).unwrap();
    }
    assert_eq!(w.frames_written() as usize, data.len() / fmt.channels);
    w.finalize().unwrap();
}

fn read_back(path: &Path) -> (FileReader, Vec<f32>) {
    let mut r = FileReader::open(path).unwrap();
    let mut all = Vec::new();
    while let Some(block) = r.read(1000).unwrap() {
        assert_eq!(block.len() % r.format().channels, 0);
        assert!(block.len() <= 1000 * r.format().channels);
        all.extend(block);
    }
    assert!(r.read(1000).unwrap().is_none(), "EOF must be sticky");
    (r, all)
}

fn roundtrip(container: Container, encoding: Encoding, channels: usize, frames: usize) {
    let dir = tempfile::tempdir().unwrap();
    let ext = match container {
        Container::Wav => "wav",
        Container::Flac => "flac",
    };
    let path = dir.path().join(format!("rt.{ext}"));
    let fmt = AudioFormat::new(48_000, channels);
    let bits = u32::from(encoding.bits());
    let data = if encoding == Encoding::Float32 {
        // Arbitrary floats, including values beyond full scale (float WAV keeps them).
        (0..frames * channels).map(|n| ((n as f32) * 0.37).sin() * 1.25).collect()
    } else {
        exact_signal(frames, channels, bits)
    };
    // An odd block size exercises FLAC's internal re-blocking.
    write_file(&path, fmt, container, encoding, &data, 777);

    let (r, got) = read_back(&path);
    assert_eq!(r.format(), fmt);
    assert_eq!(r.total_frames(), Some(frames as u64));
    assert_eq!(r.position(), frames as u64);
    assert_eq!(r.bits_per_sample(), Some(bits));
    assert_eq!(r.decode_errors(), 0);
    assert_eq!(got.len(), data.len(), "{container:?} {encoding:?} {channels}ch");
    // Bit-exact for every encoding (integer data is pre-quantised; float is stored as is).
    if let Some(i) = got.iter().zip(&data).position(|(a, b)| a != b) {
        panic!("{container:?} {encoding:?}: sample {i}: got {}, wrote {}", got[i], data[i]);
    }
}

#[test]
fn wav_roundtrips() {
    for ch in [1, 2] {
        roundtrip(Container::Wav, Encoding::Int16, ch, 10_001);
        roundtrip(Container::Wav, Encoding::Float32, ch, 10_001);
        roundtrip(Container::Wav, Encoding::Int24, ch, 5_000);
        roundtrip(Container::Wav, Encoding::Int32, ch, 5_000);
        roundtrip(Container::Wav, Encoding::Int8, ch, 5_000);
    }
}

#[test]
fn flac_roundtrips() {
    for ch in [1, 2] {
        roundtrip(Container::Flac, Encoding::Int16, ch, 10_001);
        roundtrip(Container::Flac, Encoding::Int24, ch, 10_001);
        roundtrip(Container::Flac, Encoding::Int8, ch, 4096 * 3);
    }
    // Short streams: shorter than one FLAC block, shorter than flacenc's minimum block size,
    // and a single frame.
    roundtrip(Container::Flac, Encoding::Int16, 2, 1000);
    roundtrip(Container::Flac, Encoding::Int16, 1, 5);
    roundtrip(Container::Flac, Encoding::Int16, 1, 1);
}

#[test]
fn empty_files_are_valid() {
    let dir = tempfile::tempdir().unwrap();
    for (c, name) in [(Container::Wav, "e.wav"), (Container::Flac, "e.flac")] {
        let path = dir.path().join(name);
        FileWriter::create(&path, AudioFormat::new(8000, 1), c, Encoding::Int16)
            .unwrap()
            .finalize()
            .unwrap();
        let mut r = FileReader::open(&path).unwrap();
        assert_eq!(r.format(), AudioFormat::new(8000, 1));
        assert!(r.read(100).unwrap().is_none());
    }
}

#[test]
fn quantisation_and_clipping() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("q.flac");
    let data: Vec<f32> = (0..20_000).map(|n| ((n as f32) * 0.001).sin() * 0.9).collect();
    let mut clipped = data.clone();
    clipped.extend([1.5, -1.5, f32::NAN, 1.0]);
    write_file(&path, AudioFormat::new(44_100, 1), Container::Flac, Encoding::Int16, &clipped, 4096);
    let (_, got) = read_back(&path);
    for (a, b) in got.iter().zip(&data) {
        assert!((a - b).abs() <= 0.5 / 32768.0 + 1e-9);
    }
    let tail = &got[data.len()..];
    assert_eq!(tail, &[32767.0 / 32768.0, -1.0, 0.0, 32767.0 / 32768.0]);
}

#[test]
fn unsigned_8bit_wav_is_centred() {
    // hound writes 8-bit WAV as unsigned bytes; the reader must map 128 -> 0.0.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("u8.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 8000,
        bits_per_sample: 8,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec).unwrap();
    for s in [0i8, -128, 127, 64] {
        w.write_sample(s).unwrap();
    }
    w.finalize().unwrap();
    let mut r = FileReader::open(&path).unwrap();
    assert_eq!(r.read_all().unwrap(), vec![0.0, -1.0, 127.0 / 128.0, 0.5]);
}

#[test]
fn read_all_read_into_and_seek() {
    let dir = tempfile::tempdir().unwrap();
    for (c, name) in [(Container::Wav, "s.wav"), (Container::Flac, "s.flac")] {
        let path = dir.path().join(name);
        let fmt = AudioFormat::new(24_000, 2);
        let data = exact_signal(30_000, 2, 16);
        write_file(&path, fmt, c, Encoding::Int16, &data, 30_000);

        let mut r = FileReader::open(&path).unwrap();
        assert_eq!(r.duration(), Some(std::time::Duration::from_millis(1250)));
        assert_eq!(r.read_all().unwrap(), data);

        // read_into appends.
        let mut r = FileReader::open(&path).unwrap();
        let mut buf = vec![9.0];
        assert_eq!(r.read_into(&mut buf, 10).unwrap(), 10);
        assert_eq!(&buf[1..], &data[..20]);

        // Seek into the middle of a FLAC block, backwards, to 0, and past the end.
        for target in [12_345u64, 100, 0, 29_999] {
            assert_eq!(r.seek(target).unwrap(), target);
            let got = r.read(500).unwrap().unwrap();
            let start = target as usize * 2;
            let end = (start + 1000).min(data.len());
            assert_eq!(got, &data[start..end], "{name}: seek to {target}");
        }
        assert_eq!(r.seek(1_000_000).unwrap(), 30_000);
        assert!(r.read(10).unwrap().is_none());
        assert_eq!(r.seek(5).unwrap(), 5);
        assert!(r.read(10).unwrap().is_some());
        assert_eq!(r.position(), 15);
    }
}

#[test]
fn drop_finalizes_best_effort() {
    let dir = tempfile::tempdir().unwrap();
    for (c, name) in [(Container::Wav, "d.wav"), (Container::Flac, "d.flac")] {
        let path = dir.path().join(name);
        let data = exact_signal(9000, 1, 16);
        {
            let mut w =
                FileWriter::create(&path, AudioFormat::new(48_000, 1), c, Encoding::Int16).unwrap();
            w.write(&data).unwrap();
            // dropped here without finalize()
        }
        let (r, got) = read_back(&path);
        assert_eq!(r.total_frames(), Some(9000));
        assert_eq!(got, data);
    }
}

#[test]
fn damaged_files_do_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    for (c, name) in [(Container::Wav, "t.wav"), (Container::Flac, "t.flac")] {
        let path = dir.path().join(name);
        let data = exact_signal(48_000, 1, 16);
        write_file(&path, AudioFormat::new(48_000, 1), c, Encoding::Int16, &data, 4096);
        // Truncate the file part-way through the audio data.
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() * 2 / 3]).unwrap();
        let mut r = FileReader::open(&path).unwrap();
        let mut n = 0;
        loop {
            match r.read(4096) {
                Ok(Some(b)) => n += b.len(),
                Ok(None) => break,
                Err(e) => {
                    // A decoder may report the torn last packet; it must not panic.
                    eprintln!("{name}: {e}");
                    break;
                }
            }
        }
        assert!(n > 20_000 && n < 48_000, "{name}: read {n} samples from a truncated file");
    }
}

#[test]
fn bad_inputs_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    // Missing file.
    let missing = dir.path().join("nope.wav");
    assert!(matches!(FileReader::open(&missing), Err(Error::Io { .. })));
    // Not audio.
    let junk = dir.path().join("junk.wav");
    std::fs::write(&junk, b"this is not a wave file at all, just text").unwrap();
    assert!(matches!(FileReader::open(&junk), Err(Error::Decode { .. })));
    // Zero block size.
    let ok = dir.path().join("ok.wav");
    write_file(&ok, AudioFormat::new(8000, 1), Container::Wav, Encoding::Int16, &[0.0; 10], 10);
    assert!(FileReader::open(&ok).unwrap().read(0).is_err());

    // Writer argument checks.
    let out = dir.path().join("out.flac");
    let fmt = AudioFormat::new(48_000, 2);
    assert!(FileWriter::create(&out, fmt, Container::Flac, Encoding::Float32).is_err());
    assert!(FileWriter::create(&out, fmt, Container::Flac, Encoding::Int32).is_err());
    assert!(FileWriter::create(&out, AudioFormat::new(0, 2), Container::Wav, Encoding::Int16).is_err());
    assert!(FileWriter::create(&out, AudioFormat::new(48_000, 0), Container::Wav, Encoding::Int16).is_err());
    let mut w = FileWriter::create(&out, fmt, Container::Flac, Encoding::Int16).unwrap();
    assert!(matches!(w.write(&[0.0; 3]), Err(Error::InvalidArgument(_))));
    w.write(&[0.0; 4]).unwrap();
    w.finalize().unwrap();
    // Unwritable location.
    let bad = dir.path().join("no_such_dir").join("x.wav");
    assert!(matches!(
        FileWriter::create(&bad, fmt, Container::Wav, Encoding::Int16),
        Err(Error::Io { .. })
    ));
}

#[test]
fn container_detection_ignores_extension() {
    // A FLAC file named .wav is still read correctly (content-based probing).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("actually_flac.wav");
    let data = exact_signal(2000, 1, 16);
    write_file(&path, AudioFormat::new(16_000, 1), Container::Flac, Encoding::Int16, &data, 2000);
    let mut r = FileReader::open(&path).unwrap();
    assert_eq!(r.container(), "flac");
    assert_eq!(r.read_all().unwrap(), data);
}
