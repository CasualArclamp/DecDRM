//! Diagnostic: sample-rate-offset estimate over time through a DRM channel model with a
//! known offset. `cargo run --release -p decdrm-core --example srotrace -- CHANNEL PPM SNR SECONDS [A|B|C|D] [SEED]`

use decdrm_core::channel::{ChannelConfig, ChannelSimulator, Rng};
use decdrm_core::fac::{ChannelParams, Fac, Interleaving, MscMode, SdcMode, ServiceParams};
use decdrm_core::params::{RobustnessMode, SpectrumOccupancy};
use decdrm_core::rx::{InputFormat, Receiver, ReceiverConfig, ReceiverEvent};
use decdrm_core::tx::output::{OutputConfig, OutputStage};
use decdrm_core::tx::{Transmitter, TxConfig};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let ch: u8 = a[1].parse().unwrap();
    let ppm: f64 = a[2].parse().unwrap();
    let snr: f64 = a[3].parse().unwrap();
    let secs: f64 = a[4].parse().unwrap();
    let mode = match a.get(5).map(String::as_str) {
        Some("A") => RobustnessMode::A,
        Some("C") => RobustnessMode::C,
        Some("D") => RobustnessMode::D,
        _ => RobustnessMode::B,
    };
    let seed: u64 = a.get(6).and_then(|s| s.parse().ok()).unwrap_or(1);
    let txc = TxConfig { mode, occupancy: SpectrumOccupancy::SO_3, msc_mode: MscMode::Qam16Sm, ..Default::default() };
    let mut tx = Transmitter::new(txc).unwrap();
    let cc = ChannelConfig { sample_rate_offset_ppm: ppm, seed: 40 + seed, ..ChannelConfig::drm(ch, snr).unwrap() };
    let mut chan = ChannelSimulator::new(tx.layout(), cc);
    let mut out = OutputStage::new(tx.layout(), OutputConfig::iq(0.0)).unwrap();
    let mut rx = Receiver::new(ReceiverConfig { input: InputFormat::Iq { swap: false }, channels: 2, ..Default::default() });
    let mut rng = Rng::new(seed);
    let cap = tx.msc_capacity();
    let frames = (secs / 0.4) as usize;
    let mut first_fac = None;
    let mut line = String::new();
    for f in 0..frames {
        let fac = Fac {
            channel: ChannelParams {
                enhancement: false,
                frame_index: 0,
                afs_valid: false,
                occupancy: SpectrumOccupancy::SO_3,
                interleaving: Interleaving::Long,
                msc_mode: MscMode::Qam16Sm,
                sdc_mode: SdcMode::Qam16,
                num_audio: 1,
                num_data: 0,
                reconfiguration_index: 0,
                toggle: false,
            },
            service: ServiceParams { service_id: 1, short_id: 0, audio_ca: false, language: 0, is_data: false, descriptor: 0, data_ca: false },
        };
        let msc = rng.bits(cap.total_bits());
        let sdc = (tx.frame_index() == 0).then(|| rng.bytes(tx.sdc_capacity_bytes()));
        let bb = tx.transmit_frame(&fac, &msc, sdc.as_deref()).unwrap();
        let mut c = Vec::new();
        chan.process(&bb, &mut c);
        let mut pcm = Vec::new();
        out.process(&c, &mut pcm);
        for ev in rx.push(&pcm) {
            if let ReceiverEvent::Fac(_) = ev {
                first_fac.get_or_insert(f);
            }
        }
        if (f + 1) % 5 == 0 {
            line += &format!(" {:6.2}", rx.status().sro_hz);
        }
    }
    println!("ch{ch} mode {mode} {ppm:+} ppm (true {:+.2} Hz) {snr} dB, first FAC frame {first_fac:?}; SRO every 2 s:{line}", ppm * 0.048);
}
