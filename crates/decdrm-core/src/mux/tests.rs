//! Integration tests of the multiplex layer on real recordings in `samples/`: the
//! receiver decodes FAC, SDC and MSC, the [`Ensemble`] follows the SDC and provides the
//! MSC configuration, the MSC is demultiplexed, and audio super frames and data packets
//! are checked. A test passes (skips) when its recording is absent. Run with
//! `cargo test -p decdrm-core mux -- --nocapture` to see what was decoded.

use super::audio::{AudioDeframer, AudioFrame};
use super::msc::{LogicalFrame, demultiplex};
use super::sdc::{EntityBody, StreamLengths, parse_sdc};
use super::service::{AudioCodec, AudioMode, AudioParams, Ensemble};
use super::text::{TextEvent, TextMessageDecoder};
use crate::fec::crc::crc16;
use crate::rx::{InputFormat, MscConfig, MscFrame, RealChannel, Receiver, ReceiverConfig, ReceiverEvent};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn sample_path(name: &str) -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples").join(name);
    if p.exists() {
        Some(p)
    } else {
        eprintln!("skipping: {} not found", p.display());
        None
    }
}

/// Interleaved samples at 48 kHz (resampled if needed) and the channel count; at most
/// `max_seconds` of the recording.
fn load(path: &Path, max_seconds: f64) -> (Vec<f32>, usize) {
    let mut reader = claxon::FlacReader::open(path).expect("open flac");
    let info = reader.streaminfo();
    let ch = info.channels as usize;
    let scale = 1.0 / (1u64 << (info.bits_per_sample - 1)) as f32;
    let max = (max_seconds * f64::from(info.sample_rate)) as usize * ch;
    let samples: Vec<f32> = reader.samples().take(max).map(|s| s.expect("flac sample") as f32 * scale).collect();
    if info.sample_rate == 48_000 {
        return (samples, ch);
    }
    let frames = samples.len() / ch;
    let ratio = 48_000.0 / f64::from(info.sample_rate);
    let chans: Vec<Vec<f32>> = (0..ch)
        .map(|c| {
            let x: Vec<crate::Cplx> = (0..frames).map(|i| crate::Cplx::new(f64::from(samples[i * ch + c]), 0.0)).collect();
            let mut y = Vec::new();
            crate::dsp::resampler::FracResampler::new().process(&x, ratio, &mut y);
            y.iter().map(|v| v.re as f32).collect()
        })
        .collect();
    let n = chans.iter().map(Vec::len).min().unwrap_or(0);
    ((0..n).flat_map(|i| chans.iter().map(move |c| c[i])).collect(), ch)
}

/// Checks on the first bits of a DRM AAC frame (after its CRC byte), mirroring what
/// FDK-AAC's DRM element list reads first: `ics_info` (ics_reserved_bit, window
/// sequence and shape, max_sfb within the 960-transform table, ISO 14496-3), and for
/// mono cores `tns_data_present` and `ltp_data_present`, which FDK rejects when set.
/// The CRC itself covers bit ranges of the side information only an AAC decoder can
/// delimit. A random frame passes with probability ≈ 0.2 (mono) / 0.4 (stereo).
fn aac_frame_plausible(frame: &[u8], p: &AudioParams) -> bool {
    let bits = crate::bits::unpack(&frame[..frame.len().min(4)]);
    if bits.len() < 16 {
        return false;
    }
    let mut r = crate::bits::BitReader::from_bits(&bits);
    let reserved = r.read(1);
    let window_sequence = r.read(2);
    let _shape = r.read(1);
    let short = window_sequence == 2;
    let max_sfb = r.read(if short { 4 } else { 6 });
    let limit = match (short, p.sample_rate_hz) {
        (true, _) => 15,
        (false, 24_000) => 46,
        (false, _) => 42,
    };
    if reserved != 0 || max_sfb > limit {
        return false;
    }
    if p.mode == AudioMode::Stereo {
        return true;
    }
    if short {
        r.skip(7); // scale_factor_grouping
    }
    let _tns_present = r.read(1);
    r.read(1) == 0 // ltp_data_present
}

#[derive(Debug, Default)]
struct AudioStats {
    params: Option<AudioParams>,
    stream: StreamLengths,
    super_frames: usize,
    super_frames_ok: usize,
    /// Σ frame lengths + header + CRC bytes (+ text bytes) == logical frame length.
    lengths_consistent: usize,
    frames: usize,
    frames_plausible: usize,
    crc_checked: usize,
    crc_ok: usize,
    xhe_header_crc_ok: usize,
    errors: BTreeMap<String, usize>,
    texts: Vec<String>,
}

