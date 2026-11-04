//! Configuration parsing, validation and multiplex planning (no signal generated).

use decdrm_core::fac::MscMode;
use decdrm_core::mux::sdc::StreamLengths;
use decdrm_core::mux::service::{AudioCodec, AudioMode};
use decdrm_station::{AppKind, Codec, Part, StationConfig, StationError, StreamContent};
use std::path::Path;

/// A minimal valid configuration: one HE-AAC tone service, output to `out.wav`.
fn base() -> String {
    r#"
    [output]
    file = "out.wav"
    [time]
    start = "2026-09-29T18:00:00Z"
    [[service]]
    label = "Test Radio"
    id = 0x123456
    [service.audio]
    codec = "he-aac"
    input = { tone_hz = 1000.0 }
    "#
    .to_string()
}

fn parse(text: &str) -> StationConfig {
    StationConfig::from_toml_str(text).unwrap_or_else(|e| panic!("{e}\n{text}"))
}

/// The validation problems of `text` (which must be invalid).
fn problems(text: &str) -> Vec<String> {
    match parse(text).validate() {
        Ok(plan) => panic!("expected problems, got a valid plan:\n{}", plan.describe(&parse(text))),
        Err(e) => {
            let p = e.problems().to_vec();
            assert!(!p.is_empty(), "not a configuration error: {e}");
            p
        }
    }
}

fn has(problems: &[String], needle: &str) -> bool {
    problems.iter().any(|p| p.contains(needle))
}

#[test]
fn example_configuration_is_valid() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/station.toml");
    let cfg = StationConfig::load(&path).unwrap();
    let plan = cfg.validate().unwrap_or_else(|e| panic!("{e}"));
    println!("{}", plan.describe(&cfg));
    assert_eq!(cfg.services.len(), 2);
    // Audio stream 0, one data stream shared by Journaline and EPG (packet ids 0, 1).
    assert_eq!(plan.streams.len(), 2);
    let StreamContent::Data { apps, packet_len } = &plan.streams[1].content else { panic!() };
    assert_eq!(*packet_len, 48);
    assert_eq!(apps.iter().map(|a| a.packet_id).collect::<Vec<_>>(), [0, 1]);
    assert_eq!(plan.services[1].apps.iter().map(|a| a.kind).collect::<Vec<_>>(), [AppKind::Journaline, AppKind::Epg]);
    // The news service carries the guide of the radio service.
    assert_eq!((cfg.epg_scope(0), cfg.epg_scope(1)), (0xD0D001, 0xD0D001));
    // Round trip through TOML text.
    let again = StationConfig::from_toml_str(&cfg.to_toml_string().unwrap()).unwrap();
    assert_eq!(again.services, cfg.services);
}

/// Data streams get whole packets for their bit rate, the audio stream the rest of the
/// multiplex frame, and the encoder runs at 97 % of the audio payload.
#[test]
fn allocation_fills_the_msc() {
    let text = base()
        + r#"
    text = ["Hello"]
    [[service.app]]
    type = "epg"
    bitrate = 3000
    packet_length = 60
    [[service.app.programme]]
    title = "News"
    start = "2026-09-29T18:00:00Z"
    duration_min = 30
    "#;
    let cfg = parse(&text);
    let plan = cfg.validate().unwrap();
    // Mode B / SO 3 / 64-QAM / protection 1: 1048 bytes per multiplex frame.
    assert_eq!(plan.capacity.total_bits() / 8, 1048);
    let data = &plan.streams[1];
    // 3000 bit/s = 150 bytes per 400 ms -> 3 packets of 63 bytes.
    assert_eq!(data.lengths, StreamLengths { part_a: 0, part_b: 189 });
    let audio = &plan.streams[0];
    assert_eq!(audio.bytes() + data.bytes(), 1048);
    let a = plan.services[0].audio.as_ref().unwrap();
    assert_eq!((a.codec, a.core_rate, a.frames_per_super_frame, a.input_rate), (Codec::HeAac, 12_000, 5, 24_000));
    assert!(a.text);
    assert_eq!(a.super_frame_len, audio.bytes() - 4);
    // Header of 4 borders (6 bytes) and 5 CRC bytes.
    assert_eq!(a.payload_len, a.super_frame_len - 6 - 5);
    // The encoder runs just below the payload rate (HE-AAC mono: at the 97 % cap).
    let payload_rate = a.payload_len as f64 * 8.0 / 0.4;
    let fill = f64::from(a.encoder_bitrate) / payload_rate;
    assert!((0.95..=0.97).contains(&fill), "encoder at {:.1} % of the payload", 100.0 * fill);
    assert_eq!(
        (a.params.codec, a.params.sbr, a.params.mode, a.params.sample_rate_hz, a.params.text_flag),
        (AudioCodec::Aac, true, AudioMode::Mono, 12_000, true)
    );
    assert!((plan.service_bitrate(0) - 1048.0 * 8.0 / 0.4).abs() < 1e-6);
    // The FDK encoder reports the same SDC type 9 bytes the plan will send.
    let enc = decdrm_codecs::FdkDrmEncoder::new(decdrm_codecs::FdkEncoderConfig::new(
        decdrm_codecs::AacProfile::HeAac,
        12_000,
        a.encoder_bitrate,
    ))
    .unwrap();
    assert_eq!(enc.audio_info().with_text_flag(true).to_type9_bytes(), a.params.type9_bytes);
}

