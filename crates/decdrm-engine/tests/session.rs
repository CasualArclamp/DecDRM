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
}

/// Decode up to `seconds` of a recording; `None` if the file is absent.
fn run(file: &str, seconds: f64) -> Option<Outcome> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../samples").join(file);
    if !path.exists() {
        eprintln!("skipping: {} not found", path.display());
        return None;
    }
    let mut source = Source::open(&InputSpec::File { path, realtime: false }).expect("open recording");
    let ch = source.info().channels;
    let cfg = ReceiverConfig { input: InputFormat::Real(RealChannel::Mix), channels: ch, ..Default::default() };
    let mut session = Session::new(cfg);
    let mut o = Outcome::default();
    while source.position_s() < seconds {
        let Some(frames) = source.read(4800).expect("read") else { break };
        for ev in session.push(&frames) {
            match ev {
                SessionEvent::Text(Some(t)) => o.texts.push(t),
                SessionEvent::Data { event: DataEvent::SlideShowImage { name, .. }, .. } => o.slides.push(name),
                SessionEvent::Data { event: DataEvent::WebsiteFile { .. }, .. } => o.website_files += 1,
                SessionEvent::Data { event: DataEvent::Journaline(_), .. } => o.journaline_objects += 1,
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
    Some(o)
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
