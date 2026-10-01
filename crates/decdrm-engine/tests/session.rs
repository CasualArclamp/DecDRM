//! End-to-end regression test on the user's recordings in `samples/` (skipped when a
//! file is absent): receiver → multiplex → audio / text / data pipelines, as the CLI
//! and GUI run them.

use decdrm_core::rx::{InputFormat, RealChannel, ReceiverConfig};
use decdrm_data::DataEvent;
use decdrm_engine::{InputSpec, Session, SessionEvent, Source};
use std::path::PathBuf;

/// What a run produced.
#[derive(Default)]
struct Outcome {
    labels: Vec<String>,
    texts: Vec<String>,
    slides: Vec<String>,
    website_files: usize,
    journaline_objects: usize,
    audio_ok: u64,
    audio_concealed: u64,
    codec: String,
    msc_ok: u64,
    msc_bad: u64,
    fac_ok: u64,
    fac_bad: u64,
    sdc_ok: u64,
    sdc_bad: u64,
    /// Data units of applications DecDRM does not interpret.
    raw_units: usize,
    /// Timing jumps the receiver resynchronised after.
    resyncs: usize,
}

/// Decode up to `seconds` of a recording (a real signal); `None` if the file is absent.
fn run(file: &str, seconds: f64) -> Option<Outcome> {
    run_input(file, seconds, InputFormat::Real(RealChannel::Mix))
}

/// The same for an input format.
fn run_input(file: &str, seconds: f64, input: InputFormat) -> Option<Outcome> {
    run_selecting(file, seconds, input, None)
}

/// The same with a service selected from the start (as `--service N` does).
fn run_selecting(file: &str, seconds: f64, input: InputFormat, select: Option<u8>) -> Option<Outcome> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../samples").join(file);
    if !path.exists() {
        eprintln!("skipping: {} not found", path.display());
        return None;
    }
    let mut source = Source::open(&InputSpec::File { path, realtime: false }).expect("open recording");
    let ch = source.info().channels;
    let cfg = ReceiverConfig { input, channels: ch, ..Default::default() };
    let mut session = Session::new(cfg);
    if let Some(id) = select {
        session.select_service(id);
    }
    let mut o = Outcome::default();
    while source.position_s() < seconds {
        let Some(frames) = source.read(4800).expect("read") else { break };
        for ev in session.push(&frames) {
            match ev {
                SessionEvent::Text(Some(t)) => o.texts.push(t),
                SessionEvent::Data { event: DataEvent::SlideShowImage { name, .. }, .. } => o.slides.push(name),
                SessionEvent::Data { event: DataEvent::WebsiteFile { .. }, .. } => o.website_files += 1,
                SessionEvent::Data { event: DataEvent::Journaline(_), .. } => o.journaline_objects += 1,
                SessionEvent::Data { event: DataEvent::Raw { .. }, .. } => o.raw_units += 1,
                SessionEvent::Log(l) if l.contains("timing jump") => o.resyncs += 1,
                _ => {}
            }
        }
    }
    o.labels = session.ensemble().services().filter_map(|s| s.label.clone()).collect();
    o.audio_ok = session.audio_stats.frames_ok;
    o.audio_concealed = session.audio_stats.frames_concealed;
    o.codec = session.audio_stats.codec.clone();
    o.msc_ok = session.msc_stats.ok;
    o.msc_bad = session.msc_stats.bad;
    let st = session.status();
    (o.fac_ok, o.fac_bad, o.sdc_ok, o.sdc_bad) = (st.fac_ok, st.fac_bad, st.sdc_ok, st.sdc_bad);
    Some(o)
}

/// Korean Central Broadcasting's second programme on 6140 kHz, received with KiwiSDRs
/// in I/Q mode (12 kHz). It is signalled as a data service (user application 0x000,
/// packet mode) whose MSC carries a format no standard receiver knows; DecDRM captures
/// the data units.
const KCBS_LABEL: &str = "조선중앙제2라지오방송";
const IQ: InputFormat = InputFormat::Iq { swap: false };

/// From Japan on a good night (SNR ~24 dB): everything decodes. Its data application
/// carries EVS audio (decdrm_evs::kcbs) in a nonstandard, likely encrypted form, which
/// DecDRM recognises but does not decode: its data groups are captured like any other
/// data, the service selected or not.
#[test]
fn kcbs_data_service_good_night() {
    const FILE: &str = "SND.jj8ntm.proxy.kiwisdr.com_2026-09-30T12_58_36Z_6140.00_iq.wav";
    let Some(o) = run_input(FILE, 60.0, IQ) else { return };
    assert_eq!(o.labels, [KCBS_LABEL]);
    assert_eq!((o.fac_bad, o.sdc_bad, o.msc_bad), (0, 0, 0), "FAC/SDC/MSC errors");
    assert!(o.msc_ok >= 78, "{} MSC frames", o.msc_ok);
    assert!(o.audio_ok == 0 && o.codec.is_empty(), "not played by itself: {} frames, {:?}", o.audio_ok, o.codec);
    assert!(o.raw_units >= 78, "{} data units captured", o.raw_units);

    let Some(o) = run_selecting(FILE, 60.0, IQ, Some(0)) else { return };
    assert!(o.audio_ok == 0 && o.codec.is_empty(), "not decoded when selected: {} frames, {:?}", o.audio_ok, o.codec);
    assert!(o.raw_units >= 78, "{} data units", o.raw_units);
}

