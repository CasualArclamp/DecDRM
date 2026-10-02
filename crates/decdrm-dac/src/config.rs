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
/// Length of the codec specific config: the magic and one parameter byte.
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
/// length: the bandwidth and the DRM framing. Sent as the codec specific config of SDC
/// entity type 9 ([`DacConfig::codec_config`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DacConfig {
    /// Codebooks per frame.
    pub bandwidth: Bandwidth,
    /// DAC frames covered by each CRC (one of [`GROUP_SIZES`]).
    pub group_frames: usize,
    /// Leading layers sent a second time after the main block (0 ..= 3, at most
    /// [`Bandwidth::layers`]).
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
    /// A validated configuration.
    pub fn new(bandwidth: Bandwidth, group_frames: usize, repeated_layers: usize) -> Result<Self, ConfigError> {
        if !GROUP_SIZES.contains(&group_frames) {
            return Err(ConfigError::InvalidGroup(group_frames));
        }
        let max = MAX_REPEATED_LAYERS.min(bandwidth.layers());
        if repeated_layers > max {
            return Err(ConfigError::InvalidRepetition { repeated: repeated_layers, max, bandwidth });
        }
        Ok(Self { bandwidth, group_frames, repeated_layers })
    }

    /// Codebooks per frame.
    pub fn codebooks(&self) -> usize {
        self.bandwidth.codebooks()
    }

    /// The codec specific config for SDC entity type 9: [`CONFIG_MAGIC`], then
    /// `bandwidth tier (3 bits) | group size code (3 bits) | repeated layers (2 bits)`.
    pub fn codec_config(&self) -> [u8; CONFIG_LEN] {
        let group = GROUP_SIZES.iter().position(|&g| g == self.group_frames).expect("validated group size") as u8;
        let params = (self.bandwidth.tier() << 5) | (group << 2) | self.repeated_layers as u8;
        let mut c = [0u8; CONFIG_LEN];
        c[..CONFIG_MAGIC.len()].copy_from_slice(&CONFIG_MAGIC);
        c[CONFIG_MAGIC.len()] = params;
        c
    }

    /// Parse a codec specific config. Bytes after the parameter byte are ignored (room
    /// for compatible additions within format 1).
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
        let bandwidth = Bandwidth::from_tier(p >> 5).ok_or(ConfigError::InvalidBandwidth(p >> 5))?;
        Self::new(bandwidth, GROUP_SIZES[usize::from((p >> 2) & 7)], usize::from(p & 3))
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

    /// E.g. `DAC 6 kbit/s (8 codebooks), CRC per 40 ms, codebooks 0-3 sent twice`.
    pub fn describe(&self) -> String {
        let group_ms = self.group_frames as f64 * 1000.0 / f64::from(FRAME_RATE);
        let repeated = match self.repeated_layers {
            0 => String::new(),
            r => format!(", codebooks 0-{} sent twice", crate::framing::layer_codebooks(r - 1).end - 1),
        };
        format!(
            "DAC {} ({} codebooks), CRC per {group_ms:.0} ms{repeated}",
            self.bandwidth,
            self.bandwidth.codebooks()
        )
    }
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
