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
    /// Pass the signal through DRM channel model 1-6 (ES 201 980 annex B) before the
    /// outputs, overriding the file's [simulate] section.
    #[arg(long, value_name = "1-6")]
    channel_model: Option<u8>,
    /// Add white noise for this SNR (dB, in the nominal channel bandwidth); implies
    /// channel model 1 (AWGN) unless one is set.
    #[arg(long, value_name = "DB", allow_negative_numbers = true)]
    snr: Option<f64>,
}

pub fn run(a: TxArgs) -> Result<()> {
    let mut cfg = StationConfig::load(&a.config)?;
    if let Some(out) = &a.output {
        // Relative to the current directory, not to the configuration file.
        cfg.output.file = Some(std::path::absolute(out).with_context(|| format!("output path {}", out.display()))?);
        cfg.output.device = None;
    }
    if a.channel_model.is_some() || a.snr.is_some() {
        let sim = cfg.simulate.get_or_insert_with(|| decdrm_station::SimulateSettings::new(1, None));
        if let Some(model) = a.channel_model {
            sim.channel = model;
        }
        if a.snr.is_some() {
            sim.snr_db = a.snr;
        }
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

    // Checked before the station creates its output file.
    if frames.is_none() && cfg.output.device.is_none() && !cfg.inputs_finite() {
        bail!("the signal goes to a file but has no end: give --duration SECS (or set `loop = false` on every audio input file)");
    }
    for url in cfg.services.iter().filter_map(|s| s.audio.as_ref()?.input.url.as_ref()) {
        println!("web stream: connecting to {url}");
    }
    let mut station = Station::new(cfg)?;
    if let Some(dev) = &station.status().device {
        println!("sound card: {dev}");
    }
    // The inputs' log: web stream connections, titles, reconnections.
    let print_log = |station: &mut Station| station.take_log().iter().for_each(|line| println!("{line}"));
    print_log(&mut station);
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
        print_log(&mut station);
        let s = station.status();
        if a.status_every > 0.0 && s.seconds >= next_status {
            next_status += a.status_every;
            print_status(s);
            printed_at = Some(s.frames);
        }
    }
    print_log(&mut station);
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
                ": {}, input {:.1} dBFS{}{}{}{}",
                a.codec,
                a.counters.input_rms_dbfs,
                if a.input_finished { " (ended)" } else { "" },
                a.web_stream
                    .as_ref()
                    .map(|w| format!(
                        ", web stream {} ({:.1} s buffered){}",
                        w.state,
                        w.buffer_s,
                        w.title.as_ref().map(|t| format!(", \"{t}\"")).unwrap_or_default()
                    ))
                    .unwrap_or_default(),
                a.counters.input_drift_ppm.map(|p| format!(", clock trim {p:+.0} ppm")).unwrap_or_default(),
                if a.counters.frames_dropped > 0 { format!(", {} frames dropped", a.counters.frames_dropped) } else { String::new() }
            ));
        }
        for app in &sv.apps {
            line.push_str(&format!("; {} {:.2} kbit/s", app.kind, app.bitrate / 1000.0));
        }
        println!("{line}");
    }
}
