//! Sound-card enumeration and stream-configuration selection (cpal).

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{SampleFormat, SupportedStreamConfig, SupportedStreamConfigRange};

use crate::{AudioFormat, Direction, Error, Result};

/// One supported configuration range of a device, as reported by the driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigRange {
    /// Channel count.
    pub channels: usize,
    /// Lowest supported sample rate in Hz.
    pub min_sample_rate: u32,
    /// Highest supported sample rate in Hz.
    pub max_sample_rate: u32,
    /// Native sample type, e.g. `"f32"`, `"i16"`, `"i24"`. Streams always hand you `f32`.
    pub sample_format: String,
}

/// Description of a sound-card device.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    /// Human-readable name, e.g. `"CABLE Output (VB-Audio Virtual Cable)"`. Pass it (or any
    /// unique part of it) to [`InputOptions::device`](crate::InputOptions::device) /
    /// [`OutputOptions::device`](crate::OutputOptions::device).
    pub name: String,
    /// Stable backend identifier (survives reboots and renames; good for config files), if
    /// the backend provides one. Also accepted wherever a device name is.
    pub id: Option<String>,
    /// Audio API in use, e.g. `"WASAPI"` or `"ALSA"`.
    pub host: String,
    /// Whether this is an input or output device.
    pub direction: Direction,
    /// `true` for the system's default device of this direction.
    pub is_default: bool,
    /// Format the device prefers (shared-mode mix format on Windows), if it reports one.
    pub default_format: Option<AudioFormat>,
    /// All configuration ranges the device reports.
    pub configs: Vec<ConfigRange>,
}

impl DeviceInfo {
    /// Distinct sample rates covered by [`configs`](Self::configs), ascending. Continuous
    /// ranges contribute their end points plus any common rate inside them.
    pub fn sample_rates(&self) -> Vec<u32> {
        const COMMON: [u32; 8] = [8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100];
        const COMMON_HI: [u32; 4] = [48000, 88200, 96000, 192000];
        let mut rates: Vec<u32> = Vec::new();
        for c in &self.configs {
            rates.push(c.min_sample_rate);
            rates.push(c.max_sample_rate);
            rates.extend(
                COMMON
                    .iter()
                    .chain(COMMON_HI.iter())
                    .copied()
                    .filter(|r| (c.min_sample_rate..=c.max_sample_rate).contains(r)),
            );
        }
        rates.sort_unstable();
        rates.dedup();
        rates
    }

    /// Distinct channel counts offered, ascending.
    pub fn channel_counts(&self) -> Vec<usize> {
        let mut ch: Vec<usize> = self.configs.iter().map(|c| c.channels).collect();
        ch.sort_unstable();
        ch.dedup();
        ch
    }
}

/// Lists the input (capture) devices of the default audio host.
pub fn list_input_devices() -> Result<Vec<DeviceInfo>> {
    list_devices(Direction::Input)
}

/// Lists the output (playback) devices of the default audio host.
pub fn list_output_devices() -> Result<Vec<DeviceInfo>> {
    list_devices(Direction::Output)
}

/// Lists the devices of one direction on the default audio host (WASAPI on Windows, ALSA on
/// Linux). A machine without sound cards yields an empty list, not an error.
pub fn list_devices(direction: Direction) -> Result<Vec<DeviceInfo>> {
    let host = cpal::default_host();
    let host_name = host.id().name().to_string();
    // On WASAPI the default device is a special handle that follows the system default (and
    // is not `==` to the enumerated endpoint), so identify it by its resolved id, or name.
    let default = default_device(&host, direction);
    let default_id = default.as_ref().and_then(|d| d.id().ok());
    let default_name = default.as_ref().map(device_name);
    let devices = match direction {
        Direction::Input => host.input_devices()?.collect::<Vec<_>>(),
        Direction::Output => host.output_devices()?.collect::<Vec<_>>(),
    };
    Ok(devices
        .into_iter()
        .map(|dev| {
            let configs = supported_ranges(&dev, direction)
                .unwrap_or_default()
                .into_iter()
                .map(|r| ConfigRange {
                    channels: usize::from(r.channels()),
                    min_sample_rate: r.min_sample_rate(),
                    max_sample_rate: r.max_sample_rate(),
                    sample_format: r.sample_format().to_string(),
                })
                .collect();
            let id = dev.id().ok();
            let name = device_name(&dev);
            let is_default = match (&id, &default_id) {
                (Some(a), Some(b)) => a == b,
                _ => default_name.as_deref() == Some(name.as_str()),
            };
            DeviceInfo {
                name,
                id: id.map(|id| id.to_string()),
                host: host_name.clone(),
                direction,
                is_default,
                default_format: default_config(&dev, direction)
                    .ok()
                    .map(|c| AudioFormat::new(c.sample_rate(), usize::from(c.channels()))),
                configs,
            }
        })
        .collect())
}

