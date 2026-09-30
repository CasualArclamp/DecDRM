//! End-to-end loopback: a station writes a few seconds of signal to a WAV/FLAC file in
//! a temporary directory, and the receiver (`decdrm_engine::Session`) decodes it.
//!
//! * `he_aac_64qam_*` — HE-AAC from a 44.1 kHz stereo WAV (resampled and mixed to
//!   mono), text messages, labels, FAC and SDC contents, time and date.
//! * `opus_16qam_*` — stereo Opus from the test tone, a slideshow data service, I/Q
//!   output, 4-QAM SDC.
//! * `aac_hmmix_*` — hierarchical 64-QAM (Journaline in the hierarchical stream), an
//!   EPG in part A (unequal error protection), AAC with a 24 kHz core, FLAC output.
//! * `tpeg_raw_*` — TPEG and raw data applications (captured byte for byte) and
//!   alternative-frequency signalling.
//!
//! Run with `--nocapture` to see the multiplex plans and what was decoded.

use decdrm_core::fac::{Interleaving, MscMode, SdcMode};
use decdrm_core::mux::service::AudioParams;
use decdrm_core::params::SpectrumOccupancy;
use decdrm_data::DataEvent;
use decdrm_engine::{InputFormat, RealChannel, ReceiverConfig, Session, SessionEvent};
use decdrm_io::{AudioFormat, Container, Encoding, FileReader, FileWriter};
use decdrm_station::{Station, StationConfig, StationStatus};
use std::collections::BTreeSet;
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
    data: Vec<(u8, DataEvent)>,
}

/// Run the receiver over a recording.
fn decode(path: &Path, iq: bool) -> Decoded {
    let started = Instant::now();
    let mut reader = FileReader::open(path).unwrap();
    let channels = reader.format().channels;
    assert_eq!(reader.format().sample_rate, 48_000);
    let input = if iq { InputFormat::Iq { swap: false } } else { InputFormat::Real(RealChannel::Mix) };
    let mut d = Decoded {
        session: Session::new(ReceiverConfig { input, channels, ..Default::default() }),
        audio: Vec::new(),
        channels: 0,
        rate: 0,
        texts: Vec::new(),
        data: Vec::new(),
    };
    while let Some(block) = reader.read(4800).unwrap() {
        for ev in d.session.push(&block) {
            match ev {
                SessionEvent::Audio(pcm) => {
                    d.channels = usize::from(pcm.channels);
                    d.rate = pcm.sample_rate;
                    d.audio.extend_from_slice(&pcm.samples);
                }
                SessionEvent::Text(Some(t)) => d.texts.push(t),
                SessionEvent::Data { short_id, event } => d.data.push((short_id, event)),
                SessionEvent::Log(l) => println!("rx: {l}"),
                _ => {}
            }
        }
    }
    let st = &d.session.audio_stats;
    println!(
        "decoded {:.1} s of signal in {:.2} s: audio \"{}\" {} frames ok, {} concealed, {} super frame errors; texts {:?}",
        d.session.time_s(),
        started.elapsed().as_secs_f64(),
        st.codec,
        st.frames_ok,
        st.frames_concealed,
        st.super_frame_errors,
        d.texts
    );
    d
}

