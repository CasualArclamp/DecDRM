//! Diagnostic: per-multiplex-frame trace of reception quality and MSC content checks.
//! `cargo run --release -p decdrm-engine --example msctrace -- FILE [from_s] [to_s] [iq]`

use decdrm_core::rx::{InputFormat, RealChannel, ReceiverConfig};
use decdrm_engine::{InputSpec, Session, SessionEvent, Source};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: msctrace FILE [from_s] [to_s] [iq]");
    let from: f64 = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(0.0);
    let to: f64 = args.get(3).and_then(|a| a.parse().ok()).unwrap_or(f64::INFINITY);
    let iq = args.iter().any(|a| a == "iq");
    let mut source = Source::open(&InputSpec::File { path: path.into(), realtime: false })?;
    let ch = source.info().channels;
    let input = if iq { InputFormat::Iq { swap: false } } else { InputFormat::Real(RealChannel::Mix) };
    let mut session = Session::new(ReceiverConfig { input, channels: ch, ..Default::default() });
    let mut last = session.msc_stats;
    while let Some(frames) = source.read(2400)? {
        for ev in session.push(&frames) {
            if let SessionEvent::Log(l) = ev {
                println!("{l}");
            }
        }
        let m = session.msc_stats;
        if m.frames != last.frames {
            let t = session.time_s();
            if t >= from && t <= to {
                let s = session.status();
                let f1 = |v: Option<f64>| v.map_or("-".to_string(), |x| format!("{x:5.1}"));
                let verdict = if m.bad > last.bad {
                    "BAD"
                } else if m.ok > last.ok {
                    "ok"
                } else {
                    "-"
                };
                println!(
                    "{t:7.2}s  SNR {} MER {} WMER {} FAC-MER {} delay {:5.2} ms doppler {:4.2} Hz SRO {:7.2} Hz  {verdict}",
                    f1(s.snr_db),
                    f1(s.mer_db),
                    f1(s.wmer_db),
                    f1(s.fac_mer_db),
                    s.delay_ms,
                    s.doppler_hz,
                    s.sro_hz
                );
            }
            last = m;
        }
    }
    println!("MSC frames {} ok {} bad {}", last.frames, last.ok, last.bad);
    Ok(())
}
