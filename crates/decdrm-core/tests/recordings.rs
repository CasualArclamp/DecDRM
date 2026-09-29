//! Regression test on the user's real recordings in `samples/` (skipped when the
//! directory is absent, e.g. on CI). Each file is decoded for up to 30 s and must
//! reach a minimum number of valid FAC blocks with the expected mode/occupancy.

use decdrm_core::params::RobustnessMode;
use decdrm_core::rx::{InputFormat, RealChannel, Receiver, ReceiverConfig, ReceiverEvent};
use std::path::PathBuf;

struct Case {
    file: &'static str,
    iq: bool,
    mode: RobustnessMode,
    so: u8,
    /// Minimum valid FACs in the first 30 s (a clean signal gives ~70).
    min_fac: usize,
}

const CASES: &[Case] = &[
    Case { file: "DW_ModeB_10kHz.flac", iq: false, mode: RobustnessMode::B, so: 3, min_fac: 60 },
    Case { file: "DWwithJournaline_ModeB_10kHz.flac", iq: false, mode: RobustnessMode::B, so: 3, min_fac: 60 },
    Case { file: "BouquetFlevoNL_ModeB_10kHz_14kbps.flac", iq: false, mode: RobustnessMode::B, so: 3, min_fac: 55 },
    Case { file: "RTL_ModeB_10kHz.flac", iq: false, mode: RobustnessMode::B, so: 3, min_fac: 60 },
    Case { file: "RTLwithSlideshow_ModeB_10kHz.flac", iq: false, mode: RobustnessMode::B, so: 3, min_fac: 60 },
    Case { file: "VoiceOfRussia_ModeB_10kHz.flac", iq: false, mode: RobustnessMode::B, so: 3, min_fac: 60 },
    Case { file: "Deutschlandradio_ModeA_10kHz.flac", iq: false, mode: RobustnessMode::A, so: 2, min_fac: 55 },
    Case { file: "DreamWebcamApp_ModeA_10kHz.flac", iq: false, mode: RobustnessMode::A, so: 3, min_fac: 60 },
    Case { file: "vtc_mot.flac", iq: false, mode: RobustnessMode::A, so: 3, min_fac: 55 },
    Case { file: "endless_gapless_test.flac", iq: false, mode: RobustnessMode::A, so: 5, min_fac: 30 },
    Case { file: "ProjectQoSAM_ModeC_10kHz.flac", iq: false, mode: RobustnessMode::C, so: 3, min_fac: 60 },
    Case { file: "R_Nigeria_Mode_C_10kHz_flipped_spectrum.flac", iq: false, mode: RobustnessMode::C, so: 3, min_fac: 60 },
    Case { file: "Opus_Codec_Test_Mode_A_20kHz.flac", iq: false, mode: RobustnessMode::A, so: 5, min_fac: 50 },
    Case { file: "Opus_Codec_Test_Mode_B_10kHz.flac", iq: false, mode: RobustnessMode::B, so: 3, min_fac: 50 },
    Case { file: "Test_Mode_A_10kHz_freq_offset_+60Hz.flac", iq: false, mode: RobustnessMode::A, so: 3, min_fac: 50 },
    Case { file: "Test_Mode_B_10kHz_IQ_Pos_26dB_SNR.flac", iq: true, mode: RobustnessMode::B, so: 3, min_fac: 50 },
    Case {
        file: "Test_Mode_A_20kHz_IQ_Pos_Split_2X_Upscale_26dB_SNR.flac",
        iq: true,
        mode: RobustnessMode::A,
        so: 5,
        min_fac: 45,
    },
    Case { file: "FMGold_xHE_ModeB_9khz.flac", iq: false, mode: RobustnessMode::A, so: 2, min_fac: 50 },
];

fn samples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../samples")
}

/// Read up to `seconds` of a FLAC file, resampled to 48 kHz with the core resampler.
fn load(path: &PathBuf, seconds: f64) -> Option<(Vec<f32>, usize)> {
    let mut r = claxon::FlacReader::open(path).ok()?;
    let info = r.streaminfo();
    let ch = info.channels as usize;
    let rate = info.sample_rate;
    let scale = 1.0 / (1u64 << (info.bits_per_sample - 1)) as f32;
    let max = (seconds * f64::from(rate)) as usize * ch;
    let raw: Vec<f32> = r.samples().take(max).map(|s| s.unwrap_or(0) as f32 * scale).collect();
    if rate == 48_000 {
        return Some((raw, ch));
    }
    let frames = raw.len() / ch;
    let mut chans = Vec::new();
    for c in 0..ch {
        let x: Vec<decdrm_core::Cplx> =
            (0..frames).map(|i| decdrm_core::Cplx::new(f64::from(raw[i * ch + c]), 0.0)).collect();
        let mut rs = decdrm_core::dsp::resampler::FracResampler::new();
        let mut y = Vec::new();
        rs.process(&x, 48_000.0 / f64::from(rate), &mut y);
        chans.push(y);
    }
    let n = chans.iter().map(Vec::len).min().unwrap_or(0);
    let mut out = Vec::with_capacity(n * ch);
    for i in 0..n {
        for c in &chans {
            out.push(c[i].re as f32);
        }
    }
    Some((out, ch))
}

#[test]
fn recordings_decode_fac() {
    let dir = samples_dir();
    if !dir.is_dir() {
        eprintln!("samples/ not present, skipping");
        return;
    }
    let mut failures = Vec::new();
    for case in CASES {
        let path = dir.join(case.file);
        let Some((samples, ch)) = load(&path, 30.0) else {
            eprintln!("{} missing, skipped", case.file);
            continue;
        };
        let cfg = ReceiverConfig {
            input: if case.iq { InputFormat::Iq { swap: false } } else { InputFormat::Real(RealChannel::Mix) },
            channels: ch,
            ..Default::default()
        };
        let mut rx = Receiver::new(cfg);
        let mut fac_ok = 0usize;
        let mut last = None;
        for chunk in samples.chunks(4800 * ch) {
            for ev in rx.push(chunk) {
                if let ReceiverEvent::Fac(f) = ev {
                    fac_ok += 1;
                    last = Some(f);
                }
            }
        }
        let st = rx.status();
        let ok_layout = st.mode == Some(case.mode) && last.is_some_and(|f| f.channel.occupancy.value() == case.so);
        eprintln!(
            "{:55} FAC {:3} (min {:3}) mode {:?} SO {:?} SNR {:?}",
            case.file,
            fac_ok,
            case.min_fac,
            st.mode,
            last.map(|f| f.channel.occupancy.value()),
            st.snr_db.map(|v| (v * 10.0).round() / 10.0)
        );
        if fac_ok < case.min_fac || !ok_layout {
            failures.push(case.file);
        }
    }
    assert!(failures.is_empty(), "recordings failing: {failures:?}");
}