/// Frequency of the strongest component between `lo` and `hi` Hz (1 Hz steps) in the
/// last second of channel `ch`, and the fraction of that second's energy it holds.
fn dominant_tone(d: &Decoded, ch: usize, lo: f64, hi: f64) -> (f64, f64, f64) {
    let x: Vec<f64> = d.audio.chunks_exact(d.channels).map(|c| f64::from(c[ch])).collect();
    let n = d.rate as usize;
    assert!(x.len() > 2 * n, "only {} samples of audio decoded", x.len());
    let seg = &x[x.len() - n..];
    // Goertzel power at f (N = one second: integer frequencies are exact bins).
    let power = |f: f64| {
        let w = 2.0 * std::f64::consts::PI * f / f64::from(d.rate);
        let (c, mut s1, mut s2) = (2.0 * w.cos(), 0.0, 0.0);
        for &v in seg {
            let s0 = v + c * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        s1 * s1 + s2 * s2 - c * s1 * s2
    };
    let (mut best, mut best_p) = (lo, 0.0);
    let mut f = lo;
    while f <= hi {
        let p = power(f);
        if p > best_p {
            (best, best_p) = (f, p);
        }
        f += 1.0;
    }
    let energy: f64 = seg.iter().map(|v| v * v).sum();
    // A sine of amplitude A gives |X|² = (A·N/2)² and energy A²·N/2.
    let tone_energy = 2.0 * best_p / n as f64;
    let amplitude = (4.0 * best_p).sqrt() / n as f64;
    (best, tone_energy / energy, amplitude)
}

/// A looping tone file: `seconds` of a sine at `freq` (a whole number of cycles).
fn write_tone(path: &Path, rate: u32, channels: usize, freq: f64, amp: f32, seconds: f64) {
    let mut w = FileWriter::create(path, AudioFormat::new(rate, channels), Container::Wav, Encoding::Float32).unwrap();
    let n = (seconds * f64::from(rate)) as usize;
    let mut block = Vec::with_capacity(4096 * channels);
    for i in 0..n {
        let v = amp * (2.0 * std::f64::consts::PI * freq * i as f64 / f64::from(rate)).sin() as f32;
        block.extend(std::iter::repeat_n(v, channels));
        if block.len() >= 4096 * channels {
            w.write(&block).unwrap();
            block.clear();
        }
    }
    w.write(&block).unwrap();
    w.finalize().unwrap();
}

/// Build the station of `toml` (paths relative to `dir`), transmit `frames` frames.
fn transmit(dir: &Path, toml: &str, frames: u64) -> (StationConfig, decdrm_station::MultiplexPlan, StationStatus) {
    let mut cfg = StationConfig::from_toml_str(toml).unwrap();
    cfg.base_dir = Some(dir.to_path_buf());
    let started = Instant::now();
    let mut station = Station::new(cfg.clone()).unwrap_or_else(|e| panic!("{e}"));
    let plan = station.plan().clone();
    print!("{}", plan.describe(&cfg));
    station.run_frames(frames).unwrap();
    let status = station.finish().unwrap();
    println!(
        "transmitted {:.1} s in {:.2} s: output {:.1} dBFS rms / {:.1} dBFS peak, {} clipped, SDC {}/{} bytes, time {:?}",
        status.seconds,
        started.elapsed().as_secs_f64(),
        status.output_rms_dbfs,
        status.output_peak_dbfs,
        status.clipped_samples,
        status.sdc_bytes_used,
        status.sdc_capacity,
        status.time_sent
    );
    for s in &status.services {
        println!("  {s:?}");
    }
    (cfg, plan, status)
}

fn assert_clean_encoding(status: &StationStatus) {
    for s in &status.services {
        if let Some(a) = &s.audio {
            assert_eq!(a.counters.frames_dropped, 0, "{}: audio frames dropped", s.label);
            assert_eq!(a.counters.encoder_errors, 0, "{}: encoder errors", s.label);
            assert!(a.counters.super_frames > 0);
        }
    }
    // A handful of OFDM peaks may clip at -15 dBFS; not more than 1e-5 of the samples.
    assert!((status.clipped_samples as f64) < 1e-5 * status.seconds * 48_000.0, "{} clipped", status.clipped_samples);
}

#[test]
fn he_aac_64qam_tone_text_and_service_information() {
    let dir = tempfile::tempdir().unwrap();
    // 44.1 kHz stereo: the station resamples to HE-AAC's 24 kHz input and mixes to mono.
    write_tone(&dir.path().join("tone.wav"), 44_100, 2, 1000.0, 0.25, 3.0);
    let toml = r#"
        [channel]
        mode = "B"
        occupancy = 3
        msc_mode = "64-QAM"
        sdc_mode = "16-QAM"
        interleaving = "short"
        protection_b = 1
        [output]
        file = "he-aac.wav"
        [time]
        # The minute changes 6 s into the signal, when the receiver is surely locked.
        start = "2026-09-29T18:00:55Z"
        local_offset_minutes = 60
        [[service]]
        label = "DecDRM Test"
        id = 0xD0D0A1
        language = "German"
        iso_language = "deu"
        iso_country = "de"
        programme_type = "Rock Music"
        [service.audio]
        codec = "he-aac"
        core_rate = 12000
        text = ["Hello DecDRM", "Second message"]
        input = { file = "tone.wav" }
    "#;
    let (_, plan, status) = transmit(dir.path(), toml, 23);
    assert_clean_encoding(&status);
    assert_eq!(status.time_sent.as_deref(), Some("2026-09-29 18:01 UTC"));

    let d = decode(&dir.path().join("he-aac.wav"), false);
    let st = &d.session.audio_stats;
    // ~5.5 s of audio after acquisition at 12.5 frames per second.
    assert!(st.frames_ok >= 40, "{} frames ok", st.frames_ok);
    assert_eq!(st.frames_concealed, 0, "every frame must pass FDK's CRC check");
    assert_eq!(st.super_frame_errors, 0);
    assert_eq!((d.rate, d.channels), (24_000, 1));
    let (f, fraction, amp) = dominant_tone(&d, 0, 200.0, 5000.0);
    println!("tone {f} Hz, {:.1} % of the energy, amplitude {amp:.3}", 100.0 * fraction);
    assert_eq!(f, 1000.0);
    assert!(fraction > 0.9, "tone holds only {:.1} % of the energy", 100.0 * fraction);
    // Input amplitude 0.25 (both channels, mixed to mono).
    assert!((amp / 0.25 - 1.0).abs() < 0.15, "amplitude {amp}");

    // Text messages (rotating).
    assert!(!d.texts.is_empty(), "no text message");
    let sent: BTreeSet<&str> = ["Hello DecDRM", "Second message"].into();
    assert!(d.texts.iter().all(|t| sent.contains(t.as_str())), "{:?}", d.texts);

    // FAC channel and service parameters.
    let ens = d.session.ensemble();
    let ch = ens.channel().expect("FAC decoded");
    assert_eq!(ch.occupancy, SpectrumOccupancy::SO_3);
    assert_eq!((ch.msc_mode, ch.sdc_mode, ch.interleaving), (MscMode::Qam64Sm, SdcMode::Qam16, Interleaving::Short));
    assert_eq!((ch.num_audio, ch.num_data), (1, 0));
    let svc = ens.service(0).expect("service 0");
    let fac = svc.fac.unwrap();
    assert_eq!((fac.service_id, fac.language, fac.is_data, fac.descriptor), (0xD0D0A1, 7, false, 11));
    // SDC entities.
    assert_eq!(ens.multiplex(), Some(&plan.multiplex));
    let sent_audio = &plan.services[0].audio.as_ref().unwrap().params;
    assert_eq!(svc.audio.as_ref(), Some(sent_audio));
    assert_eq!(AudioParams::from_entity(&sent_audio.to_entity(0)).as_ref(), Ok(sent_audio));
    assert_eq!(svc.label.as_deref(), Some("DecDRM Test"));
    assert_eq!((svc.language_code.as_deref(), svc.country_code.as_deref()), (Some("deu"), Some("de")));
    let t = ens.time().expect("time and date entity");
    assert_eq!((t.date(), t.hour, t.minute, t.local_offset_minutes()), ((2026, 9, 29), 18, 1, Some(60)));
}

/// A tiny test picture.
fn picture(w: u32, h: u32, seed: u8) -> image::RgbImage {
    image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 10) as u8 ^ seed, (y * 15) as u8, seed.wrapping_mul(3)]))
}

