//! Diversity reception end to end: one transmitter, two independently fading channels
//! (the same DRM channel model and SNR, different seeds), the second branch delayed and
//! on a sample clock of its own, into a `DiversityReceiver` and, for comparison, into a
//! receiver per branch. Counts the multiplex frames recovered bit-exactly.
//!
//! `diversity_sweep` (ignored, long) prints a table over channels and SNRs:
//! `cargo test --release -p decdrm-core --test diversity -- --ignored --nocapture`.

use decdrm_core::channel::{ChannelConfig, ChannelModel, ChannelSimulator, Rng};
use decdrm_core::fac::{ChannelParams, Fac, Interleaving, MscMode, SdcMode, ServiceParams};
use decdrm_core::params::{SAMPLE_RATE, SAMPLES_PER_FRAME, SpectrumOccupancy};
use decdrm_core::rx::{DiversityReceiver, DiversityStats, InputFormat, MscConfig, Receiver, ReceiverConfig, ReceiverEvent};
use decdrm_core::tx::output::{OutputConfig, OutputStage};
use decdrm_core::tx::{Transmitter, TxConfig};
use decdrm_core::{Cplx, Real};

/// Seconds per transmission frame.
const FRAME_S: Real = SAMPLES_PER_FRAME as Real / SAMPLE_RATE as Real;

/// A FAC with a random service ID, so every frame's FAC is unique.
fn test_fac(rng: &mut Rng) -> Fac {
    Fac {
        channel: ChannelParams {
            enhancement: false,
            frame_index: 0,
            afs_valid: true,
            // The transmitter overwrites occupancy, interleaving and the modes.
            occupancy: SpectrumOccupancy::SO_3,
            interleaving: Interleaving::Long,
            msc_mode: MscMode::Qam16Sm,
            sdc_mode: SdcMode::Qam16,
            num_audio: 1,
            num_data: 0,
            reconfiguration_index: 0,
            toggle: rng.bit() == 1,
        },
        service: ServiceParams {
            service_id: (rng.next_u64() & 0xFF_FFFF) as u32,
            short_id: 0,
            audio_ca: false,
            language: rng.below(16) as u8,
            is_data: false,
            descriptor: rng.below(32) as u8,
            data_ca: false,
        },
    }
}

fn iq_config() -> ReceiverConfig {
    ReceiverConfig { input: InputFormat::Iq { swap: false }, channels: 2, ..ReceiverConfig::default() }
}

/// Multiplex frames recovered bit-exactly (complete frames only).
fn correct(events: &[ReceiverEvent], sent: &[Vec<u8>]) -> usize {
    events
        .iter()
        .filter(|e| match e {
            ReceiverEvent::Msc(m) if m.complete => {
                let mut all = m.vspp.clone();
                all.extend_from_slice(&m.bits);
                sent.contains(&all)
            }
            _ => false,
        })
        .count()
}

/// What one run gave: correct frames combined, from branch A alone, from branch B
/// alone, frames sent, and the combiner's counts.
struct Outcome {
    diversity: usize,
    alone: [usize; 2],
    frames: usize,
    stats: DiversityStats,
}

/// `seconds` of 16-QAM (protection 1, long interleaving) through DRM channel `model`
/// at `snr_db` per branch; branch B starts `delay_s` later.
fn run(model: u8, snr_db: Real, seconds: Real, delay_s: Real, seeds: (u64, u64)) -> Outcome {
    run_with(model, snr_db, seconds, delay_s, seeds, None)
}

