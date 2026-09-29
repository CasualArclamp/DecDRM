//! Fast Access Channel content (ES 201 980 §6.3): channel parameters of the
//! transmission plus the parameters of one service per frame.

use crate::bits::{BitReader, BitWriter};
use crate::fec::crc::Crc;
use crate::fec::qam::Mapping;
use crate::params::SpectrumOccupancy;

/// Interleaver depth of the MSC (§7.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Interleaving {
    /// 2 s (5 multiplex frames).
    Long,
    /// 400 ms (1 multiplex frame).
    Short,
}

/// MSC constellation (FAC "MSC mode" field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MscMode {
    Qam64Sm,
    Qam64HmMix,
    Qam64HmSym,
    Qam16Sm,
}

impl MscMode {
    pub fn mapping(self) -> Mapping {
        match self {
            Self::Qam64Sm => Mapping::Qam64Sm,
            Self::Qam64HmMix => Mapping::Qam64HmMix,
            Self::Qam64HmSym => Mapping::Qam64HmSym,
            Self::Qam16Sm => Mapping::Qam16,
        }
    }

    pub fn from_bits(v: u32) -> Self {
        match v & 3 {
            0 => Self::Qam64Sm,
            1 => Self::Qam64HmMix,
            2 => Self::Qam64HmSym,
            _ => Self::Qam16Sm,
        }
    }

    pub fn bits(self) -> u32 {
        match self {
            Self::Qam64Sm => 0,
            Self::Qam64HmMix => 1,
            Self::Qam64HmSym => 2,
            Self::Qam16Sm => 3,
        }
    }

    pub fn is_hierarchical(self) -> bool {
        matches!(self, Self::Qam64HmMix | Self::Qam64HmSym)
    }
}

/// SDC constellation (FAC "SDC mode" field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SdcMode {
    Qam16,
    Qam4,
}

impl SdcMode {
    pub fn mapping(self) -> Mapping {
        match self {
            Self::Qam16 => Mapping::Qam16,
            Self::Qam4 => Mapping::Qam4,
        }
    }
}

/// Channel parameters (first 20 bits of the FAC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelParams {
    pub enhancement: bool,
    /// Frame index within the super frame (0..=2).
    pub frame_index: u8,
    /// Identity field value 3: first frame, and the SDC AFS index is valid.
    pub afs_valid: bool,
    pub occupancy: SpectrumOccupancy,
    pub interleaving: Interleaving,
    pub msc_mode: MscMode,
    pub sdc_mode: SdcMode,
    pub num_audio: u8,
    pub num_data: u8,
    pub reconfiguration_index: u8,
    pub toggle: bool,
}

/// Service parameters (44 bits) of the service signalled in this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceParams {
    pub service_id: u32,
    pub short_id: u8,
    pub audio_ca: bool,
    pub language: u8,
    pub is_data: bool,
    /// Programme type (audio) or application identifier (data).
    pub descriptor: u8,
    pub data_ca: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fac {
    pub channel: ChannelParams,
    pub service: ServiceParams,
}

/// Number-of-services code (§6.3.3, table 60): (audio, data) → 4-bit code.
const NUM_SERVICES: [[i8; 5]; 5] = [
    [-1, 1, 2, 3, 15],
    [4, 5, 6, 7, -1],
    [8, 9, 10, -1, -1],
    [12, 13, -1, -1, -1],
    [0, -1, -1, -1, -1],
];

fn decode_num_services(code: u32) -> Option<(u8, u8)> {
    for (a, row) in NUM_SERVICES.iter().enumerate() {
        for (d, &v) in row.iter().enumerate() {
            if v as i32 == code as i32 {
                return Some((a as u8, d as u8));
            }
        }
    }
    None
}

/// Language names of the 4-bit FAC language code (§6.3.4, table 62).
pub const LANGUAGES: [&str; 16] = [
    "No language specified",
    "Arabic",
    "Bengali",
    "Chinese (Mandarin)",
    "Dutch",
    "English",
    "French",
    "German",
    "Hindi",
    "Japanese",
    "Javanese",
    "Korean",
    "Portuguese",
    "Russian",
    "Spanish",
    "Other language",
];

/// Programme type names (§6.3.4, table 63).
pub const PROGRAMME_TYPES: [&str; 32] = [
    "No programme type",
    "News",
    "Current Affairs",
    "Information",
    "Sport",
    "Education",
    "Drama",
    "Culture",
    "Science",
    "Varied",
    "Pop Music",
    "Rock Music",
    "Easy Listening Music",
    "Light Classical",
    "Serious Classical",
    "Other Music",
    "Weather/meteorology",
    "Finance/Business",
    "Children's programmes",
    "Social Affairs",
    "Religion",
    "Phone In",
    "Travel",
    "Leisure",
    "Jazz Music",
    "Country Music",
    "National Music",
    "Oldies Music",
    "Folk Music",
    "Documentary",
    "Not used",
    "Not used",
];

