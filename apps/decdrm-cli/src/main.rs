//! `decdrm` — command-line front end for the DecDRM receiver.

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use decdrm_engine::{Command, Engine, EngineConfig, EngineEvent, InputFormat, InputSpec, RealChannel, ReceiverConfig};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "decdrm", version, about = "Digital Radio Mondiale (DRM30) receiver")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Receive from a recording or a sound card.
    Rx(RxArgs),
    /// List sound-card input and output devices.
    Devices,
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Devices => devices(),
        Cmd::Rx(args) => rx(args),
    }
}

fn devices() -> Result<()> {
    for (title, list) in [("Input", decdrm_io::list_input_devices()?), ("Output", decdrm_io::list_output_devices()?)] {
        println!("{title} devices:");
        for d in list {
            let fmt = d.default_format.map(|f| format!("{} Hz, {} ch", f.sample_rate, f.channels)).unwrap_or_default();
            println!("  {}{} [{}] {}", if d.is_default { "* " } else { "  " }, d.name, d.host, fmt);
        }
    }
    Ok(())
}

fn rx(a: RxArgs) -> Result<()> {
    let input = match (&a.file, &a.device, a.default_device) {
        (Some(p), _, _) => InputSpec::File { path: p.clone(), realtime: a.realtime },
        (None, Some(d), _) => InputSpec::Device { name: Some(d.clone()), channels: None },
        (None, None, true) => InputSpec::Device { name: None, channels: None },
        _ => anyhow::bail!("give a file, --device NAME or --default-device"),
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
        play_audio: false,
        output_device: None,
        record_audio: None,
        data_dir: None,
    });

    // Stop cleanly on Ctrl-C by polling a flag (no extra crates needed: the engine
    // stops when its handle is dropped at the end of main).
    let started = Instant::now();
    let mut next_status = a.status_every;
    loop {
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
        if a.status_every > 0.0 {
            let s = engine.snapshot();
            if s.input.position_s >= next_status {
                next_status += a.status_every;
                print_status(&s);
            }
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
    engine.command(Command::Stop);
    Ok(())
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