#[test]
fn opus_16qam_slideshow_service() {
    let dir = tempfile::tempdir().unwrap();
    let slides = dir.path().join("slides");
    std::fs::create_dir(&slides).unwrap();
    picture(24, 16, 7).save(slides.join("a.png")).unwrap();
    picture(24, 16, 99).save(slides.join("b.jpg")).unwrap();
    let files: Vec<(String, Vec<u8>)> =
        ["a.png", "b.jpg"].iter().map(|n| (n.to_string(), std::fs::read(slides.join(n)).unwrap())).collect();
    println!("slides: {:?}", files.iter().map(|(n, b)| (n, b.len())).collect::<Vec<_>>());
    let toml = r#"
        [channel]
        mode = "A"
        occupancy = 3
        msc_mode = "16-QAM"
        sdc_mode = "4-QAM"
        interleaving = "short"
        protection_b = 1
        [output]
        file = "opus.wav"
        format = "iq"
        [[service]]
        label = "Opus Test"
        id = 0xD0D0B1
        language = "English"
        programme_type = "Jazz Music"
        [service.audio]
        codec = "opus"
        stereo = true
        input = { tone_hz = 700.0, level_dbfs = -10.0 }
        [[service]]
        label = "Slides"
        id = 0xD0D0B2
        [service.data]
        type = "slideshow"
        path = "slides"
        bitrate = 4000
        packet_length = 60
    "#;
    let (_, plan, status) = transmit(dir.path(), toml, 23);
    assert_clean_encoding(&status);
    assert_eq!(plan.services[1].apps[0].stream, 1);

    let d = decode(&dir.path().join("opus.wav"), true);
    let st = &d.session.audio_stats;
    // 50 Opus frames per second.
    assert!(st.frames_ok >= 150, "{} frames ok", st.frames_ok);
    assert_eq!(st.frames_concealed, 0, "every packet must pass its CRC");
    assert_eq!((d.rate, d.channels), (48_000, 2));
    for ch in 0..2 {
        let (f, fraction, amp) = dominant_tone(&d, ch, 200.0, 5000.0);
        println!("channel {ch}: tone {f} Hz, {:.1} % of the energy, amplitude {amp:.3}", 100.0 * fraction);
        assert_eq!(f, 700.0);
        assert!(fraction > 0.8, "tone holds only {:.1} % of the energy", 100.0 * fraction);
    }

    // Every slide arrives intact, from the data service (Short Id 1).
    let got: Vec<(String, Vec<u8>)> = d
        .data
        .iter()
        .filter_map(|(sid, e)| match e {
            DataEvent::SlideShowImage { name, data, .. } if *sid == 1 => Some((name.clone(), data.clone())),
            _ => None,
        })
        .collect();
    println!("slides received: {:?}", got.iter().map(|(n, b)| (n, b.len())).collect::<Vec<_>>());
    for (name, bytes) in &files {
        assert!(got.iter().any(|(n, b)| n == name && b == bytes), "slide {name} not received intact");
    }

    let ens = d.session.ensemble();
    let ch = ens.channel().unwrap();
    assert_eq!((ch.msc_mode, ch.sdc_mode, ch.num_audio, ch.num_data), (MscMode::Qam16Sm, SdcMode::Qam4, 1, 1));
    let data = ens.service(1).expect("data service");
    assert!(data.is_data());
    assert_eq!(data.label.as_deref(), Some("Slides"));
    let app = &data.applications[0];
    assert_eq!((app.stream_id, app.packet_mode, app.packet_length, app.user_app_id()), (1, true, 60, Some(0x002)));
    let audio = ens.service(0).unwrap();
    assert_eq!(audio.fac.unwrap().descriptor, 24);
    assert_eq!(audio.label.as_deref(), Some("Opus Test"));
    assert_eq!(ens.multiplex(), Some(&plan.multiplex));
}