impl Fac {
    /// Parse a decoded FAC block (72 bits, one bit per byte). Returns `None` if the
    /// CRC fails or a field is invalid.
    pub fn parse(bits: &[u8]) -> Option<Self> {
        if bits.len() < 72 {
            return None;
        }
        let mut crc = Crc::crc8();
        for &b in &bits[..64] {
            crc.add_bit(b & 1 == 1);
        }
        let mut r = BitReader::from_bits(bits);
        let rx_crc = {
            let mut t = BitReader::from_bits(&bits[64..72]);
            t.read(8)
        };
        if crc.value() != rx_crc {
            return None;
        }
        let enhancement = r.read(1) == 1;
        let identity = r.read(2) as u8;
        let occupancy = SpectrumOccupancy::new(r.read(4) as u8)?;
        let interleaving = if r.read(1) == 0 { Interleaving::Long } else { Interleaving::Short };
        let msc_mode = MscMode::from_bits(r.read(2));
        let sdc_mode = if r.read(1) == 0 { SdcMode::Qam16 } else { SdcMode::Qam4 };
        let (num_audio, num_data) = decode_num_services(r.read(4))?;
        let reconfiguration_index = r.read(3) as u8;
        let toggle = r.read(1) == 1;
        let _rfu = r.read(1);
        let service_id = r.read(24);
        let short_id = r.read(2) as u8;
        let audio_ca = r.read(1) == 1;
        let language = r.read(4) as u8;
        let is_data = r.read(1) == 1;
        let descriptor = r.read(5) as u8;
        let data_ca = r.read(1) == 1;
        Some(Fac {
            channel: ChannelParams {
                enhancement,
                frame_index: if identity == 3 { 0 } else { identity },
                afs_valid: identity == 3,
                occupancy,
                interleaving,
                msc_mode,
                sdc_mode,
                num_audio,
                num_data,
                reconfiguration_index,
                toggle,
            },
            service: ServiceParams { service_id, short_id, audio_ca, language, is_data, descriptor, data_ca },
        })
    }

    /// Serialise to 72 bits including the CRC (transmitter side).
    pub fn to_bits(&self) -> Vec<u8> {
        let c = &self.channel;
        let s = &self.service;
        let mut w = BitWriter::new();
        w.write(u32::from(c.enhancement), 1);
        let identity = if c.afs_valid && c.frame_index == 0 { 3 } else { u32::from(c.frame_index) };
        w.write(identity, 2);
        w.write(u32::from(c.occupancy.value()), 4);
        w.write(u32::from(c.interleaving == Interleaving::Short), 1);
        w.write(c.msc_mode.bits(), 2);
        w.write(u32::from(c.sdc_mode == SdcMode::Qam4), 1);
        let code = NUM_SERVICES
            .get(c.num_audio as usize)
            .and_then(|r| r.get(c.num_data as usize))
            .copied()
            .unwrap_or(-1);
        w.write(code.max(0) as u32, 4);
        w.write(u32::from(c.reconfiguration_index), 3);
        w.write(u32::from(c.toggle), 1);
        w.write(0, 1);
        w.write(s.service_id & 0xFF_FFFF, 24);
        w.write(u32::from(s.short_id), 2);
        w.write(u32::from(s.audio_ca), 1);
        w.write(u32::from(s.language), 4);
        w.write(u32::from(s.is_data), 1);
        w.write(u32::from(s.descriptor), 5);
        w.write(u32::from(s.data_ca), 1);
        w.write(0, 6);
        let mut bits = w.into_bits();
        let mut crc = Crc::crc8();
        for &b in &bits {
            crc.add_bit(b == 1);
        }
        let v = crc.value();
        for i in (0..8).rev() {
            bits.push(((v >> i) & 1) as u8);
        }
        bits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let fac = Fac {
            channel: ChannelParams {
                enhancement: false,
                frame_index: 2,
                afs_valid: false,
                occupancy: SpectrumOccupancy::SO_3,
                interleaving: Interleaving::Long,
                msc_mode: MscMode::Qam64Sm,
                sdc_mode: SdcMode::Qam16,
                num_audio: 1,
                num_data: 1,
                reconfiguration_index: 0,
                toggle: true,
            },
            service: ServiceParams {
                service_id: 0x123456,
                short_id: 1,
                audio_ca: false,
                language: 5,
                is_data: false,
                descriptor: 1,
                data_ca: false,
            },
        };
        let bits = fac.to_bits();
        assert_eq!(bits.len(), 72);
        assert_eq!(Fac::parse(&bits), Some(fac));
        let mut bad = bits.clone();
        bad[10] ^= 1;
        assert_eq!(Fac::parse(&bad), None);
    }
}
