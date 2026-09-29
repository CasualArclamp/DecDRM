//! Persistent GUI settings: the last source, input format, audio and view choices,
//! stored as TOML in the per-user configuration directory:
//! `%APPDATA%\decdrm\gui.toml` on Windows, `$XDG_CONFIG_HOME/decdrm/gui.toml` (or
//! `~/.config/decdrm/gui.toml`) elsewhere.
//!
//! Rust notes: `#[derive(Serialize, Deserialize)]` makes serde generate the TOML
//! (de)serialisation code at compile time. `#[serde(default)]` on the struct fills
//! every field missing from the file with its `Default` value, so settings files
//! written by older versions still load after new fields are added (and unknown fields
//! from newer versions are ignored).

use decdrm_engine::{EngineConfig, InputFormat, InputSpec, RealChannel, ReceiverConfig};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Where the signal comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    /// A WAV/FLAC recording.
    #[default]
    File,
    /// A sound-card input (e.g. a virtual audio cable fed by a web SDR).
    Device,
}

/// How the input samples represent the signal (maps to [`InputFormat`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SignalFormat {
    /// Real IF / audio signal on one channel (or a mix of two).
    #[default]
    Real,
    /// I/Q, I on the left channel.
    Iq,
    /// I/Q, I on the right channel (mirrors the spectrum).
    IqSwapped,
}

impl SignalFormat {
    pub const ALL: [SignalFormat; 3] = [Self::Real, Self::Iq, Self::IqSwapped];

    pub fn label(self) -> &'static str {
        match self {
            Self::Real => "Real (IF)",
            Self::Iq => "I/Q",
            Self::IqSwapped => "I/Q swapped",
        }
    }

    pub fn is_iq(self) -> bool {
        !matches!(self, Self::Real)
    }
}

/// Which channel of a real input carries the signal (maps to [`RealChannel`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChannelChoice {
    Left,
    Right,
    /// (L + R) / 2 — also right for mono sources.
    #[default]
    Mix,
    /// (L − R) / 2.
    Diff,
}

impl ChannelChoice {
    pub const ALL: [ChannelChoice; 4] = [Self::Left, Self::Right, Self::Mix, Self::Diff];

    pub fn label(self) -> &'static str {
        match self {
            Self::Left => "Left",
            Self::Right => "Right",
            Self::Mix => "L+R",
            Self::Diff => "L−R",
        }
    }

    pub fn real_channel(self) -> RealChannel {
        match self {
            Self::Left => RealChannel::Left,
            Self::Right => RealChannel::Right,
            Self::Mix => RealChannel::Mix,
            Self::Diff => RealChannel::Diff,
        }
    }
}

/// Colour theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeChoice {
    /// Follow the operating system.
    #[default]
    System,
    Dark,
    Light,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [Self::System, Self::Dark, Self::Light];

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System theme",
            Self::Dark => "Dark",
            Self::Light => "Light",
        }
    }

    pub fn preference(self) -> eframe::egui::ThemePreference {
        use eframe::egui::ThemePreference;
        match self {
            Self::System => ThemePreference::System,
            Self::Dark => ThemePreference::Dark,
            Self::Light => ThemePreference::Light,
        }
    }
}

/// Tab of the plot area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlotTab {
    #[default]
    Overview,
    Spectrum,
    Constellations,
    Channel,
    Impulse,
    Snr,
}

impl PlotTab {
    pub const ALL: [PlotTab; 6] = [
        Self::Overview,
        Self::Spectrum,
        Self::Constellations,
        Self::Channel,
        Self::Impulse,
        Self::Snr,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Spectrum => "Spectrum",
            Self::Constellations => "Constellations",
            Self::Channel => "Channel",
            Self::Impulse => "Impulse response",
            Self::Snr => "SNR per carrier",
        }
    }
}

/// Tab of the data-service area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DataTab {
    #[default]
    Slideshow,
    Journaline,
    Info,
}

impl DataTab {
    pub const ALL: [DataTab; 3] = [Self::Slideshow, Self::Journaline, Self::Info];

    pub fn label(self) -> &'static str {
        match self {
            Self::Slideshow => "Slideshow",
            Self::Journaline => "Journaline",
            Self::Info => "Data info",
        }
    }
}

