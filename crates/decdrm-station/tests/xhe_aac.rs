//! xHE-AAC in the station (libxaac encoder, ES 201 980 §5.3.1):
//!
//! * `xhe_stereo_mode_b_10khz_loopback` — mode B, 10 kHz, 64-QAM: stereo at ≈21 kbit/s
//!   (24 kHz, 2:1 SBR) from a 44.1 kHz stereo file with a different tone per channel;
//! * `xhe_stereo_mode_a_20khz_loopback` — mode A, 20 kHz, 64-QAM: stereo at ≈55 kbit/s
//!   (48 kHz);
//! * `xhe_mono_mode_d_loopback` — mode D, 10 kHz, 16-QAM, long interleaving: mono at
//!   ≈7.5 kbit/s from the built-in tone;
//! * `xhe_configuration_*` — the sampling-rate policy and the validation errors.
//!
//! The loopbacks write a few seconds of signal to a WAV file in a temporary directory and
//! decode it with the receiver (`decdrm_engine::Session`, FDK-AAC). Run with
//! `--nocapture` to see the multiplex plans and what was decoded.

use decdrm_core::mux::service::{AudioCodec, AudioMode};
use decdrm_engine::{InputFormat, RealChannel, ReceiverConfig, Session, SessionEvent};
use decdrm_io::{AudioFormat, Container, Encoding, FileReader, FileWriter};
use decdrm_station::{MultiplexPlan, Station, StationConfig, StationStatus};
use std::path::Path;
use std::time::Instant;

/// Everything the receiver produced.
struct Decoded {
    session: Session,
    /// Decoded audio (interleaved) of the selected service.
    audio: Vec<f32>,
    channels: usize,
    rate: u32,
    texts: Vec<String>,
    /// Audio frames concealed before the first clean one (tune-in: the decoder waits
    /// for an independent frame) and after it.
    concealed_at_start: u64,
    concealed_after_start: u64,
    /// Cleanly decoded audio frames.
    clean: u64,
}

fn decode(path: &Path) -> Decoded {
    let started = Instant::now();
    let mut reader = FileReader::open(path).unwrap();
    let channels = reader.format().channels;
    let input = InputFormat::Real(RealChannel::Mix);
    let mut d = Decoded {
        session: Session::new(ReceiverConfig { input, channels, ..Default::default() }),
        audio: Vec::new(),
        channels: 0,
        rate: 0,
        texts: Vec::new(),
        concealed_at_start: 0,
        concealed_after_start: 0,
        clean: 0,
    };
    while let Some(block) = reader.read(4800).unwrap() {
        for ev in d.session.push(&block) {
            match ev {
                SessionEvent::Audio(pcm) => {
                    match (pcm.concealed, d.clean > 0) {
                        (false, _) => d.clean += 1,
                        (true, false) => d.concealed_at_start += 1,
                        (true, true) => d.concealed_after_start += 1,
                    }
                    d.channels = usize::from(pcm.channels);
                    d.rate = pcm.sample_rate;
                    d.audio.extend_from_slice(&pcm.samples);
                }
                SessionEvent::Text(Some(t)) => d.texts.push(t),
                SessionEvent::Log(l) => println!("rx: {l}"),
                _ => {}
            }
        }
    }
    let st = &d.session.audio_stats;
    println!(
        "decoded {:.1} s of signal in {:.2} s: \"{}\", {} frames clean, {} concealed at tune-in, {} after, {} super \
         frame errors; texts {:?}",
        d.session.time_s(),
        started.elapsed().as_secs_f64(),
        st.codec,
        d.clean,
        d.concealed_at_start,
        d.concealed_after_start,
        st.super_frame_errors,
        d.texts
    );
    d
}

