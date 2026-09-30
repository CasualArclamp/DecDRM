//! MSC bit error rate after channel decoding versus SNR, through the DRM channel
//! models, for comparison with ES 201 980 annex A (table A.1: 64-QAM, R = 0.6,
//! required C/N for BER 1e-4 with *ideal* channel estimation and synchronisation:
//! channel 1 14.9 dB, 2 16.5, 3 23.2, 4 22.3, 5 20.4 dB; modes A for channels 1–2,
//! B for 3–5). The SNR is defined the same way (signal power incl. pilots and guard,
//! noise in the nominal bandwidth).
//!
//! `cargo run --release -p decdrm-core --example bercurve -- CHANNEL MODE SNR... [--secs S]
//!  [--so N] [--qam 16|64] [--prot P] [--iter I] [--seed N] [--short]
//!  [--dream | --euclid | --huber C | --huber-amplitude C]`
//!
//! The MSC soft metric is the receiver's default (`rx::MSC_METRIC`) unless one is chosen:
//! `--dream` (Dream's |r/h − s|·|h|), `--euclid` (|r/h − s|²·|h|²), `--huber C` or
//! `--huber-amplitude C` (see `fec::qam::MetricKind`).
//!
//! e.g. `bercurve 1 A 14 15 16 17` or `bercurve 3 B 22 24 26 --secs 60`.

use decdrm_core::channel::{ChannelConfig, ChannelSimulator, Rng};
use decdrm_core::fac::{ChannelParams, Fac, Interleaving, MscMode, SdcMode, ServiceParams};
use decdrm_core::fec::mlc::MscProtection;
use decdrm_core::fec::qam::MetricKind;
use decdrm_core::params::{RobustnessMode, SpectrumOccupancy};
use decdrm_core::rx::{InputFormat, MscConfig, Receiver, ReceiverConfig, ReceiverEvent};
use decdrm_core::tx::output::{OutputConfig, OutputStage};
use decdrm_core::tx::{Transmitter, TxConfig};
use std::collections::VecDeque;

/// Decoded frames are only counted after this long (acquisition, interleaver fill).
const WARMUP_S: f64 = 5.0;

struct Args {
    channel: u8,
    mode: RobustnessMode,
    snrs: Vec<f64>,
    secs: f64,
    so: u8,
    qam: u8,
    prot: usize,
    iter: usize,
    seed: u64,
    short: bool,
    euclid: bool,
    huber: Option<f64>,
    huber_amplitude: Option<f64>,
    dream: bool,
}

fn parse() -> Args {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let mut it = a.iter().peekable();
    let channel: u8 = it.next().and_then(|s| s.parse().ok()).expect("channel 1..6");
    let mode = match it.next().map(String::as_str) {
        Some("A") => RobustnessMode::A,
        Some("B") => RobustnessMode::B,
        Some("C") => RobustnessMode::C,
        Some("D") => RobustnessMode::D,
        other => panic!("mode A..D, got {other:?}"),
    };
    let mut args = Args { channel, mode, snrs: Vec::new(), secs: 30.0, so: 3, qam: 64, prot: 1, iter: 2, seed: 1, short: false, euclid: false, huber: None, huber_amplitude: None, dream: false };
    while let Some(s) = it.next() {
        let mut val = || it.next().expect("value").clone();
        match s.as_str() {
            "--secs" => args.secs = val().parse().unwrap(),
            "--so" => args.so = val().parse().unwrap(),
            "--qam" => args.qam = val().parse().unwrap(),
            "--prot" => args.prot = val().parse().unwrap(),
            "--iter" => args.iter = val().parse().unwrap(),
            "--seed" => args.seed = val().parse().unwrap(),
            "--short" => args.short = true,
            "--euclid" => args.euclid = true,
            "--dream" => args.dream = true,
            "--huber" => args.huber = Some(val().parse().unwrap()),
            "--huber-amplitude" => args.huber_amplitude = Some(val().parse().unwrap()),
            v => args.snrs.push(v.parse().expect("SNR in dB")),
        }
    }
    args
}

fn fac(so: SpectrumOccupancy) -> Fac {
    Fac {
        channel: ChannelParams {
            enhancement: false,
            frame_index: 0,
            afs_valid: false,
            occupancy: so,
            interleaving: Interleaving::Long,
            msc_mode: MscMode::Qam64Sm,
            sdc_mode: SdcMode::Qam16,
            num_audio: 1,
            num_data: 0,
            reconfiguration_index: 0,
            toggle: false,
        },
        service: ServiceParams { service_id: 1, short_id: 0, audio_ca: false, language: 0, is_data: false, descriptor: 0, data_ca: false },
    }
}

