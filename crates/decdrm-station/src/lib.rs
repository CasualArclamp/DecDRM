//! # decdrm-station — the DecDRM transmitter application layer
//!
//! A *station* is a complete DRM30 transmission described by one configuration file
//! (`station.toml`, see `examples/station.toml`): the channel (robustness mode,
//! bandwidth, MSC/SDC constellations, protection levels, interleaving), the outputs
//! (WAV/FLAC file and/or sound card, real IF or I/Q), and up to four services — audio
//! services (AAC, HE-AAC, HE-AAC v2, xHE-AAC, Opus or — with the `dac` feature —
//! DecDRM's neural codec DAC, from a file, a sound card, an internet radio stream
//! ([`webstream`]) or a test tone, with text messages) and data services (MOT
//! slideshow, broadcast website,
//! Journaline, EPG, TPEG or raw data), whose applications may also ride along with an
//! audio service; plus alternative-frequency signalling ([`afs`]).
//!
//! ```text
//! StationConfig ──validate──▶ MultiplexPlan (streams, lengths, codec parameters)
//!        │
//!        ▼ Station::new
//!  audio input ─▶ FDK-AAC / libxaac / Opus / DAC ─▶ super frame + text ─────┐
//!  data carousels ─▶ packet mode ─────────────────────────────┼▶ MSC multiplex ─┐
//!  SDC entities ─▶ SDC scheduler (cycling) ────────────────────────────────────┼▶ Transmitter ─▶ OutputStage ─▶ file / sound card
//!  FAC service rotation (table 60) ────────────────────────────────────────────┘
//! ```
//!
//! * [`StationConfig`] — the serde model of the file, with lenient parsing of names.
//! * [`StationConfig::validate`] — checks everything (codec/rate combinations, SDC and
//!   MSC capacities, input files, data content) and returns the [`MultiplexPlan`]:
//!   data streams get their requested bit rates in whole packets, audio streams share
//!   what is left, and the encoders' bit rates follow from the stream lengths.
//! * [`Station`] — runs the chain frame by frame ([`Station::transmit_frame`]) and
//!   reports a [`StationStatus`] (frames, per-service bit rates, levels, clipping).
//!   Journaline page files are loaded again when they change while transmitting
//!   ([`Station::reload_journaline`]).
//!
//! The CLI's `decdrm tx station.toml` runs a station; the GUI can hold a [`Station`] on
//! a worker thread (it is `Send`).

#![forbid(unsafe_code)]

pub mod afs;
mod audio;
pub mod config;
mod data;
mod error;
mod fac;
pub mod mdi;
mod modulator;
pub mod plan;
mod output;
mod sdc;
mod station;
pub mod time;
pub mod webstream;

pub use afs::{AfsMultiplexSettings, AfsOtherSettings, AfsRegionSettings, AfsScheduleSettings, AfsSettings, OtherSystem};
pub use audio::AudioCounters;
pub use config::{
    AppKind, AppSettings, AudioInputSettings, AudioSettings, ChannelSettings, Codec, EpgProgramme, FacLanguage,
    JournalineFile, JournalinePage, MdiSettings, OutputSettings, Part, ProgrammeType, SampleFormat, ServiceSettings,
    SignalFormat, SimulateSettings, StationConfig, TimeSettings,
};
pub use modulator::ModulatorStatus;
pub use data::load_journaline;
pub use error::{ConfigProblems, Result, StationError};
pub use plan::{AppPlan, AudioPlan, MultiplexPlan, ServicePlan, StreamContent, StreamPlan};
pub use station::{AppStatus, AudioStatus, JournalineStatus, ServiceStatus, Station, StationStatus, StopHandle};
pub use webstream::{WebStreamState, WebStreamStatus};
