//! Plays a WAV or FLAC file through [`AudioPlayer`].
//!
//! ```text
//! cargo run -p decdrm-io --example play_file -- <file.wav|file.flac> [output device]
//! ```
//!
//! The device may be given as any unique part of its name (see the `list_devices` example).
//! The file is decoded much faster than real time; `push_blocking` paces decoding to the
//! sound card, which is how file playback should be driven.

use std::time::{Duration, Instant};

use decdrm_io::{AudioPlayer, FileReader, PlayerOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or("usage: play_file <file.wav|file.flac> [output device]")?;
    let device = args.next();

    let mut reader = FileReader::open(&path)?;
    let fmt = reader.format();
    let length = reader
        .duration()
        .map_or("unknown length".to_string(), |d| format!("{:.1} s", d.as_secs_f64()));
    println!("{path}: {fmt}, {} bit, {length}", reader.bits_per_sample().unwrap_or(0));

    let mut player = AudioPlayer::open(PlayerOptions {
        device,
        // File playback is paced by the sound card, so there is no drift to compensate.
        drift_compensation: false,
        ..Default::default()
    })?;
    println!("playing on {} ({})", player.device_name(), player.device_format());

    let block_frames = (fmt.sample_rate / 10) as usize; // 100 ms blocks
    let mut last_report = Instant::now();
    while let Some(block) = reader.read(block_frames)? {
        player.push_blocking(&block, fmt.sample_rate, fmt.channels)?;
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            let st = player.status();
            println!(
                "  {:6.1} s   buffered {:4.0} ms   {:?}   underruns {}",
                fmt.frames_to_duration(reader.position()).as_secs_f64(),
                st.buffered.as_secs_f64() * 1e3,
                st.state,
                st.underruns
            );
        }
    }
    let drained = player.drain(Duration::from_secs(5));
    let st = player.status();
    println!(
        "finished{}: underruns {}, backend xruns {}",
        if drained { "" } else { " (drain timed out)" },
        st.underruns,
        st.xruns
    );
    for e in player.take_errors() {
        println!("device error: {e}");
    }
    Ok(())
}