#[derive(Debug, Default, Clone, Copy)]
struct PacketStats {
    packet_length: u8,
    packets: usize,
    crc_ok: usize,
}

#[derive(Debug, Default)]
struct Report {
    ensemble: Ensemble,
    fac: usize,
    sdc_ok: usize,
    sdc_bad: usize,
    entity_types: BTreeSet<u8>,
    /// Distinct raw audio information entities (type 9) seen.
    audio_entities: BTreeSet<String>,
    msc_config: Option<MscConfig>,
    msc_frames: usize,
    demuxed: usize,
    audio: AudioStats,
    /// Packet-mode data streams by stream id.
    packets: BTreeMap<u8, PacketStats>,
}

struct Runner {
    rep: Report,
    deframer: Option<(AudioParams, StreamLengths, AudioDeframer)>,
    text: TextMessageDecoder,
}

impl Runner {
    fn msc(&mut self, frame: &MscFrame) {
        self.rep.msc_frames += 1;
        if !frame.complete {
            // The long interleaver is still filling: erasures, not a demux problem.
            return;
        }
        let Some(mux) = self.rep.ensemble.multiplex() else { return };
        let streams = demultiplex(frame, mux);
        if streams.iter().any(Option::is_some) {
            self.rep.demuxed += 1;
        }
        let lengths = self.rep.ensemble.stream_lengths();
        // Audio: the first service with audio parameters.
        let audio = self.rep.ensemble.services().find_map(|s| s.audio.clone());
        if let Some(p) = audio
            && let Some(Some(lf)) = streams.get(usize::from(p.stream_id))
        {
            let sl = lengths[usize::from(p.stream_id)];
            self.audio(&p, sl, lf);
        }
        // Packet-mode data streams: every packet carries a CRC-16 (§6.6.1).
        let apps: Vec<(u8, u8)> = self
            .rep
            .ensemble
            .services()
            .flat_map(|s| s.applications.iter())
            .filter(|a| a.packet_mode)
            .map(|a| (a.stream_id, a.packet_length))
            .collect();
        for (stream_id, packet_length) in apps {
            let Some(Some(lf)) = streams.get(usize::from(stream_id)) else { continue };
            let st = self.rep.packets.entry(stream_id).or_default();
            st.packet_length = packet_length;
            let total = usize::from(packet_length) + 3;
            for p in lf.data.chunks_exact(total) {
                st.packets += 1;
                if crc16(&p[..total - 2]) == u16::from_be_bytes([p[total - 2], p[total - 1]]) {
                    st.crc_ok += 1;
                }
            }
        }
    }

    fn audio(&mut self, p: &AudioParams, sl: StreamLengths, lf: &LogicalFrame) {
        let rebuild = self.deframer.as_ref().is_none_or(|(q, l, _)| q != p || *l != sl);
        if rebuild {
            match AudioDeframer::new(p, sl) {
                Ok(d) => self.deframer = Some((p.clone(), sl, d)),
                Err(e) => {
                    *self.rep.audio.errors.entry(e.to_string()).or_default() += 1;
                    return;
                }
            }
            self.text.reset();
        }
        let Some((_, _, d)) = self.deframer.as_mut() else { return };
        let out = d.push_frame(lf);
        let a = &mut self.rep.audio;
        a.params = Some(p.clone());
        a.stream = sl;
        a.super_frames += 1;
        if let Some(e) = &out.error {
            *a.errors.entry(e.to_string()).or_default() += 1;
        } else {
            a.super_frames_ok += 1;
        }
        if out.xhe.is_some_and(|h| h.header_crc_ok) {
            a.xhe_header_crc_ok += 1;
        }
        if let Some(n) = out.nominal_frames
            && out.error.is_none()
        {
            let header = match p.codec {
                AudioCodec::Opus => 30,
                _ => (12 * (n - 1)).div_ceil(8),
            };
            let text = if p.text_flag { 4 } else { 0 };
            let sum: usize = out.frames.iter().map(|f: &AudioFrame| f.data.len()).sum();
            let consistent = if p.codec == AudioCodec::Opus {
                sum + header + n + text <= lf.data.len()
            } else {
                sum + header + n + text == lf.data.len()
            };
            if consistent {
                a.lengths_consistent += 1;
            }
        }
        for f in &out.frames {
            a.frames += 1;
            if let Some(ok) = f.crc_ok {
                a.crc_checked += 1;
                a.crc_ok += usize::from(ok);
            }
            if p.codec == AudioCodec::Aac && aac_frame_plausible(&f.data, p) {
                a.frames_plausible += 1;
            }
        }
        if let Some(piece) = out.text
            && let Some(TextEvent::Message(m)) = self.text.push(piece)
        {
            a.texts.push(m.text());
        }
    }
}