/// Everything the GUI remembers between runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub source: SourceKind,
    /// Last recording.
    pub file: Option<PathBuf>,
    /// Sound-card input by name (`None` = system default).
    pub input_device: Option<String>,
    pub format: SignalFormat,
    pub real_channel: ChannelChoice,
    /// Mirror the spectrum.
    pub flip: bool,
    /// Also accept spectrally inverted signals during acquisition.
    pub auto_flip: bool,
    /// Pace recordings to real time (otherwise decode as fast as possible).
    pub realtime: bool,
    pub play_audio: bool,
    /// Sound-card output by name (`None` = system default).
    pub output_device: Option<String>,
    pub theme: ThemeChoice,
    pub plot_tab: PlotTab,
    pub data_tab: DataTab,
    pub show_log: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            source: SourceKind::File,
            file: None,
            input_device: None,
            format: SignalFormat::Real,
            real_channel: ChannelChoice::Mix,
            flip: false,
            auto_flip: true,
            realtime: true,
            play_audio: true,
            output_device: None,
            theme: ThemeChoice::System,
            plot_tab: PlotTab::Overview,
            data_tab: DataTab::Slideshow,
            show_log: true,
        }
    }
}

impl Settings {
    /// The receiver's input format.
    pub fn input_format(&self) -> InputFormat {
        match self.format {
            SignalFormat::Real => InputFormat::Real(self.real_channel.real_channel()),
            SignalFormat::Iq => InputFormat::Iq { swap: false },
            SignalFormat::IqSwapped => InputFormat::Iq { swap: true },
        }
    }

    /// Engine configuration for the selected source, or why none can be built.
    pub fn engine_config(&self) -> Result<EngineConfig, String> {
        let input = match self.source {
            SourceKind::File => {
                let path = self
                    .file
                    .clone()
                    .ok_or("no recording selected — use “Open…” first")?;
                InputSpec::File {
                    path,
                    realtime: self.realtime,
                }
            }
            SourceKind::Device => InputSpec::Device {
                name: self.input_device.clone(),
                // I/Q needs both channels; a real signal uses the device default.
                channels: self.format.is_iq().then_some(2),
            },
        };
        // Start from the engine's own constructor and overwrite fields, rather than
        // writing a struct literal: a literal must name every field, so it would stop
        // compiling whenever the engine gains a new option.
        let mut cfg = EngineConfig::file(PathBuf::new());
        cfg.input = input;
        cfg.receiver = ReceiverConfig {
            input: self.input_format(),
            flip: self.flip,
            auto_flip: self.auto_flip,
            ..ReceiverConfig::default()
        };
        cfg.play_audio = self.play_audio;
        cfg.output_device = self.output_device.clone();
        Ok(cfg)
    }

    /// One-line description of the selected source for the log.
    pub fn source_label(&self) -> String {
        let fmt = match self.format {
            SignalFormat::Real => format!("real, {}", self.real_channel.label()),
            f => f.label().to_string(),
        };
        match self.source {
            SourceKind::File => {
                let name = self
                    .file
                    .as_deref()
                    .and_then(Path::file_name)
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "(no file)".into());
                format!(
                    "{name} ({fmt}{})",
                    if self.realtime { ", real time" } else { "" }
                )
            }
            SourceKind::Device => {
                format!(
                    "{} ({fmt})",
                    self.input_device.as_deref().unwrap_or("default input")
                )
            }
        }
    }

    /// Select a newly opened recording and guess its signal format from the name
    /// (DecDRM/Dream recordings of I/Q signals carry an `IQ` token, e.g.
    /// `Test_Mode_B_10kHz_IQ_Pos_26dB_SNR.flac`). Returns the guessed format.
    pub fn open_file(&mut self, path: PathBuf) -> SignalFormat {
        let iq = path
            .file_stem()
            .is_some_and(|s| name_has_iq_token(&s.to_string_lossy()));
        self.format = match (iq, self.format) {
            (true, SignalFormat::Real) => SignalFormat::Iq,
            (true, f) => f,
            (false, _) => SignalFormat::Real,
        };
        self.file = Some(path);
        self.source = SourceKind::File;
        self.format
    }
}

