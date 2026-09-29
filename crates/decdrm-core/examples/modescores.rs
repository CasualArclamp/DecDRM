//! Diagnostic: robustness-mode detection score ratios (true mode / best other) and
//! detection times per layout and SNR; SNR -99 = noise only (false detections).
//! `cargo run --release -p decdrm-core --example modescores -- 99 10 5 2 -99`

use decdrm_core::channel::{ChannelConfig, ChannelSimulator, Rng};
use decdrm_core::fac::{ChannelParams, Fac, Interleaving, MscMode, SdcMode, ServiceParams};
use decdrm_core::params::{RobustnessMode, SpectrumOccupancy};
use decdrm_core::rx::timesync::{TimeSync, TimeSyncEvent};
use decdrm_core::tx::{Transmitter, TxConfig};

fn main() {
    let snrs: Vec<f64> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let snrs = if snrs.is_empty() { vec![99.0, 10.0, 5.0] } else { snrs };
    for mode in RobustnessMode::ALL {
        for so in SpectrumOccupancy::ALL {
            let Ok(mut tx) = Transmitter::new(TxConfig { mode, occupancy: so, ..Default::default() }) else { continue };
            for &snr in &snrs {
                let mut tx2 = Transmitter::new(TxConfig { mode, occupancy: so, ..Default::default() }).unwrap();
                std::mem::swap(&mut tx, &mut tx2);
                let mut chan = (snr < 90.0 && snr > -50.0).then(|| ChannelSimulator::new(tx.layout(), ChannelConfig::awgn(snr)));
                let mut ts = TimeSync::new(RobustnessMode::B);
                let mut rng = Rng::new(5);
                let cap = tx.msc_capacity();
                let mut ratios = Vec::new();
                let mut detections = Vec::new();
                let mut wrong = 0;
                let mut samples = 0usize;
                let mut last_det = 0usize;
                for _ in 0..75 {
                    let fac = Fac {
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
                        service: ServiceParams {
                            service_id: 1,
                            short_id: 0,
                            audio_ca: false,
                            language: 0,
                            is_data: false,
                            descriptor: 0,
                            data_ca: false,
                        },
                    };
                    let msc = rng.bits(cap.total_bits());
                    let sdc = (tx.frame_index() == 0).then(|| rng.bytes(tx.sdc_capacity_bytes()));
                    let bb = tx.transmit_frame(&fac, &msc, sdc.as_deref()).unwrap();
                    let bb = if snr < -50.0 {
                        (0..bb.len()).map(|_| rng.complex_gaussian()).collect()
                    } else {
                        bb
                    };
                    let bb = match chan.as_mut() {
                        Some(c) => {
                            let mut o = Vec::new();
                            c.process(&bb, &mut o);
                            o
                        }
                        None => bb,
                    };
                    for piece in bb.chunks(960) {
                        samples += piece.len();
                        for ev in ts.push(piece) {
                            let TimeSyncEvent::ModeDetected { mode: m, .. } = ev;
                            if m != mode {
                                wrong += 1;
                            }
                            detections.push((samples - last_det) as f64 / 48000.0);
                            last_det = samples;
                            ts.restart(RobustnessMode::B);
                        }
                        let s = ts.last_mode_scores;
                        if s.iter().all(|&v| v > 0.0) {
                            let t = s[mode.index()];
                            let other = s.iter().enumerate().filter(|&(i, _)| i != mode.index()).map(|(_, &v)| v).fold(0.0, f64::max);
                            ratios.push(t / other);
                        }
                        ts.trim_unsynchronised();
                        while ts.next_window().is_some() {}
                    }
                }
                ratios.sort_by(f64::total_cmp);
                let q = |p: f64| ratios.get(((ratios.len() as f64 - 1.0) * p) as usize).copied().unwrap_or(0.0);
                let mean_det = if detections.is_empty() { 0.0 } else { detections.iter().sum::<f64>() / detections.len() as f64 };
                println!(
                    "{mode}/SO{} snr {snr:>4}: ratio min {:5.2} p10 {:5.2} p50 {:5.2}  detections {:3} (mean {:.2} s) wrong {}",
                    so.value(),
                    q(0.0),
                    q(0.1),
                    q(0.5),
                    detections.len(),
                    mean_det,
                    wrong
                );
            }
        }
    }
}