/// Several audio services split the rest by `share`; shared data streams multiplex
/// packet ids; the FAC/SDC service counts follow.
#[test]
fn shares_and_shared_streams() {
    let text = r#"
    [channel]
    mode = "A"
    occupancy = 5
    [output]
    file = "out.flac"
    format = "iq"
    [[service]]
    label = "One"
    id = 1
    [service.audio]
    codec = "aac"
    share = 2
    input = { tone_hz = 500.0 }
    [[service]]
    label = "Two"
    id = 2
    [service.audio]
    codec = "opus"
    stereo = true
    input = { tone_hz = 700.0 }
    [[service]]
    label = "Data"
    id = 3
    [service.data]
    type = "epg"
    stream = "shared"
    packet_id = 3
    [[service.data.programme]]
    title = "X"
    start = "2026-01-01T00:00:00Z"
    duration_min = 10
    [[service.app]]
    type = "epg"
    stream = "shared"
    [[service.app.programme]]
    title = "Y"
    start = "2026-01-01T00:00:00Z"
    duration_min = 10
    "#;
    let cfg = parse(text);
    let plan = cfg.validate().unwrap();
    println!("{}", plan.describe(&cfg));
    assert_eq!(plan.streams.len(), 3);
    let (a, b) = (plan.streams[0].bytes(), plan.streams[1].bytes());
    assert!((a as f64 / b as f64 - 2.0).abs() < 0.01, "{a} vs {b}");
    let StreamContent::Data { apps, .. } = &plan.streams[2].content else { panic!() };
    // The explicit id 3 is kept, the other application takes the first free id.
    assert_eq!(apps.iter().map(|a| a.packet_id).collect::<Vec<_>>(), [3, 0]);
    // 2 × 2000 bit/s = 200 bytes -> 5 packets of 48 bytes.
    assert_eq!(plan.streams[2].bytes(), 240);
    assert_eq!(plan.streams.iter().map(|s| s.bytes()).sum::<usize>(), plan.capacity.total_bits() / 8);
    let opus = plan.services[1].audio.as_ref().unwrap();
    assert_eq!((opus.frames_per_super_frame, opus.input_channels), (20, 2));
    assert_eq!(opus.opus_packet_bytes, (b - 30 - 20) / 20);
}

/// Unequal error protection: a stream in part A changes the MLC's split, and the
/// plan's part A length matches the multiplex description.
#[test]
fn unequal_error_protection() {
    let text = base()
        + r#"
    [[service.app]]
    type = "epg"
    part = "A"
    bitrate = 4000
    [[service.app.programme]]
    title = "News"
    start = "2026-09-29T18:00:00Z"
    duration_min = 30
    "#;
    let text = text.replace("[output]", "[channel]\nprotection_a = 0\nprotection_b = 2\n[output]");
    let cfg = parse(&text);
    let plan = cfg.validate().unwrap();
    let data = &plan.streams[1];
    assert_eq!(data.lengths.part_b, 0);
    assert_eq!(plan.tx.part_a_bytes, data.lengths.part_a);
    assert_eq!(plan.multiplex.part_a_bytes(false), plan.tx.part_a_bytes);
    let total: usize = plan.streams.iter().map(|s| s.bytes()).sum();
    assert!(8 * total <= plan.capacity.hpp_bits + plan.capacity.lpp_bits);
    assert!(8 * plan.tx.part_a_bytes <= plan.capacity.hpp_bits);
    // Nearly nothing is wasted.
    assert!(plan.capacity.hpp_bits + plan.capacity.lpp_bits - 8 * total < 8 * 8, "{:?} vs {total}", plan.capacity);
}