/// Run the receiver over (at most `max_seconds` of) a recording.
fn run(name: &str, max_seconds: f64) -> Option<Report> {
    let path = sample_path(name)?;
    let (samples, ch) = load(&path, max_seconds);
    let cfg = ReceiverConfig { input: InputFormat::Real(RealChannel::Mix), channels: ch, ..Default::default() };
    let mut rx = Receiver::new(cfg);
    let mut run = Runner { rep: Report::default(), deframer: None, text: TextMessageDecoder::new() };
    for chunk in samples.chunks(4800 * ch) {
        for ev in rx.push(chunk) {
            match ev {
                ReceiverEvent::Fac(f) => {
                    run.rep.fac += 1;
                    run.rep.ensemble.update_fac(&f);
                }
                ReceiverEvent::Sdc(b) if b.crc_ok => {
                    run.rep.sdc_ok += 1;
                    for e in parse_sdc(&b.data) {
                        run.rep.entity_types.insert(e.entity_type());
                        if let EntityBody::Audio(a) = &e.body {
                            run.rep.audio_entities.insert(format!("{a:?} (version {})", e.version));
                        }
                    }
                    run.rep.ensemble.update_sdc(&b.data);
                }
                ReceiverEvent::Sdc(_) => run.rep.sdc_bad += 1,
                ReceiverEvent::Msc(frame) => run.msc(&frame),
                _ => {}
            }
        }
        let cfg = run.rep.ensemble.msc_config();
        if cfg != run.rep.msc_config {
            rx.set_msc_config(cfg);
            run.rep.msc_config = cfg;
        }
    }
    let rep = run.rep;
    print_report(name, &rep);
    Some(rep)
}

fn print_report(name: &str, r: &Report) {
    println!("=== {name}");
    println!("FAC ok {}, SDC ok {} bad {}, entity types {:?}", r.fac, r.sdc_ok, r.sdc_bad, r.entity_types);
    for a in &r.audio_entities {
        println!("type 9 entity: {a}");
    }
    if let Some(c) = r.ensemble.channel() {
        println!(
            "channel: {:?}, {:?}, MSC {:?}, SDC {:?}, services {}+{}",
            c.occupancy, c.interleaving, c.msc_mode, c.sdc_mode, c.num_audio, c.num_data
        );
    }
    if let Some(m) = r.ensemble.multiplex() {
        println!(
            "multiplex: protection A {} B {}, streams {:?}",
            m.protection_a,
            m.protection_b,
            r.ensemble.stream_lengths()
        );
    }
    println!("MSC config: {:?}", r.msc_config);
    for s in r.ensemble.services() {
        println!(
            "service {}: id {:06X} label {:?} {} lang {:?} ({:?}/{:?}) type {:?} app id {:?}",
            s.short_id,
            s.service_id().unwrap_or(0),
            s.label,
            if s.is_audio() { "audio" } else { "data" },
            s.fac_language(),
            s.language_code,
            s.country_code,
            s.programme_type(),
            s.application_id()
        );
        if let Some(a) = &s.audio {
            println!(
                "  audio: {:?} sbr={} {:?} {} Hz text={} stream {} type9 {:02X?}",
                a.codec, a.sbr, a.mode, a.sample_rate_hz, a.text_flag, a.stream_id, a.type9_bytes
            );
        }
        for app in &s.applications {
            println!(
                "  application: stream {} packet_mode {} id {} len {} domain {} user app {:?} data {:02X?}",
                app.stream_id,
                app.packet_mode,
                app.packet_id,
                app.packet_length,
                app.app_domain,
                app.user_app_id().map(|v| format!("{v:#05X}")),
                app.application_data
            );
        }
    }
    if let Some(t) = r.ensemble.time() {
        let (y, m, d) = t.date();
        println!("time: {y:04}-{m:02}-{d:02} {:02}:{:02} UTC, offset {:?} min", t.hour, t.minute, t.local_offset_minutes());
    }
    let afs = r.ensemble.alternative_frequencies();
    if !afs.is_empty() {
        println!("AFS: {:?}", afs);
    }
    println!("MSC frames {}, demultiplexed {}", r.msc_frames, r.demuxed);
    let a = &r.audio;
    println!(
        "audio super frames {} ok {} consistent lengths {}; frames {} plausible {} crc {}/{} xHE header crc ok {}; errors {:?}",
        a.super_frames,
        a.super_frames_ok,
        a.lengths_consistent,
        a.frames,
        a.frames_plausible,
        a.crc_ok,
        a.crc_checked,
        a.xhe_header_crc_ok,
        a.errors
    );
    for t in &a.texts {
        println!("text: {t:?}");
    }
    for (s, p) in &r.packets {
        println!("stream {s}: packets of {}+3 bytes: {} ({} CRC ok)", p.packet_length, p.packets, p.crc_ok);
    }
}

