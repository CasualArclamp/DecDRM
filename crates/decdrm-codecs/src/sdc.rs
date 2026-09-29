//! SDC data entity type 9 — *audio information* (ES 201 980 §6.4.3.10).
//!
//! The entity body is
//!
//! | field                | bits | notes                                           |
//! |----------------------|------|-------------------------------------------------|
//! | short Id             | 2    | service                                         |
//! | stream Id            | 2    | MSC stream carrying the audio                   |
//! | audio coding         | 2    | 0 AAC, 1 Opus (Dream extension; was CELP), 2 reserved (was HVXC), 3 xHE-AAC |
//! | SBR flag             | 1    | AAC only                                        |
//! | audio mode           | 2    | 0 mono, 1 parametric stereo, 2 stereo           |
//! | audio sampling rate  | 3    | coding dependent, see [`AudioInfo::sample_rate_code`] |
//! | text flag            | 1    | text message in the audio stream                |
//! | enhancement flag     | 1    |                                                 |
//! | coder field          | 5    | AAC/xHE-AAC: 3-bit MPEG Surround config + 2 rfa |
//! | rfa                  | 1    |                                                 |
//! | xHE-AAC config       | 8·n  | only for xHE-AAC                                 |
//!
//! FDK-AAC (`aacDecoder_ConfigRaw` with `TT_DRM`) and Dream's `CAudioParam::getType9Bytes()`
//! use the body **without** the leading short Id / stream Id nibble: 2 bytes (audio coding
//! … rfa) followed by the xHE-AAC config bytes. That byte string is what this crate calls
//! the *type-9 bytes* ([`AudioInfo::to_type9_bytes`], [`AudioInfo::from_type9_bytes`]).
//!
//! # Opus signalling (Dream extension)
//!
//! Opus is not part of ES 201 980. Dream signals it in two ways and its receiver
//! (`CAudioParam::setFromType9Bits`) accepts both:
//!
//! * **legacy** (Dream 2.1): audio coding AAC, SBR off, mono, sampling-rate code 7
//!   (reserved for AAC);
//! * **dream-mjf**: audio coding 1 (formerly CELP).
//!
//! Dream's receiver rejects either form unless SBR is off and the mode is mono (the Opus
//! packets themselves carry mode, bandwidth and channel count). [`OpusSignalling`] selects
//! the form written by [`AudioInfo::opus`].

use crate::{CodecError, DrmAudioCoding};

/// The 2-bit audio coding field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioCodingField {
    /// 0: AAC (also used by Dream's legacy Opus signalling together with rate code 7).
    Aac,
    /// 1: Opus in Dream's extension (CELP in early versions of the standard).
    Opus,
    /// 2: reserved (HVXC in early versions of the standard).
    Reserved,
    /// 3: xHE-AAC.
    XheAac,
}

impl AudioCodingField {
    fn from_bits(v: u8) -> Self {
        match v & 3 {
            0 => Self::Aac,
            1 => Self::Opus,
            2 => Self::Reserved,
            _ => Self::XheAac,
        }
    }
    fn bits(self) -> u8 {
        match self {
            Self::Aac => 0,
            Self::Opus => 1,
            Self::Reserved => 2,
            Self::XheAac => 3,
        }
    }
}

/// The 2-bit audio mode field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioMode {
    /// 0: mono.
    Mono,
    /// 1: parametric stereo (HE-AAC v2: mono core + SBR + PS). AAC only.
    ParametricStereo,
    /// 2: stereo.
    Stereo,
    /// 3: reserved.
    Reserved,
}

impl AudioMode {
    fn from_bits(v: u8) -> Self {
        match v & 3 {
            0 => Self::Mono,
            1 => Self::ParametricStereo,
            2 => Self::Stereo,
            _ => Self::Reserved,
        }
    }
    fn bits(self) -> u8 {
        match self {
            Self::Mono => 0,
            Self::ParametricStereo => 1,
            Self::Stereo => 2,
            Self::Reserved => 3,
        }
    }
}

/// How [`AudioInfo::opus`] signals Opus in the SDC (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OpusSignalling {
    /// AAC + sampling-rate code 7, mono, no SBR (Dream 2.1). Understood by every Dream
    /// version with Opus support — the default.
    #[default]
    Legacy,
    /// Audio coding field 1 (dream-mjf), mono, no SBR, rate code 5 (48 kHz).
    CodingField,
}

