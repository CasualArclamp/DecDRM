//! Diagnostic: receiver recovery after the input loses or gains samples (e.g. a web
//! SDR stream with network hiccups). Transmits `SECONDS` of signal through AWGN, and at
//! `AT` seconds drops `N` samples (N > 0) or inserts |N| zeros (N < 0); prints when the
//! FAC stream stops and resumes, restarts, and the audio (MSC) frames lost.
//!
//! `cargo run --release -p decdrm-core --example dropout -- N [AT] [SECONDS] [MODE] [SNR]`

use decdrm_core::channel::{ChannelConfig, ChannelSimulator, Rng};
use decdrm_core::fac::{ChannelParams, Fac, Interleaving, MscMode, SdcMode, ServiceParams};
use decdrm_core::params::{RobustnessMode, SpectrumOccupancy};
use decdrm_core::rx::{InputFormat, MscConfig, RealChannel, Receiver, ReceiverConfig, ReceiverEvent};
use decdrm_core::tx::output::{OutputConfig, OutputStage, suggested_if_hz};
use decdrm_core::tx::{Transmitter, TxConfig};

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let n: i64 = a.first().and_then(|s| s.parse().ok()).expect("N (samples to drop; negative = insert zeros)");
    let at: f64 = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(10.0);
    let secs: f64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(30.0);
    let mode = match a.get(3).map(String::as_str) {
        Some("A") => RobustnessMode::A,
        Some("C") => RobustnessMode::C,
        Some("D") => RobustnessMode::D,
        _ => RobustnessMode::B,
    };
    let snr: f64 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(25.0);
    let txc = TxConfig { mode, occupancy: SpectrumOccupancy::SO_3, ..Default::default() };
    let mut tx = Transmitter::new(txc).unwrap();
    let layout = tx.layout();
    let mut chan = ChannelSimulator::new(layout, ChannelConfig::awgn(snr));
    let mut out = OutputStage::new(layout, OutputConfig::real(suggested_if_hz(layout))).unwrap();
    let mut rx = Receiver::new(ReceiverConfig { input: InputFormat::Real(RealChannel::Mix), channels: 1, ..Default::default() });
    rx.set_msc_config(Some(MscConfig {
        mode: txc.msc_mode,
        protection: txc.protection,
        part_a_bytes: 0,
        interleaving: txc.interleaving,
    }));
    let mut rng = Rng::new(3);
    let cap = tx.msc_capacity();
    let fac = Fac {
        channel: ChannelParams {
            enhancement: false,
            frame_index: 0,
            afs_valid: false,
            occupancy: SpectrumOccupancy::SO_3,
            interleaving: Interleaving::Long,
            msc_mode: MscMode::Qam64Sm,
            sdc_mode: SdcMode::Qam16,
            num_audio: 1,
            num_data: 0,
            reconfiguration_index: 0,
            toggle: false,
        },
        service: ServiceParams { service_id: 1, short_id: 0, audio_ca: false, language: 0, is_data: false, descriptor: 0, data_ca: false },
    };
    let (mut samples_in, mut done_glitch) = (0usize, false);
    let mut last_fac_before = None;
    let mut first_fac_after = None;
    let (mut restarts, mut fac_bad, mut msc_ok_after, mut msc_wrong_after) = (0, 0, 0usize, 0usize);
    let mut first_ok_msc_after = None;
    let mut sent: std::collections::VecDeque<Vec<u8>> = std::collections::VecDeque::new();
    let frames = (secs / 0.4) as usize;
    for _ in 0..frames {
        let msc = rng.bits(cap.total_bits());
        let sdc = (tx.frame_index() == 0).then(|| rng.bytes(tx.sdc_capacity_bytes()));
        let bb = tx.transmit_frame(&fac, &msc, sdc.as_deref()).unwrap();
        sent.push_back(msc);
        if sent.len() > 16 {
            sent.pop_front();
        }
        let mut c = Vec::new();
        chan.process(&bb, &mut c);
        let mut pcm = Vec::new();
        out.process(&c, &mut pcm);
        let glitch_at = (at * 48_000.0) as usize;
        if !done_glitch && samples_in + pcm.len() > glitch_at {
            let i = glitch_at - samples_in;
            if n > 0 {
                let end = (i + n as usize).min(pcm.len());
                pcm.drain(i..end);
            } else {
                pcm.splice(i..i, std::iter::repeat_n(0.0f32, (-n) as usize));
            }
            done_glitch = true;
        }
        samples_in += pcm.len();
        let t = samples_in as f64 / 48_000.0;
        for ev in rx.push(&pcm) {
            match ev {
                ReceiverEvent::Fac(_) if t < at => last_fac_before = Some(t),
                ReceiverEvent::Fac(_) => {
                    first_fac_after.get_or_insert(t);
                }
                ReceiverEvent::FacError if t >= at => fac_bad += 1,
                ReceiverEvent::Restarted => restarts += 1,
                ReceiverEvent::Resynchronising => {
                    restarts += 1;
                    eprintln!("resync at {t:.1} s");
                }
                ReceiverEvent::Msc(m) if t >= at && m.complete => {
                    let mut got = m.vspp.clone();
                    got.extend_from_slice(&m.bits);
                    if sent.iter().any(|s| *s == got) {
                        msc_ok_after += 1;
                        if msc_wrong_after > 0 || restarts > 0 {
                            first_ok_msc_after.get_or_insert(t);
                        }
                    } else {
                        msc_wrong_after += 1;
                    }
                }
                _ => {}
            }
        }
    }
    println!(
        "{mode} {snr} dB, {} {} samples at {at} s: last FAC before {:?}, first FAC after {:?} (+{:.1} s), \
         bad FACs {fac_bad}, restarts {restarts}, correct MSC frames again at {:?}, after the glitch {msc_ok_after} correct / {msc_wrong_after} wrong",
        if n > 0 { "dropped" } else { "inserted" },
        n.abs(),
        last_fac_before.map(|v| (v * 10.0).round() / 10.0),
        first_fac_after.map(|v| (v * 10.0).round() / 10.0),
        first_fac_after.map_or(f64::NAN, |v| v - at),
        first_ok_msc_after.map(|v| (v * 10.0).round() / 10.0),
    );
}
