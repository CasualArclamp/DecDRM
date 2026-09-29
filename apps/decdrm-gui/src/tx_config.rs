//! Station configuration handling for the Transmitter tab: the shipped example, the
//! quick overrides that do not touch the file, and the checks made before
//! transmitting. Plain logic, so it can be unit-tested.

use crate::settings::TxOutput;
use decdrm_core::params::ChannelLayout;
use decdrm_core::tx::output::OutputFormat;
use decdrm_station::{MultiplexPlan, StationConfig, StationError};
use std::path::{Path, PathBuf};

/// The shipped example (`crates/decdrm-station/examples/station.toml`). Rust note:
/// `include_str!` reads the file at compile time, so the program always has it.
pub const EXAMPLE_STATION: &str =
    include_str!("../../../crates/decdrm-station/examples/station.toml");
/// The Journaline page file the example refers to (`path = "journaline.toml"`).
pub const EXAMPLE_JOURNALINE: &str =
    include_str!("../../../crates/decdrm-station/examples/journaline.toml");

/// Frame duration, seconds.
const FRAME_SECONDS: f64 = 0.4;

/// Directory for the untitled example's companion files and relative outputs:
/// `station-example` next to the GUI settings file (so a test run with `--config`
/// keeps everything in its own directory).
pub fn example_dir(settings_file: Option<&Path>) -> PathBuf {
    settings_file
        .and_then(Path::parent)
        .map(|d| d.join("station-example"))
        .unwrap_or_else(|| std::env::temp_dir().join("decdrm-station-example"))
}

/// Write the files the example refers to into `dir` (existing files are kept, so
/// edits survive).
pub fn materialize_example(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let pages = dir.join("journaline.toml");
    if !pages.exists() {
        std::fs::write(pages, EXAMPLE_JOURNALINE)?;
    }
    Ok(())
}

/// The quick overrides of the Transmitter tab.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Overrides {
    pub output: TxOutput,
    /// File for [`TxOutput::File`].
    pub file: Option<PathBuf>,
    /// Sound card for [`TxOutput::Device`] (`None` = system default).
    pub device: Option<String>,
}

/// Every problem an error describes: the configuration problems, else the message.
pub fn problems_of(e: &StationError) -> Vec<String> {
    match e.problems() {
        [] => vec![e.to_string()],
        p => p.to_vec(),
    }
}

/// Why the editor text cannot be transmitted: every problem found, and for a TOML
/// syntax error the (line, column) where the parser stopped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Problems {
    pub list: Vec<String>,
    pub location: Option<(usize, usize)>,
}

impl Problems {
    fn one(problem: impl Into<String>) -> Self {
        Self {
            list: vec![problem.into()],
            location: None,
        }
    }
}

impl From<&StationError> for Problems {
    fn from(e: &StationError) -> Self {
        Self {
            list: problems_of(e),
            location: e.location(),
        }
    }
}

/// Parse the editor text, resolve relative paths against `base_dir` (the file's
/// directory) and apply the overrides.
pub fn prepare(text: &str, base_dir: &Path, ov: &Overrides) -> Result<StationConfig, Problems> {
    let mut cfg = StationConfig::from_toml_str(text).map_err(|e| Problems::from(&e))?;
    cfg.base_dir = Some(base_dir.to_path_buf());
    match ov.output {
        TxOutput::Config => {}
        TxOutput::File => {
            let Some(file) = &ov.file else {
                return Err(Problems::one(
                    "output: choose the file to write (\"…\" next to File)",
                ));
            };
            cfg.output.file = Some(file.clone());
            cfg.output.device = None;
        }
        TxOutput::Device => {
            cfg.output.device = Some(ov.device.clone().unwrap_or_else(|| "default".into()));
            cfg.output.file = None;
        }
    }
    Ok(cfg)
}

/// [`prepare`] plus the station's own check; the plan on success.
pub fn check(
    text: &str,
    base_dir: &Path,
    ov: &Overrides,
) -> Result<(StationConfig, MultiplexPlan), Problems> {
    let cfg = prepare(text, base_dir, ov)?;
    let plan = cfg.validate().map_err(|e| Problems::from(&e))?;
    Ok((cfg, plan))
}

/// Character index (not byte index) where line `line` (counted from 1) of `text`
/// starts, for placing an editor cursor; the end of the text past the last line.
pub fn line_start_char(text: &str, line: usize) -> usize {
    if line <= 1 {
        return 0;
    }
    let mut seen = 1;
    for (i, c) in text.chars().enumerate() {
        if c == '\n' {
            seen += 1;
            if seen == line {
                return i + 1;
            }
        }
    }
    text.chars().count()
}

