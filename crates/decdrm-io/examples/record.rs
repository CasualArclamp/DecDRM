//! Records a sound-card input to a WAV (32-bit float) or FLAC (16-bit) file, in the
//! device's own format — e.g. to capture a virtual audio cable for offline decoding.
//!
//! ```text
//! cargo run --release -p decdrm-io --example record -- "CABLE-A Output" 20 capture.wav
//! ```

use decdrm_io::{Container, Encoding, FileWriter, InputOptions, InputStream};
use std::path::Path;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Rust note: slice patterns destructure a fixed number of arguments in one step.
    let [device, seconds, path] = args.as_slice() else {
        eprintln!("usage: record DEVICE SECONDS OUT.wav|OUT.flac");
        std::process::exit(2);
    };
    let path = Path::new(path);
    let container = Container::from_path(path).ok_or("the output must end in .wav or .flac")?;
    let encoding = match container {
        Container::Wav => Encoding::Float32,
        Container::Flac => Encoding::Int16,
    };
    let opts = InputOptions { device: Some(device.clone()), sample_rate: None, channels: None, buffer: Duration::from_secs(2) };
    let mut stream = InputStream::open(&opts)?;
    let fmt = stream.format();
    let mut writer = FileWriter::create(path, fmt, container, encoding)?;
    let total = (seconds.parse::<f64>()? * f64::from(fmt.sample_rate)) as usize;
    let mut done = 0;
    while done < total {
        let block = stream.read_blocking(4096.min(total - done), Duration::from_secs(2))?;
        if block.is_empty() {
            eprintln!("the device delivered no audio for 2 s");
            break;
        }
        done += block.len() / fmt.channels;
        writer.write(&block)?;
    }
    writer.finalize()?;
    let stats = stream.stats();
    eprintln!(
        "recorded {:.1} s from {} ({fmt}), {} frames dropped",
        done as f64 / f64::from(fmt.sample_rate),
        stream.device_name(),
        stats.dropped_frames
    );
    Ok(())
}