/// From Japan, with a KiwiSDR stream glitch at 28 s: one resynchronisation, and the MSC
/// keeps decoding around it.
#[test]
fn kcbs_timing_jump_in_a_web_sdr_stream() {
    let Some(o) = run_selecting("SND.jj8ntm.proxy.kiwisdr.com_2026-09-30T12_52_02Z_6140.00_iq.wav", 60.0, IQ, Some(0)) else { return };
    assert_eq!(o.labels, [KCBS_LABEL]);
    assert_eq!(o.resyncs, 1, "timing jumps");
    assert!(o.msc_ok >= 77 && o.msc_bad <= 2, "MSC {} ok, {} bad", o.msc_ok, o.msc_bad);
}

/// From Australia over a very bad path (SNR ~9 dB, delay spread beyond mode B's guard
/// interval, ~2 Hz Doppler): the 16-QAM MSC is beyond reach (as with the simulated DRM
/// channels 5/6 at that SNR), but the FAC and part of the SDC decode.
#[test]
fn kcbs_very_bad_path() {
    let Some(o) = run_input("SND.kiwisdr.areg.org.au_2026-09-30T12_46_51Z_6140.00_iq.wav", 60.0, IQ) else { return };
    assert_eq!(o.labels, [KCBS_LABEL]);
    assert!(o.fac_ok >= 95 && o.fac_bad <= 5, "FAC {} ok, {} bad", o.fac_ok, o.fac_bad);
    assert!(o.sdc_ok >= 10, "SDC {} ok, {} bad", o.sdc_ok, o.sdc_bad);
}

#[test]
fn dw_aac_audio_and_text() {
    let Some(o) = run("DW_ModeB_10kHz.flac", 44.0) else { return };
    assert!(o.labels.iter().any(|l| l == "DW DRM"), "labels {:?}", o.labels);
    assert!(o.texts.iter().any(|t| t.contains("Deutsche Welle")), "texts {:?}", o.texts);
    assert!(o.codec.contains("HE-AAC"), "codec {}", o.codec);
    assert!(o.audio_ok >= 1000 && o.audio_concealed == 0, "audio ok {} concealed {}", o.audio_ok, o.audio_concealed);
    assert!(o.msc_ok >= 100 && o.msc_bad == 0, "MSC ok {} bad {}", o.msc_ok, o.msc_bad);
}

#[test]
fn rtl_slideshow_each_slide_once() {
    let Some(o) = run("RTLwithSlideshow_ModeB_10kHz.flac", 170.0) else { return };
    assert!(o.labels.iter().any(|l| l == "RTL Data"), "labels {:?}", o.labels);
    assert_eq!(o.slides, ["bce.jpg", "transmitter.jpg"], "slides");
    assert!(o.audio_ok >= 4000 && o.audio_concealed == 0, "audio ok {} concealed {}", o.audio_ok, o.audio_concealed);
}

#[test]
fn journaline_objects() {
    let Some(o) = run("DWwithJournaline_ModeB_10kHz.flac", 60.0) else { return };
    assert!(o.journaline_objects >= 5, "{} Journaline objects", o.journaline_objects);
}

#[test]
fn opus_audio() {
    let Some(o) = run("Opus_Codec_Test_Mode_B_10kHz.flac", 60.0) else { return };
    assert!(o.codec.contains("Opus"), "codec {}", o.codec);
    assert!(o.audio_ok >= 2500, "audio ok {} concealed {}", o.audio_ok, o.audio_concealed);
}

#[test]
fn xhe_aac_audio() {
    let Some(o) = run("FMGold_xHE_ModeB_9khz.flac", 60.0) else { return };
    assert!(o.codec.contains("xHE-AAC"), "codec {}", o.codec);
    assert!(o.audio_ok >= 400 && o.audio_concealed == 0, "audio ok {} concealed {}", o.audio_ok, o.audio_concealed);
}

#[test]
fn broadcast_website_files() {
    let Some(o) = run("vtc_mot.flac", 127.0) else { return };
    assert!(o.labels.iter().any(|l| l == "VTC Sea Trial"), "labels {:?}", o.labels);
    assert!(o.website_files >= 10, "{} website files", o.website_files);
}
