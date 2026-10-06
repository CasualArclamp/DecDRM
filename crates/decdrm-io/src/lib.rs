//! Audio file and sound-card I/O plus resampling for DecDRM.
//!
//! This crate is the boundary between DecDRM's signal processing (which works on a stream of
//! `f32` samples at the 48 kHz [`WORKING_RATE`]) and the outside world:
//!
//! | Need | Type |
//! |---|---|
//! | Read WAV/FLAC recordings (any rate, any bit depth), streaming | [`FileReader`] |
//! | Write WAV (16/24-bit int, 32-bit float) or FLAC (8/16/24-bit) | [`FileWriter`] |
//! | Convert between sample rates, with fine ratio trimming for drift | [`Resampler`], [`To48k`] |
//! | Enumerate sound cards | [`list_devices`], [`DeviceInfo`] |
//! | Capture from a sound card / virtual audio cable | [`InputStream`] |
//! | Play raw samples to a sound card | [`OutputStream`] |
//! | Play decoded audio with clock-drift compensation | [`AudioPlayer`] |
//! | Smooth the SBR band of decoded audio (encoders that switch it on and off) | [`HighBandSmoother`] |
//!
//! # Sample conventions
//!
//! All sample data crossing this API is `f32`, **interleaved** (`L0 R0 L1 R1 ...`), and
//! normalised so that integer full scale maps to `[-1.0, 1.0)`: a 16-bit sample `s` becomes
//! `s / 32768`, an unsigned 8-bit sample `u` becomes `(u - 128) / 128`, and so on. A *frame* is
//! one sample per channel, so a buffer of `n` frames holds `n * channels` values. For the
//! receiver, a real IF signal is one channel (mono, or one channel picked out of a stereo pair
//! with [`channels::extract_channel`]); complex I/Q is two channels with `L = I`, `R = Q`.
//!
//! # Threading model (sound cards)
//!
//! Sound-card callbacks run on a high-priority thread owned by the audio driver. They must
//! never block: no locks, no allocation, no file I/O — any of those can stall for longer than
//! the few milliseconds the driver gives us, which is heard as a click or a dropout. This
//! crate therefore connects each callback to your code through a lock-free single-producer /
//! single-consumer ring buffer ([`rtrb`]) plus a handful of atomic counters for statistics.
//! See the [`stream`] module documentation for the details.
//!
//! # Errors
//!
//! Every fallible operation returns [`Result`], whose error type is the [`Error`] enum. The
//! crate does not panic on bad input (corrupt files, odd sample counts, unplugged devices).
//!
//! # Platform notes
//!
//! Sound-card access uses cpal's default host: WASAPI on Windows and ALSA on Linux (cpal's
//! PipeWire/PulseAudio hosts are not enabled, so only `libasound2-dev` is needed to build;
//! on a PipeWire or PulseAudio desktop the ALSA `default` device is routed through them).

#![warn(missing_docs)]

pub mod channels;
mod device;
mod error;
mod file_reader;
mod file_writer;
mod player;
mod resample;
pub mod sbr_smooth;
pub mod stream;

use std::fmt;
use std::time::Duration;

pub use device::{
    default_device_name, list_devices, list_input_devices, list_output_devices, ConfigRange,
    DeviceInfo,
};
pub use error::{Error, Result};
pub use file_reader::FileReader;
pub use file_writer::{Container, Encoding, FileWriter};
pub use player::{AudioPlayer, PlayerOptions, PlayerStatus};
pub use resample::{resample, Resampler, ResamplerQuality, To48k};
pub use sbr_smooth::HighBandSmoother;
pub use stream::{
    InputOptions, InputStats, InputStream, OutputOptions, OutputState, OutputStats, OutputStream,
};

/// The receiver's working sample rate in Hz (Dream's `SOUNDCRD_SAMPLE_RATE`).
///
/// At this rate the useful OFDM symbol lengths are 1152/1024/704/448 samples for robustness
/// modes A/B/C/D.
pub const WORKING_RATE: u32 = 48_000;