fn ratio(n: usize, d: usize) -> f64 {
    if d == 0 { 0.0 } else { n as f64 / d as f64 }
}

#[test]
fn dw_mode_b_sdc_and_aac_super_frames() {
    let Some(r) = run("DW_ModeB_10kHz.flac", 60.0) else { return };
    assert!(r.sdc_ok >= 10, "SDC blocks: {}", r.sdc_ok);
    let mux = r.ensemble.multiplex().expect("multiplex description");
    assert!(!mux.streams.is_empty());
    let svc = r.ensemble.services().find(|s| s.audio.is_some()).expect("audio service");
    assert!(svc.label.as_deref().is_some_and(|l| !l.trim().is_empty()), "label {:?}", svc.label);
    assert!(r.msc_config.is_some());
    assert!(r.msc_frames >= 50, "MSC frames {}", r.msc_frames);
    let a = &r.audio;
    assert_eq!(a.params.as_ref().map(|p| p.codec), Some(AudioCodec::Aac));
    assert!(a.super_frames >= 50);
    assert!(ratio(a.super_frames_ok, a.super_frames) > 0.9, "{a:?}");
    assert!(ratio(a.lengths_consistent, a.super_frames) > 0.9, "{a:?}");
    assert!(ratio(a.frames_plausible, a.frames) > 0.9, "{a:?}");
}

#[test]
fn dw_journaline_packet_stream() {
    let Some(r) = run("DWwithJournaline_ModeB_10kHz.flac", 60.0) else { return };
    let apps: Vec<_> = r.ensemble.services().flat_map(|s| s.applications.iter()).collect();
    assert!(!apps.is_empty(), "no application information");
    assert!(r.packets.values().any(|p| p.packets > 100 && ratio(p.crc_ok, p.packets) > 0.9), "{:?}", r.packets);
    assert!(ratio(r.audio.super_frames_ok, r.audio.super_frames) > 0.9, "{:?}", r.audio);
}

#[test]
fn rtl_slideshow_packet_stream() {
    let Some(r) = run("RTLwithSlideshow_ModeB_10kHz.flac", 60.0) else { return };
    let apps: Vec<_> = r.ensemble.services().flat_map(|s| s.applications.iter()).collect();
    assert!(apps.iter().any(|a| a.user_app_id() == Some(0x002)), "no MOT slideshow: {apps:?}");
    assert!(r.packets.values().any(|p| p.packets > 100 && ratio(p.crc_ok, p.packets) > 0.9), "{:?}", r.packets);
}

#[test]
fn opus_mode_b_super_frames() {
    let Some(r) = run("Opus_Codec_Test_Mode_B_10kHz.flac", 60.0) else { return };
    let a = &r.audio;
    assert_eq!(a.params.as_ref().map(|p| p.codec), Some(AudioCodec::Opus), "{a:?}");
    assert!(ratio(a.super_frames_ok, a.super_frames) > 0.9, "{a:?}");
    assert!(ratio(a.crc_ok, a.crc_checked) > 0.9, "{a:?}");
}

/// The other Opus recordings use the two other Dream signallings: AAC + sampling rate
/// code 7 (`_V2`) and audio coding 11 without codec config (mode A 20 kHz).
#[test]
fn opus_signalling_variants() {
    for name in ["Opus_Codec_Test_Mode_B_10kHz_V2.flac", "Opus_Codec_Test_Mode_A_20kHz.flac"] {
        let Some(r) = run(name, 40.0) else { continue };
        let a = &r.audio;
        assert_eq!(a.params.as_ref().map(|p| p.codec), Some(AudioCodec::Opus), "{name}: {a:?}");
        assert!(a.frames > 500 && ratio(a.crc_ok, a.crc_checked) > 0.9, "{name}: {a:?}");
    }
}