/// `true` if `name` contains `IQ` as a separate token (split at anything that is not a
/// letter or digit), case-insensitively.
pub fn name_has_iq_token(name: &str) -> bool {
    name.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|t| t.eq_ignore_ascii_case("iq"))
}

/// Default settings-file location (see the module docs).
pub fn default_config_path() -> Option<PathBuf> {
    config_path_for(cfg!(windows), |key| std::env::var_os(key))
}

/// Settings-file location for a platform, with the environment passed in as a lookup
/// function so tests do not depend on the real environment. (`impl Fn(&str) -> …` is a
/// generic parameter: any closure taking a `&str` works.)
fn config_path_for(windows: bool, env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let non_empty = |key: &str| env(key).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if windows {
        non_empty("APPDATA")?
    } else {
        non_empty("XDG_CONFIG_HOME").or_else(|| non_empty("HOME").map(|h| h.join(".config")))?
    };
    Some(base.join("decdrm").join("gui.toml"))
}

/// Parse a settings file's text.
pub fn parse(text: &str) -> Result<Settings, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

/// Serialise settings to TOML text.
pub fn to_toml(settings: &Settings) -> Result<String, String> {
    toml::to_string_pretty(settings).map_err(|e| e.to_string())
}

/// Loads and saves [`Settings`], writing only when they changed.
pub struct SettingsStore {
    path: Option<PathBuf>,
    saved: Option<Settings>,
}

