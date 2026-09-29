//! DecDRM's EnCodec extension in the station:
//!
//! * `encodec_configuration` — planning and validation (with or without the `encodec`
//!   feature);
//! * `encodec_mode_b_*`, `encodec_mode_d_*` — end-to-end loopbacks: the station writes a
//!   few seconds of signal to a WAV file and the receiver (`decdrm_engine::Session`)
//!   decodes it. They need `--features encodec` and the model weights
//!   (`decdrm models download encodec`) and are skipped without the weights.
//!
//! Run with `--nocapture` to see the multiplex plans and what was decoded.

use decdrm_station::{Codec, StationConfig};

/// The stream a single audio service gets in mode D, 10 kHz, 16-QAM, protection 1
/// (381 bytes) carries 6 kbit/s with 40 ms CRC groups, next to text messages; and the
/// configuration is checked like any other.
#[test]
fn encodec_configuration() {
    let toml = |audio: &str| {
        format!(
            "[channel]\nmode = \"D\"\nmsc_mode = \"16-QAM\"\nsdc_mode = \"4-QAM\"\n[output]\nfile = \"x.wav\"\n\
             [[service]]\nlabel = \"EnCodec\"\nid = 0xD0D0E1\n[service.audio]\n{audio}\ninput = {{ tone_hz = 440.0 }}\n"
        )
    };
    let cfg = StationConfig::from_toml_str(&toml("codec = \"encodec\"\ntext = [\"Hi\"]")).unwrap();
    assert_eq!(cfg.services[0].audio.as_ref().unwrap().codec, Codec::Encodec);
    match cfg.validate() {
        Ok(plan) => {
            let a = plan.services[0].audio.as_ref().unwrap();
            let c = a.encodec.expect("EnCodec configuration");
            println!("{}", plan.describe(&cfg));
            assert_eq!((c.bandwidth.kbps(), c.group_frames, c.repeated_layers), (6.0, 3, 0));
            assert_eq!((a.input_rate, a.input_channels, a.frames_per_super_frame), (24_000, 1, 30));
            assert_eq!(a.super_frame_len, 381 - 4);
            assert_eq!(a.params.codec, decdrm_core::mux::service::AudioCodec::Encodec);
            assert_eq!(a.params.codec_config, c.codec_config());
            assert!(plan.describe(&cfg).contains("EnCodec 6 kbit/s (8 codebooks)"));
        }
        // Without the codec or the weights the station says so up front.
        Err(e) if !decdrm_encodec::BUILT_IN => assert!(e.to_string().contains("not built in"), "{e}"),
        Err(e) => {
            assert!(decdrm_encodec::find_weights().is_err(), "{e}");
            assert!(e.to_string().contains("decdrm models download encodec"), "{e}");
        }
    }
    // Invalid settings.
    let problems = |audio: &str| StationConfig::from_toml_str(&toml(audio)).unwrap().validate().unwrap_err().to_string();
    let e = problems("codec = \"encodec\"\nstereo = true\ncore_rate = 12000\nbandwidth_kbps = 5");
    assert!(e.contains("mono") && e.contains("24 kHz") && e.contains("bandwidth_kbps 5"), "{e}");
    assert!(problems("codec = \"aac\"\nbandwidth_kbps = 6").contains("only used by codec = \"encodec\""));
    if decdrm_encodec::BUILT_IN && decdrm_encodec::find_weights().is_ok() {
        // 12 kbit/s does not fit into 381 bytes.
        let e = problems("codec = \"encodec\"\nbandwidth_kbps = 12");
        assert!(e.contains("EnCodec 12 kbit/s needs at least") && e.contains("lower bandwidth_kbps"), "{e}");
    }
}

#[cfg(feature = "encodec")]
mod loopback {
    use decdrm_core::mux::service::AudioCodec;
    use decdrm_engine::{InputFormat, RealChannel, ReceiverConfig, Session, SessionEvent};
    use decdrm_io::{AudioFormat, Container, Encoding, FileReader, FileWriter};
    use decdrm_station::{MultiplexPlan, Station, StationConfig, StationStatus};
    use std::path::Path;
    use std::time::Instant;