/// Name of the system default device of `direction`, if there is one.
pub fn default_device_name(direction: Direction) -> Option<String> {
    default_device(&cpal::default_host(), direction).map(|d| device_name(&d))
}

fn default_device(host: &cpal::Host, direction: Direction) -> Option<cpal::Device> {
    match direction {
        Direction::Input => host.default_input_device(),
        Direction::Output => host.default_output_device(),
    }
}

pub(crate) fn device_name(dev: &cpal::Device) -> String {
    match dev.description() {
        Ok(desc) => desc.name().to_string(),
        Err(_) => dev
            .id()
            .map(|id| id.to_string())
            .unwrap_or_else(|_| "<unnamed device>".to_string()),
    }
}

fn supported_ranges(
    dev: &cpal::Device,
    direction: Direction,
) -> Result<Vec<SupportedStreamConfigRange>> {
    Ok(match direction {
        Direction::Input => dev.supported_input_configs()?.collect(),
        Direction::Output => dev.supported_output_configs()?.collect(),
    })
}

fn default_config(dev: &cpal::Device, direction: Direction) -> Result<SupportedStreamConfig> {
    Ok(match direction {
        Direction::Input => dev.default_input_config()?,
        Direction::Output => dev.default_output_config()?,
    })
}

/// Finds a device by name: `None` selects the default device; otherwise an exact name or id
/// match wins, then a unique case-insensitive substring of the name.
pub(crate) fn find_device(direction: Direction, name: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    let Some(wanted) = name.map(str::trim).filter(|n| !n.is_empty()) else {
        return default_device(&host, direction).ok_or(Error::NoDefaultDevice(direction));
    };
    let devices: Vec<cpal::Device> = match direction {
        Direction::Input => host.input_devices()?.collect(),
        Direction::Output => host.output_devices()?.collect(),
    };
    let mut named: Vec<(String, cpal::Device)> =
        devices.into_iter().map(|d| (device_name(&d), d)).collect();

    if let Some(i) = named.iter().position(|(n, d)| {
        n == wanted || d.id().map(|id| id.to_string() == wanted).unwrap_or(false)
    }) {
        return Ok(named.swap_remove(i).1);
    }
    let lower = wanted.to_lowercase();
    let hits: Vec<usize> = named
        .iter()
        .enumerate()
        .filter(|(_, (n, _))| n.to_lowercase().contains(&lower))
        .map(|(i, _)| i)
        .collect();
    let list = |idx: &mut dyn Iterator<Item = usize>| {
        idx.map(|i| named[i].0.as_str()).collect::<Vec<_>>().join(", ")
    };
    match hits.as_slice() {
        [i] => {
            let i = *i;
            Ok(named.swap_remove(i).1)
        }
        [] => Err(Error::DeviceNotFound {
            direction,
            name: wanted.to_string(),
            available: list(&mut (0..named.len())),
        }),
        many => Err(Error::AmbiguousDevice {
            direction,
            name: wanted.to_string(),
            matches: list(&mut many.iter().copied()),
        }),
    }
}

/// The stream configuration picked for a device.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChosenConfig {
    pub config: cpal::StreamConfig,
    pub sample_format: SampleFormat,
}

impl ChosenConfig {
    pub fn format(&self) -> AudioFormat {
        AudioFormat::new(self.config.sample_rate, usize::from(self.config.channels))
    }
}

/// Sample types the stream callbacks can convert to/from `f32`.
pub(crate) fn is_convertible(fmt: SampleFormat) -> bool {
    matches!(
        fmt,
        SampleFormat::I8
            | SampleFormat::I16
            | SampleFormat::I24
            | SampleFormat::I32
            | SampleFormat::U8
            | SampleFormat::U16
            | SampleFormat::U32
            | SampleFormat::F32
            | SampleFormat::F64
    )
}