impl SettingsStore {
    /// Load from `path` (or the default location). A missing file gives the defaults;
    /// an unreadable one gives the defaults plus a warning for the log.
    pub fn load(path: Option<PathBuf>) -> (Self, Settings, Option<String>) {
        let path = path.or_else(default_config_path);
        let Some(p) = path.clone() else {
            return (
                Self { path, saved: None },
                Settings::default(),
                Some("no settings directory found".into()),
            );
        };
        match std::fs::read_to_string(&p) {
            Ok(text) => match parse(&text) {
                Ok(s) => (
                    Self {
                        path,
                        saved: Some(s.clone()),
                    },
                    s,
                    None,
                ),
                Err(e) => {
                    let warn = format!("ignoring unreadable settings file {}: {e}", p.display());
                    (Self { path, saved: None }, Settings::default(), Some(warn))
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                (Self { path, saved: None }, Settings::default(), None)
            }
            Err(e) => {
                let warn = format!("cannot read settings file {}: {e}", p.display());
                (Self { path, saved: None }, Settings::default(), Some(warn))
            }
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// `true` if `settings` differ from what was last loaded or saved.
    pub fn is_dirty(&self, settings: &Settings) -> bool {
        self.saved.as_ref() != Some(settings)
    }

    /// Write `settings` if they changed. The file is written to a temporary name and
    /// then renamed over the old one, so a crash cannot leave a truncated file.
    pub fn save_if_changed(&mut self, settings: &Settings) -> Result<(), String> {
        if !self.is_dirty(settings) {
            return Ok(());
        }
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        // Remember the attempt even if it fails, so a read-only directory does not
        // cause a retry (and an error message) on every frame.
        self.saved = Some(settings.clone());
        let text = to_toml(settings)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("replacing {}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_round_trip() {
        let s = Settings {
            source: SourceKind::Device,
            file: Some(PathBuf::from("samples/DW_ModeB_10kHz.flac")),
            input_device: Some("CABLE-A Output".into()),
            format: SignalFormat::IqSwapped,
            real_channel: ChannelChoice::Diff,
            flip: true,
            auto_flip: false,
            realtime: false,
            play_audio: false,
            output_device: Some("Speakers".into()),
            theme: ThemeChoice::Light,
            plot_tab: PlotTab::Impulse,
            data_tab: DataTab::Journaline,
            show_log: false,
        };
        let text = to_toml(&s).unwrap();
        assert!(text.contains("format = \"iq-swapped\""), "{text}");
        assert_eq!(parse(&text).unwrap(), s);
    }

    #[test]
    fn missing_and_unknown_fields_are_tolerated() {
        let s = parse("flip = true\nsome_future_option = 3\n").unwrap();
        assert!(s.flip);
        assert_eq!(s.format, SignalFormat::Real);
        assert!(s.auto_flip, "defaults fill missing fields");
        assert!(parse("flip = \"yes\"").is_err());
    }

    #[test]
    fn engine_config_mapping() {
        let mut s = Settings {
            file: Some("a.flac".into()),
            ..Settings::default()
        };
        let cfg = s.engine_config().unwrap();
        assert!(matches!(cfg.input, InputSpec::File { realtime: true, .. }));
        assert_eq!(cfg.receiver.input, InputFormat::Real(RealChannel::Mix));
        assert!(cfg.receiver.auto_flip && !cfg.receiver.flip);

        s.source = SourceKind::Device;
        s.format = SignalFormat::IqSwapped;
        s.flip = true;
        s.input_device = Some("CABLE-B Output".into());
        let cfg = s.engine_config().unwrap();
        match cfg.input {
            InputSpec::Device { name, channels } => {
                assert_eq!(name.as_deref(), Some("CABLE-B Output"));
                assert_eq!(channels, Some(2));
            }
            other => panic!("unexpected input {other:?}"),
        }
        assert_eq!(cfg.receiver.input, InputFormat::Iq { swap: true });
        assert!(cfg.receiver.flip);

        s.format = SignalFormat::Real;
        assert!(matches!(
            s.engine_config().unwrap().input,
            InputSpec::Device { channels: None, .. }
        ));

        let no_file = Settings::default();
        assert!(no_file.engine_config().is_err());
    }

    #[test]
    fn iq_guess_from_file_name() {
        assert!(name_has_iq_token("Test_Mode_B_10kHz_IQ_Pos_26dB_SNR"));
        assert!(name_has_iq_token("rec-iq"));
        assert!(!name_has_iq_token("DW_ModeB_10kHz"));
        assert!(!name_has_iq_token("LIQUID_ModeA")); // only whole tokens count

        let mut s = Settings::default();
        assert_eq!(
            s.open_file("x/Test_Mode_B_10kHz_IQ_Pos.flac".into()),
            SignalFormat::Iq
        );
        s.format = SignalFormat::IqSwapped;
        assert_eq!(
            s.open_file("y/Other_IQ.flac".into()),
            SignalFormat::IqSwapped,
            "keeps the I/Q flavour"
        );
        assert_eq!(
            s.open_file("z/DW_ModeB_10kHz.flac".into()),
            SignalFormat::Real
        );
        assert_eq!(s.source, SourceKind::File);
    }

    #[test]
    fn config_paths() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == k)
                    .map(|(_, v)| OsString::from(*v))
            }
        };
        assert_eq!(
            config_path_for(true, env(&[("APPDATA", r"C:\Users\me\AppData\Roaming")])),
            Some(
                PathBuf::from(r"C:\Users\me\AppData\Roaming")
                    .join("decdrm")
                    .join("gui.toml")
            )
        );
        assert_eq!(config_path_for(true, env(&[])), None);
        assert_eq!(
            config_path_for(
                false,
                env(&[("XDG_CONFIG_HOME", "/cfg"), ("HOME", "/home/me")])
            ),
            Some(PathBuf::from("/cfg/decdrm/gui.toml"))
        );
        assert_eq!(
            config_path_for(false, env(&[("XDG_CONFIG_HOME", ""), ("HOME", "/home/me")])),
            Some(PathBuf::from("/home/me/.config/decdrm/gui.toml"))
        );
    }

    #[test]
    fn store_saves_only_changes() {
        let dir = std::env::temp_dir().join(format!("decdrm-gui-test-{}", std::process::id()));
        let path = dir.join("gui.toml");
        let _ = std::fs::remove_file(&path);
        let (mut store, mut s, warn) = SettingsStore::load(Some(path.clone()));
        assert!(warn.is_none());
        assert!(store.is_dirty(&s), "nothing saved yet");
        s.flip = true;
        store.save_if_changed(&s).unwrap();
        assert!(!store.is_dirty(&s));
        let (_, loaded, _) = SettingsStore::load(Some(path.clone()));
        assert_eq!(loaded, s);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