fn main() {
    let a = parse();
    let so = SpectrumOccupancy::new(a.so).expect("occupancy 0..5");
    let tx_cfg = TxConfig {
        mode: a.mode,
        occupancy: so,
        msc_mode: if a.qam == 16 { MscMode::Qam16Sm } else { MscMode::Qam64Sm },
        interleaving: if a.short { Interleaving::Short } else { Interleaving::Long },
        protection: MscProtection { part_a: 0, part_b: a.prot, hierarchical: 0 },
        ..TxConfig::default()
    };
    println!(
        "channel {} ({}), mode {} SO{} {}-QAM prot {} {} interleaving, {} MSC iteration(s), {} s per point",
        a.channel,
        decdrm_core::channel::ChannelModel::drm_name(a.channel),
        a.mode,
        a.so,
        a.qam,
        a.prot,
        if a.short { "short" } else { "long" },
        a.iter,
        a.secs
    );
    println!(
        "{:>6} {:>10} {:>9} {:>7} {:>8} {:>8} {:>7} {:>7} {:>7}",
        "SNR", "BER", "bits", "FER", "frames", "rx SNR", "MER", "FAC ok", "SDC ok"
    );
    for &snr in &a.snrs {
        let mut tx = Transmitter::new(tx_cfg).expect("transmitter");
        let layout = tx.layout();
        let cc = ChannelConfig { seed: 100 + a.seed, ..ChannelConfig::drm(a.channel, snr).expect("channel 1..6") };
        let mut chan = ChannelSimulator::new(layout, cc);
        let mut out = OutputStage::new(layout, OutputConfig::iq(0.0)).expect("output");
        let mut rx = Receiver::new(ReceiverConfig {
            input: InputFormat::Iq { swap: false },
            channels: 2,
            msc_iterations: a.iter,
            metric: match (a.huber, a.huber_amplitude, a.euclid, a.dream) {
                (Some(c), ..) => MetricKind::Huber(c),
                (None, Some(c), ..) => MetricKind::HuberAmplitude(c),
                (None, None, true, _) => MetricKind::Euclidean,
                (None, None, false, true) => MetricKind::DreamLinear,
                (None, None, false, false) => decdrm_core::rx::MSC_METRIC,
            },
            ..Default::default()
        });
        rx.set_msc_config(Some(MscConfig {
            mode: tx_cfg.msc_mode,
            protection: tx_cfg.protection,
            part_a_bytes: 0,
            interleaving: tx_cfg.interleaving,
        }));
        let mut rng = Rng::new(a.seed);
        let cap = tx.msc_capacity();
        let mut sent: VecDeque<Vec<u8>> = VecDeque::new();
        let (mut bits, mut errors, mut frames, mut frame_errors) = (0u64, 0u64, 0u64, 0u64);
        let n_frames = (a.secs / 0.4).ceil() as usize;
        let fac = fac(so);
        let (mut bb, mut ch_out, mut pcm) = (Vec::new(), Vec::new(), Vec::new());
        for f in 0..n_frames {
            let msc = rng.bits(cap.total_bits());
            let sdc = (tx.frame_index() == 0).then(|| rng.bytes(tx.sdc_capacity_bytes()));
            bb.clear();
            tx.transmit_frame_into(&fac, &msc, sdc.as_deref(), &mut bb).expect("transmit");
            sent.push_back(msc);
            if sent.len() > 16 {
                sent.pop_front();
            }
            ch_out.clear();
            chan.process(&bb, &mut ch_out);
            pcm.clear();
            out.process(&ch_out, &mut pcm);
            for ev in rx.push(&pcm) {
                let ReceiverEvent::Msc(m) = ev else { continue };
                if !m.complete || (f as f64) * 0.4 < WARMUP_S {
                    continue;
                }
                let mut got = m.vspp.clone();
                got.extend_from_slice(&m.bits);
                // The matching transmitted frame is the one at the smallest distance
                // among the recent ones (the pipeline delay is a few frames).
                let e = sent
                    .iter()
                    .filter(|s| s.len() == got.len())
                    .map(|s| s.iter().zip(&got).filter(|(x, y)| x != y).count() as u64)
                    .min()
                    .unwrap_or(got.len() as u64);
                bits += got.len() as u64;
                errors += e;
                frames += 1;
                frame_errors += u64::from(e > 0);
            }
        }
        let st = rx.status();
        let ber = if bits > 0 { errors as f64 / bits as f64 } else { f64::NAN };
        let share = |ok: u64, bad: u64| if ok + bad > 0 { format!("{:.0}%", 100.0 * ok as f64 / (ok + bad) as f64) } else { "-".into() };
        println!(
            "{snr:>6.1} {ber:>10.2e} {bits:>9} {:>7.3} {frames:>8} {:>8} {:>7} {:>7} {:>7}",
            if frames > 0 { frame_errors as f64 / frames as f64 } else { f64::NAN },
            st.snr_db.map_or("-".into(), |v| format!("{v:.1}")),
            st.mer_db.map_or("-".into(), |v| format!("{v:.1}")),
            share(st.fac_ok, st.fac_bad),
            share(st.sdc_ok, st.sdc_bad),
        );
    }
}