#[test]
fn aac_hmmix_hierarchical_journaline_and_uep_epg() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pages.toml"),
        r#"
        [[page]]
        id = 0
        title = "Layers"
        menu = [{ link = 1, text = "About" }]
        [[page]]
        id = 1
        title = "About"
        text = "Journaline in the hierarchical stream"
        "#,
    )
    .unwrap();
    let toml = r#"
        [channel]
        mode = "B"
        occupancy = 3
        msc_mode = "HMmix"
        protection_a = 0
        protection_b = 1
        protection_hierarchical = 1
        interleaving = "short"
        [output]
        file = "hmmix.flac"
        [[service]]
        label = "Layers"
        id = 0xD0D0C1
        [service.audio]
        codec = "aac"
        core_rate = 24000
        input = { tone_hz = 1500.0 }
        [[service.app]]
        type = "journaline"
        path = "pages.toml"
        hierarchical = true
        packet_length = 40
        [[service.app]]
        type = "epg"
        part = "A"
        bitrate = 1600
        [[service.app.programme]]
        title = "Layered Hour"
        start = "2026-09-29T18:00:00Z"
        duration_min = 60
    "#;
    let (_, plan, status) = transmit(dir.path(), toml, 23);
    assert_clean_encoding(&status);
    assert!(plan.streams[0].hierarchical);
    assert!(plan.tx.part_a_bytes > 0);

    let d = decode(&dir.path().join("hmmix.flac"), false);
    let st = &d.session.audio_stats;
    assert!(st.frames_ok >= 80, "{} frames ok", st.frames_ok);
    assert_eq!(st.frames_concealed, 0);
    let (f, fraction, _) = dominant_tone(&d, 0, 200.0, 5000.0);
    assert_eq!(f, 1500.0);
    assert!(fraction > 0.9);

    let pages: BTreeSet<u16> = d
        .data
        .iter()
        .filter_map(|(_, e)| match e {
            DataEvent::Journaline(u) => Some(u.object_id()),
            _ => None,
        })
        .collect();
    assert_eq!(pages, [0, 1].into(), "Journaline pages");
    let epg: Vec<&String> = d
        .data
        .iter()
        .filter_map(|(_, e)| match e {
            DataEvent::Epg { xml, .. } => Some(xml),
            _ => None,
        })
        .collect();
    assert!(epg.iter().any(|x| x.contains("<mediumName>Layered Hour</mediumName>")), "EPG: {epg:?}");
    let ens = d.session.ensemble();
    assert_eq!(ens.channel().unwrap().msc_mode, MscMode::Qam64HmMix);
    assert_eq!(ens.multiplex(), Some(&plan.multiplex));
    assert_eq!(ens.service(0).unwrap().applications.len(), 2);
}

