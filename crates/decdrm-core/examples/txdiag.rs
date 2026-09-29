//! Transmit a layout with our transmitter and trace the receiver's acquisition.
//! Usage: txdiag <mode A-D> <so 0-5> [seconds]
use decdrm_core::fac::{ChannelParams, Fac, ServiceParams};
use decdrm_core::params::{RobustnessMode, SpectrumOccupancy};
use decdrm_core::rx::{InputFormat, Receiver, ReceiverConfig, ReceiverEvent};
use decdrm_core::tx::output::{OutputConfig, OutputStage};
use decdrm_core::tx::{Transmitter, TxConfig};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mode = match a[1].as_str() { "A" => RobustnessMode::A, "B" => RobustnessMode::B, "C" => RobustnessMode::C, _ => RobustnessMode::D };
    let so = SpectrumOccupancy::new(a[2].parse().unwrap()).unwrap();
    let secs: f64 = a.get(3).map(|s| s.parse().unwrap()).unwrap_or(6.0);
    let txc = TxConfig { mode, occupancy: so, ..Default::default() };
    let mut tx = Transmitter::new(txc.clone()).unwrap();
    let mut out = OutputStage::new(tx.layout(), OutputConfig::iq(0.0)).unwrap();
    let cap = tx.msc_capacity();
    let fac = Fac {
        channel: ChannelParams { enhancement: false, frame_index: 0, afs_valid: false, occupancy: so, interleaving: txc.interleaving,
            msc_mode: txc.msc_mode, sdc_mode: txc.sdc_mode, num_audio: 1, num_data: 0, reconfiguration_index: 0, toggle: false },
        service: ServiceParams { service_id: 0x123456, short_id: 0, audio_ca: false, language: 5, is_data: false, descriptor: 1, data_ca: false },
    };
    let sdc = vec![0u8; tx.sdc_capacity_bytes()];
    let mut rx = Receiver::new(ReceiverConfig { input: InputFormat::Iq { swap: false }, channels: 2, ..Default::default() });
    let frames = (secs / 0.4) as usize;
    let mut seed = 1u32;
    for f in 0..frames {
        let msc: Vec<u8> = (0..cap.total_bits()).map(|_| { seed ^= seed << 13; seed ^= seed >> 17; seed ^= seed << 5; (seed & 1) as u8 }).collect();
        let bb = tx.transmit_frame(&fac, &msc, if tx.frame_index() == 0 { Some(&sdc) } else { None }).unwrap();
        let mut pcm = Vec::new();
        out.process(&bb, &mut pcm);
        for ev in rx.push(&pcm) {
            let t = f as f64 * 0.4;
            match ev {
                ReceiverEvent::Fac(fc) => println!("{t:5.1}s FAC ok frame {} so {}", fc.channel.frame_index, fc.channel.occupancy.value()),
                ReceiverEvent::FacError => println!("{t:5.1}s FAC error"),
                ReceiverEvent::Sdc(b) => println!("{t:5.1}s SDC crc {}", b.crc_ok),
                ReceiverEvent::Msc(_) => {}
                other => println!("{t:5.1}s {other:?}"),
            }
        }
        let s = rx.status();
        println!("  frame {f:2}: state {:?} fsync {:?} facMER {:?} snr {:?}", s.state, s.frame_sync, s.fac_mer_db.map(|v| v.round()), s.snr_db.map(|v| v.round()));
    }
}