/// Hierarchical modulation: the marked stream is stream 0 and fills the VSPP.
#[test]
fn hierarchical_stream_is_stream_zero() {
    let text = base().replace("[output]", "[channel]\nmsc_mode = \"HMsym\"\nprotection_hierarchical = 1\n[output]")
        + r#"
    [[service.app]]
    type = "epg"
    hierarchical = true
    [[service.app.programme]]
    title = "News"
    start = "2026-09-29T18:00:00Z"
    duration_min = 30
    "#;
    let cfg = parse(&text);
    let plan = cfg.validate().unwrap();
    assert_eq!(plan.tx.msc_mode, MscMode::Qam64HmSym);
    assert!(plan.streams[0].hierarchical);
    assert!(matches!(plan.streams[0].content, StreamContent::Data { .. }));
    assert!(8 * plan.streams[0].bytes() <= plan.capacity.vspp_bits);
    assert!(8 * (plan.streams[0].bytes() + 48) > plan.capacity.vspp_bits, "whole packets fill the VSPP");
    assert_eq!(plan.multiplex.hierarchical_protection(), 1);
    assert_eq!(plan.services[0].audio.as_ref().unwrap().stream, 1);
    // Without a marked stream the mode is rejected.
    let p = problems(&base().replace("[output]", "[channel]\nmsc_mode = \"HMmix\"\n[output]"));
    assert!(has(&p, "hierarchical"), "{p:?}");
}

/// Audio can also be the hierarchical stream, or sit in part A.
#[test]
fn audio_in_hierarchical_stream_or_part_a() {
    let text = base()
        .replace("[output]", "[channel]\nmsc_mode = \"HMsym\"\nprotection_hierarchical = 3\n[output]")
        .replace("codec = \"he-aac\"", "codec = \"aac\"\ncore_rate = 12000\nhierarchical = true");
    let plan = parse(&text).validate().unwrap_or_else(|e| panic!("{e}"));
    assert!(plan.streams[0].hierarchical);
    assert!(matches!(plan.streams[0].content, StreamContent::Audio { service: 0 }));
    assert_eq!(plan.multiplex.streams(true)[0].part_b, plan.streams[0].bytes());
    // With HMmix and the strongest protection the hierarchical part is too small for AAC.
    let p = problems(&text.replace("HMsym", "HMmix").replace("protection_hierarchical = 3", "protection_hierarchical = 0"));
    assert!(has(&p, "the hierarchical stream's length is fixed by the channel"), "{p:?}");
    // Part A: the audio stream fills part A, with the higher protection.
    let text = base().replace("[output]", "[channel]\nprotection_a = 0\nprotection_b = 3\n[output]").replace(
        "codec = \"he-aac\"",
        "codec = \"he-aac\"\npart = \"A\"",
    ) + r#"
    [[service.app]]
    type = "epg"
    bitrate = 2000
    [[service.app.programme]]
    title = "N"
    start = "2026-09-29T18:00:00Z"
    duration_min = 5
    "#;
    let plan = parse(&text).validate().unwrap();
    let audio = &plan.streams[0];
    assert_eq!((audio.lengths.part_b, audio.lengths.part_a), (0, plan.tx.part_a_bytes));
    let total: usize = plan.streams.iter().map(|s| s.bytes()).sum();
    assert!(8 * total <= plan.capacity.hpp_bits + plan.capacity.lpp_bits);
    // One more byte of audio would not fit (the bisection found the maximum).
    let mut lens: Vec<StreamLengths> = plan.streams.iter().map(|s| s.lengths).collect();
    lens[0].part_a += 1;
    let cells = decdrm_core::cellmap::CellMap::new(plan.tx.mode, plan.tx.occupancy).unwrap().msc_cells_per_frame;
    let p = decdrm_core::fec::mlc::MlcParams::msc(plan.tx.msc_mode.mapping(), cells, plan.tx.protection, lens[0].part_a);
    assert!(8 * (total + 1) > p.bits_hpp + p.bits_lpp || p.n1 == 0);
}

/// Part A must be the more strongly protected part. Without a stream in part A its
/// level is neither used nor checked, and the SDC sends 0 (§6.4.3.1).
#[test]
fn part_a_protection_level() {
    let with_part_a = |channel: &str| {
        base()
            .replace("[output]", &format!("[channel]\n{channel}\n[output]"))
            .replace("codec = \"he-aac\"", "codec = \"he-aac\"\npart = \"A\"")
    };
    let p = problems(&with_part_a("protection_a = 1\nprotection_b = 1"));
    assert!(has(&p, "protection_a (1) must be below protection_b (1)"), "{p:?}");
    // 16-QAM with part B at the most robust level leaves nothing stronger for part A.
    let p = problems(&with_part_a("msc_mode = \"16-QAM\"\nprotection_a = 0\nprotection_b = 0"));
    assert!(has(&p, "raise protection_b"), "{p:?}");
    // Equal error protection: a stale part A level out of 16-QAM's range is ignored.
    let text = base().replace("[output]", "[channel]\nmsc_mode = \"16-QAM\"\nprotection_a = 3\nprotection_b = 1\n[output]");
    let plan = parse(&text).validate().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(plan.tx.part_a_bytes, 0);
    assert_eq!((plan.multiplex.protection_a, plan.multiplex.protection_b), (0, 1));
    assert_eq!(plan.tx.protection.part_a, 0);
}

