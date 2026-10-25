//! DAC constants, bandwidths and the codec configuration signalled in SDC entity
//! type 9 (see the crate docs for the byte layout).

use std::fmt;

/// Sampling rate of the DAC 24 kHz model, Hz.
pub const SAMPLE_RATE: u32 = 24_000;
/// PCM samples per DAC frame: the encoder's total stride 2·4·5·8.
pub const FRAME_SAMPLES: usize = 320;
/// DAC frames per second.
pub const FRAME_RATE: u32 = 75;
/// DAC frames per 400 ms DRM audio super frame.
pub const FRAMES_PER_SUPER_FRAME: usize = 30;
/// PCM samples per audio super frame (400 ms).
pub const SUPER_FRAME_SAMPLES: usize = FRAMES_PER_SUPER_FRAME * FRAME_SAMPLES;
/// Entries per codebook.
pub const CODEBOOK_SIZE: usize = 1024;
/// Bits per code (log2 of [`CODEBOOK_SIZE`]).
pub const CODE_BITS: usize = 10;
/// Codebooks of the model (24 kbit/s).
pub const MAX_CODEBOOKS: usize = 32;
/// Dimension of the latent vectors the codebooks quantise (the decoder's input).
pub const LATENT_DIM: usize = 1024;

/// SDC type 9 "audio coding" value of a DAC service (10, reserved in
/// ES 201 980 V4 §6.4.3.10).
pub const AUDIO_CODING: u8 = 2;
/// Start of the codec specific config: a zero byte (read as the SDC end marker by
/// receivers that do not skip the config by the entity length, e.g. Dream), `"DAC"`
/// and the framing format version `'1'`. Mirrors
/// `decdrm_core::mux::service::DAC_CONFIG_MAGIC` plus the version digit.
pub const CONFIG_MAGIC: [u8; 5] = [0x00, b'D', b'A', b'C', b'1'];
/// The start of the codec specific config of the EnCodec services DecDRM 0.4.6 and
/// earlier sent (the same framing with Meta's EnCodec model, which DAC replaced).
pub const ENCODEC_MAGIC: [u8; 4] = [0x00, b'E', b'N', b'C'];
/// Length of the codec specific config of a mono service: the magic and one parameter
/// byte. A joint stereo service adds the side signal's parameter byte.
pub const CONFIG_LEN: usize = CONFIG_MAGIC.len() + 1;
/// Frames per CRC group, by their 3-bit code in the parameter byte (the divisors of 30).
pub const GROUP_SIZES: [usize; 8] = [1, 2, 3, 5, 6, 10, 15, 30];
/// Most layers that can be sent twice (2 bits in the parameter byte).
pub const MAX_REPEATED_LAYERS: usize = 3;

/// The five bit-rate tiers. Each doubles the number of codebooks (residual vector
/// quantiser stages) of the previous one; the codebooks a tier adds over the previous
/// one form a *layer* (see [`crate::framing::layer_codebooks`]). (DAC decodes any
/// number of codebooks; the framing, inherited from DecDRM's earlier EnCodec codec,
/// uses these five.)
///
/// (Rust note: `#[derive(PartialOrd, Ord)]` orders the variants as declared, so
/// `Bandwidth::Kbps3 < Bandwidth::Kbps6`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Bandwidth {
    /// 1.5 kbit/s: 2 codebooks.
    Kbps1_5,
    /// 3 kbit/s: 4 codebooks.
    Kbps3,
    /// 6 kbit/s: 8 codebooks.
    Kbps6,
    /// 12 kbit/s: 16 codebooks.
    Kbps12,
    /// 24 kbit/s: 32 codebooks.
    Kbps24,
}

impl Bandwidth {
    /// All tiers, lowest first.
    pub const ALL: [Bandwidth; 5] =
        [Bandwidth::Kbps1_5, Bandwidth::Kbps3, Bandwidth::Kbps6, Bandwidth::Kbps12, Bandwidth::Kbps24];

    /// Index 0–4 (the 3-bit code in the parameter byte).
    pub fn tier(self) -> u8 {
        self as u8
    }

    /// The tier with this index.
    pub fn from_tier(tier: u8) -> Option<Self> {
        Self::ALL.get(usize::from(tier)).copied()
    }

    /// Codebooks per frame: 2, 4, 8, 16 or 32.
    pub fn codebooks(self) -> usize {
        2 << self.tier()
    }