/// Decoded SDC type-9 audio information (without short Id and stream Id).
///
/// Mirrors the type-9 part of Dream's `CAudioParam`. Sampling rates are kept in Hz; for
/// AAC they are the *core* rate (SBR doubles the output rate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioInfo {
    /// Raw audio coding field. Use [`AudioInfo::drm_audio_coding`] to decide which decoder
    /// applies (it also recognises Dream's legacy Opus signalling).
    pub coding: AudioCodingField,
    /// SBR flag (AAC).
    pub sbr: bool,
    /// Audio mode.
    pub mode: AudioMode,
    /// Raw 3-bit sampling-rate code (its meaning depends on `coding`; see
    /// [`AudioInfo::sample_rate`]).
    pub sample_rate_code: u8,
    /// A text message is carried in the last 4 bytes of the audio super frame.
    pub text_flag: bool,
    /// Enhancement flag.
    pub enhancement_flag: bool,
    /// 5-bit coder field (AAC/xHE-AAC: MPEG Surround configuration in the top 3 bits).
    pub coder_field: u8,
    /// xHE-AAC static configuration bytes (empty for other codings).
    pub xhe_aac_config: Vec<u8>,
}

impl AudioInfo {
    /// Parses the type-9 bytes (body without short/stream Id; see the module docs).
    pub fn from_type9_bytes(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() < 2 {
            return Err(CodecError::InvalidConfig(format!(
                "SDC type 9 needs at least 2 bytes, got {}",
                bytes.len()
            )));
        }
        let (b0, b1) = (bytes[0], bytes[1]);
        let coding = AudioCodingField::from_bits(b0 >> 6);
        let info = AudioInfo {
            coding,
            sbr: (b0 >> 5) & 1 != 0,
            mode: AudioMode::from_bits(b0 >> 3),
            sample_rate_code: b0 & 7,
            text_flag: (b1 >> 7) & 1 != 0,
            enhancement_flag: (b1 >> 6) & 1 != 0,
            coder_field: (b1 >> 1) & 0x1F,
            xhe_aac_config: if coding == AudioCodingField::XheAac {
                bytes[2..].to_vec()
            } else {
                Vec::new()
            },
        };
        Ok(info)
    }

    /// Parses a complete SDC type-9 entity body (starting with short Id and stream Id,
    /// i.e. the bytes after the entity header). Returns `(short_id, stream_id, info)`.
    pub fn from_entity_body(body: &[u8]) -> Result<(u8, u8, Self), CodecError> {
        if body.len() < 3 {
            return Err(CodecError::InvalidConfig(format!(
                "SDC type 9 body needs at least 3 bytes, got {}",
                body.len()
            )));
        }
        let short_id = body[0] >> 6;
        let stream_id = (body[0] >> 4) & 3;
        // Shift the whole body left by 4 bits to drop the two Ids.
        let shifted: Vec<u8> = (0..body.len() - 1)
            .map(|i| (body[i] << 4) | (body[i + 1] >> 4))
            .collect();
        Ok((short_id, stream_id, Self::from_type9_bytes(&shifted)?))
    }

    /// Serialises to type-9 bytes — a port of Dream's `CAudioParam::getType9Bytes()`
    /// (2 bytes plus the xHE-AAC config). This is the configuration FDK-AAC expects.
    pub fn to_type9_bytes(&self) -> Vec<u8> {
        let b0 = (self.coding.bits() << 6)
            | (u8::from(self.sbr) << 5)
            | (self.mode.bits() << 3)
            | (self.sample_rate_code & 7);
        // Dream writes the coder field for AAC/xHE-AAC and 5 zero rfa bits otherwise.
        let coder = match self.coding {
            AudioCodingField::Aac | AudioCodingField::XheAac => self.coder_field & 0x1F,
            _ => 0,
        };
        let b1 =
            (u8::from(self.text_flag) << 7) | (u8::from(self.enhancement_flag) << 6) | (coder << 1);
        let mut out = vec![b0, b1];
        if self.coding == AudioCodingField::XheAac {
            out.extend_from_slice(&self.xhe_aac_config);
        }
        out
    }

