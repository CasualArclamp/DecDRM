//! Signal sinks: a WAV/FLAC file and a sound card.
//!
//! The output stage delivers 48 kHz interleaved `f32` (one channel real, two I/Q). The
//! file takes it as it is; the sound card may run at another rate (resampled) or with
//! more channels (a real signal goes to every channel, I/Q to the first two).

use crate::config::{OutputSettings, SampleFormat, StationConfig};
use crate::error::{Result, StationError};
use crate::station::StopHandle;
use decdrm_io::{AudioFormat, Container, Encoding, FileWriter, OutputOptions, OutputStream, Resampler, ResamplerQuality};
use std::time::{Duration, Instant};

/// How long one wait for room on the sound card lasts before the stop flag and the
/// stall timeout are checked again.
const WAIT_SLICE: Duration = Duration::from_millis(50);

/// The working sample rate of the transmitter.
const RATE: u32 = decdrm_core::params::SAMPLE_RATE;

/// Create the output file of `cfg`, if any.
pub(crate) fn open_file(cfg: &StationConfig, channels: usize) -> Result<Option<FileWriter>> {
    let Some(f) = &cfg.output.file else { return Ok(None) };
    let path = cfg.resolve(f);
    let container = Container::from_path(&path)
        .ok_or_else(|| StationError::config(format!("output: {} must end in .wav or .flac", path.display())))?;
    let encoding = match cfg.output.sample_format {
        SampleFormat::Int16 => Encoding::Int16,
        SampleFormat::Int24 => Encoding::Int24,
        SampleFormat::Float32 => Encoding::Float32,
    };
    FileWriter::create(&path, AudioFormat::new(RATE, channels), container, encoding).map(Some).map_err(StationError::Output)
}

/// A sound-card output.
pub(crate) struct DeviceSink {
    stream: OutputStream,
    channels: usize,
    resampler: Option<Resampler>,
    resampled: Vec<f32>,
    mapped: Vec<f32>,
    /// A device that takes no samples for this long is considered stalled: twice its
    /// buffer (a healthy one frees room within one buffer length) plus a second.
    stall_timeout: Duration,
}

impl DeviceSink {
    /// Open the device of `out` for a signal of `channels` channels at 48 kHz.
    pub fn open(out: &OutputSettings, channels: usize) -> Result<Option<Self>> {
        let Some(name) = &out.device else { return Ok(None) };
        let buffer = Duration::from_millis(u64::from(out.device_buffer_ms.clamp(100, 5000)));
        let opts = OutputOptions {
            device: (!name.eq_ignore_ascii_case("default")).then(|| name.clone()),
            sample_rate: Some(RATE),
            channels: Some(channels),
            buffer: buffer * 2,
            start_threshold: buffer,
        };
        let stream = OutputStream::open(&opts).map_err(StationError::Output)?;
        let fmt = stream.format();
        if fmt.channels < channels {
            return Err(StationError::config(format!(
                "output: sound card \"{}\" has {} channel(s), I/Q output needs 2",
                stream.device_name(),
                fmt.channels
            )));
        }
        let resampler = (fmt.sample_rate != RATE)
            .then(|| Resampler::new(RATE, fmt.sample_rate, channels, ResamplerQuality::High))
            .transpose()
            .map_err(StationError::Output)?;
        Ok(Some(Self {
            stream,
            channels,
            resampler,
            resampled: Vec::new(),
            mapped: Vec::new(),
            stall_timeout: buffer * 2 + Duration::from_secs(1),
        }))
    }

    /// Name of the device.
    pub fn name(&self) -> &str {
        self.stream.device_name()
    }

    /// Queue interleaved 48 kHz samples, waiting while the device's buffer is full
    /// (this paces the station to real time). The wait ends early when `stop` is set
    /// (the rest is dropped), and fails if the device takes nothing for
    /// `stall_timeout`.
    pub fn write(&mut self, samples: &[f32], stop: &StopHandle) -> Result<()> {
        let data: &[f32] = match self.resampler.as_mut() {
            Some(r) => {
                self.resampled.clear();
                r.process_into(samples, &mut self.resampled);
                &self.resampled
            }
            None => samples,
        };
        let dev_ch = self.stream.format().channels;
        let frames: &[f32] = if dev_ch == self.channels {
            data
        } else {
            self.mapped.clear();
            for frame in data.chunks_exact(self.channels) {
                for c in 0..dev_ch {
                    let v = if self.channels == 1 { frame[0] } else { frame.get(c).copied().unwrap_or(0.0) };
                    self.mapped.push(v);
                }
            }
            &self.mapped
        };
        // Wait in short slices, so that a stop request or a stalled device (e.g. a
        // virtual cable whose reader went away) is noticed within ~50 ms instead of
        // blocking for many seconds.
        let mut done = 0;
        let mut progress_at = Instant::now();
        while done < frames.len() {
            if stop.is_stopped() {
                return Ok(());
            }
            let n = self.stream.write_blocking(&frames[done..], WAIT_SLICE).map_err(StationError::Output)?;
            if n > 0 {
                done += n * dev_ch;
                progress_at = Instant::now();
            } else if progress_at.elapsed() >= self.stall_timeout {
                return Err(StationError::DeviceStalled {
                    device: self.stream.device_name().to_string(),
                    seconds: progress_at.elapsed().as_secs_f64(),
                });
            }
        }
        Ok(())
    }

    /// Signal queued on the device, not yet played.
    pub fn queued(&self) -> Duration {
        self.stream.buffered()
    }

    /// Underruns of the device so far.
    pub fn underruns(&self) -> u64 {
        self.stream.stats().underruns
    }

    /// Play out what is queued: as long as that takes at the device's rate plus half a
    /// second, not at all once `stop` is set (a stalled device therefore delays the
    /// end by at most its buffer length and the margin).
    pub fn drain(&mut self, stop: &StopHandle) {
        let deadline = Instant::now() + self.queued() + Duration::from_millis(500);
        while !stop.is_stopped() && Instant::now() < deadline {
            if self.stream.drain(WAIT_SLICE) {
                break;
            }
        }
    }
}