    /// Layers of the tier (1–5): layer 0 = codebooks 0–1, layer *l* ≥ 1 = codebooks
    /// 2^l … 2^(l+1) − 1.
    pub fn layers(self) -> usize {
        usize::from(self.tier()) + 1
    }

    /// Bit rate of the codes, bit/s (75 frames × codebooks × 10 bits).
    pub fn bits_per_second(self) -> u32 {
        (self.codebooks() * CODE_BITS) as u32 * FRAME_RATE
    }

    /// Bit rate of the codes, kbit/s: 1.5, 3, 6, 12 or 24.
    pub fn kbps(self) -> f64 {
        f64::from(self.bits_per_second()) / 1000.0
    }

    /// The tier of exactly `kbps` kbit/s.
    pub fn from_kbps(kbps: f64) -> Option<Self> {
        Self::ALL.into_iter().find(|b| (b.kbps() - kbps).abs() < 1e-9)
    }

    /// The tier with `codebooks` codebooks.
    pub fn from_codebooks(codebooks: usize) -> Option<Self> {
        Self::ALL.into_iter().find(|b| b.codebooks() == codebooks)
    }
}

impl fmt::Display for Bandwidth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} kbit/s", self.kbps())
    }
}

/// Everything a receiver needs to know about a DAC service besides its stream
/// length: the bandwidth and the DRM framing, and for joint stereo those of the side
/// signal. Sent as the codec specific config of SDC entity type 9
/// ([`DacConfig::codec_config`]).
///
/// `bandwidth`, `group_frames` and `repeated_layers` describe the mono signal, or in
/// joint stereo the mid signal (L + R) / 2, which comes first in the super frame; `side`
/// codes (L − R) / 2 after it (see [`crate::framing`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DacConfig {
    /// Codebooks per frame.
    pub bandwidth: Bandwidth,
    /// DAC frames covered by each CRC (one of [`GROUP_SIZES`]).
    pub group_frames: usize,
    /// Leading layers sent a second time after the main block (0 ..= 3, at most
    /// [`Bandwidth::layers`]).
    pub repeated_layers: usize,
    /// Joint stereo: the coding of the side signal; `None` for a mono service.
    pub side: Option<SideCoding>,
}

/// The coding of the side signal of a joint stereo service: its own bandwidth (usually
/// far below the mid signal's) and framing, with the same meaning as in [`DacConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SideCoding {
    pub bandwidth: Bandwidth,
    pub group_frames: usize,
    pub repeated_layers: usize,
}

/// A codec specific config that is not a valid DAC configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The magic is missing: another codec, or DecDRM's DAC signalling is absent.
    #[error("not a DAC configuration")]
    NotDac,
    /// An EnCodec service of DecDRM 0.4.6 or earlier.
    #[error("EnCodec, the neural codec of DecDRM 0.4.6 and earlier, is no longer supported (DecDRM now uses DAC)")]
    Encodec,
    /// The configuration ends early.
    #[error("DAC configuration of {0} bytes is truncated")]
    Truncated(usize),
    /// A framing format this version of DecDRM does not know.
    #[error("DAC framing format {0:#04x} is not supported (this receiver knows format '1')")]
    UnsupportedVersion(u8),
    /// Bandwidth code 5–7.
    #[error("DAC bandwidth code {0} is reserved")]
    InvalidBandwidth(u8),
    /// The group size does not divide the 30 frames of a super frame.
    #[error("{0} frames per CRC group is not one of 1, 2, 3, 5, 6, 10, 15, 30")]
    InvalidGroup(usize),
    /// More repeated layers than the tier has, or than the format allows.
    #[error("{repeated} repeated layers: {bandwidth} allows at most {max}")]
    InvalidRepetition { repeated: usize, max: usize, bandwidth: Bandwidth },
}

impl DacConfig {
    /// A validated mono configuration.
    pub fn new(bandwidth: Bandwidth, group_frames: usize, repeated_layers: usize) -> Result<Self, ConfigError> {
        validate(bandwidth, group_frames, repeated_layers)?;
        Ok(Self { bandwidth, group_frames, repeated_layers, side: None })
    }

