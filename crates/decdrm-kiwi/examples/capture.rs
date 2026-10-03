//! Record KiwiSDRs' I/Q to WAV files for decoding later (`decdrm rx FILE --format iq`):
//! `cargo run --release -p decdrm-kiwi --example capture -- KHZ SECONDS HOST[:PORT]=FILE.wav
//! [HOST[:PORT]=FILE.wav ...]`. The files are 16-bit stereo, I left and Q right, at the
//! Kiwi's rate rounded to 1 Hz (the receiver tracks the rest) — exactly the Kiwi's
//! samples. Several Kiwis record at the same time, e.g. a pair for diversity reception.

use decdrm_kiwi::{KiwiAddress, KiwiConfig, KiwiState, KiwiStream};
use std::io::{BufWriter, Write};
use std::time::{Duration, Instant};

/// Wait this long for a Kiwi to start streaming.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let khz: f64 = args.first().expect("KHZ").parse().expect("frequency in kHz");
    let secs: f64 = args.get(1).expect("SECONDS").parse().expect("seconds");
    let jobs: Vec<(KiwiAddress, String)> = args[2..]
        .iter()
        .map(|a| {
            let (host, file) = a.split_once('=').expect("HOST[:PORT]=FILE.wav");
            (KiwiAddress::parse(host).expect("KiwiSDR address"), file.to_string())
        })
        .collect();
    assert!(!jobs.is_empty(), "give at least one HOST[:PORT]=FILE.wav");
    // One thread per Kiwi (each stream has its own connection thread too), so the
    // recordings run side by side.
    std::thread::scope(|scope| {
        for (address, file) in &jobs {
            scope.spawn(move || record(address.clone(), khz, secs, file));
        }
    });
}

fn record(address: KiwiAddress, khz: f64, secs: f64, file: &str) {
    let s = KiwiStream::start(KiwiConfig::new(address.clone(), khz));
    let t0 = Instant::now();
    while s.status().state != KiwiState::Streaming || s.sample_rate().is_none() {
        let st = s.status();
        if matches!(st.state, KiwiState::Failed | KiwiState::Stopped) || t0.elapsed() > CONNECT_TIMEOUT {
            for line in s.take_log() {
                println!("{address}: {line}");
            }
            println!("{address}: not streaming: {:?}", st.error);
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let rate = s.sample_rate().unwrap_or(12_000.0);
    let st = s.status();
    println!(
        "{address}: streaming at {rate:.3} Hz — {} ({})",
        st.name.as_deref().unwrap_or("?"),
        st.location.as_deref().unwrap_or("?")
    );
    let want = (secs * rate) as usize * 2;
    let mut samples: Vec<f32> = Vec::with_capacity(want);
    let mut rssi = Vec::new();
    while samples.len() < want {
        match s.read_blocking(4096, Duration::from_millis(200)) {
            Ok(v) => samples.extend(v),
            Err(e) => {
                println!("{address}: {e}");
                break;
            }
        }
        if let Some(r) = s.status().rssi_dbm {
            rssi.push(r);
        }
    }
    samples.truncate(want);
    for line in s.take_log() {
        println!("{address}: {line}");
    }
    s.stop_and_join();
    rssi.sort_by(f32::total_cmp);
    let median = rssi.get(rssi.len() / 2).copied().unwrap_or(f32::NAN);
    write_wav(file, rate.round() as u32, &samples).expect("writing the WAV file");
    println!("{address}: {:.1} s to {file}, median S-meter {median:.1} dBm", samples.len() as f64 / 2.0 / rate);
}

/// A 16-bit stereo WAV file of interleaved samples in ±1 (the Kiwi's i16 / 32768).
fn write_wav(path: &str, rate: u32, samples: &[f32]) -> std::io::Result<()> {
    let mut w = BufWriter::new(std::fs::File::create(path)?);
    let data_len = (samples.len() * 2) as u32;
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_len).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?; // PCM
    w.write_all(&2u16.to_le_bytes())?; // channels
    w.write_all(&rate.to_le_bytes())?;
    w.write_all(&(rate * 4).to_le_bytes())?; // bytes per second
    w.write_all(&4u16.to_le_bytes())?; // bytes per frame
    w.write_all(&16u16.to_le_bytes())?; // bits per sample
    w.write_all(b"data")?;
    w.write_all(&data_len.to_le_bytes())?;
    for &x in samples {
        w.write_all(&((x * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes())?;
    }
    w.flush()
}