/// Frequency of the strongest component between `lo` and `hi` Hz (1 Hz steps) in the
/// last second of channel `ch`, the fraction of that second's energy it holds, and its
/// amplitude.
fn dominant_tone(d: &Decoded, ch: usize, lo: f64, hi: f64) -> (f64, f64, f64) {
    let x: Vec<f64> = d.audio.chunks_exact(d.channels).map(|c| f64::from(c[ch])).collect();
    let n = d.rate as usize;
    assert!(x.len() > 2 * n, "only {} samples of audio decoded", x.len());
    let seg = &x[x.len() - n..];
    // Goertzel power at f (N = one second: integer frequencies are exact bins).
    let power = |f: f64| {
        let w = std::f64::consts::TAU * f / f64::from(d.rate);
        let (c, mut s1, mut s2) = (2.0 * w.cos(), 0.0, 0.0);
        for &v in seg {
            let s0 = v + c * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        s1 * s1 + s2 * s2 - c * s1 * s2
    };
    let (mut best, mut best_p, mut f) = (lo, 0.0, lo);
    while f <= hi {
        let p = power(f);
        if p > best_p {
            (best, best_p) = (f, p);
        }
        f += 1.0;
    }
    let energy: f64 = seg.iter().map(|v| v * v).sum();
    // A sine of amplitude A gives |X|² = (A·N/2)² and energy A²·N/2.
    (best, 2.0 * best_p / n as f64 / energy, (4.0 * best_p).sqrt() / n as f64)
}

/// `seconds` of a sine per channel (`freqs[ch]`, whole cycles per second) at `rate`.
fn write_tones(path: &Path, rate: u32, freqs: &[f64], amp: f32, seconds: f64) {
    let channels = freqs.len();
    let mut w = FileWriter::create(path, AudioFormat::new(rate, channels), Container::Wav, Encoding::Float32).unwrap();
    let n = (seconds * f64::from(rate)) as usize;
    let mut block = Vec::with_capacity(4096 * channels);
    for i in 0..n {
        for &f in freqs {
            block.push(amp * (std::f64::consts::TAU * f * i as f64 / f64::from(rate)).sin() as f32);
        }
        if block.len() >= 4096 * channels {
            w.write(&block).unwrap();
            block.clear();
        }
    }
    w.write(&block).unwrap();
    w.finalize().unwrap();
}

/// Build the station of `toml` (paths relative to `dir`) and transmit `frames` frames;
/// the encoding must be clean (no frame dropped, no encoder error).
fn transmit(dir: &Path, toml: &str, frames: u64) -> (MultiplexPlan, StationStatus) {
    let mut cfg = StationConfig::from_toml_str(toml).unwrap();
    cfg.base_dir = Some(dir.to_path_buf());
    let started = Instant::now();
    let mut station = Station::new(cfg.clone()).unwrap_or_else(|e| panic!("{e}"));
    let plan = station.plan().clone();
    print!("{}", plan.describe(&cfg));
    station.run_frames(frames).unwrap();
    let status = station.finish().unwrap();
    println!("transmitted {:.1} s in {:.2} s", status.seconds, started.elapsed().as_secs_f64());
    for s in &status.services {
        let a = s.audio.as_ref().expect("audio service");
        let (encoder, stream) = (f64::from(a.encoder_bitrate) / 1e3, a.stream_bitrate / 1e3);
        println!("  {}: {} at {encoder:.2} kbit/s (stream {stream:.2} kbit/s)", s.label, a.codec);
        assert_eq!((a.counters.encoder_errors, a.counters.frames_dropped), (0, 0), "clean encoding");
        assert_eq!(a.counters.super_frames, frames);
    }
    (plan, status)
}

/// Checks shared by the loopbacks: clean decoding after tune-in, the tones, the text
/// and the SDC contents.
fn check_service(d: &Decoded, plan: &MultiplexPlan, rate: u32, tones: &[f64], text: &str) {
    let st = &d.session.audio_stats;
    assert!(st.codec.starts_with("xHE-AAC (USAC)"), "decoder: {}", st.codec);
    assert_eq!(st.super_frame_errors, 0);
    assert_eq!(d.concealed_after_start, 0, "no frame concealed once decoding has started");
    assert!(d.concealed_at_start <= 16, "{} frames concealed at tune-in", d.concealed_at_start);
    // At least 3 s of clean audio.
    let frame_len = plan.services[0].audio.as_ref().unwrap().xhe.as_ref().unwrap().budget().unwrap().frame_len;
    assert!(d.clean as usize * frame_len >= 3 * rate as usize, "{} clean frames", d.clean);
    assert_eq!((d.rate, d.channels), (rate, tones.len()));
    for (ch, &freq) in tones.iter().enumerate() {
        let (f, fraction, amp) = dominant_tone(d, ch, 100.0, 5000.0);
        println!("channel {ch}: tone {f} Hz, {:.1} % of the energy, amplitude {amp:.3}", 100.0 * fraction);
        assert_eq!(f, freq, "channel {ch}");
        assert!(fraction > 0.9, "channel {ch}: the tone holds only {:.1} % of the energy", 100.0 * fraction);
        assert!((amp / 0.25 - 1.0).abs() < 0.15, "channel {ch}: amplitude {amp}");
    }
    assert!(d.texts.iter().any(|t| t == text), "texts {:?}", d.texts);

    // The receiver sees exactly the signalling the plan sent.
    let ens = d.session.ensemble();
    assert_eq!(ens.multiplex(), Some(&plan.multiplex));
    let svc = ens.service(0).expect("service 0");
    let audio = svc.audio.as_ref().expect("audio information");
    let sent = &plan.services[0].audio.as_ref().unwrap().params;
    assert_eq!(audio, sent);
    assert_eq!(audio.codec, AudioCodec::XheAac);
    assert_eq!(audio.sample_rate_hz, rate);
    assert_eq!(audio.mode, if tones.len() == 2 { AudioMode::Stereo } else { AudioMode::Mono });
    assert!(audio.text_flag && !audio.codec_config.is_empty());
    let views = d.session.service_views();
    println!("service 0: {}", views[0].description);
    let mode = if tones.len() == 2 { "stereo" } else { "mono" };
    let expected = format!("xHE-AAC {mode} {} kHz, text", rate / 1000);
    assert!(views[0].description.starts_with(&expected), "{}", views[0].description);
}

/// Mode B, 10 kHz, 64-QAM, protection 1: a ≈21 kbit/s stream, stereo at 24 kHz (2:1
/// SBR) from a 44.1 kHz file — 700 Hz left, 1100 Hz right.
#[test]
fn xhe_stereo_mode_b_10khz_loopback() {
    let dir = tempfile::tempdir().unwrap();
    write_tones(&dir.path().join("stereo.wav"), 44_100, &[700.0, 1100.0], 0.25, 4.0);
    let toml = r#"
        [channel]
        mode = "B"
        occupancy = 3
        msc_mode = "64-QAM"
        sdc_mode = "16-QAM"
        interleaving = "short"
        protection_b = 1
        [output]
        file = "xhe_b.wav"
        [[service]]
        label = "xHE Stereo"
        id = 0xD0D0F1
        programme_type = "Pop Music"
        [service.audio]
        codec = "xhe-aac"
        stereo = true
        text = ["xHE-AAC over DRM"]
        input = { file = "stereo.wav" }
    "#;
    let (plan, _) = transmit(dir.path(), toml, 25);
    let a = plan.services[0].audio.as_ref().unwrap();
    assert_eq!((a.input_rate, a.core_rate, a.input_channels), (24_000, 12_000, 2));
    let kbps = plan.streams[0].bitrate() / 1e3;
    assert!((18.0..24.0).contains(&kbps), "{kbps} kbit/s");
    let d = decode(&dir.path().join("xhe_b.wav"));
    check_service(&d, &plan, 24_000, &[700.0, 1100.0], "xHE-AAC over DRM");
}

/// Mode A, 20 kHz, 64-QAM: a stream above 48 kbit/s, stereo at 48 kHz.
#[test]
fn xhe_stereo_mode_a_20khz_loopback() {
    let dir = tempfile::tempdir().unwrap();
    write_tones(&dir.path().join("stereo.wav"), 44_100, &[500.0, 1300.0], 0.25, 4.0);
    let toml = r#"
        [channel]
        mode = "A"
        occupancy = 5
        msc_mode = "64-QAM"
        interleaving = "short"
        protection_b = 1
        [output]
        file = "xhe_a.wav"
        format = "iq"
        [[service]]
        label = "xHE Wide"
        id = 0xD0D0F2
        [service.audio]
        codec = "xhe-aac"
        stereo = true
        text = ["Wideband xHE-AAC"]
        input = { file = "stereo.wav" }
    "#;
    let (plan, _) = transmit(dir.path(), toml, 20);
    let a = plan.services[0].audio.as_ref().unwrap();
    assert_eq!((a.input_rate, a.core_rate), (48_000, 24_000));
    assert!(plan.streams[0].bitrate() > 48_000.0);
    // I/Q file.
    let started = Instant::now();
    let mut reader = FileReader::open(dir.path().join("xhe_a.wav")).unwrap();
    assert_eq!(reader.format().channels, 2);
    let mut d = Decoded {
        session: Session::new(ReceiverConfig {
            input: InputFormat::Iq { swap: false },
            channels: 2,
            ..Default::default()
        }),
        audio: Vec::new(),
        channels: 0,
        rate: 0,
        texts: Vec::new(),
        concealed_at_start: 0,
        concealed_after_start: 0,
        clean: 0,
    };
    while let Some(block) = reader.read(4800).unwrap() {
        for ev in d.session.push(&block) {
            match ev {
                SessionEvent::Audio(pcm) => {
                    match (pcm.concealed, d.clean > 0) {
                        (false, _) => d.clean += 1,
                        (true, false) => d.concealed_at_start += 1,
                        (true, true) => d.concealed_after_start += 1,
                    }
                    d.channels = usize::from(pcm.channels);
                    d.rate = pcm.sample_rate;
                    d.audio.extend_from_slice(&pcm.samples);
                }
                SessionEvent::Text(Some(t)) => d.texts.push(t),
                _ => {}
            }
        }
    }
    println!(
        "decoded in {:.2} s: \"{}\", {} clean, {} concealed at tune-in, {} after",
        started.elapsed().as_secs_f64(),
        d.session.audio_stats.codec,
        d.clean,
        d.concealed_at_start,
        d.concealed_after_start
    );
    check_service(&d, &plan, 48_000, &[500.0, 1300.0], "Wideband xHE-AAC");
}

/// Mode D, 10 kHz, 16-QAM, long interleaving: the robust case, a ≈7.5 kbit/s mono
/// stream (24 kHz, 2:1 SBR) from the built-in tone.
#[test]
fn xhe_mono_mode_d_loopback() {
    let dir = tempfile::tempdir().unwrap();
    let toml = r#"
        [channel]
        mode = "D"
        occupancy = 3
        msc_mode = "16-QAM"
        sdc_mode = "16-QAM"
        interleaving = "long"
        protection_b = 1
        [output]
        file = "xhe_d.wav"
        [[service]]
        label = "xHE Robust"
        id = 0xD0D0F3
        language = "English"
        [service.audio]
        codec = "xhe-aac"
        text = ["Robust xHE-AAC"]
        input = { tone_hz = 440.0, level_dbfs = -12.041 }
    "#;
    let (plan, _) = transmit(dir.path(), toml, 30);
    let a = plan.services[0].audio.as_ref().unwrap();
    assert_eq!((a.input_rate, a.input_channels), (24_000, 1));
    let kbps = plan.streams[0].bitrate() / 1e3;
    assert!((6.0..12.0).contains(&kbps), "{kbps} kbit/s");
    let d = decode(&dir.path().join("xhe_d.wav"));
    check_service(&d, &plan, 24_000, &[440.0], "Robust xHE-AAC");
}

/// A configuration for the checks below: one service with `audio` lines (and a 440 Hz
/// tone unless they name an input), in a channel given by `channel` lines.
fn config(channel: &str, audio: &str) -> StationConfig {
    let input = if audio.contains("input") { "" } else { "input = { tone_hz = 440.0 }" };
    StationConfig::from_toml_str(&format!(
        "[channel]\n{channel}\n[output]\nfile = \"x.wav\"\n[[service]]\nlabel = \"xHE\"\nid = 0xD0D0F4\n\
         [service.audio]\ncodec = \"xhe-aac\"\n{audio}\n{input}\n"
    ))
    .unwrap()
}

/// The sampling rate follows the stream bit rate; `sample_rate` and `sbr_ratio`
/// override it; the plan's description and SDC parameters match.
#[test]
fn xhe_configuration_rate_policy() {
    use decdrm_station::plan::default_xhe_rate;
    use decdrm_station::config::SbrRatio;
    assert_eq!(default_xhe_rate(1000, SbrRatio::Auto), 24_000); // 20 kbit/s
    assert_eq!(default_xhe_rate(1200, SbrRatio::Auto), 24_000); // 24 kbit/s
    assert_eq!(default_xhe_rate(1201, SbrRatio::Auto), 32_000);
    assert_eq!(default_xhe_rate(2400, SbrRatio::Auto), 32_000); // 48 kbit/s
    assert_eq!(default_xhe_rate(2401, SbrRatio::Auto), 48_000);
    assert_eq!(default_xhe_rate(400, SbrRatio::Ratio4To1), 48_000);
    assert_eq!(default_xhe_rate(3000, SbrRatio::None), 32_000);

    // Mode B, 10 kHz, 64-QAM, protection 3 (≈27 kbit/s): 32 kHz.
    let cfg = config("mode = \"B\"\nmsc_mode = \"64-QAM\"\nprotection_b = 3", "stereo = true\ntext = [\"Hi\"]");
    let plan = cfg.validate().unwrap_or_else(|e| panic!("{e}"));
    let description = plan.describe(&cfg);
    println!("{description}");
    let a = plan.services[0].audio.as_ref().unwrap();
    assert_eq!((a.input_rate, a.core_rate, a.input_channels), (32_000, 16_000, 2));
    assert_eq!(a.super_frame_len, plan.streams[0].bytes() - 4);
    assert!(description.contains("xHE-AAC stereo, 32 kHz (16 kHz core)"), "{description}");
    let p = &a.params;
    assert_eq!((p.codec, p.sample_rate_hz, p.mode), (AudioCodec::XheAac, 32_000, AudioMode::Stereo));
    assert!(a.params.text_flag);
    assert_eq!(a.params.codec_config, {
        let enc = decdrm_codecs::XheAacEncoder::new(a.xhe.clone().unwrap()).unwrap();
        enc.static_config().to_vec()
    });
    // The encoder runs at the channel capacity for access units: the stream less the
    // header and 4 bytes per frame.
    let net = f64::from(a.encoder_bitrate);
    let stream = plan.streams[0].bitrate() - 4.0 * 8.0 / 0.4;
    assert!(net < stream && net > 0.9 * stream, "{net} of {stream}");

    // Explicit rate and ratio: 38.4 kHz mono with 4:1 SBR.
    let cfg = config("mode = \"B\"\nmsc_mode = \"16-QAM\"", "sample_rate = 38400\nsbr_ratio = \"4:1\"");
    let plan = cfg.validate().unwrap_or_else(|e| panic!("{e}"));
    let a = plan.services[0].audio.as_ref().unwrap();
    assert_eq!((a.input_rate, a.core_rate), (38_400, 9_600));
    assert!(plan.describe(&cfg).contains("xHE-AAC mono, 38.4 kHz (9.6 kHz core)"), "{}", plan.describe(&cfg));
    // No SBR at 12 kHz.
    let cfg = config("mode = \"B\"\nmsc_mode = \"16-QAM\"", "sample_rate = 12000");
    let plan = cfg.validate().unwrap_or_else(|e| panic!("{e}"));
    assert!(plan.describe(&cfg).contains("xHE-AAC mono, 12 kHz (no SBR)"), "{}", plan.describe(&cfg));

    // The settings survive a TOML round trip (and are not written when unset).
    let mut cfg = config("", "sample_rate = 24000\nsbr_ratio = \"2:1\"\nstereo = true");
    cfg.base_dir = None;
    let text = cfg.to_toml_string().unwrap();
    assert!(text.contains("codec = \"xhe-aac\"") && text.contains("sbr_ratio = \"2:1\""), "{text}");
    assert_eq!(StationConfig::from_toml_str(&text).unwrap(), cfg);
    assert!(!config("", "").to_toml_string().unwrap().contains("sample_rate"));
}

/// Unsupported combinations and too small streams are reported with the reason.
#[test]
fn xhe_configuration_errors() {
    let problems = |channel: &str, audio: &str| {
        let e = config(channel, audio).validate().unwrap_err();
        println!("{e}");
        e.to_string()
    };
    let b64 = "mode = \"B\"\nmsc_mode = \"64-QAM\"";
    // 38.4 kHz stereo: libxaac has no SBR tables for it and 4:1 would need MPS212.
    let e = problems(b64, "stereo = true\nsample_rate = 38400");
    assert!(e.contains("38.4 kHz") && e.contains("stereo"), "{e}");
    // 4:1 stereo needs MPS212.
    let e = problems(b64, "stereo = true\nsbr_ratio = \"4:1\"");
    assert!(e.contains("4:1") && e.contains("MPS212"), "{e}");
    // No SBR above 32 kHz: too many frames per super frame.
    let e = problems(b64, "sample_rate = 48000\nsbr_ratio = \"none\"");
    assert!(e.contains("15 frames"), "{e}");
    // Settings of other codecs, and invalid values.
    let e = problems(b64, "core_rate = 24000\nsample_rate = 44100");
    assert!(e.contains("no core_rate setting") && e.contains("sample_rate 44100 Hz is not allowed"), "{e}");
    let e = StationConfig::from_toml_str(
        "[output]\nfile = \"x.wav\"\n[[service]]\nlabel = \"A\"\nid = 1\n[service.audio]\ncodec = \"aac\"\n\
         sample_rate = 24000\nsbr_ratio = \"2:1\"\ninput = { tone_hz = 440.0 }\n",
    )
    .unwrap()
    .validate()
    .unwrap_err()
    .to_string();
    assert!(e.contains("sample_rate is only used by codec = \"xhe-aac\""), "{e}");
    assert!(e.contains("sbr_ratio is only used"), "{e}");
    let e = StationConfig::from_toml_str(
        "[[service]]\nlabel = \"A\"\nid = 1\n[service.audio]\ncodec = \"xhe\"\nsbr_ratio = \"3:1\"\n\
         input = { tone_hz = 1.0 }\n",
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("unknown SBR ratio"), "{e}");
    // The chosen rate (24 kHz for the ≈14 kbit/s of 16-QAM) limits the test tone.
    let e = problems("mode = \"B\"\nmsc_mode = \"16-QAM\"", "input = { tone_hz = 15000.0 }");
    assert!(e.contains("tone_hz 15000") && e.contains("half the xHE-AAC sampling rate"), "{e}");

    // Too small a stream: data take all but ≈3.5 kbit/s of mode B, 10 kHz, 16-QAM.
    let cfg = StationConfig::from_toml_str(
        "[channel]\nmode = \"B\"\nmsc_mode = \"16-QAM\"\nprotection_b = 0\n[output]\nfile = \"x.wav\"\n\
         [[service]]\nlabel = \"xHE\"\nid = 1\n[service.audio]\ncodec = \"xhe-aac\"\ninput = { tone_hz = 440.0 }\n\
         [[service.app]]\ntype = \"epg\"\nbitrate = 8000\n[[service.app.programme]]\ntitle = \"N\"\n\
         start = \"2026-09-30T18:00:00Z\"\nduration_min = 5\n",
    )
    .unwrap();
    let e = cfg.validate().unwrap_err().to_string();
    println!("{e}");
    assert!(e.contains("left for the audio stream"), "{e}");
    assert!(e.contains("xHE-AAC mono at 24 kHz needs at least 4.8 kbit/s"), "{e}");
    assert!(e.contains("reduce the data bit rates"), "{e}");
}