    /// Joint stereo: `mid`'s bandwidth and framing for the mid signal, `side`'s for the
    /// side signal (both validated mono configurations).
    ///
    /// (Rust note: `Self { side: …, ..mid }` copies every other field from `mid`.)
    pub fn joint_stereo(mid: DacConfig, side: DacConfig) -> Self {
        let side =
            SideCoding { bandwidth: side.bandwidth, group_frames: side.group_frames, repeated_layers: side.repeated_layers };
        Self { side: Some(side), ..mid }
    }

    /// Whether the service is joint stereo.
    pub fn is_stereo(&self) -> bool {
        self.side.is_some()
    }

    /// Output channels: 1, or 2 for joint stereo.
    pub fn channels(&self) -> usize {
        if self.is_stereo() { 2 } else { 1 }
    }

    /// The mono (or mid) signal's coding, as a mono configuration.
    pub fn mid(&self) -> DacConfig {
        Self { side: None, ..*self }
    }

    /// The side signal's coding as a mono configuration (joint stereo only).
    pub fn side_config(&self) -> Option<DacConfig> {
        self.side.map(|s| Self {
            bandwidth: s.bandwidth,
            group_frames: s.group_frames,
            repeated_layers: s.repeated_layers,
            side: None,
        })
    }

    /// Codebooks per frame (of the mono or mid signal).
    pub fn codebooks(&self) -> usize {
        self.bandwidth.codebooks()
    }

    /// Bit rate of all codes (mid and side), bit/s.
    pub fn bits_per_second(&self) -> u32 {
        self.bandwidth.bits_per_second() + self.side.map_or(0, |s| s.bandwidth.bits_per_second())
    }

    /// Smallest audio super frame that holds this configuration (mid and side), bytes.
    pub fn min_bytes(&self) -> usize {
        use crate::framing::FrameLayout;
        FrameLayout::new(self.mid()).min_bytes() + self.side_config().map_or(0, |s| FrameLayout::new(s).min_bytes())
    }

    /// The codec specific config for SDC entity type 9: [`CONFIG_MAGIC`], then the
    /// parameter byte `bandwidth tier (3 bits) | group size code (3 bits) | repeated
    /// layers (2 bits)`; joint stereo adds the side signal's parameter byte, laid out
    /// the same way. (Receivers that know only mono ignore it and decode the mid
    /// signal, which comes first in the super frame.)
    pub fn codec_config(&self) -> Vec<u8> {
        let mut c = CONFIG_MAGIC.to_vec();
        c.push(param_byte(self.bandwidth, self.group_frames, self.repeated_layers));
        if let Some(s) = self.side {
            c.push(param_byte(s.bandwidth, s.group_frames, s.repeated_layers));
        }
        c
    }

    /// Parse a codec specific config. A byte after the parameter byte is the side
    /// signal's parameter byte (joint stereo); bytes after that are ignored (room for
    /// compatible additions within format 1).
    pub fn from_codec_config(config: &[u8]) -> Result<Self, ConfigError> {
        if config.starts_with(&ENCODEC_MAGIC) {
            return Err(ConfigError::Encodec);
        }
        if config.len() < 4 || config[..4] != CONFIG_MAGIC[..4] {
            return Err(ConfigError::NotDac);
        }
        match config.get(4) {
            None => return Err(ConfigError::Truncated(config.len())),
            Some(&v) if v != CONFIG_MAGIC[4] => return Err(ConfigError::UnsupportedVersion(v)),
            Some(_) => {}
        }
        let &p = config.get(CONFIG_MAGIC.len()).ok_or(ConfigError::Truncated(config.len()))?;
        let mono = parse_param_byte(p)?;
        match config.get(CONFIG_LEN) {
            None => Ok(mono),
            Some(&s) => Ok(Self::joint_stereo(mono, parse_param_byte(s)?)),
        }
    }

    /// Parse the type 9 bytes of a service (the entity body after Short Id and Stream
    /// Id: two bytes, then the codec specific config — `AudioParams::type9_bytes`).
    pub fn from_type9_bytes(type9: &[u8]) -> Result<Self, ConfigError> {
        match type9.first() {
            None => Err(ConfigError::Truncated(0)),
            Some(b0) if b0 >> 6 != AUDIO_CODING => Err(ConfigError::NotDac),
            Some(_) => Self::from_codec_config(type9.get(2..).unwrap_or(&[])),
        }
    }