/// Survey of every other recording (prints what was decoded; no assertions).
#[test]
#[ignore = "survey: run with --ignored --nocapture"]
fn survey_other_recordings() {
    for name in [
        "BBCWS648.flac",
        "BouquetFlevoNL_ModeB_10kHz_14kbps.flac",
        "Deutschlandradio_ModeA_10kHz.flac",
        "Opus_Codec_Test_Mode_A_20kHz_2.flac",
        "Opus_Codec_Test_Mode_A_20kHz_V2.flac",
        "ProjectQoSAM_ModeC_10kHz.flac",
        "RTL_ModeB_10kHz.flac",
        "R_Nigeria_Mode_C_10kHz_flipped_spectrum.flac",
        "Test_Mode_A_10kHz_freq_offset_+60Hz.flac",
        "VoiceOfRussia_ModeB_10kHz.flac",
        "endless_gapless_test.flac",
        "vtc_mot.flac",
    ] {
        let _ = run(name, 60.0);
    }
}

#[test]
fn xhe_aac_super_frames() {
    let Some(r) = run("FMGold_xHE_ModeB_9khz.flac", 70.0) else { return };
    let a = &r.audio;
    assert_eq!(a.params.as_ref().map(|p| p.codec), Some(AudioCodec::XheAac), "{a:?}");
    assert!(a.frames > 100, "{a:?}");
    assert!(ratio(a.crc_ok, a.crc_checked) > 0.9, "{a:?}");
}

#[test]
fn dream_webcam_mode_a() {
    let Some(r) = run("DreamWebcamApp_ModeA_10kHz.flac", 60.0) else { return };
    assert!(r.ensemble.multiplex().is_some());
    assert!(r.ensemble.services().any(|s| s.label.is_some()));
}

