//! Test MDI for the receiver's MDI/RSCI input and the modulator: a station's frames as
//! MDI (TS 102 820), written to a recording or sent over UDP in real time.
//!
//! ```text
//! cargo run --release -p decdrm-station --example mdi_source -- station.toml out.rsM 60
//! cargo run --release -p decdrm-station --example mdi_source -- station.toml udp:127.0.0.1:8000 600 [pft] [rsci]
//! ```
//!
//! The third argument is the length in seconds. A `.pcap` name writes a capture (UDP to
//! port 8000), other names the file framing Dream writes as `.rsX`. `pft` sends PFT
//! fragments with Reed–Solomon protection (one lost fragment per packet survives);
//! `rsci` adds made-up receiver status items (profile A), as an RSCI receiver sends.
//! The station's own output (`[output]`) is still written; point it at a file.

use decdrm_mdi::file::{DcpFileWriter, FileKind};
use decdrm_mdi::net::UdpSender;
use decdrm_mdi::pft::{PftConfig, fragment};
use decdrm_mdi::rsci::{ImpulseResponse, RxFlags};
use decdrm_mdi::{Protocol, RsciStatus};
use decdrm_station::{Station, StationConfig};
use std::path::Path;
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("usage: mdi_source STATION.toml (OUT.rsM | OUT.pcap | udp:HOST:PORT) SECONDS [pft] [rsci]");
        std::process::exit(2);
    }
    let cfg = StationConfig::load(&args[0])?;
    let seconds: f64 = args[2].parse()?;
    let pft = args.iter().any(|a| a == "pft");
    let rsci = args.iter().any(|a| a == "rsci");
    let mut station = Station::new(cfg)?;
    station.capture_mdi(true);
    let frames = (seconds / 0.4).ceil() as u64;

    enum Sink {
        File(DcpFileWriter),
        Udp(UdpSender),
    }
    let mut sink = match args[1].strip_prefix("udp:") {
        Some(dest) => Sink::Udp(UdpSender::new(&dest.parse()?)?),
        None => {
            let path = Path::new(&args[1]);
            let pcap = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("pcap"));
            let kind = if pcap {
                FileKind::Pcap
            } else if pft {
                FileKind::RawPft
            } else {
                FileKind::FileIo
            };
            Sink::File(DcpFileWriter::create(path, kind, 8000)?)
        }
    };
    let t0 = Instant::now();
    for n in 0..frames {
        station.transmit_frame()?;
        let mut f = station.last_mdi().cloned().expect("capturing");
        if rsci {
            f.protocol = Some(Protocol::RSCI);
            f.rsci = status(n);
        }
        let af = f.to_af(n as u16).to_bytes();
        let packets: Vec<Vec<u8>> = if pft {
            let cfg = PftConfig { max_payload: 1400, fec: Some(1), ..PftConfig::default() };
            fragment(&af, n as u16, &cfg).iter().map(|f| f.to_bytes()).collect()
        } else {
            vec![af]
        };
        let time = n as f64 * 0.4;
        for p in &packets {
            match &mut sink {
                Sink::File(w) => w.write_packet(p, Some(time))?,
                Sink::Udp(u) => u.send(p)?,
            }
        }
        if matches!(sink, Sink::Udp(_)) {
            // Real time: one frame per 400 ms.
            let due = t0 + Duration::from_secs_f64(time + 0.4);
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
    }
    if let Sink::File(w) = sink {
        w.finish()?;
    }
    station.finish()?;
    println!("{frames} frames of MDI written in {:.1} s", t0.elapsed().as_secs_f64());
    Ok(())
}

/// Made-up receiver status: a slowly varying SNR, a two-path impulse response.
fn status(n: u64) -> RsciStatus {
    let wmer = 18.0 + 3.0 * (n as f64 * 0.05).sin();
    RsciStatus {
        profile: Some('A'),
        signal_dbuv: Some(35.0 + wmer / 4.0),
        flags: Some(RxFlags::default()),
        wmer_fac_db: Some(wmer + 2.0),
        wmer_msc_db: Some(wmer),
        mer_db: Some(wmer + 1.0),
        doppler_hz: Some(0.4),
        delay: vec![(95, 1.2), (99, 2.4)],
        psd_db: Some(
            (0..85)
                .map(|i| {
                    let f = -7.875 + 0.1875 * f64::from(i);
                    if (-4.5..=4.5).contains(&f) { -35.0 - (f * 3.0).sin().abs() } else { -75.0 }
                })
                .collect(),
        ),
        impulse_response: Some(ImpulseResponse {
            start_ms: -1.0,
            end_ms: 7.0,
            db: (0..33).map(|i| match i { 4 => 0.0, 9 => -8.0, _ => -45.0 }).collect(),
        }),
        frequency_hz: Some(6_070_000),
        demodulation: Some("drm_".into()),
        receiver_info: Some("DecDRM test RSCI".into()),
        ..RsciStatus::default()
    }
}