    /// E.g. `DAC 6 kbit/s (8 codebooks), CRC per 40 ms, codebooks 0-3 sent twice`, or
    /// for joint stereo `DAC joint stereo, mid 12 kbit/s (16 codebooks), CRC per 40 ms,
    /// codebooks 0-7 sent twice; side 1.5 kbit/s (2 codebooks), CRC per 40 ms`.
    pub fn describe(&self) -> String {
        let channel = |bandwidth: Bandwidth, group_frames: usize, repeated_layers: usize| {
            let group_ms = group_frames as f64 * 1000.0 / f64::from(FRAME_RATE);
            let repeated = match repeated_layers {
                0 => String::new(),
                r => format!(", codebooks 0-{} sent twice", crate::framing::layer_codebooks(r - 1).end - 1),
            };
            format!("{bandwidth} ({} codebooks), CRC per {group_ms:.0} ms{repeated}", bandwidth.codebooks())
        };
        let mid = channel(self.bandwidth, self.group_frames, self.repeated_layers);
        match self.side {
            None => format!("DAC {mid}"),
            Some(s) => {
                format!("DAC joint stereo, mid {mid}; side {}", channel(s.bandwidth, s.group_frames, s.repeated_layers))
            }
        }
    }

    /// The bit rates in a few words: `6 kbit/s mono` or `12 kbit/s mid + 1.5 kbit/s
    /// side`.
    pub fn rates(&self) -> String {
        match self.side {
            None => format!("{} mono", self.bandwidth),
            Some(s) => format!("{} mid + {} side", self.bandwidth, s.bandwidth),
        }
    }
}

/// Checks of one signal's coding.
fn validate(bandwidth: Bandwidth, group_frames: usize, repeated_layers: usize) -> Result<(), ConfigError> {
    if !GROUP_SIZES.contains(&group_frames) {
        return Err(ConfigError::InvalidGroup(group_frames));
    }
    let max = MAX_REPEATED_LAYERS.min(bandwidth.layers());
    if repeated_layers > max {
        return Err(ConfigError::InvalidRepetition { repeated: repeated_layers, max, bandwidth });
    }
    Ok(())
}

/// `bandwidth tier (3 bits) | group size code (3 bits) | repeated layers (2 bits)`.
fn param_byte(bandwidth: Bandwidth, group_frames: usize, repeated_layers: usize) -> u8 {
    let group = GROUP_SIZES.iter().position(|&g| g == group_frames).expect("validated group size") as u8;
    (bandwidth.tier() << 5) | (group << 2) | repeated_layers as u8
}