/// Frames for a duration limit (400 ms each, rounded up); `None` for a non-positive
/// or non-finite duration.
pub fn frames_for(seconds: f64) -> Option<u64> {
    (seconds.is_finite() && seconds > 0.0).then(|| (seconds / FRAME_SECONDS).ceil() as u64)
}

/// `true` if the programme ends by itself: there is audio and every audio input is a
/// non-looping file (`StationConfig::inputs_finite`, checked before any output file
/// is created).
pub fn inputs_finite(cfg: &StationConfig) -> bool {
    cfg.inputs_finite()
}

/// Why a checked configuration cannot be transmitted as it stands. `allow_device` is
/// false when sound-card output is disabled for this run (`--no-audio`).
pub fn start_check(
    cfg: &StationConfig,
    frames: Option<u64>,
    allow_device: bool,
) -> Result<(), String> {
    let out = &cfg.output;
    if out.file.is_none() && out.device.is_none() {
        return Err(
            "the configuration has no output: set [output] file or device, or choose one above"
                .into(),
        );
    }
    if out.device.is_some() && !allow_device {
        return Err("sound-card output is disabled in this run (--no-audio)".into());
    }
    if frames.is_none() && out.device.is_none() && !inputs_finite(cfg) {
        return Err(
            "the signal goes to a file but has no end: set a duration limit \
             (or `loop = false` on every audio input file)"
                .into(),
        );
    }
    Ok(())
}

/// One line describing where the signal goes and in which form.
pub fn describe_output(cfg: &StationConfig, plan: &MultiplexPlan) -> String {
    let o = &cfg.output;
    let mut sinks = Vec::new();
    if let Some(f) = &o.file {
        sinks.push(format!("file {}", cfg.resolve(f).display()));
    }
    if let Some(d) = &o.device {
        sinks.push(format!("sound card \"{d}\""));
    }
    if sinks.is_empty() {
        sinks.push("no output".into());
    }
    // The plan's output format has the defaults resolved (e.g. the IF frequency).
    let form = match plan.output.format {
        OutputFormat::Real { if_hz } => format!("real IF, DC carrier at {:.1} kHz", if_hz / 1e3),
        OutputFormat::Iq { offset_hz, swap } => format!(
            "I/Q{}, DC carrier at {:+.1} kHz",
            if swap { " (swapped)" } else { "" },
            offset_hz / 1e3
        ),
    };
    format!(
        "{} — {form}, {:.0} dBFS RMS",
        sinks.join(" and "),
        o.level_dbfs
    )
}