/// A music-like file: harmonics with vibrato, noise and bursts (hard for the encoder).
fn write_music(path: &Path, rate: u32, seconds: f64) {
    let mut w = FileWriter::create(path, AudioFormat::new(rate, 2), Container::Wav, Encoding::Int16).unwrap();
    let mut seed = 12_345u32;
    let n = (seconds * f64::from(rate)) as usize;
    let mut block = Vec::new();
    for i in 0..n {
        let t = i as f64 / f64::from(rate);
        let f0 = 196.0 * (1.0 + 0.01 * (2.0 * std::f64::consts::PI * 5.0 * t).sin());
        let burst = if (t * 2.5).fract() > 0.7 { 0.4 } else { 0.0 };
        for ch in 0..2 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = f64::from(seed >> 8) / f64::from(1u32 << 24) * 2.0 - 1.0;
            let v: f64 = (1..12).map(|h| 0.2 / h as f64 * (2.0 * std::f64::consts::PI * f0 * h as f64 * t + (ch * h) as f64).sin()).sum();
            block.push((v + (0.05 + burst) * noise).clamp(-1.0, 1.0) as f32);
        }
        if block.len() >= 8192 {
            w.write(&block).unwrap();
            block.clear();
        }
    }
    w.write(&block).unwrap();
    w.finalize().unwrap();
}