    /// Serialises a complete entity body: short Id, stream Id, then the type-9 bytes. The
    /// body is `4 + 8·len(type-9 bytes)` bits long; the returned bytes are zero-padded in
    /// the last nibble. The SDC entity header's length field (which excludes the first 4
    /// bits of the body, ES 201 980 §6.4.2) is therefore `to_type9_bytes().len()`.
    pub fn to_entity_body(&self, short_id: u8, stream_id: u8) -> Vec<u8> {
        let t9 = self.to_type9_bytes();
        let mut out = Vec::with_capacity(t9.len() + 1);
        let mut carry = ((short_id & 3) << 6) | ((stream_id & 3) << 4);
        for &b in &t9 {
            out.push(carry | (b >> 4));
            carry = b << 4;
        }
        out.push(carry);
        out
    }

    /// Which DecDRM decoder handles this service, or `None` for reserved / unsupported
    /// codings (CELP/HVXC in old versions of the standard).
    pub fn drm_audio_coding(&self) -> Option<DrmAudioCoding> {
        match self.coding {
            AudioCodingField::Aac if self.sample_rate_code == 7 => Some(DrmAudioCoding::Opus),
            AudioCodingField::Aac => Some(DrmAudioCoding::Aac),
            AudioCodingField::Opus => Some(DrmAudioCoding::Opus),
            AudioCodingField::XheAac => Some(DrmAudioCoding::XheAac),
            AudioCodingField::Reserved => None,
        }
    }

    /// Sampling rate in Hz, or `None` for reserved codes. For AAC this is the core rate
    /// (before SBR); Opus always runs at 48 kHz.
    pub fn sample_rate(&self) -> Option<u32> {
        match self.drm_audio_coding()? {
            DrmAudioCoding::Opus => Some(48_000),
            DrmAudioCoding::XheAac => Some(
                [
                    9_600, 12_000, 16_000, 19_200, 24_000, 32_000, 38_400, 48_000,
                ][usize::from(self.sample_rate_code & 7)],
            ),
            DrmAudioCoding::Aac => match self.sample_rate_code {
                0 => Some(8_000),
                1 => Some(12_000),
                2 => Some(16_000),
                3 => Some(24_000),
                5 => Some(48_000),
                _ => None,
            },
        }
    }

    /// Rate code for `rate_hz` under the given coding, as Dream's `EnqueueType9()` maps
    /// it (`None` if the rate cannot be signalled).
    pub fn sample_rate_code(coding: DrmAudioCoding, rate_hz: u32) -> Option<u8> {
        match coding {
            DrmAudioCoding::XheAac => match rate_hz {
                9_600 => Some(0),
                12_000 => Some(1),
                16_000 => Some(2),
                19_200 => Some(3),
                24_000 => Some(4),
                32_000 => Some(5),
                38_400 => Some(6),
                48_000 => Some(7),
                _ => None,
            },
            DrmAudioCoding::Aac | DrmAudioCoding::Opus => match rate_hz {
                8_000 => Some(0),
                12_000 => Some(1),
                16_000 => Some(2),
                24_000 => Some(3),
                48_000 => Some(5),
                _ => None,
            },
        }
    }

    /// Audio information for an AAC service. `core_rate_hz` is the AAC core rate (12 or
    /// 24 kHz in DRM30); `mode` must be [`AudioMode::ParametricStereo`] only with `sbr`.
    pub fn aac(core_rate_hz: u32, sbr: bool, mode: AudioMode) -> Result<Self, CodecError> {
        let code = Self::sample_rate_code(DrmAudioCoding::Aac, core_rate_hz).ok_or_else(|| {
            CodecError::InvalidConfig(format!(
                "AAC core rate {core_rate_hz} Hz cannot be signalled"
            ))
        })?;
        if mode == AudioMode::ParametricStereo && !sbr {
            return Err(CodecError::InvalidConfig(
                "parametric stereo requires SBR".into(),
            ));
        }
        if mode == AudioMode::Reserved {
            return Err(CodecError::InvalidConfig("reserved audio mode".into()));
        }
        Ok(AudioInfo {
            coding: AudioCodingField::Aac,
            sbr,
            mode,
            sample_rate_code: code,
            text_flag: false,
            enhancement_flag: false,
            coder_field: 0,
            xhe_aac_config: Vec::new(),
        })
    }