#[test]
fn settings_that_would_be_ignored_are_rejected() {
    let p = problems(&base().replace("id = 0x123456", "id = 0x123456\nfac_app_id = 3"));
    assert!(has(&p, "fac_app_id is only used by data services"), "{p:?}");
    let text = r#"
    [output]
    file = "x.wav"
    [[service]]
    label = "Data"
    id = 1
    programme_type = "News"
    [service.data]
    type = "epg"
    [[service.data.programme]]
    title = "N"
    start = "2026-09-29T18:00:00Z"
    duration_min = 5
    "#;
    let p = problems(text);
    assert!(has(&p, "programme_type is only used by audio services"), "{p:?}");
}

/// Raw applications need a user application type that DecDRM does not interpret;
/// alternative frequencies are checked against the services and the system's rules.
#[test]
fn raw_applications_and_alternative_frequencies_are_checked() {
    let text = base()
        + r#"
    [[service.app]]
    type = "raw"
    path = "missing.bin"
    [[service.app]]
    type = "raw"
    app_id = 2
    path = "missing.bin"
    [[service.app]]
    type = "tpeg"
    app_id = 0x123
    path = "missing.bin"
    [[afs.other]]
    service = 3
    system = "dab"
    channels = ["12B"]
    [[afs.multiplex]]
    khz = [5990]
    schedule = 4
    "#;
    let p = problems(&text);
    for needle in [
        "application 0 (raw): needs `app_id`",
        "application 1 (raw): app_id 0x002 is an application DecDRM interprets",
        "application 2 (tpeg): app_id is only used with type = \"raw\"",
        "missing.bin",
        "afs.other 0: there is no service 3",
        "afs.other 0: a DAB service needs its `id`",
        "afs.multiplex 0: schedule 4 is not defined",
    ] {
        assert!(has(&p, needle), "{needle}: {p:#?}");
    }
    let p = problems(&(base() + "
    [simulate]
    channel = 7
    snr_db = 100
    sample_rate_offset_ppm = 9000
"));
    for needle in ["channel 7 is not a DRM channel model", "snr_db 100", "sample_rate_offset_ppm 9000"] {
        assert!(has(&p, needle), "{needle}: {p:#?}");
    }
    // Written back as TOML, the [afs] section survives; an empty one is left out.
    let cfg = parse(&text);
    let again = parse(&cfg.to_toml_string().unwrap());
    assert_eq!(again.afs, cfg.afs);
    assert!(!parse(&base()).to_toml_string().unwrap().contains("afs"));
}

#[test]
fn every_problem_is_reported() {
    let text = r#"
    [channel]
    mode = "C"
    occupancy = 2
    protection_b = 3
    [output]
    [time]
    start = "tomorrow"
    [[service]]
    label = "A label that is far too long"
    id = 0x1000000
    iso_language = "english"
    [service.audio]
    codec = "aac"
    core_rate = 48000
    text = ["€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€"]
    input = { tone_hz = 1000.0, file = "missing.wav" }
    [[service]]
    label = "No content"
    id = 5
    "#;
    let p = problems(text);
    for needle in [
        "only defined with occupancy 3 or 5",
        "set `file`, `device` and/or `hackrf`",
        "not an ISO 8601 time",
        "longer than 16 characters",
        "does not fit in 24 bits",
        "ISO 639-2",
        "core_rate 48000",
        "text message 0 has 138 bytes",
        "exactly one of `file`, `device`, `url` and `tone_hz`",
        "missing.wav",
        "needs [service.audio]",
    ] {
        assert!(has(&p, needle), "missing \"{needle}\" in {p:#?}");
    }
    // 16-QAM allows protection levels 0-1 only.
    let p = problems(&base().replace("[output]", "[channel]\nmsc_mode = \"16-QAM\"\nprotection_b = 2\n[output]"));
    assert!(has(&p, "protection_b 2 is out of range 0-1"), "{p:?}");
}

/// A web stream input is checked without connecting: the URL's form, and
/// `stream_titles` only with a URL. Its titles need the text message bytes even without
/// configured messages.
#[test]
fn web_stream_input() {
    let with_input = |input: &str| base().replace("input = { tone_hz = 1000.0 }", input);
    for (input, problem) in [
        ("input = { url = \"ftp://radio.example/live\" }", "not an http:// or https:// URL"),
        ("input = { url = \"http://\" }", "has no host name"),
        ("input = { url = \"https://radio.example:99999/\" }", "bad port"),
        ("input = { tone_hz = 1000.0, stream_titles = false }", "stream_titles is only used with a web stream input"),
        ("input = { url = \"http://radio.example/\", device = \"default\" }", "exactly one of"),
    ] {
        let p = problems(&with_input(input));
        assert!(has(&p, problem), "{input}: {p:?}");
    }
    // Titles on (the default): the audio stream carries text.
    let cfg = parse(&with_input("input = { url = \"http://radio.example:8000/live.mp3\" }"));
    assert!(cfg.services[0].audio.as_ref().unwrap().input.sends_titles());
    let plan = cfg.validate().unwrap_or_else(|e| panic!("{e}"));
    assert!(plan.services[0].audio.as_ref().unwrap().text, "text bytes for the titles");
    assert!(!cfg.inputs_finite(), "a web stream does not end");
    // Titles off and no messages: no text bytes.
    let cfg = parse(&with_input("input = { url = \"http://radio.example/live\", stream_titles = false }"));
    assert!(!cfg.validate().unwrap().services[0].audio.as_ref().unwrap().text);
    // `stream_titles` is written back only when set.
    assert!(!parse(&base()).to_toml_string().unwrap().contains("stream_titles"));
    assert!(cfg.to_toml_string().unwrap().contains("stream_titles = false"));
}

#[test]
fn capacity_problems() {
    // Data asking for more than the MSC holds.
    let p = problems(
        &(base()
            + r#"
    [[service.app]]
    type = "epg"
    bitrate = 50000
    [[service.app.programme]]
    title = "News"
    start = "2026-09-29T18:00:00Z"
    duration_min = 30
    "#),
    );
    assert!(has(&p, "the data applications need"), "{p:?}");
    // HE-AAC stereo does not fit a 4.5 kHz 16-QAM channel.
    let text = base()
        .replace("[output]", "[channel]\noccupancy = 0\nmsc_mode = \"16-QAM\"\nsdc_mode = \"4-QAM\"\n[output]")
        .replace("codec = \"he-aac\"", "codec = \"he-aac\"\nstereo = true");
    let p = problems(&text);
    assert!(has(&p, "HE-AAC with a 12 kHz core in stereo needs at least"), "{p:?}");
    // Mode B / 4.5 kHz / 4-QAM SDC: 13 bytes cannot hold the multiplex description and
    // a 16-byte label.
    let mut text = base()
        .replace("[output]", "[channel]\noccupancy = 0\nmsc_mode = \"16-QAM\"\nsdc_mode = \"4-QAM\"\n[output]")
        .replace("Test Radio", "Sixteen chars ok")
        .replace("codec = \"he-aac\"", "codec = \"aac\"\ncore_rate = 12000");
    text += r#"
    [[service.app]]
    type = "epg"
    bitrate = 100
    packet_length = 10
    [[service.app.programme]]
    title = "News"
    start = "2026-09-29T18:00:00Z"
    duration_min = 30
    "#;
    let p = problems(&text);
    assert!(has(&p, "the SDC holds 13 bytes per super frame"), "{p:?}");
}

#[test]
fn stream_and_service_limits() {
    let mut text = String::from("[output]\nfile = \"x.wav\"\n");
    for i in 0..5 {
        text += &format!(
            "[[service]]\nlabel = \"S{i}\"\nid = {i}\n[service.audio]\ncodec = \"aac\"\ninput = {{ tone_hz = 440.0 }}\n"
        );
    }
    let p = problems(&text);
    assert!(has(&p, "at most 4"), "{p:?}");
    // Three audio services and two data applications: five streams.
    let mut text = String::from("[channel]\nmode = \"A\"\noccupancy = 5\n[output]\nfile = \"x.wav\"\n");
    for i in 0..3 {
        text += &format!(
            "[[service]]\nlabel = \"S{i}\"\nid = {i}\n[service.audio]\ncodec = \"aac\"\ninput = {{ tone_hz = 440.0 }}\n"
        );
    }
    for _ in 0..2 {
        text += "[[service.app]]\ntype = \"epg\"\n[[service.app.programme]]\ntitle = \"N\"\nstart = \"2026-09-29T18:00:00Z\"\nduration_min = 5\n";
    }
    let p = problems(&text);
    assert!(has(&p, "5 streams needed"), "{p:?}");
    // Shared stream with different packet lengths and a duplicate packet id.
    let text = base()
        + r#"
    [[service.app]]
    type = "epg"
    stream = "s"
    packet_id = 1
    [[service.app.programme]]
    title = "N"
    start = "2026-09-29T18:00:00Z"
    duration_min = 5
    [[service.app]]
    type = "epg"
    stream = "s"
    packet_id = 1
    packet_length = 50
    [[service.app.programme]]
    title = "N"
    start = "2026-09-29T18:00:00Z"
    duration_min = 5
    "#;
    let p = problems(&text);
    assert!(has(&p, "45-byte packets, this application asks for 50"), "{p:?}");
    assert!(has(&p, "packet id 1"), "{p:?}");
}

#[test]
fn data_content_is_checked() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pages.toml"), "[[page]]\nid = 0\ntitle = \"Root\"\nmenu = [{ link = 7, text = \"x\" }]\n")
        .unwrap();
    std::fs::create_dir(dir.path().join("empty")).unwrap();
    let text = base()
        + r#"
    [[service.app]]
    type = "journaline"
    path = "pages.toml"
    [[service.app]]
    type = "slideshow"
    path = "empty"
    [[service.app]]
    type = "website"
    path = "nowhere"
    "#;
    let mut cfg = parse(&text);
    cfg.base_dir = Some(dir.path().to_path_buf());
    let p = cfg.validate().unwrap_err().problems().to_vec();
    assert!(has(&p, "links to page 7"), "{p:?}");
    assert!(has(&p, "no JPEG or PNG images"), "{p:?}");
    assert!(has(&p, "does not exist"), "{p:?}");
}

#[test]
fn parse_errors_name_the_problem() {
    let e = StationConfig::from_toml_str("[[service]]\nlabel = \"x\"\nid = 1\n[service.audio]\ncodec = \"mp3\"\ninput = {}\n")
        .unwrap_err();
    assert!(matches!(e, StationError::Parse { .. }));
    assert!(e.to_string().contains("unknown codec \"mp3\""), "{e}");
    let e = StationConfig::from_toml_str("[[service]]\nlabel = \"x\"\nid = 1\nlanguage = \"Klingon\"\n").unwrap_err();
    assert!(e.to_string().contains("English"), "{e}");
    let cfg = parse(&base().replace("codec = \"he-aac\"", "codec = \"he-aac\"\npart = \"A\""));
    assert_eq!(cfg.services[0].audio.as_ref().unwrap().part, Part::A);
}

/// A modulator (`[mdi]`): the input is checked, the channel and services are not used;
/// a TOML round trip keeps the section.
#[test]
fn modulator_section() {
    let text = format!("{}\n[mdi]\ninput = \"239.1.2.3:8000\"\n", base());
    let cfg = parse(&text);
    assert_eq!(cfg.mdi.as_ref().map(|m| (m.input.as_str(), m.buffer_frames)), Some(("239.1.2.3:8000", 3)));
    let plan = cfg.validate().unwrap();
    assert_eq!(plan.mdi.as_deref(), Some("UDP multicast 239.1.2.3:8000"));
    assert!(plan.describe(&cfg).contains("the file's services are not used"), "{}", plan.describe(&cfg));
    assert!(plan.services.is_empty() && plan.streams.is_empty());
    assert!(!cfg.inputs_finite(), "UDP has no end");
    let back = parse(&cfg.to_toml_string().unwrap());
    assert_eq!(back.mdi, cfg.mdi);

    let bad = "[output]\nfile = \"out.wav\"\n[mdi]\ninput = \"missing.rsM\"\nbuffer_frames = 99\n";
    let mut cfg = parse(bad);
    cfg.base_dir = Some(Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf());
    let p = match cfg.validate() {
        Err(e) => e.problems().to_vec(),
        Ok(_) => panic!("expected problems"),
    };
    assert!(p.iter().any(|x| x.contains("missing.rsM") && x.contains("does not exist")), "{p:?}");
    assert!(p.iter().any(|x| x.contains("buffer_frames 99")), "{p:?}");
    assert!(problems("[mdi]\ninput = \"8000\"\n").iter().any(|x| x.contains("output: set")));
    assert!(problems("[output]\nfile = \"o.wav\"\n[mdi]\ninput = \"nonsense\"\n").iter().any(|x| x.starts_with("mdi: input")));
}

/// The HackRF output: needs I/Q format (zero offset, not swapped), a frequency,
/// sample rate and IF within the radio's ranges, and works without a file or
/// sound card.
#[cfg(feature = "hackrf")]
#[test]
fn hackrf_output() {
    use decdrm_station::SignalFormat;

    // Valid on its own: no file, no sound card, just the radio.
    let mut cfg = parse(&base());
    cfg.output.file = None;
    cfg.output.format = SignalFormat::Iq;
    cfg.output.hackrf = Some(Default::default());
    let plan = cfg.validate().unwrap();
    assert!(plan.describe(&cfg).contains("hackrf"), "the plan names the output");

    // The defaults: 1.53 MHz, 2 Msps, IF -250 kHz, LO 1.78 MHz.
    let h = cfg.output.hackrf.clone().unwrap();
    assert_eq!(
        (h.frequency_hz, h.sample_rate_hz, h.tx_gain_db),
        (1_530_000, 2_000_000, 40)
    );
    assert_eq!(h.if_offset_hz, -250_000.0);
    assert_eq!(h.lo_hz(), 1_780_000);

    // Real format, a non-zero I/Q offset and swapped channels are rejected.
    for (what, format, offset, swap) in [
        ("format", SignalFormat::Real, 0.0, false),
        ("iq_offset_hz", SignalFormat::Iq, 1000.0, false),
        ("iq_swap", SignalFormat::Iq, 0.0, true),
    ] {
        let mut cfg = parse(&base());
        cfg.output.file = None;
        cfg.output.format = format;
        cfg.output.iq_offset_hz = offset;
        cfg.output.iq_swap = swap;
        cfg.output.hackrf = Some(Default::default());
        let p = cfg.validate().unwrap_err().problems().to_vec();
        assert!(p.iter().any(|x| x.contains(what)), "{what}: {p:?}");
    }

    // Out-of-range settings are rejected.
    for (what, h) in [
        ("frequency_hz", decdrm_station::HackrfSettings { frequency_hz: 500_000, ..Default::default() }),
        ("sample_rate_hz", decdrm_station::HackrfSettings { sample_rate_hz: 1_000_000, ..Default::default() }),
        ("tx_gain_db", decdrm_station::HackrfSettings { tx_gain_db: 48, ..Default::default() }),
        ("if_offset_hz", decdrm_station::HackrfSettings { if_offset_hz: 10_000.0, ..Default::default() }),
        ("if_offset_hz", decdrm_station::HackrfSettings { if_offset_hz: 1_000_000.0, ..Default::default() }),
        ("buffer_s", decdrm_station::HackrfSettings { buffer_s: 0.0, ..Default::default() }),
        ("buffer_s", decdrm_station::HackrfSettings { buffer_s: 11.0, ..Default::default() }),
        // With a positive offset, the oscillator at 1.1 MHz - 250 kHz = 850 kHz is
        // below the radio's floor.
        (
            "local oscillator",
            decdrm_station::HackrfSettings { frequency_hz: 1_100_000, if_offset_hz: 250_000.0, ..Default::default() },
        ),
        // ... and at 5.9999 GHz - (-250 kHz) = 6 000.15 MHz, above its ceiling.
        (
            "local oscillator",
            decdrm_station::HackrfSettings { frequency_hz: 5_999_900_000, if_offset_hz: -250_000.0, ..Default::default() },
        ),
    ] {
        let mut cfg = parse(&base());
        cfg.output.file = None;
        cfg.output.format = SignalFormat::Iq;
        cfg.output.hackrf = Some(h);
        let p = cfg.validate().unwrap_err().problems().to_vec();
        assert!(p.iter().any(|x| x.contains(what)), "{what}: {p:?}");
    }

    // The ends of the buffer range are fine.
    for buffer_s in [0.05, 1.0, 10.0] {
        let mut cfg = parse(&base());
        cfg.output.file = None;
        cfg.output.format = SignalFormat::Iq;
        cfg.output.hackrf =
            Some(decdrm_station::HackrfSettings { buffer_s, ..Default::default() });
        if let Err(e) = cfg.validate() {
            panic!("buffer_s {buffer_s} should validate: {:?}", e.problems());
        }
    }

    // A negative offset moves the local oscillator above the channel: 1.1 MHz is
    // then usable (the oscillator lands at 1.35 MHz), where +250 kHz was not.
    let mut cfg = parse(&base());
    cfg.output.file = None;
    cfg.output.format = SignalFormat::Iq;
    cfg.output.hackrf = Some(decdrm_station::HackrfSettings {
        frequency_hz: 1_100_000,
        if_offset_hz: -250_000.0,
        ..Default::default()
    });
    let h = cfg.output.hackrf.clone().unwrap();
    assert_eq!(h.lo_hz(), 1_350_000);
    if let Err(e) = cfg.validate() {
        panic!("1.1 MHz with a negative IF should validate: {:?}", e.problems());
    }

    // Unknown keys are denied, as everywhere.
    assert!(StationConfig::from_toml_str("[output]\nformat = \"iq\"\n[output.hackrf]\nfrequency = 1\n").is_err());

    // A rate the station cannot usually feed in real time is flagged in the plan, but
    // stays valid: it is a warning about the machine, not about the configuration.
    let plan_of = |rate: u32| {
        let mut cfg = parse(&base());
        cfg.output.file = None;
        cfg.output.format = SignalFormat::Iq;
        cfg.output.hackrf = Some(decdrm_station::HackrfSettings {
            sample_rate_hz: rate,
            ..Default::default()
        });
        let plan = cfg.validate().expect("valid");
        plan.describe(&cfg)
    };
    assert!(!plan_of(2_000_000).contains("may not keep real time"));
    assert!(!plan_of(4_000_000).contains("may not keep real time"));
    assert!(plan_of(8_000_000).contains("may not keep real time"));
}

/// The channel and the local oscillator must both fit the radio's 1 MHz-6 GHz
/// span, so the usable frequency range follows the IF offset.
#[cfg(feature = "hackrf")]
#[test]
fn hackrf_frequency_span_follows_the_if() {
    use decdrm_station::HackrfSettings;
    // The default: the oscillator 250 kHz above the channel.
    assert_eq!(
        HackrfSettings::frequency_range_for(-250_000.0),
        (1_000_000, 5_999_750_000)
    );
    // A positive offset sits below the channel: the top stays reachable, the bottom moves up.
    assert_eq!(
        HackrfSettings::frequency_range_for(250_000.0),
        (1_250_000, 6_000_000_000)
    );
    assert_eq!(HackrfSettings::frequency_range_for(0.0), (1_000_000, 6_000_000_000));
    assert_eq!(HackrfSettings::default().frequency_range(), (1_000_000, 5_999_750_000));

    // The top of the default span validates; a channel above it reports the oscillator.
    let with_hackrf = |hz: u64| {
        let mut text = base().replace("[output]", "[output]\nformat = \"iq\"");
        text.push_str(&format!("\n[output.hackrf]\nfrequency_hz = {hz}\n"));
        text
    };
    assert!(parse(&with_hackrf(5_999_700_000)).validate().is_ok());
    assert!(has(&problems(&with_hackrf(5_999_800_000)), "local oscillator"));
}

/// The 18 and 20 kHz occupancies are not symmetric about the DC carrier (the signal
/// reaches about 14.6 kHz above it and 4.6 kHz below), so the check that the channel
/// fits the radio's passband must use the real span, not half the nominal bandwidth.
#[cfg(feature = "hackrf")]
#[test]
fn hackrf_if_check_uses_the_real_spectrum() {
    let check = |occupancy: u8, if_offset_hz: f64| {
        let mut cfg = parse(&base());
        cfg.output.file = None;
        cfg.output.format = decdrm_station::SignalFormat::Iq;
        cfg.channel.occupancy = occupancy;
        // A high frequency, so only the passband check can object (the oscillator
        // would otherwise leave the radio's range).
        cfg.output.hackrf = Some(decdrm_station::HackrfSettings {
            frequency_hz: 100_000_000,
            if_offset_hz,
            ..Default::default()
        });
        cfg.validate().map(|_| ()).map_err(|e| e.problems().to_vec())
    };
    // 10 kHz: -4.83..+4.83 kHz around the IF, so +990 kHz still fits into +-1 MHz.
    assert!(check(3, 990_000.0).is_ok());
    // 20 kHz: -4.64..+14.58 kHz, so the top carrier of +990 kHz lands at 1004.6 kHz.
    let p = check(5, 990_000.0).unwrap_err();
    assert!(p.iter().any(|x| x.contains("outside +-1000 kHz")), "{p:?}");
    assert!(check(5, 985_000.0).is_ok());
}

/// The whole channel, not only the nominal frequency, has to stay inside the radio's
/// range: a 10 kHz channel on 1 MHz reaches 4.8 kHz below the floor.
#[cfg(feature = "hackrf")]
#[test]
fn hackrf_channel_stays_inside_the_radios_range() {
    let check = |frequency_hz: u64| {
        let mut cfg = parse(&base());
        cfg.output.file = None;
        cfg.output.format = decdrm_station::SignalFormat::Iq;
        cfg.output.hackrf = Some(decdrm_station::HackrfSettings {
            frequency_hz,
            ..Default::default()
        });
        cfg.validate().map(|_| ()).map_err(|e| e.problems().to_vec())
    };
    let p = check(1_000_000).unwrap_err();
    assert!(
        p.iter().any(|x| x.contains("outside the HackRF's 1 MHz")),
        "the lower carriers are below the floor: {p:?}"
    );
    assert!(check(1_010_000).is_ok(), "1.01 MHz clears it");
}

/// Every step of the GUI's -950..+950 kHz IF preset grid is usable with a 20 kHz
/// channel at 2 Msps and a channel frequency that keeps the oscillator in range.
#[cfg(feature = "hackrf")]
#[test]
fn hackrf_if_preset_grid_validates() {
    for if_khz in (-950..=950).step_by(50) {
        let mut cfg = parse(&base());
        cfg.output.file = None;
        cfg.output.format = decdrm_station::SignalFormat::Iq;
        cfg.channel.occupancy = 5;
        cfg.output.hackrf = Some(decdrm_station::HackrfSettings {
            frequency_hz: 100_000_000,
            if_offset_hz: f64::from(if_khz) * 1_000.0,
            ..Default::default()
        });
        if let Err(e) = cfg.validate() {
            panic!("if_offset_hz {if_khz} kHz should validate: {:?}", e.problems());
        }
    }
}
