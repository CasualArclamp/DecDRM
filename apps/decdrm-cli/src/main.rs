//! `decdrm` — command-line front end for the DecDRM receiver.

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use decdrm_engine::{
    Command, Engine, EngineConfig, EngineEvent, InputFormat, InputSpec, LogConfig, RealChannel, ReceiverConfig,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

mod models;
mod schedule;
mod tx;

#[derive(Parser)]
#[command(name = "decdrm", version, about = "Digital Radio Mondiale (DRM30) receiver")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

// Parsed once at start-up, so the size of the receive arguments does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Cmd {
    /// Receive from a recording, a sound card or KiwiSDRs.
    Rx(RxArgs),
    /// Transmit: run the station described by a TOML file.
    Tx(tx::TxArgs),
    /// List sound-card input and output devices.
    Devices,
    /// Neural codec model weights (DAC): download, show status.
    Models(models::ModelsArgs),
    /// Broadcast schedule: which DRM stations are on the air now (EiBi or Dream lists;
    /// `--update` downloads them).
    Schedule(schedule::ScheduleArgs),
}

#[derive(clap::Args)]
struct RxArgs {
    /// WAV/FLAC recording to decode (omit when using --device).
    file: Option<PathBuf>,
    /// Sound-card input device name (or unique part of it), e.g. "CABLE Output".
    #[arg(long, conflicts_with = "file")]
    device: Option<String>,
    /// Use the default sound-card input.
    #[arg(long, conflicts_with_all = ["file", "device"])]
    default_device: bool,
    /// Receive from a KiwiSDR on the internet: its address (host, host:port, or a URL
    /// copied from the browser, whose `f=` also gives the frequency). The input is the
    /// Kiwi's I/Q; --format does not apply.
    #[arg(long, value_name = "ADDRESS", conflicts_with_all = ["file", "device", "default_device"])]
    kiwi: Option<String>,
    /// Diversity reception: a second KiwiSDR on the same frequency (far from the first,
    /// so their signals fade independently); the two are combined before decoding.
    #[arg(long, value_name = "ADDRESS", requires = "kiwi")]
    kiwi2: Option<String>,
    /// Frequency to tune the KiwiSDR to, kHz: the DRM frequency.
    #[arg(long, value_name = "KHZ", requires = "kiwi")]
    freq: Option<f64>,
    /// Password of a KiwiSDR whose channels need one.
    #[arg(long, value_name = "PASSWORD", requires = "kiwi")]
    kiwi_password: Option<String>,
    /// Name shown in the KiwiSDR's user list.
    #[arg(long, value_name = "NAME", default_value = "DecDRM")]
    kiwi_name: String,
    /// Signal format of the input.
    #[arg(long, value_enum, default_value_t = Format::Real)]
    format: Format,
    /// Channel carrying a real signal (stereo inputs).
    #[arg(long, value_enum, default_value_t = Channel::Mix)]
    channel: Channel,
    /// Mirror the spectrum (e.g. lower-sideband reception).
    #[arg(long)]
    flip: bool,
    /// Do not try spectrally inverted signals automatically.
    #[arg(long)]
    no_auto_flip: bool,
    /// Pace file decoding to real time.
    #[arg(long)]
    realtime: bool,
    /// Print a status line every N seconds of signal (0 = never).
    #[arg(long, default_value_t = 5.0)]
    status_every: f64,
    /// Play the decoded audio on the default (or --output-device) sound card.
    /// Implies --realtime for files.
    #[arg(long)]
    play: bool,
    /// Sound-card output device for --play.
    #[arg(long)]
    output_device: Option<String>,
    /// Playback volume for --play, percent (0-100; a squared law, 50 is about -12 dB).
    #[arg(long, value_name = "PERCENT", default_value_t = 100.0)]
    volume: f32,
    /// Smooth the SBR band of HE-AAC/xHE-AAC audio: its level may change by at most 3 dB
    /// per 16 ms, for stations whose encoder switches the band on and off (adds ~0.1 s of
    /// delay; played and written audio alike).
    #[arg(long)]
    smooth_sbr: bool,
    /// Write the decoded audio to a WAV/FLAC file (as decoded, 16-bit; a change of the
    /// audio format carries on in FILE-2.wav, …).
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,
    /// Save slideshow images, websites, EPG and other data objects here.
    #[arg(long, value_name = "DIR")]
    data_dir: Option<PathBuf>,
    /// Short id (0-3) of the service to decode (default: first audio service).
    #[arg(long)]
    service: Option<u8>,
    /// Reception log: metrics rows (and, for JSON Lines, events such as text
    /// messages and data objects). `.csv` → CSV, otherwise JSON Lines.
    #[arg(long, value_name = "FILE")]
    log: Option<PathBuf>,
    /// Seconds of signal between log rows.
    #[arg(long, default_value_t = 1.0, value_name = "SECS")]
    log_interval: f64,
    /// Stop after this many seconds of signal (e.g. for scripted sound-card captures).
    #[arg(long, value_name = "SECS")]
    duration: Option<f64>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    /// Real IF / audio signal.
    Real,
    /// I/Q with I on the left channel.
    Iq,
    /// I/Q with I on the right channel.
    IqSwapped,
}

