//! `decdrm tx` — run the DRM transmitter described by a station configuration file.

use anyhow::{Context, Result, bail};
use decdrm_station::{Station, StationConfig, StationStatus};
use std::path::PathBuf;
use std::time::Instant;

#[derive(clap::Args)]
pub struct TxArgs {
    /// Station configuration (TOML; see crates/decdrm-station/examples/station.toml).
    config: PathBuf,
    /// Seconds of signal to transmit. Default: until every (non-looping) input file has
    /// ended; without an end, sound-card output runs until interrupted.
    #[arg(long, value_name = "SECS")]
    duration: Option<f64>,
    /// Write the signal to this WAV/FLAC file instead of the configured outputs.
    #[arg(long, value_name = "FILE")]
    output: Option<PathBuf>,
    /// Print a status line every N seconds of signal (0 = never).
    #[arg(long, default_value_t = 5.0)]
    status_every: f64,
    /// Only check the configuration and print the multiplex.
    #[arg(long)]
    check: bool,
}

pub fn run(a: TxArgs) -> Result<()> {
    let mut cfg = StationConfig::load(&a.config)?;
    if let Some(out) = &a.output {
        // Relative to the current directory, not to the configuration file.
        cfg.output.file = Some(std::path::absolute(out).with_context(|| format!("output path {}", out.display()))?);
        cfg.output.device = None;
    }
    let plan = cfg.validate()?;
    print!("{}", plan.describe(&cfg));
    if a.check {
        return Ok(());
    }
    if let Some(d) = a.duration
        && !(d.is_finite() && d > 0.0)
    {
        bail!("--duration must be a positive number of seconds");
    }
    let frames = a.duration.map(|d| (d / 0.4).ceil() as u64);

    let mut station = Station::new(cfg)?;
    if frames.is_none() && station.config().output.device.is_none() && !station.inputs_finite() {
        bail!("the signal goes to a file but has no end: give --duration SECS (or set `loop = false` on every audio input file)");
    }
    if let Some(dev) = &station.status().device {
        println!("sound card: {dev}");
    }
    let started = Instant::now();
    let mut next_status = a.status_every;
    let mut printed_at = None;
    loop {
        match frames {
            Some(n) if station.status().frames >= n => break,
            None if station.inputs_finished() => break,
            _ if crate::interrupted() => break,
            _ => {}
        }
        station.transmit_frame()?;
        let s = station.status();
        if a.status_every > 0.0 && s.seconds >= next_status {
            next_status += a.status_every;
            print_status(s);
            printed_at = Some(s.frames);
        }
    }
    let s = station.finish()?;
    if printed_at != Some(s.frames) {
        print_status(&s);
    }
    println!("transmitted {:.1} s of signal in {:.2} s", s.seconds, started.elapsed().as_secs_f64());
    Ok(())
}

fn print_status(s: &StationStatus) {
    println!(
        "[{:6.1}s] output {:5.1} dBFS rms, {:5.1} dBFS peak, {} clipped; SDC {}/{} bytes{}{}",
        s.seconds,
        s.output_rms_dbfs,
        s.output_peak_dbfs,
        s.clipped_samples,
        s.sdc_bytes_used,
        s.sdc_capacity,
        s.time_sent.as_ref().map(|t| format!(", time {t}")).unwrap_or_default(),
        if s.device.is_some() { format!(", {} underruns", s.device_underruns) } else { String::new() }
    );
    for sv in &s.services {
        let mut line = format!("          service {} {:06X} \"{}\" {:.2} kbit/s", sv.short_id, sv.service_id, sv.label, sv.bitrate / 1000.0);
        if let Some(a) = &sv.audio {
            line.push_str(&format!(
                ": {}, input {:.1} dBFS{}{}",
                a.codec,
                a.counters.input_rms_dbfs,
                if a.input_finished { " (ended)" } else { "" },
                if a.counters.frames_dropped > 0 { format!(", {} frames dropped", a.counters.frames_dropped) } else { String::new() }
            ));
        }
        for app in &sv.apps {
            line.push_str(&format!("; {} {:.2} kbit/s", app.kind, app.bitrate / 1000.0));
        }
        println!("{line}");
    }
}