/// With demanding programme material every AAC flavour fits its audio super frames
/// (the encoder runs at 97 % of the payload): no frame is dropped, no encoder error.
#[test]
fn aac_super_frames_fit_with_music() {
    let dir = tempfile::tempdir().unwrap();
    write_music(&dir.path().join("music.wav"), 48_000, 4.0);
    for (codec, rate, stereo, mode, so, msc) in [
        ("aac", 12_000, false, "B", 0, "64-QAM"),
        ("aac", 24_000, true, "B", 3, "64-QAM"),
        ("he-aac", 12_000, false, "B", 2, "64-QAM"),
        ("he-aac", 24_000, true, "A", 3, "64-QAM"),
        ("he-aac-v2", 12_000, false, "B", 3, "64-QAM"),
        ("he-aac-v2", 24_000, false, "A", 5, "64-QAM"),
        // Close to FDK's lowest bit rates.
        ("aac", 24_000, true, "B", 2, "16-QAM"),
        ("he-aac", 12_000, true, "A", 3, "16-QAM"),
        ("he-aac", 12_000, false, "B", 0, "64-QAM"),
    ] {
        let toml = format!(
            r#"
            [channel]
            mode = "{mode}"
            occupancy = {so}
            msc_mode = "{msc}"
            [output]
            file = "out.wav"
            format = "iq"
            [[service]]
            label = "Music"
            id = 1
            [service.audio]
            codec = "{codec}"
            core_rate = {rate}
            stereo = {stereo}
            text = ["Music"]
            input = {{ file = "music.wav" }}
            "#
        );
        let mut cfg = StationConfig::from_toml_str(&toml).unwrap();
        cfg.base_dir = Some(dir.path().to_path_buf());
        let mut station = Station::new(cfg).unwrap_or_else(|e| panic!("{codec} {rate} {stereo}: {e}"));
        station.run_frames(25).unwrap();
        let status = station.finish().unwrap();
        let a = status.services[0].audio.as_ref().unwrap();
        println!(
            "{codec} {rate} Hz stereo={stereo} mode {mode} SO{so} {msc}: stream {:.1} kbit/s, encoder {:.1} kbit/s, {} dropped, {} errors",
            a.stream_bitrate / 1000.0,
            f64::from(a.encoder_bitrate) / 1000.0,
            a.counters.frames_dropped,
            a.counters.encoder_errors
        );
        assert_eq!((a.counters.frames_dropped, a.counters.encoder_errors), (0, 0), "{codec} {rate} Hz stereo={stereo}");
    }
}

/// Two audio services: the capacity is split by `share`, the FAC alternates between
/// them, and each decodes when the receiver selects it.
#[test]
fn two_audio_services() {
    let dir = tempfile::tempdir().unwrap();
    let toml = r#"
        [channel]
        mode = "A"
        occupancy = 5
        interleaving = "short"
        [output]
        file = "two.wav"
        format = "iq"
        iq_offset_hz = 2000
        [[service]]
        label = "First"
        id = 0xD0D0E1
        [service.audio]
        codec = "he-aac"
        share = 1
        input = { tone_hz = 600.0 }
        [[service]]
        label = "Second"
        id = 0xD0D0E2
        programme_type = "News"
        [service.audio]
        codec = "aac"
        core_rate = 24000
        share = 2
        text = ["Second service"]
        input = { tone_hz = 900.0 }
    "#;
    let (_, plan, status) = transmit(dir.path(), toml, 20);
    assert_clean_encoding(&status);
    let (a, b) = (plan.streams[0].bytes() as f64, plan.streams[1].bytes() as f64);
    assert!((b / a - 2.0).abs() < 0.01);
    let path = dir.path().join("two.wav");
    for (sid, freq, rate) in [(0u8, 600.0, 24_000), (1, 900.0, 24_000)] {
        // The receiver decodes the selected audio service.
        let mut reader = FileReader::open(&path).unwrap();
        let mut session = Session::new(ReceiverConfig { input: InputFormat::Iq { swap: false }, channels: 2, ..Default::default() });
        session.select_service(sid);
        let mut d = Decoded { session, audio: Vec::new(), channels: 0, rate: 0, texts: Vec::new(), data: Vec::new() };
        while let Some(block) = reader.read(4800).unwrap() {
            for ev in d.session.push(&block) {
                match ev {
                    SessionEvent::Audio(pcm) => {
                        d.channels = usize::from(pcm.channels);
                        d.rate = pcm.sample_rate;
                        d.audio.extend_from_slice(&pcm.samples);
                    }
                    SessionEvent::Text(Some(t)) => d.texts.push(t),
                    _ => {}
                }
            }
        }
        assert_eq!(d.session.current_audio_service(), Some(sid));
        assert_eq!(d.session.audio_stats.frames_concealed, 0);
        assert_eq!(d.rate, rate);
        let (f, fraction, _) = dominant_tone(&d, 0, 200.0, 5000.0);
        println!("service {sid}: {} frames ok, tone {f} Hz ({:.1} %), texts {:?}", d.session.audio_stats.frames_ok, 100.0 * fraction, d.texts);
        assert_eq!(f, freq);
        assert!(fraction > 0.9);
        let ens = d.session.ensemble();
        assert_eq!(ens.services().count(), 2);
        assert_eq!(ens.service(1).unwrap().label.as_deref(), Some("Second"));
        assert_eq!(ens.service(1).unwrap().fac.unwrap().descriptor, 1);
        if sid == 1 {
            assert_eq!(d.texts.first().map(String::as_str), Some("Second service"));
        }
    }
}

