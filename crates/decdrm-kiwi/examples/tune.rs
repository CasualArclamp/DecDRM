//! Retune a KiwiSDR during a session and watch its S-meter follow (protocol check):
//! `cargo run -p decdrm-kiwi --example tune -- HOST[:PORT] KHZ KHZ [KHZ...]`. Stays 5 s
//! on each frequency and prints every S-meter change of the first 1.5 s after a retune
//! (the Kiwi's delay plus the network's), when samples resume (after
//! `RETUNE_SETTLE`), and the median S-meter of the last 3 s.

use decdrm_kiwi::{KiwiAddress, KiwiConfig, KiwiState, KiwiStream, RETUNE_SETTLE};
use std::time::{Duration, Instant};

const DWELL: Duration = Duration::from_secs(5);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let address = KiwiAddress::parse(args.first().expect("HOST[:PORT]")).expect("KiwiSDR address");
    let freqs: Vec<f64> = args[1..].iter().map(|f| f.parse().expect("frequency in kHz")).collect();
    assert!(!freqs.is_empty(), "give at least one frequency");
    let s = KiwiStream::start(KiwiConfig::new(address, freqs[0]));
    let t0 = Instant::now();
    while s.status().state != KiwiState::Streaming {
        let st = s.status();
        if matches!(st.state, KiwiState::Failed | KiwiState::Stopped) || t0.elapsed() > Duration::from_secs(20) {
            for line in s.take_log() {
                println!("{line}");
            }
            println!("not streaming: {:?}", st.error);
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("retune settling time: {} ms", RETUNE_SETTLE.as_millis());
    for (i, &freq) in freqs.iter().enumerate() {
        if i > 0 {
            s.tune(freq);
        }
        let start = Instant::now();
        let mut last = None;
        let mut resumed = None;
        let mut late = Vec::new();
        while start.elapsed() < DWELL {
            let got = s.read_blocking(4096, Duration::from_millis(10)).unwrap_or_default();
            if resumed.is_none() && !got.is_empty() {
                resumed = Some(start.elapsed());
            }
            let Some(rssi) = s.status().rssi_dbm else { continue };
            let t = start.elapsed();
            if i > 0 && t < Duration::from_millis(1500) && last != Some(rssi) {
                println!("{freq:9.3} kHz  {:6.3} s  S-meter {rssi:6.1} dBm", t.as_secs_f64());
            }
            if t > DWELL - Duration::from_secs(3) {
                late.push(rssi);
            }
            last = Some(rssi);
        }
        late.sort_by(f32::total_cmp);
        let median = late.get(late.len() / 2).copied().unwrap_or(f32::NAN);
        let resumed = resumed.map_or("never".to_string(), |t| format!("{:.3} s", t.as_secs_f64()));
        println!("{freq:9.3} kHz: samples from {resumed}, median S-meter {median:.1} dBm");
    }
    for line in s.take_log() {
        println!("{line}");
    }
    s.stop_and_join();
}