    /// What the receiver produced.
    struct Decoded {
        session: Session,
        audio: Vec<f32>,
        rate: u32,
        channels: usize,
        texts: Vec<String>,
    }

    fn weights_installed() -> bool {
        match decdrm_encodec::find_weights() {
            Ok(_) => true,
            Err(e) => {
                eprintln!("skipped: {e}");
                false
            }
        }
    }

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
            assert_eq!((a.counters.encoder_errors, a.counters.frames_dropped), (0, 0));
        }
        (plan, status)
    }

    fn decode(path: &Path) -> Decoded {
        let started = Instant::now();
        let mut reader = FileReader::open(path).unwrap();
        let channels = reader.format().channels;
        let input = InputFormat::Real(RealChannel::Mix);
        let mut d = Decoded {
            session: Session::new(ReceiverConfig { input, channels, ..Default::default() }),
            audio: Vec::new(),
            rate: 0,
            channels: 0,
            texts: Vec::new(),
        };
        while let Some(block) = reader.read(4800).unwrap() {
            for ev in d.session.push(&block) {
                match ev {
                    SessionEvent::Audio(pcm) => {
                        d.rate = pcm.sample_rate;
                        d.channels = usize::from(pcm.channels);
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
            "decoded {:.1} s in {:.2} s: \"{}\", {} super frames ok, {} concealed, {} super frame errors; texts {:?}",
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

    /// Frequency (1 Hz steps) of the strongest component in the last second, and its
    /// amplitude.
    fn dominant_tone(d: &Decoded, lo: f64, hi: f64) -> (f64, f64) {
        let n = d.rate as usize;
        assert!(d.audio.len() > 2 * n, "only {} samples decoded", d.audio.len());
        let seg = &d.audio[d.audio.len() - n..];
        let power = |f: f64| {
            let w = std::f64::consts::TAU * f / f64::from(d.rate);
            let (c, mut s1, mut s2) = (2.0 * w.cos(), 0.0, 0.0);
            for &v in seg {
                let s0 = f64::from(v) + c * s1 - s2;
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
        (best, (4.0 * best_p).sqrt() / n as f64)
    }

    /// A tone WAV at 44.1 kHz stereo (the station resamples to 24 kHz and mixes).
    fn write_tone(path: &Path, freq: f64, amp: f32, seconds: f64) {
        let mut w = FileWriter::create(path, AudioFormat::new(44_100, 2), Container::Wav, Encoding::Float32).unwrap();
        let block: Vec<f32> = (0..(seconds * 44_100.0) as usize)
            .flat_map(|i| {
                let v = amp * (std::f64::consts::TAU * freq * i as f64 / 44_100.0).sin() as f32;
                [v, v]
            })
            .collect();
        w.write(&block).unwrap();
        w.finalize().unwrap();
    }

    fn check_service(d: &Decoded, plan: &MultiplexPlan, kbps: f64, text: &str) {
        let st = &d.session.audio_stats;
        assert!(st.codec.starts_with(&format!("EnCodec {kbps} kbit/s")), "{}", st.codec);
        assert!(st.frames_ok >= 12, "{} super frames decoded", st.frames_ok);
        assert_eq!((st.frames_concealed, st.super_frame_errors), (0, 0), "every CRC must pass");
        assert_eq!((d.rate, d.channels), (24_000, 1));
        let (f, amp) = dominant_tone(d, 100.0, 3000.0);
        println!("tone {f} Hz, amplitude {amp:.3}");
        assert_eq!(f, 440.0);
        assert!((amp / 0.25 - 1.0).abs() < 0.3, "amplitude {amp}");
        assert!(d.texts.iter().any(|t| t == text), "{:?}", d.texts);
        // The receiver sees exactly the signalling the plan sent.
        let svc = d.session.ensemble().service(0).expect("service 0");
        let audio = svc.audio.as_ref().expect("audio information");
        assert_eq!(audio.codec, AudioCodec::Encodec);
        assert_eq!(Some(audio), plan.services[0].audio.as_ref().map(|a| &a.params));
        let views = d.session.service_views();
        assert!(views[0].description.starts_with(&format!("EnCodec {kbps} kbit/s mono 24 kHz, text")), "{}", views[0].description);
    }

    /// Mode B, 10 kHz, 64-QAM: 12 kbit/s with codebooks 0-7 sent twice.
    #[test]
    fn encodec_mode_b_10khz_loopback() {
        if !weights_installed() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        write_tone(&dir.path().join("tone.wav"), 440.0, 0.25, 3.0);
        let toml = r#"
            [channel]
            mode = "B"
            occupancy = 3
            msc_mode = "64-QAM"
            interleaving = "short"
            protection_b = 1
            [output]
            file = "encodec_b.wav"
            [[service]]
            label = "EnCodec B"
            id = 0xD0D0E2
            [service.audio]
            codec = "encodec"
            text = ["EnCodec over DRM"]
            input = { file = "tone.wav" }
        "#;
        let (plan, _) = transmit(dir.path(), toml, 25);
        let c = plan.services[0].audio.as_ref().unwrap().encodec.unwrap();
        assert_eq!((c.bandwidth.kbps(), c.repeated_layers), (12.0, 3));
        let d = decode(&dir.path().join("encodec_b.wav"));
        check_service(&d, &plan, 12.0, "EnCodec over DRM");
    }

    /// Real residual errors: the mode B / 64-QAM signal with white noise, decoded by the
    /// receiver chain and an EnCodec decoder whose statistics show the CRCs, the
    /// repeated layers and the concealment at work. A measurement (about a minute):
    /// `cargo test -p decdrm-station --features encodec --test encodec -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement: run with --ignored --nocapture"]
    fn encodec_under_noise() {
        use decdrm_core::mux::audio::AudioDeframer;
        use decdrm_core::mux::{Ensemble, demultiplex};
        use decdrm_core::rx::{Receiver, ReceiverEvent};
        use decdrm_encodec::{EncodecConfig, EncodecDecoder, EncodecModel, ModelParts};
        if !weights_installed() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"
            [channel]
            mode = "B"
            occupancy = 3
            msc_mode = "64-QAM"
            interleaving = "long"
            protection_b = 1
            [output]
            file = "clean.wav"
            [[service]]
            label = "EnCodec"
            id = 0xD0D0E4
            [service.audio]
            codec = "encodec"
            input = { tone_hz = 700.0 }
        "#;
        transmit(dir.path(), toml, 60);
        // The same channel with HE-AAC, for comparison.
        let aac = toml.replace("codec = \"encodec\"", "codec = \"he-aac\"").replace("clean.wav", "aac.wav");
        transmit(dir.path(), &aac, 60);
        let read = |name: &str| {
            let mut reader = FileReader::open(dir.path().join(name)).unwrap();
            let mut v = Vec::new();
            while let Some(b) = reader.read(48_000).unwrap() {
                v.extend(b);
            }
            v
        };
        let (clean, clean_aac) = (read("clean.wav"), read("aac.wav"));
        let power = clean.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / clean.len() as f64;
        let model = EncodecModel::load_default(ModelParts::Decoder).unwrap();
        println!(
            "{:>7} {:>7} {:>6} {:>15} {:>9} {:>10} {:>11} {:>15}",
            "SNR dB", "rx SNR", "SFs", "regions failed", "repaired", "concealed", "unverified", "HE-AAC conc."
        );
        let mut rng = 0x2545_F491_4F6C_DD1Du64;
        let mut uniform = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            ((rng >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        };
        for snr_db in [20.0, 16.0, 15.5, 15.0, 14.5, 14.0, 13.0] {
            // White noise over 0-24 kHz; the SNR counts the 10 kHz of the channel.
            let sigma = (power * 24.0 / 10.0 / 10f64.powf(snr_db / 10.0)).sqrt();
            let mut add_noise = |x: &[f32]| -> Vec<f32> {
                x.iter()
                    .map(|&v| {
                        let g = (-2.0 * uniform().ln()).sqrt() * (std::f64::consts::TAU * uniform()).cos();
                        (f64::from(v) + sigma * g) as f32
                    })
                    .collect()
            };
            let noisy = add_noise(&clean);
            // HE-AAC: frames FDK had to conceal (its CRC covers the sensitive bits only).
            let mut session = Session::new(ReceiverConfig { input: InputFormat::Real(RealChannel::Mix), channels: 1, ..Default::default() });
            for block in add_noise(&clean_aac).chunks(4800) {
                session.push(block);
            }
            let a = &session.audio_stats;
            let aac_concealed = 100.0 * a.frames_concealed as f64 / (a.frames_ok + a.frames_concealed).max(1) as f64;
            let mut rx = Receiver::new(ReceiverConfig { input: InputFormat::Real(RealChannel::Mix), channels: 1, ..Default::default() });
            let mut ens = Ensemble::new();
            let mut msc_config = None;
            let mut audio: Option<(AudioDeframer, EncodecDecoder)> = None;
            for block in noisy.chunks(4800) {
                for ev in rx.push(block) {
                    match ev {
                        ReceiverEvent::Fac(f) => {
                            ens.update_fac(&f);
                        }
                        ReceiverEvent::Sdc(b) if b.crc_ok => {
                            ens.update_sdc(&b.data);
                        }
                        ReceiverEvent::Msc(frame) if frame.complete => {
                            let Some(mux) = ens.multiplex() else { continue };
                            if audio.is_none()
                                && let Some(p) = ens.service(0).and_then(|s| s.audio.clone())
                            {
                                let stream = ens.stream_lengths()[usize::from(p.stream_id)];
                                let config = EncodecConfig::from_codec_config(&p.codec_config).unwrap();
                                let dec = EncodecDecoder::new(std::sync::Arc::clone(&model), config).unwrap();
                                audio = Some((AudioDeframer::new(&p, stream).unwrap(), dec));
                            }
                            if let (Some((deframer, dec)), Some(Some(lf))) = (audio.as_mut(), demultiplex(&frame, mux).first()) {
                                for f in deframer.push_frame(lf).frames {
                                    dec.decode_super_frame(&f.data).unwrap();
                                }
                            }
                        }
                        _ => {}
                    }
                    let c = ens.msc_config();
                    if c != msc_config {
                        msc_config = c;
                        rx.set_msc_config(c);
                    }
                }
            }
            let Some((_, dec)) = audio else {
                println!("{snr_db:>8.1}  no audio service acquired");
                continue;
            };
            let s = dec.stats();
            let frames = (s.super_frames * 30).max(1) as f64;
            println!(
                "{snr_db:>7.1} {:>7} {:>6} {:>15} {:>9} {:>9.2}% {:>10.2}% {:>14.2}%",
                rx.status().snr_db.map(|v| format!("{v:.1}")).unwrap_or_default(),
                s.super_frames,
                s.regions_failed,
                s.regions_repaired,
                100.0 * s.frames_concealed as f64 / frames,
                100.0 * s.frames_unverified as f64 / frames,
                aac_concealed
            );
        }
    }

    /// Mode D, 10 kHz, 16-QAM, long interleaving and the small 4-QAM SDC (15 bytes: the
    /// multiplex description and the EnCodec audio information fill it exactly): the
    /// robust case, 6 kbit/s.
    #[test]
    fn encodec_mode_d_10khz_loopback() {
        if !weights_installed() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        write_tone(&dir.path().join("tone.wav"), 440.0, 0.25, 3.0);
        let toml = r#"
            [channel]
            mode = "D"
            occupancy = 3
            msc_mode = "16-QAM"
            sdc_mode = "4-QAM"
            interleaving = "long"
            protection_b = 1
            [output]
            file = "encodec_d.wav"
            [time]
            enabled = false
            [[service]]
            label = "Mode D"
            id = 0xD0D0E3
            [service.audio]
            codec = "encodec"
            bandwidth_kbps = 6
            text = ["Robust"]
            input = { file = "tone.wav" }
        "#;
        let (plan, _) = transmit(dir.path(), toml, 35);
        assert_eq!(plan.sdc_capacity, 15);
        let d = decode(&dir.path().join("encodec_d.wav"));
        check_service(&d, &plan, 6.0, "Robust");
    }
}