/// Preference among sample types (lower is better): float first, then widest integer.
fn format_rank(fmt: SampleFormat) -> u32 {
    match fmt {
        SampleFormat::F32 => 0,
        SampleFormat::I32 => 1,
        SampleFormat::I24 => 2,
        SampleFormat::F64 => 3,
        SampleFormat::I16 => 4,
        SampleFormat::U32 => 5,
        SampleFormat::U16 => 6,
        SampleFormat::I8 => 7,
        _ => 8,
    }
}

/// Chooses a stream configuration as close as possible to the preferred rate and channel
/// count. Channel count matters most (an I/Q input must have two channels), then the rate
/// (exact > higher > lower, because a lower rate loses bandwidth), then the sample type.
pub(crate) fn choose_config(
    dev: &cpal::Device,
    direction: Direction,
    want_rate: Option<u32>,
    want_channels: Option<usize>,
) -> Result<ChosenConfig> {
    let name = device_name(dev);
    let default = default_config(dev, direction).ok();
    let ranges: Vec<SupportedStreamConfigRange> = supported_ranges(dev, direction)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| is_convertible(r.sample_format()) && r.channels() > 0)
        .collect();

    let want_rate = want_rate
        .or(default.as_ref().map(|d| d.sample_rate()))
        .unwrap_or(crate::WORKING_RATE);
    let want_ch = want_channels
        .or(default.as_ref().map(|d| usize::from(d.channels())))
        .unwrap_or(2);

    let best = ranges
        .iter()
        .map(|r| {
            let rate = pick_rate(r.min_sample_rate(), r.max_sample_rate(), want_rate);
            let ch = usize::from(r.channels());
            let ch_cost = if ch == want_ch {
                0
            } else if ch > want_ch {
                ch - want_ch
            } else {
                1000 + (want_ch - ch)
            };
            let score = (ch_cost, rate_cost(rate, want_rate), format_rank(r.sample_format()));
            (score, rate, r)
        })
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    match (best, default) {
        (Some((_, rate, r)), _) => Ok(ChosenConfig {
            config: cpal::StreamConfig {
                channels: r.channels(),
                sample_rate: rate,
                buffer_size: cpal::BufferSize::Default,
            },
            sample_format: r.sample_format(),
        }),
        // Some drivers list nothing but still report a usable default.
        (None, Some(d)) if is_convertible(d.sample_format()) => Ok(ChosenConfig {
            config: d.config(),
            sample_format: d.sample_format(),
        }),
        _ => Err(Error::NoUsableConfig(name)),
    }
}

/// The rate inside `[min, max]` closest to `want`, preferring standard rates.
fn pick_rate(min: u32, max: u32, want: u32) -> u32 {
    if (min..=max).contains(&want) {
        return want;
    }
    const STANDARD: [u32; 10] =
        [8000, 11025, 16000, 22050, 24000, 32000, 44100, 48000, 96000, 192000];
    let mut candidates: Vec<u32> =
        STANDARD.iter().copied().filter(|r| (min..=max).contains(r)).collect();
    candidates.push(min);
    candidates.push(max);
    candidates
        .into_iter()
        .min_by(|&a, &b| {
            rate_cost(a, want)
                .partial_cmp(&rate_cost(b, want))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(max)
}

/// Exact match is free; a higher rate costs a little (resampling, but no bandwidth lost); a
/// lower rate costs more (bandwidth lost).
fn rate_cost(rate: u32, want: u32) -> f64 {
    let (r, w) = (f64::from(rate), f64::from(want.max(1)));
    if rate == want {
        0.0
    } else if rate > want {
        1.0 + (r / w).ln()
    } else {
        10.0 + (w / r.max(1.0)).ln()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_preferences() {
        assert_eq!(pick_rate(8000, 192_000, 48_000), 48_000);
        assert_eq!(pick_rate(44_100, 44_100, 48_000), 44_100);
        assert_eq!(pick_rate(96_000, 96_000, 48_000), 96_000);
        // Prefer going up (96k) over losing bandwidth (44.1k) when both are offered.
        assert!(rate_cost(96_000, 48_000) < rate_cost(44_100, 48_000));
        assert_eq!(pick_rate(50_000, 60_000, 48_000), 50_000);
    }
}