/// Sample rate and channel count of an interleaved `f32` stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AudioFormat {
    /// Frames per second.
    pub sample_rate: u32,
    /// Samples per frame (1 = mono / real, 2 = stereo / I-Q, ...).
    pub channels: usize,
}

impl AudioFormat {
    /// Creates a format description.
    pub const fn new(sample_rate: u32, channels: usize) -> Self {
        AudioFormat { sample_rate, channels }
    }

    /// Duration of `frames` frames at this sample rate.
    pub fn frames_to_duration(&self, frames: u64) -> Duration {
        if self.sample_rate == 0 {
            return Duration::ZERO;
        }
        let rate = u64::from(self.sample_rate);
        let secs = frames / rate;
        let rem = frames % rate;
        // `rem < rate <= u32::MAX`, so `rem * 1e9` fits comfortably in u64.
        Duration::new(secs, ((rem * 1_000_000_000) / rate) as u32)
    }

    /// Number of whole frames in `duration` at this sample rate (rounded to nearest).
    pub fn duration_to_frames(&self, duration: Duration) -> u64 {
        (duration.as_secs_f64() * f64::from(self.sample_rate)).round() as u64
    }

    /// Checks that both fields are non-zero (and that the channel count is sane).
    pub(crate) fn validate(&self) -> Result<()> {
        if self.sample_rate == 0 {
            return Err(Error::invalid("sample rate must be > 0"));
        }
        if self.channels == 0 || self.channels > 64 {
            return Err(Error::invalid(format!(
                "channel count must be 1..=64, got {}",
                self.channels
            )));
        }
        Ok(())
    }
}

impl fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} Hz, {} ch", self.sample_rate, self.channels)
    }
}

/// Whether a sound-card device or stream captures (input) or plays (output).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Capture (microphone, line in, virtual cable output).
    Input,
    /// Playback (speakers, line out, virtual cable input).
    Output,
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Direction::Input => "input",
            Direction::Output => "output",
        })
    }
}

/// Checks that an interleaved slice holds a whole number of frames.
pub(crate) fn check_whole_frames(samples: &[f32], channels: usize) -> Result<()> {
    if channels == 0 {
        return Err(Error::invalid("channel count must be > 0"));
    }
    if !samples.len().is_multiple_of(channels) {
        return Err(Error::invalid(format!(
            "{} samples is not a whole number of {}-channel frames",
            samples.len(),
            channels
        )));
    }
    Ok(())
}

// Compile-time proof that every stateful type can be moved to another thread (e.g. a reader
// thread, a decoder thread owning the player). `Send` is the marker trait for "safe to
// transfer ownership across threads"; this block fails to compile if one of them is not.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<FileReader>();
    assert_send::<FileWriter>();
    assert_send::<Resampler>();
    assert_send::<To48k>();
    assert_send::<InputStream>();
    assert_send::<OutputStream>();
    assert_send::<AudioPlayer>();
    assert_send::<Error>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_durations() {
        let f = AudioFormat::new(48_000, 2);
        assert_eq!(f.frames_to_duration(48_000), Duration::from_secs(1));
        assert_eq!(f.frames_to_duration(24_000), Duration::from_millis(500));
        assert_eq!(f.duration_to_frames(Duration::from_millis(250)), 12_000);
        assert_eq!(AudioFormat::new(0, 1).frames_to_duration(10), Duration::ZERO);
        assert!(AudioFormat::new(0, 1).validate().is_err());
        assert!(AudioFormat::new(8000, 0).validate().is_err());
        assert_eq!(f.to_string(), "48000 Hz, 2 ch");
    }

    #[test]
    fn whole_frames() {
        assert!(check_whole_frames(&[0.0; 4], 2).is_ok());
        assert!(check_whole_frames(&[0.0; 3], 2).is_err());
        assert!(check_whole_frames(&[0.0; 3], 0).is_err());
    }
}