#[derive(Clone, Copy, ValueEnum)]
enum Channel {
    Left,
    Right,
    Mix,
    Diff,
}

/// Set by Ctrl-C: long runs stop cleanly, so WAV/FLAC files and logs are finalised.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Whether Ctrl-C was pressed.
pub(crate) fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // The first Ctrl-C asks the running command to stop; a second one quits at once.
    let _ = ctrlc::set_handler(|| {
        if INTERRUPTED.swap(true, Ordering::SeqCst) {
            std::process::exit(130);
        }
        eprintln!("
stopping (Ctrl-C again to quit immediately)");
    });
    match cli.cmd {
        Cmd::Devices => devices(),
        Cmd::Rx(args) => rx(args),
        Cmd::Tx(args) => tx::run(args),
        Cmd::Models(args) => models::run(args),
        Cmd::Schedule(args) => schedule::run(args),
    }
}

fn devices() -> Result<()> {
    for (title, list) in [("Input", decdrm_io::list_input_devices()?), ("Output", decdrm_io::list_output_devices()?)] {
        println!("{title} devices:");
        if list.is_empty() {
            println!("  (none)");
        }
        for d in list {
            let fmt = d.default_format.map(|f| format!("{} Hz, {} ch", f.sample_rate, f.channels)).unwrap_or_default();
            println!("  {}{} [{}] {}", if d.is_default { "* " } else { "  " }, d.name, d.host, fmt);
        }
    }
    Ok(())
}

fn rx(a: RxArgs) -> Result<()> {
    let input = match (&a.file, &a.device, a.default_device, &a.kiwi) {
        (_, _, _, Some(k)) => {
            use decdrm_engine::decdrm_kiwi::{KiwiAddress, KiwiConfig, frequency_from_url};
            let freq = a
                .freq
                .or_else(|| frequency_from_url(k))
                .ok_or_else(|| anyhow::anyhow!("give the frequency to tune the KiwiSDR to with --freq KHZ"))?;
            let kiwi = |address: &str| -> Result<InputSpec> {
                let mut cfg = KiwiConfig::new(KiwiAddress::parse(address)?, freq);
                cfg.password = a.kiwi_password.clone().unwrap_or_default();
                cfg.ident = a.kiwi_name.clone();
                Ok(InputSpec::Kiwi(cfg))
            };
            match &a.kiwi2 {
                Some(k2) => InputSpec::Diversity(Box::new([kiwi(k)?, kiwi(k2)?])),
                None => kiwi(k)?,
            }
        }
        (Some(p), _, _, None) => InputSpec::File { path: p.clone(), realtime: a.realtime || a.play },
        (None, Some(d), _, None) => InputSpec::Device { name: Some(d.clone()), channels: None },
        (None, None, true, None) => InputSpec::Device { name: None, channels: None },
        _ => anyhow::bail!("give a file, --device NAME, --default-device or --kiwi ADDRESS"),
    };
    let format = match a.format {
        Format::Real => InputFormat::Real(match a.channel {
            Channel::Left => RealChannel::Left,
            Channel::Right => RealChannel::Right,
            Channel::Mix => RealChannel::Mix,
            Channel::Diff => RealChannel::Diff,
        }),
        Format::Iq => InputFormat::Iq { swap: false },
        Format::IqSwapped => InputFormat::Iq { swap: true },
    };
    let receiver = ReceiverConfig { input: format, flip: a.flip, auto_flip: !a.no_auto_flip, ..Default::default() };
    let engine = Engine::start(EngineConfig {
        input,
        receiver,
        play_audio: a.play,
        output_device: a.output_device.clone(),
        volume: decdrm_engine::volume_gain(a.volume),
        record_audio: a.out.clone(),
        data_dir: a.data_dir.clone(),
        smooth_sbr: a.smooth_sbr,
        log: a.log.clone().map(|p| LogConfig { interval_s: a.log_interval, ..LogConfig::new(p) }),
        ..EngineConfig::default()
    });
    if let Some(id) = a.service {
        engine.command(Command::SelectService(id));
    }

    // Stop cleanly on Ctrl-C by polling a flag (no extra crates needed: the engine
    // stops when its handle is dropped at the end of main).
    let started = Instant::now();
    let mut next_status = a.status_every;
    let mut afs_shown: Vec<String> = Vec::new();
    let mut stop_sent = false;
    loop {
        if interrupted() && !stop_sent {
            // The engine finishes the audio file and the log, then reports Stopped.
            engine.command(Command::Stop);
            stop_sent = true;
        }
        match engine.recv_event(Duration::from_millis(200)) {
            Some(EngineEvent::Log(l)) => println!("{l}"),
            Some(EngineEvent::Text(t)) => println!("text: {t}"),
            Some(EngineEvent::Data { .. }) => {}
            Some(EngineEvent::Stopped { error }) => {
                if let Some(e) = error {
                    eprintln!("error: {e}");
                }
                break;
            }
            None => {}
        }
        let s = engine.snapshot();
        if !stop_sent && a.duration.is_some_and(|d| s.input.position_s >= d) {
            engine.command(Command::Stop);
            stop_sent = true;
        }
        if s.afs != afs_shown {
            for line in &s.afs {
                println!("AFS: {line}");
            }
            afs_shown = s.afs.clone();
        }
        if a.status_every > 0.0 && s.input.position_s >= next_status {
            next_status += a.status_every;
            print_status(&s);
        }
    }
    let s = engine.snapshot();
    print_status(&s);
    println!(
        "processed {:.1} s of signal in {:.2} s; FAC ok {} bad {}; SDC ok {} bad {}",
        s.input.position_s,
        started.elapsed().as_secs_f64(),
        s.rx.fac_ok,
        s.rx.fac_bad,
        s.rx.sdc_ok,
        s.rx.sdc_bad
    );
    println!(
        "MSC frames {} (ok {} bad {}); audio {}: {} frames ok, {} concealed",
        s.msc.frames,
        s.msc.ok,
        s.msc.bad,
        if s.audio.codec.is_empty() { "-" } else { &s.audio.codec },
        s.audio.frames_ok,
        s.audio.frames_bad
    );
    if let Some(d) = &s.diversity {
        println!("{}", diversity_line(d));
    }
    engine.command(Command::Stop);
    Ok(())
}