/// The shipped example runs (with its output redirected to a temporary file), and its
/// Journaline and EPG applications arrive at the receiver.
#[test]
fn example_station_runs() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = StationConfig::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/station.toml")).unwrap();
    let out = dir.path().join("example.wav");
    cfg.output.file = Some(out.clone());
    cfg.output.device = None;
    // Short interleaving so that a few seconds suffice.
    cfg.channel.interleaving = "short".parse().unwrap();
    let mut station = Station::new(cfg).unwrap();
    station.run_frames(15).unwrap();
    let status = station.finish().unwrap();
    assert_clean_encoding(&status);
    let d = decode(&out, false);
    assert!(d.session.audio_stats.frames_ok > 20 && d.session.audio_stats.frames_concealed == 0);
    let journaline = d.data.iter().filter(|(sid, e)| *sid == 1 && matches!(e, DataEvent::Journaline(_))).count();
    let epg = d.data.iter().filter(|(sid, e)| *sid == 1 && matches!(e, DataEvent::Epg { .. })).count();
    assert!(journaline >= 4 && epg >= 1, "{journaline} Journaline pages, {epg} EPG objects");
    assert_eq!(d.session.ensemble().service(1).unwrap().label.as_deref(), Some("DecDRM News"));
}

/// The data fields of the data groups of application `app_id` of service `short_id`, in
/// order (what the engine's data store captures).
fn captured(d: &Decoded, short_id: u8, app_id: u16) -> Vec<u8> {
    d.data
        .iter()
        .filter_map(|(sid, e)| match e {
            DataEvent::Raw { user_app_id, data_group } if *sid == short_id && *user_app_id == app_id => {
                Some(decdrm_data::datagroup::DataGroup::parse(data_group).expect("intact data group").data)
            }
            _ => None,
        })
        .flatten()
        .collect()
}