    /// Audio information for a Dream-style Opus service (see [`OpusSignalling`]).
    pub fn opus(signalling: OpusSignalling) -> Self {
        let (coding, code) = match signalling {
            OpusSignalling::Legacy => (AudioCodingField::Aac, 7),
            OpusSignalling::CodingField => (AudioCodingField::Opus, 5),
        };
        AudioInfo {
            coding,
            sbr: false,
            mode: AudioMode::Mono,
            sample_rate_code: code,
            text_flag: false,
            enhancement_flag: false,
            coder_field: 0,
            xhe_aac_config: Vec::new(),
        }
    }

    /// Audio information for an xHE-AAC service with the given static configuration.
    pub fn xhe_aac(rate_hz: u32, stereo: bool, config: Vec<u8>) -> Result<Self, CodecError> {
        let code = Self::sample_rate_code(DrmAudioCoding::XheAac, rate_hz).ok_or_else(|| {
            CodecError::InvalidConfig(format!("xHE-AAC rate {rate_hz} Hz cannot be signalled"))
        })?;
        Ok(AudioInfo {
            coding: AudioCodingField::XheAac,
            sbr: false,
            mode: if stereo {
                AudioMode::Stereo
            } else {
                AudioMode::Mono
            },
            sample_rate_code: code,
            text_flag: false,
            enhancement_flag: false,
            coder_field: 0,
            xhe_aac_config: config,
        })
    }

    /// Builder-style setter for the text flag.
    pub fn with_text_flag(mut self, text: bool) -> Self {
        self.text_flag = text;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aac_roundtrip_matches_dream_layout() {
        // HE-AAC, 12 kHz core, mono, text: coding 00, SBR 1, mode 00, rate 001 -> 0x21;
        // text 1, enh 0, coder 00000, rfa 0 -> 0x80.
        let info = AudioInfo::aac(12_000, true, AudioMode::Mono)
            .unwrap()
            .with_text_flag(true);
        let t9 = info.to_type9_bytes();
        assert_eq!(t9, vec![0x21, 0x80]);
        assert_eq!(AudioInfo::from_type9_bytes(&t9).unwrap(), info);
        assert_eq!(info.sample_rate(), Some(12_000));
        assert_eq!(info.drm_audio_coding(), Some(DrmAudioCoding::Aac));

        // AAC stereo 24 kHz, no SBR: 00 0 10 011 = 0x13.
        let st = AudioInfo::aac(24_000, false, AudioMode::Stereo).unwrap();
        assert_eq!(st.to_type9_bytes(), vec![0x13, 0x00]);
        // HE-AAC v2: 00 1 01 001 = 0x29.
        let ps = AudioInfo::aac(12_000, true, AudioMode::ParametricStereo).unwrap();
        assert_eq!(ps.to_type9_bytes()[0], 0x29);
        assert!(AudioInfo::aac(12_000, false, AudioMode::ParametricStereo).is_err());
        assert!(AudioInfo::aac(11_025, false, AudioMode::Mono).is_err());
    }

    #[test]
    fn entity_body_roundtrip() {
        let info = AudioInfo::xhe_aac(24_000, true, vec![0xDE, 0xAD]).unwrap();
        let body = info.to_entity_body(2, 1);
        // 4 bits of ids + 20 bits + 16 bits of config = 40 bits = 5 bytes.
        assert_eq!(body.len(), 5);
        let (sid, stid, back) = AudioInfo::from_entity_body(&body).unwrap();
        assert_eq!((sid, stid), (2, 1));
        assert_eq!(back, info);
        assert_eq!(back.sample_rate(), Some(24_000));
        assert_eq!(back.drm_audio_coding(), Some(DrmAudioCoding::XheAac));
    }

    #[test]
    fn opus_signalling_forms() {
        let legacy = AudioInfo::opus(OpusSignalling::Legacy);
        assert_eq!(legacy.to_type9_bytes(), vec![0x07, 0x00]);
        assert_eq!(legacy.drm_audio_coding(), Some(DrmAudioCoding::Opus));
        assert_eq!(legacy.sample_rate(), Some(48_000));
        let mjf = AudioInfo::opus(OpusSignalling::CodingField);
        assert_eq!(mjf.to_type9_bytes(), vec![0x45, 0x00]);
        assert_eq!(
            AudioInfo::from_type9_bytes(&[0x45, 0x00])
                .unwrap()
                .drm_audio_coding(),
            Some(DrmAudioCoding::Opus)
        );
        // Reserved coding -> no decoder.
        assert_eq!(
            AudioInfo::from_type9_bytes(&[0x80, 0])
                .unwrap()
                .drm_audio_coding(),
            None
        );
    }
}