/// A parameter byte as a mono configuration.
fn parse_param_byte(p: u8) -> Result<DacConfig, ConfigError> {
    let bandwidth = Bandwidth::from_tier(p >> 5).ok_or(ConfigError::InvalidBandwidth(p >> 5))?;
    DacConfig::new(bandwidth, GROUP_SIZES[usize::from((p >> 2) & 7)], usize::from(p & 3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers() {
        let rates: Vec<(usize, u32)> = Bandwidth::ALL.iter().map(|b| (b.codebooks(), b.bits_per_second())).collect();
        assert_eq!(rates, [(2, 1500), (4, 3000), (8, 6000), (16, 12_000), (32, 24_000)]);
        assert_eq!(Bandwidth::from_kbps(1.5), Some(Bandwidth::Kbps1_5));
        assert_eq!(Bandwidth::from_kbps(12.0), Some(Bandwidth::Kbps12));
        assert_eq!(Bandwidth::from_kbps(5.0), None);
        assert_eq!(Bandwidth::Kbps6.to_string(), "6 kbit/s");
        assert_eq!(Bandwidth::Kbps1_5.to_string(), "1.5 kbit/s");
        assert_eq!(Bandwidth::Kbps24.layers(), 5);
    }

    #[test]
    fn codec_config_round_trip() {
        for bw in Bandwidth::ALL {
            for g in GROUP_SIZES {
                for r in 0..=MAX_REPEATED_LAYERS.min(bw.layers()) {
                    let c = DacConfig::new(bw, g, r).unwrap();
                    let bytes = c.codec_config();
                    assert_eq!(&bytes[..5], b"\0DAC1");
                    assert_eq!(DacConfig::from_codec_config(&bytes), Ok(c));
                    let type9 = [&[0b1000_0011, 0x80][..], &bytes].concat();
                    assert_eq!(DacConfig::from_type9_bytes(&type9), Ok(c));
                }
            }
        }
        assert_eq!(
            DacConfig::new(Bandwidth::Kbps1_5, 3, 2),
            Err(ConfigError::InvalidRepetition { repeated: 2, max: 1, bandwidth: Bandwidth::Kbps1_5 })
        );
        assert_eq!(DacConfig::new(Bandwidth::Kbps6, 4, 0), Err(ConfigError::InvalidGroup(4)));
        assert_eq!(DacConfig::from_codec_config(b"\0DAC2\x40"), Err(ConfigError::UnsupportedVersion(b'2')));
        assert_eq!(DacConfig::from_codec_config(b"\0DAC1"), Err(ConfigError::Truncated(5)));
        assert_eq!(DacConfig::from_codec_config(b"DAC1\x40"), Err(ConfigError::NotDac));
        assert_eq!(DacConfig::from_codec_config(&[0, b'D', b'A', b'C', b'1', 0xA0]), Err(ConfigError::InvalidBandwidth(5)));
        // The EnCodec services of DecDRM 0.4.6 and earlier are named, not decoded.
        assert_eq!(DacConfig::from_codec_config(b"\0ENC1\x48"), Err(ConfigError::Encodec));
        assert!(ConfigError::Encodec.to_string().contains("no longer supported"));
        // AAC type 9 bytes are not DAC.
        assert_eq!(DacConfig::from_type9_bytes(&[0x23, 0x80]), Err(ConfigError::NotDac));
    }

    /// Joint stereo adds the side signal's parameter byte; a mono-only reading of the
    /// config (its first six bytes) gives the mid signal's configuration.
    #[test]
    fn joint_stereo_config() {
        let mid = DacConfig::new(Bandwidth::Kbps12, 3, 3).unwrap();
        let side = DacConfig::new(Bandwidth::Kbps1_5, 6, 1).unwrap();
        let c = DacConfig::joint_stereo(mid, side);
        assert!(c.is_stereo() && !mid.is_stereo());
        assert_eq!((c.channels(), c.mid(), c.side_config()), (2, mid, Some(side)));
        let bytes = c.codec_config();
        assert_eq!(bytes, [0, b'D', b'A', b'C', b'1', (3 << 5) | (2 << 2) | 3, (4 << 2) | 1]);
        assert_eq!(DacConfig::from_codec_config(&bytes), Ok(c));
        assert_eq!(DacConfig::from_codec_config(&bytes[..CONFIG_LEN]), Ok(mid));
        // Bytes after the side's are ignored.
        assert_eq!(DacConfig::from_codec_config(&[&bytes[..], &[0xFF]].concat()), Ok(c));
        assert_eq!(DacConfig::from_codec_config(&[&bytes[..6], &[0xA0]].concat()), Err(ConfigError::InvalidBandwidth(5)));
        assert_eq!(c.bits_per_second(), 13_500);
        assert_eq!(c.rates(), "12 kbit/s mid + 1.5 kbit/s side");
        assert_eq!(mid.rates(), "12 kbit/s mono");
        assert_eq!(
            c.describe(),
            "DAC joint stereo, mid 12 kbit/s (16 codebooks), CRC per 40 ms, codebooks 0-7 sent twice; \
             side 1.5 kbit/s (2 codebooks), CRC per 80 ms, codebooks 0-1 sent twice"
        );
        assert_eq!(c.min_bytes(), mid.min_bytes() + side.min_bytes());
    }

    /// The magic mirrors the prefix decdrm-core recognises.
    #[test]
    fn magic_matches_core() {
        assert_eq!(CONFIG_MAGIC[..4], decdrm_core::mux::service::DAC_CONFIG_MAGIC);
        let c = DacConfig::new(Bandwidth::Kbps6, 3, 1).unwrap().codec_config();
        assert!(decdrm_core::mux::service::is_dac_config(&c));
        assert_eq!(
            c,
            [0, b'D', b'A', b'C', b'1', (2 << 5) | (2 << 2) | 1],
            "6 kbit/s, 3-frame groups, layer 0 repeated"
        );
        assert_eq!(
            DacConfig::new(Bandwidth::Kbps6, 3, 1).unwrap().describe(),
            "DAC 6 kbit/s (8 codebooks), CRC per 40 ms, codebooks 0-1 sent twice"
        );
    }
}