/// [`run`] with the transmitter's FAC identity fixed
/// (`Transmitter::set_fixed_fac_identity`).
fn run_with(model: u8, snr_db: Real, seconds: Real, delay_s: Real, seeds: (u64, u64), identity: Option<u8>) -> Outcome {
    let tx_cfg = TxConfig { msc_mode: MscMode::Qam16Sm, ..TxConfig::default() };
    let mut tx = Transmitter::new(tx_cfg).expect("transmitter");
    tx.set_fixed_fac_identity(identity);
    let layout = tx.layout();
    // Two receivers far apart: independent fading and noise, clocks 20 ppm slow and
    // 35 ppm fast.
    let chan = |seed: u64, ppm: Real| ChannelConfig { seed, sample_rate_offset_ppm: ppm, ..ChannelConfig::drm(model, snr_db).expect("model") };
    let mut chans = [ChannelSimulator::new(layout, chan(seeds.0, -20.0)), ChannelSimulator::new(layout, chan(seeds.1, 35.0))];
    let mut outs = [OutputStage::new(layout, OutputConfig::iq(0.0)).unwrap(), OutputStage::new(layout, OutputConfig::iq(0.0)).unwrap()];
    let msc = MscConfig { mode: tx_cfg.msc_mode, protection: tx_cfg.protection, part_a_bytes: 0, interleaving: tx_cfg.interleaving };
    let mut div = DiversityReceiver::new([iq_config(), iq_config()]);
    div.set_msc_config(Some(msc));
    let mut single = [Receiver::new(iq_config()), Receiver::new(iq_config())];
    for r in &mut single {
        r.set_msc_config(Some(msc));
    }

    let mut rng = Rng::new(5);
    let frames = (seconds / FRAME_S).ceil() as usize;
    let cap = tx.msc_capacity();
    let mut sent = Vec::new();
    let (mut ev_div, mut ev_single) = (Vec::new(), [Vec::new(), Vec::new()]);
    let (mut faded, mut pcm) = (Vec::<Cplx>::new(), [Vec::<f32>::new(), Vec::new()]);
    // Branch B arrives `delay_s` later (a slower network path): its samples are held
    // back, behind silence, and handed over at branch A's pace.
    pcm[1].resize(2 * (delay_s * Real::from(SAMPLE_RATE)) as usize, 0.0);
    for _ in 0..frames {
        let fac = test_fac(&mut rng);
        let bits = rng.bits(cap.total_bits());
        let sdc = (tx.frame_index() == 0).then(|| rng.bytes(tx.sdc_capacity_bytes()));
        let base = tx.transmit_frame(&fac, &bits, sdc.as_deref()).expect("transmit");
        sent.push(bits);
        for b in 0..2 {
            faded.clear();
            chans[b].process(&base, &mut faded);
            outs[b].process(&faded, &mut pcm[b]);
        }
        let n = pcm[0].len().min(pcm[1].len());
        let chunks = [pcm[0].split_off(0), pcm[1].drain(..n).collect::<Vec<f32>>()];
        for (b, chunk) in chunks.iter().enumerate() {
            ev_single[b].extend(single[b].push(chunk));
            ev_div.extend(div.push(b, chunk));
        }
    }
    ev_div.extend(div.flush());
    Outcome {
        diversity: correct(&ev_div, &sent),
        alone: [correct(&ev_single[0], &sent), correct(&ev_single[1], &sent)],
        frames,
        stats: div.stats(),
    }
}

/// On a fading channel at an SNR where each branch alone loses many frames, combining
/// recovers clearly more than the better branch, and never fewer.
#[test]
fn combining_beats_either_branch() {
    let o = run(3, 13.0, 40.0, 0.9, (31, 32));
    println!(
        "ch3 13 dB, 40 s: combined {} / A {} / B {} of {} frames; {:?}",
        o.diversity, o.alone[0], o.alone[1], o.frames, o.stats
    );
    let best = o.alone[0].max(o.alone[1]);
    assert!(o.diversity >= best + 5, "combined {} vs best branch {best}", o.diversity);
    assert!(o.stats.combined > o.stats.single[0] + o.stats.single[1], "{:?}", o.stats);
    assert!(o.stats.lead_frames.is_some_and(|l| (1..=4).contains(&l)), "branch A leads by ~0.9 s: {:?}", o.stats.lead_frames);
}

/// A transmitter that sends the same FAC identity in every frame (as one on 1557 kHz,
/// heard through two KiwiSDRs): each branch finds the super frame start from the SDC,
/// and combining still gains.
#[test]
fn combining_with_a_fixed_fac_identity() {
    let o = run_with(3, 13.0, 40.0, 0.9, (31, 32), Some(2));
    println!(
        "fixed identity, ch3 13 dB, 40 s: combined {} / A {} / B {} of {} frames; {:?}",
        o.diversity, o.alone[0], o.alone[1], o.frames, o.stats
    );
    let best = o.alone[0].max(o.alone[1]);
    assert!(o.diversity >= best + 5, "combined {} vs best branch {best}", o.diversity);
    assert!(o.stats.combined > o.stats.single[0] + o.stats.single[1], "{:?}", o.stats);
}

/// Combined against alone over channels and SNRs.
#[test]
#[ignore = "long: a sweep over channels and SNRs"]
fn diversity_sweep() {
    println!("{:<28} {:>6} {:>6} {:>6} {:>6}", "channel / SNR", "A", "B", "both", "of");
    for model in [1, 2, 3, 4] {
        for snr in [8.0, 10.0, 12.0, 14.0, 17.0, 20.0] {
            let o = run(model, snr, 60.0, 0.9, (40 + u64::from(model), 50 + u64::from(model)));
            println!(
                "{:<28} {:>6} {:>6} {:>6} {:>6}",
                format!("ch{model} {} {snr} dB", ChannelModel::drm_name(model)),
                o.alone[0],
                o.alone[1],
                o.diversity,
                o.frames
            );
        }
    }
}