/// The combiner's counts and both branches' SNR, for the status line and the summary.
fn diversity_line(d: &decdrm_engine::DiversityView) -> String {
    let st = &d.stats;
    let snr = |b: usize| d.branches[b].snr_db.map_or_else(|| "-".into(), |x| format!("{x:.1} dB"));
    let share = st.share.map_or_else(String::new, |w| format!(", weights {:.0}/{:.0} %", 100.0 * w, 100.0 * (1.0 - w)));
    let lead = match st.lead_frames {
        Some(l) if l > 0 => format!(", KiwiSDR 1 {:.1} s ahead", l as f64 * 0.4),
        Some(l) if l < 0 => format!(", KiwiSDR 2 {:.1} s ahead", -l as f64 * 0.4),
        Some(_) => ", in step".to_string(),
        None => ", not paired yet".to_string(),
    };
    format!(
        "diversity: {} frames combined, {} / {} from one KiwiSDR alone, {} lost; SNR {} / {}{share}{lead}",
        st.combined,
        st.single[0],
        st.single[1],
        st.lost,
        snr(0),
        snr(1)
    )
}

fn print_status(s: &decdrm_engine::Snapshot) {
    let r = &s.rx;
    let f1 = |v: Option<f64>| v.map(|x| format!("{x:.1}")).unwrap_or_else(|| "-".into());
    println!(
        "[{:6.1}s] {:?} mode {} {} DC {} Hz SNR {} dB MER {} dB WMER {} dB Doppler {:.2} Hz delay {:.2} ms SRO {:.2} Hz",
        s.input.position_s,
        r.state,
        r.mode.map(|m| m.to_string()).unwrap_or_else(|| "-".into()),
        r.occupancy.map(|o| o.to_string()).unwrap_or_default(),
        f1(r.dc_frequency_hz),
        f1(r.snr_db),
        f1(r.mer_db),
        f1(r.wmer_db),
        r.doppler_hz,
        r.delay_ms,
        r.sro_hz
    );
    for k in s.input.kiwi.iter().chain(&s.input.kiwi2) {
        let rssi = k.rssi_dbm.map_or_else(|| "-".into(), |r| format!("{r:.1}"));
        let name = k.name.as_deref().map(|n| format!(" \"{n}\"")).unwrap_or_default();
        let overflow = if k.adc_overflows > 0 { format!(", ADC overloads {}", k.adc_overflows) } else { String::new() };
        let reconnects = if k.reconnects > 0 { format!(", reconnected {}x", k.reconnects) } else { String::new() };
        println!("          KiwiSDR {}{name} {} at {:.3} kHz, S-meter {rssi} dBm{overflow}{reconnects}", k.address, k.state, k.freq_khz);
    }
    if let Some(d) = &s.diversity {
        println!("          {}", diversity_line(d));
    }
    if let Some(t) = &s.time_utc {
        println!("          broadcast time {t}");
    }
    for sv in &s.services {
        println!(
            "          service {} id {:06X} {} {}",
            sv.short_id,
            sv.service_id,
            if sv.label.is_empty() { "(no label yet)" } else { &sv.label },
            sv.description
        );
    }
}