/// Transmitter-side encoders feeding the receiver-side parsers, without the PHY:
/// SDC entities → data field → [`Ensemble`] → MSC configuration and stream lengths;
/// AAC super frames with text message pieces, a packet data stream and a hierarchical
/// stream → multiplex frame → demultiplex → [`AudioDeframer`] and text decoder.
#[test]
fn multiplex_loopback() {
    use super::audio::{AacSuperFrameFormat, build_aac_super_frame, insert_text_message};
    use super::msc::{MscGeometry, multiplex};
    use super::sdc::{ApplicationInfo, Label, MultiplexDescription, SdcEntity, TimeAndDate, encode_sdc_data};
    use super::text::TextMessageEncoder;
    use crate::cellmap::CellMap;
    use crate::fac::{ChannelParams, Fac, Interleaving, MscMode, SdcMode, ServiceParams};
    use crate::fec::mlc::MlcParams;
    use crate::params::{RobustnessMode, SpectrumOccupancy};

    let map = CellMap::new(RobustnessMode::B, SpectrumOccupancy::SO_3).unwrap();
    let audio = AudioParams::new(1, AudioCodec::Aac, true, AudioMode::ParametricStereo, 12_000, true, vec![]);
    // Stream 0 hierarchical (VSPP), stream 1 audio (UEP split, pre-V4 style), stream 2
    // packet data (4 packets of 45 + 3 bytes).
    let streams = [StreamLengths { part_a: 40, part_b: 380 }, StreamLengths { part_a: 0, part_b: 4 * 48 }];
    let mut mux = MultiplexDescription::new_hierarchical(0, 1, 2, 0, &streams);
    let params = MlcParams::msc(MscMode::Qam64HmSym.mapping(), map.msc_cells_per_frame, mux.protection(true), mux.part_a_bytes(true));
    mux.streams[0].len_b = (params.bits_vspp / 8) as u16;
    let geometry = MscGeometry::from(&params);

    // --- SDC -> Ensemble
    let app = ApplicationInfo {
        short_id: 1,
        stream_id: 2,
        packet_mode: true,
        data_unit_indicator: true,
        packet_id: 1,
        app_domain: 1,
        packet_length: 45,
        application_data: vec![0x00, 0x02],
        ..Default::default()
    };
    let entities = [
        SdcEntity::new(false, EntityBody::Multiplex(mux.clone())),
        SdcEntity::new(false, EntityBody::Audio(audio.to_entity(0))),
        SdcEntity::new(false, EntityBody::Application(app.clone())),
        SdcEntity::new(false, EntityBody::Label(Label::new(0, "Loopback"))),
        SdcEntity::new(false, EntityBody::TimeDate(TimeAndDate::from_utc(2026, 9, 29, 20, 15))),
    ];
    let data = encode_sdc_data(&entities, 76).unwrap();
    assert!(data.skipped.is_empty());
    let mut ens = Ensemble::new();
    let fac = |short_id: u8, is_data: bool| Fac {
        channel: ChannelParams {
            enhancement: false,
            frame_index: 0,
            afs_valid: false,
            occupancy: SpectrumOccupancy::SO_3,
            interleaving: Interleaving::Short,
            msc_mode: MscMode::Qam64HmSym,
            sdc_mode: SdcMode::Qam16,
            num_audio: 1,
            num_data: 1,
            reconfiguration_index: 0,
            toggle: false,
        },
        service: ServiceParams {
            service_id: 0xD0_0000 + u32::from(short_id),
            short_id,
            audio_ca: false,
            language: 5,
            is_data,
            descriptor: 2,
            data_ca: false,
        },
    };
    ens.update_fac(&fac(0, false));
    ens.update_fac(&fac(1, true));
    ens.update_sdc(&data.data);
    let cfg = ens.msc_config().unwrap();
    assert_eq!(cfg.part_a_bytes, 40);
    assert_eq!(cfg.protection.hierarchical, 2);
    let svc = ens.service(0).unwrap();
    assert_eq!(svc.label.as_deref(), Some("Loopback"));
    assert_eq!(svc.audio.as_ref(), Some(&audio));
    assert_eq!(ens.service(1).unwrap().applications, vec![app]);
    let lengths = ens.stream_lengths();
    assert_eq!(lengths[1], streams[0]);

    // --- MSC: build frames, multiplex, demultiplex, deframe.
    let fmt = AacSuperFrameFormat::aac(5, lengths[1]);
    let sf_len = lengths[1].total() - 4;
    let payload = fmt.payload_len(sf_len).unwrap();
    let mut text_enc = TextMessageEncoder::new();
    text_enc.add_message("Loopback text message across several segments of the text application");
    let mut deframer = AudioDeframer::new(&audio, lengths[1]).unwrap();
    let mut text_dec = TextMessageDecoder::new();
    let mut texts = Vec::new();
    for k in 0..40u8 {
        let sizes = [payload / 5 - 3, payload / 5 + 2, payload / 5, payload / 5 + 1];
        let last = payload - sizes.iter().sum::<usize>();
        let frames: Vec<AudioFrame> = sizes
            .iter()
            .chain(std::iter::once(&last))
            .enumerate()
            .map(|(i, &l)| AudioFrame::with_crc(vec![k.wrapping_add(i as u8); l], k ^ i as u8))
            .collect();
        let mut audio_lf = build_aac_super_frame(&frames, &fmt, sf_len).unwrap();
        audio_lf.extend_from_slice(&[0; 4]);
        insert_text_message(&mut audio_lf, text_enc.next_piece());
        let hier: Vec<u8> = vec![k; lengths[0].total()];
        let packets: Vec<u8> = (0..4u8)
            .flat_map(|p| {
                let mut pkt = vec![0xC0 | (1 << 4) | (p & 7)];
                pkt.extend(std::iter::repeat_n(k ^ p, 45));
                let crc = crc16(&pkt);
                pkt.extend(crc.to_be_bytes());
                pkt
            })
            .collect();
        let block = multiplex(&[&hier, &audio_lf, &packets], &mux, geometry).unwrap();
        let (vspp, main) = block.split_at(geometry.vspp_bits);
        let out = super::msc::demultiplex_bits(vspp, main, &mux);
        assert_eq!(out[0].as_ref().unwrap().data, hier);
        assert!(out[0].as_ref().unwrap().hierarchical);
        let data_lf = out[2].as_ref().unwrap();
        assert!(data_lf.data.chunks(48).all(|p| crc16(&p[..46]) == u16::from_be_bytes([p[46], p[47]])));
        let got = deframer.push_frame(out[1].as_ref().unwrap());
        assert_eq!(got.error, None);
        assert_eq!(got.frames, frames);
        if let Some(TextEvent::Message(m)) = text_dec.push(got.text.unwrap()) {
            texts.push(m.text());
        }
    }
    assert_eq!(texts, vec!["Loopback text message across several segments of the text application".to_string()]);
}