/// TPEG and raw data applications and alternative frequencies (SDC types 3, 4, 7, 11):
/// the receiver captures the data byte for byte and lists the frequencies, with the
/// schedule active at the signalled time.
#[test]
fn tpeg_raw_data_and_alternative_frequencies() {
    let dir = tempfile::tempdir().unwrap();
    let tpeg: Vec<u8> = (0..3000u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(dir.path().join("tpeg.bin"), &tpeg).unwrap();
    let other = b"DecDRM raw application data. ".repeat(20);
    std::fs::write(dir.path().join("other.bin"), &other).unwrap();
    let toml = r#"
        [channel]
        mode = "B"
        occupancy = 3
        interleaving = "short"
        [output]
        file = "afs.flac"
        [time]
        start = "2026-09-30T07:59:50Z"   # a Wednesday, inside the schedule below
        [[service]]
        label = "Radio"
        id = 0xD0D0A1
        [service.audio]
        codec = "aac"
        core_rate = 12000
        input = { tone_hz = 1000.0 }
        [[service]]
        label = "Traffic"
        id = 0xD0D0A2
        [service.data]
        type = "tpeg"
        path = "tpeg.bin"
        bitrate = 4000
        segment_size = 200
        [[service.app]]
        type = "raw"
        app_id = 0x123
        path = "other.bin"
        bitrate = 1000
        [[afs.multiplex]]
        khz = [5990, 7440]
        schedule = 1
        [[afs.other]]
        service = 0
        system = "fm"
        id = 0xD3C2
        mhz = [98.1]
        region = 1
        [[afs.schedule]]
        id = 1
        days = "Mon-Fri"
        start = "06:00"
        duration_min = 180
        [[afs.region]]
        id = 1
        latitude = 45
        longitude = -10
        latitude_extent = 15
        longitude_extent = 30
        ciraf = [27, 28]
    "#;
    let (_, _, status) = transmit(dir.path(), toml, 40);
    assert_clean_encoding(&status);
    let d = decode(&dir.path().join("afs.flac"), false);

    // The capture starts somewhere in the carousel: it must be a gapless run of the file
    // repeated, at least one full copy long.
    for (app_id, file) in [(0x004, &tpeg), (0x123, &other)] {
        let got = captured(&d, 1, app_id);
        let cycle = file.repeat(2 + got.len() / file.len());
        assert!(got.len() >= file.len(), "application {app_id:#X}: only {} bytes captured", got.len());
        assert!(
            cycle.windows(got.len()).any(|w| w == got.as_slice()),
            "application {app_id:#X}: the {} bytes captured are not the file's",
            got.len()
        );
    }
    let apps: Vec<Option<u16>> =
        d.session.ensemble().service(1).unwrap().applications.iter().map(|a| a.user_app_id()).collect();
    assert_eq!(apps, [Some(0x004), Some(0x123)]);

    let ens = d.session.ensemble();
    let lines = decdrm_engine::afs::describe(ens.alternative_frequencies(), ens.time());
    println!("AFS: {lines:#?}");
    for needle in [
        "this multiplex: 5990 kHz, 7440 kHz · schedule 1 (active)",
        "service 0 also on FM id D3C2: 98.1 MHz · region 1",
        "schedule 1: Mon Tue Wed Thu Fri 06:00 UTC for 180 min",
        "region 1: lat 45..60°, lon -10..20°, CIRAF 27 28",
    ] {
        assert!(lines.iter().any(|l| l == needle), "missing \"{needle}\" in {lines:#?}");
    }
}

/// A non-looping input file ends the programme: the station reports it (the CLI then
/// stops), and the file gets the frames plus the channel filter's tail.
#[test]
fn finite_input_file_ends() {
    let dir = tempfile::tempdir().unwrap();
    write_tone(&dir.path().join("short.wav"), 32_000, 1, 440.0, 0.3, 1.0);
    let toml = r#"
        [output]
        file = "out.wav"
        [[service]]
        label = "Short"
        id = 7
        [service.audio]
        codec = "aac"
        input = { file = "short.wav", loop = false }
    "#;
    let mut cfg = StationConfig::from_toml_str(toml).unwrap();
    cfg.base_dir = Some(dir.path().to_path_buf());
    let mut station = Station::new(cfg).unwrap();
    assert!(station.inputs_finite() && !station.inputs_finished());
    let mut frames = 0u64;
    while !station.inputs_finished() {
        station.transmit_frame().unwrap();
        frames += 1;
        assert!(frames < 20, "the input never ended");
    }
    // 1 s of audio = 2.5 frames of 400 ms.
    assert_eq!(frames, 3);
    let status = station.finish().unwrap();
    assert!(status.services[0].audio.as_ref().unwrap().input_finished);
    let written = FileReader::open(dir.path().join("out.wav")).unwrap().total_frames().unwrap();
    assert!((frames * 19_200..frames * 19_200 + 2_000).contains(&written), "{written} frames in the file");
}

/// Sound-card output: 3 s to a (virtual) device. Never run automatically.
#[test]
#[ignore = "plays to a sound card: DECDRM_TX_DEVICE (default \"CABLE-A Input\")"]
fn sound_card_output() {
    let device = std::env::var("DECDRM_TX_DEVICE").unwrap_or_else(|_| "CABLE-A Input".into());
    let toml = format!(
        r#"
        [output]
        device = "{device}"
        [[service]]
        label = "Card Test"
        id = 1
        [service.audio]
        codec = "he-aac"
        input = {{ tone_hz = 1000.0 }}
        "#
    );
    let cfg = StationConfig::from_toml_str(&toml).unwrap();
    let mut station = Station::new(cfg).unwrap();
    let started = Instant::now();
    station.run_frames(8).unwrap();
    let status = station.finish().unwrap();
    println!("{:?}: {:.1} s in {:.2} s", status.device, status.seconds, started.elapsed().as_secs_f64());
    assert!(started.elapsed().as_secs_f64() > 2.0, "the sound card paces the station");
}