/// DC carrier and occupied band (lowest carrier − ½ spacing … highest + ½ spacing) of
/// the transmitted signal, Hz, in the spectrum of the output channels as a receiver
/// expecting I on the left sees it: a swapped I/Q output appears mirrored.
pub fn signal_band(layout: ChannelLayout, format: OutputFormat) -> (f64, (f64, f64)) {
    let (kmin, kmax) = layout.carrier_range();
    let df = layout.mode.carrier_spacing();
    let lo = f64::from(kmin) * df - df / 2.0;
    let hi = f64::from(kmax) * df + df / 2.0;
    match format {
        OutputFormat::Real { if_hz } => (if_hz, (if_hz + lo, if_hz + hi)),
        OutputFormat::Iq {
            offset_hz,
            swap: false,
        } => (offset_hz, (offset_hz + lo, offset_hz + hi)),
        OutputFormat::Iq {
            offset_hz,
            swap: true,
        } => (-offset_hz, (-offset_hz - hi, -offset_hz - lo)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdrm_core::params::{RobustnessMode, SpectrumOccupancy};

    /// A fresh example directory per test (tests run in parallel threads).
    fn example_dir_for_tests(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("decdrm-gui-tx-{test}-{}", std::process::id()));
        materialize_example(&dir).unwrap();
        dir
    }

    #[test]
    fn the_example_is_valid() {
        let dir = example_dir_for_tests("valid");
        let (cfg, plan) = check(EXAMPLE_STATION, &dir, &Overrides::default()).unwrap();
        assert_eq!(cfg.services.len(), 2);
        assert!(cfg.output.file.is_some(), "the example writes a file");
        let out = describe_output(&cfg, &plan);
        assert!(
            out.contains("drm_station.wav") && out.contains("real IF"),
            "{out}"
        );
        assert!(!inputs_finite(&cfg), "a test tone never ends");
        assert!(
            start_check(&cfg, None, true).is_err(),
            "file output without an end"
        );
        assert!(start_check(&cfg, frames_for(10.0), true).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overrides_replace_the_output() {
        let dir = example_dir_for_tests("overrides");
        let file = Overrides {
            output: TxOutput::File,
            file: Some(dir.join("x.wav")),
            device: None,
        };
        let cfg = prepare(EXAMPLE_STATION, &dir, &file).unwrap();
        assert_eq!(
            cfg.output.file.as_deref(),
            Some(dir.join("x.wav").as_path())
        );
        assert!(cfg.output.device.is_none());

        let no_file = Overrides { file: None, ..file };
        assert!(prepare(EXAMPLE_STATION, &dir, &no_file).is_err());

        let device = Overrides {
            output: TxOutput::Device,
            file: None,
            device: None,
        };
        let cfg = prepare(EXAMPLE_STATION, &dir, &device).unwrap();
        assert_eq!(cfg.output.device.as_deref(), Some("default"));
        assert!(cfg.output.file.is_none());
        assert!(
            start_check(&cfg, None, true).is_ok(),
            "a sound card needs no end"
        );
        assert!(
            start_check(&cfg, None, false).is_err(),
            "--no-audio forbids it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn problems_are_listed() {
        let dir = example_dir_for_tests("problems");
        let broken = EXAMPLE_STATION.replace("occupancy = 3", "occupancy = 9");
        let problems = check(&broken, &dir, &Overrides::default()).unwrap_err();
        assert!(
            problems.list.iter().any(|p| p.contains("occupancy")),
            "{problems:?}"
        );
        assert_eq!(
            problems.location, None,
            "a semantic problem has no position"
        );
        let parse = check(
            "[channel]\nmode = \"B\"\nmode = \"A\"\n",
            &dir,
            &Overrides::default(),
        )
        .unwrap_err();
        assert_eq!(
            parse.list.len(),
            1,
            "a parse error is one message: {parse:?}"
        );
        assert_eq!(
            parse.location.map(|(line, _)| line),
            Some(3),
            "the duplicate key's line"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn line_starts() {
        let text = "a\nbé\n\nc";
        assert_eq!(line_start_char(text, 1), 0);
        assert_eq!(line_start_char(text, 2), 2);
        assert_eq!(line_start_char(text, 3), 5, "characters, not bytes");
        assert_eq!(line_start_char(text, 4), 6);
        assert_eq!(line_start_char(text, 9), 7, "past the end: the end");
    }

    #[test]
    fn finite_inputs() {
        let mut cfg = StationConfig::from_toml_str(EXAMPLE_STATION).unwrap();
        cfg.output.device = None;
        let audio = cfg.services[0].audio.as_mut().unwrap();
        audio.input.tone_hz = None;
        audio.input.file = Some("programme.flac".into());
        audio.input.looped = true;
        assert!(!inputs_finite(&cfg));
        cfg.services[0].audio.as_mut().unwrap().input.looped = false;
        assert!(inputs_finite(&cfg));
        assert!(start_check(&cfg, None, true).is_ok());
        cfg.services.retain(|s| s.audio.is_none());
        assert!(!inputs_finite(&cfg), "data-only stations never end");
        cfg.output.file = None;
        assert!(
            start_check(&cfg, frames_for(1.0), true).is_err(),
            "no output at all"
        );
    }

    #[test]
    fn durations_become_frames() {
        assert_eq!(frames_for(0.4), Some(1));
        assert_eq!(frames_for(1.0), Some(3));
        assert_eq!(frames_for(60.0), Some(150));
        assert_eq!(frames_for(0.0), None);
        assert_eq!(frames_for(f64::NAN), None);
    }

    #[test]
    fn transmitted_band() {
        let layout = ChannelLayout::new(RobustnessMode::B, SpectrumOccupancy::SO_3).unwrap();
        let half = 103.5 * 46.875;
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        let (dc, (lo, hi)) = signal_band(layout, OutputFormat::Real { if_hz: 12_000.0 });
        assert!(dc == 12_000.0 && close(lo, 12_000.0 - half) && close(hi, 12_000.0 + half));
        let (dc, (lo, hi)) = signal_band(
            layout,
            OutputFormat::Iq {
                offset_hz: 5_000.0,
                swap: false,
            },
        );
        assert!(dc == 5_000.0 && close(lo, 5_000.0 - half) && close(hi, 5_000.0 + half));
        let (dc, (lo, hi)) = signal_band(
            layout,
            OutputFormat::Iq {
                offset_hz: 5_000.0,
                swap: true,
            },
        );
        assert!(dc == -5_000.0 && close(lo, -5_000.0 - half) && close(hi, -5_000.0 + half));
        // A 5 kHz (SO 1) channel lies entirely above its DC carrier.
        let so1 = ChannelLayout::new(RobustnessMode::B, SpectrumOccupancy::SO_1).unwrap();
        let (_, (lo, _)) = signal_band(so1, OutputFormat::Real { if_hz: 12_000.0 });
        assert!(lo > 12_000.0);
    }
}
