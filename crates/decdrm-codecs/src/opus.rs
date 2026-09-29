//! Opus in DRM — Dream's non-standard extension (Dream `sourcedecoders/opus_codec.cpp`,
//! `AudioSourceEncoder.cpp`), reproduced bit for bit:
//!
//! * 48 kHz, 20 ms Opus packets (960 samples per channel); 20 packets per 400 ms audio
//!   super frame, each with an explicit frame border (the super-frame layout is handled by
//!   `decdrm-core`);
//! * every packet has a one-byte CRC carried in the super frame's `aac_crc_bits`
//!   position: the DRM CRC-8 over the packet **without its last byte**
//!   ([`crate::crc::dream_opus_crc`]);
//! * the encoder runs CBR (VBR off), complexity 10, 16-bit LSB depth, no DTX;
//! * the receiver decodes with a 48 kHz **stereo** Opus decoder; when the CRC fails the
//!   packet is decoded with Opus' in-band FEC flag set (`decode_fec = 1`), which yields the
//!   packet's redundant (LBRR) data or packet-loss concealment.
//!
//! Deviation from Dream: Dream asks for 80 ms of output (`frame_size = 3840`) in its FEC
//! call, which makes libopus run PLC for 60 ms before the FEC part; DecDRM asks for exactly
//! the packet's duration so every call returns 20 ms.
//!
//! See [`crate::fdk`] for why the wrappers are `Send` but not `Sync`.

use std::ffi::CStr;
use std::ptr::NonNull;

use decdrm_opus_sys as ffi;

use crate::crc::dream_opus_crc;
use crate::sdc::{AudioInfo, OpusSignalling};
use crate::{CodecError, DrmAudioDecoder, PcmFrame};

/// Opus sampling rate used by Dream's scheme.
pub const OPUS_SAMPLE_RATE: u32 = 48_000;
/// Samples per channel of one Dream Opus packet (20 ms).
pub const OPUS_FRAME_LEN: usize = 960;
/// Largest packet Opus can produce (bytes).
pub const OPUS_MAX_PACKET: usize = 1275;
/// Longest possible Opus packet duration (120 ms) in samples per channel.
const MAX_DURATION: usize = 5760;

fn opus_err(code: i32, context: &'static str) -> CodecError {
    // SAFETY: opus_strerror returns a static NUL-terminated string for any input.
    let msg = unsafe { CStr::from_ptr(ffi::opus_strerror(code)) }
        .to_string_lossy()
        .into_owned();
    CodecError::Opus {
        code,
        message: msg,
        context,
    }
}

/// The libopus version string, e.g. `"libopus 1.6.1"`.
pub fn opus_version() -> String {
    // SAFETY: returns a static NUL-terminated string.
    unsafe { CStr::from_ptr(ffi::opus_get_version_string()) }
        .to_string_lossy()
        .into_owned()
}

/// Human-readable description of an Opus TOC byte (RFC 6716 §3.1).
fn describe_toc(toc: u8) -> String {
    let config = toc >> 3;
    let stereo = if toc & 0x04 != 0 { "stereo" } else { "mono" };
    let (mode, bw, ms) = match config {
        0..=11 => (
            "SILK",
            ["narrowband", "mediumband", "wideband"][usize::from(config / 4)],
            [10.0, 20.0, 40.0, 60.0][usize::from(config % 4)],
        ),
        12..=15 => (
            "hybrid",
            ["super-wideband", "fullband"][usize::from((config - 12) / 2)],
            [10.0, 20.0][usize::from(config % 2)],
        ),
        _ => (
            "CELT",
            ["narrowband", "wideband", "super-wideband", "fullband"]
                [usize::from((config - 16) / 4)],
            [2.5, 5.0, 10.0, 20.0][usize::from(config % 4)],
        ),
    };
    format!("Opus {mode} {bw} {stereo}, {ms} ms frames, 48 kHz")
}

/// Decoder for Dream-style Opus-in-DRM frames (always 48 kHz stereo output).
pub struct OpusDrmDecoder {
    handle: NonNull<ffi::OpusDecoder>,
    pcm: Vec<f32>,
    last_duration: usize,
    last_good_toc: Option<u8>,
    /// State of libopus' soft clipper (one value per channel).
    softclip: [f32; 2],
}

// SAFETY: exclusively owned libopus state without thread affinity (see crate::fdk docs).
unsafe impl Send for OpusDrmDecoder {}

impl Drop for OpusDrmDecoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_decoder_create, destroyed exactly once.
        unsafe { ffi::opus_decoder_destroy(self.handle.as_ptr()) }
    }
}

impl OpusDrmDecoder {
    /// Creates a 48 kHz stereo decoder (as Dream's `OpusCodec::DecOpen`).
    pub fn new() -> Result<Self, CodecError> {
        let mut err = 0;
        // SAFETY: valid arguments and out-pointer.
        let raw = unsafe { ffi::opus_decoder_create(OPUS_SAMPLE_RATE as i32, 2, &mut err) };
        let handle = NonNull::new(raw).ok_or_else(|| opus_err(err, "opus_decoder_create"))?;
        Ok(Self {
            handle,
            pcm: vec![0.0; 2 * MAX_DURATION],
            last_duration: OPUS_FRAME_LEN,
            last_good_toc: None,
            softclip: [0.0; 2],
        })
    }

    /// Runs `opus_decode_float`; `packet = None` runs packet-loss concealment.
    fn run(
        &mut self,
        packet: Option<&[u8]>,
        frame_size: usize,
        fec: bool,
    ) -> Result<usize, CodecError> {
        let (ptr, len) = match packet {
            Some(p) => (p.as_ptr(), p.len() as i32),
            None => (std::ptr::null(), 0),
        };
        // SAFETY: `ptr`/`len` describe a live slice (or NULL/0 for PLC); `pcm` holds
        // 2 × MAX_DURATION floats and frame_size <= MAX_DURATION.
        let n = unsafe {
            ffi::opus_decode_float(
                self.handle.as_ptr(),
                ptr,
                len,
                self.pcm.as_mut_ptr(),
                frame_size.min(MAX_DURATION) as i32,
                i32::from(fec),
            )
        };
        if n < 0 {
            return Err(opus_err(n, "opus_decode_float"));
        }
        // Float output is not clipped by libopus; damaged packets can exceed ±1. Apply the
        // same soft clipper its 16-bit API uses.
        // SAFETY: `pcm` holds at least 2 × n floats; `softclip` has one entry per channel.
        unsafe { ffi::opus_pcm_soft_clip(self.pcm.as_mut_ptr(), n, 2, self.softclip.as_mut_ptr()) };
        // Guard against rounding just outside the range.
        for s in &mut self.pcm[..2 * n as usize] {
            *s = s.clamp(-1.0, 1.0);
        }
        Ok(n as usize)
    }

    fn frame(&self, n: usize, concealed: bool) -> PcmFrame {
        PcmFrame {
            sample_rate: OPUS_SAMPLE_RATE,
            channels: 2,
            samples: self.pcm[..2 * n].to_vec(),
            concealed,
        }
    }

    /// Duration of a packet in samples at 48 kHz, if it parses.
    fn packet_duration(packet: &[u8]) -> Option<usize> {
        // SAFETY: valid slice pointer/length.
        let n = unsafe {
            ffi::opus_packet_get_nb_samples(
                packet.as_ptr(),
                packet.len() as i32,
                OPUS_SAMPLE_RATE as i32,
            )
        };
        (n > 0 && n as usize <= MAX_DURATION).then_some(n as usize)
    }
}

impl DrmAudioDecoder for OpusDrmDecoder {
    /// Decodes one Opus packet. With `crc = Some(c)` the packet is checked with Dream's CRC
    /// and decoded with FEC when it does not match (`concealed = true`); `None` skips the
    /// check. Undecodable packets are replaced by packet-loss concealment.
    fn decode(&mut self, frame: &[u8], crc: Option<u8>) -> Result<PcmFrame, CodecError> {
        if frame.is_empty() {
            return self.conceal();
        }
        let crc_ok = crc.is_none_or(|c| c == dream_opus_crc(frame));
        let duration = Self::packet_duration(frame);
        let result = if crc_ok {
            self.run(Some(frame), MAX_DURATION, false)
        } else {
            match duration {
                Some(d) => self.run(Some(frame), d, true),
                None => Err(CodecError::CorruptFrame("unparsable Opus packet".into())),
            }
        };
        match result {
            Ok(n) if n > 0 => {
                self.last_duration = n;
                if crc_ok {
                    self.last_good_toc = Some(frame[0]);
                }
                Ok(self.frame(n, !crc_ok))
            }
            _ => {
                let mut pcm = self.conceal()?;
                pcm.concealed = true;
                Ok(pcm)
            }
        }
    }

    /// Packet-loss concealment for one packet duration (20 ms normally).
    fn conceal(&mut self) -> Result<PcmFrame, CodecError> {
        let d = self.last_duration;
        let n = self.run(None, d, false)?;
        Ok(self.frame(n, true))
    }

    fn describe(&self) -> String {
        match self.last_good_toc {
            Some(toc) => describe_toc(toc),
            None => "Opus (48 kHz)".to_string(),
        }
    }
}

/// Opus application / tuning, as Dream's `EOPUSApplication`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OpusApplication {
    /// `OPUS_APPLICATION_AUDIO` (Dream's default).
    #[default]
    Audio,
    /// `OPUS_APPLICATION_VOIP`.
    Voip,
}

/// Opus signal hint, as Dream's `EOPUSSignal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OpusSignal {
    /// Music (Dream's default).
    #[default]
    Music,
    /// Speech.
    Voice,
}

/// Opus audio bandwidth, as Dream's `EOPUSBandwidth`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OpusBandwidth {
    /// 4 kHz.
    Narrowband,
    /// 6 kHz.
    Mediumband,
    /// 8 kHz.
    Wideband,
    /// 12 kHz.
    SuperWideband,
    /// 20 kHz (Dream's default).
    #[default]
    Fullband,
}

/// Configuration of [`OpusDrmEncoder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusEncoderConfig {
    /// 1 or 2 input channels (interleaved at 48 kHz).
    pub channels: usize,
    /// Size of every Opus packet in bytes (CBR), *excluding* the CRC byte, which travels
    /// in the super frame's CRC position. Dream derives it from the super frame:
    /// `(audio payload bytes − header bytes) / 20`, minus one for the CRC.
    pub packet_bytes: usize,
    /// Encoder application mode.
    pub application: OpusApplication,
    /// Signal-type hint.
    pub signal: OpusSignal,
    /// Audio bandwidth (also used as the maximum bandwidth, as Dream does).
    pub bandwidth: OpusBandwidth,
    /// In-band FEC (Dream then also sets 100 % expected packet loss).
    pub fec: bool,
}

impl OpusEncoderConfig {
    /// Mono/stereo configuration with Dream's defaults.
    pub fn new(channels: usize, packet_bytes: usize) -> Self {
        Self {
            channels,
            packet_bytes,
            application: OpusApplication::default(),
            signal: OpusSignal::default(),
            bandwidth: OpusBandwidth::default(),
            fec: false,
        }
    }
}

/// One Opus packet ready for the DRM audio super frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusDrmFrame {
    /// Dream CRC byte (goes where AAC's `aac_crc_bits` go).
    pub crc: u8,
    /// The Opus packet.
    pub data: Vec<u8>,
}

/// Encoder producing Dream-compatible Opus-in-DRM frames.
pub struct OpusDrmEncoder {
    handle: NonNull<ffi::OpusEncoder>,
    config: OpusEncoderConfig,
    buf: Vec<u8>,
}

// SAFETY: exclusively owned libopus state without thread affinity.
unsafe impl Send for OpusDrmEncoder {}

impl Drop for OpusDrmEncoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_encoder_create, destroyed exactly once.
        unsafe { ffi::opus_encoder_destroy(self.handle.as_ptr()) }
    }
}

impl OpusDrmEncoder {
    /// Creates an encoder with Dream's settings (CBR, complexity 10, LSB depth 16, no DTX).
    pub fn new(config: OpusEncoderConfig) -> Result<Self, CodecError> {
        if !(1..=2).contains(&config.channels) {
            return Err(CodecError::InvalidConfig(
                "Opus needs 1 or 2 channels".into(),
            ));
        }
        if !(2..=OPUS_MAX_PACKET).contains(&config.packet_bytes) {
            return Err(CodecError::InvalidConfig(format!(
                "Opus packet size {} outside 2..={OPUS_MAX_PACKET} bytes",
                config.packet_bytes
            )));
        }
        let app = match config.application {
            OpusApplication::Audio => ffi::OPUS_APPLICATION_AUDIO,
            OpusApplication::Voip => ffi::OPUS_APPLICATION_VOIP,
        };
        let mut err = 0;
        // SAFETY: valid arguments and out-pointer.
        let raw = unsafe {
            ffi::opus_encoder_create(
                OPUS_SAMPLE_RATE as i32,
                config.channels as i32,
                app,
                &mut err,
            )
        };
        let handle = NonNull::new(raw).ok_or_else(|| opus_err(err, "opus_encoder_create"))?;
        let enc = Self {
            handle,
            buf: vec![0; OPUS_MAX_PACKET],
            config,
        };
        let c = &enc.config;
        let bw = match c.bandwidth {
            OpusBandwidth::Narrowband => ffi::OPUS_BANDWIDTH_NARROWBAND,
            OpusBandwidth::Mediumband => ffi::OPUS_BANDWIDTH_MEDIUMBAND,
            OpusBandwidth::Wideband => ffi::OPUS_BANDWIDTH_WIDEBAND,
            OpusBandwidth::SuperWideband => ffi::OPUS_BANDWIDTH_SUPERWIDEBAND,
            OpusBandwidth::Fullband => ffi::OPUS_BANDWIDTH_FULLBAND,
        };
        let signal = match c.signal {
            OpusSignal::Music => ffi::OPUS_SIGNAL_MUSIC,
            OpusSignal::Voice => ffi::OPUS_SIGNAL_VOICE,
        };
        // 50 packets per second.
        let bitrate = (c.packet_bytes * 8 * 50) as i32;
        let settings: [(i32, i32, &'static str); 12] = [
            (ffi::OPUS_SET_VBR_REQUEST, 0, "VBR off"),
            (ffi::OPUS_SET_VBR_CONSTRAINT_REQUEST, 0, "VBR constraint"),
            (ffi::OPUS_SET_BITRATE_REQUEST, bitrate, "bit rate"),
            (ffi::OPUS_SET_COMPLEXITY_REQUEST, 10, "complexity"),
            (ffi::OPUS_SET_LSB_DEPTH_REQUEST, 16, "LSB depth"),
            (ffi::OPUS_SET_DTX_REQUEST, 0, "DTX off"),
            (
                ffi::OPUS_SET_FORCE_CHANNELS_REQUEST,
                c.channels as i32,
                "force channels",
            ),
            (ffi::OPUS_SET_BANDWIDTH_REQUEST, bw, "bandwidth"),
            (ffi::OPUS_SET_MAX_BANDWIDTH_REQUEST, bw, "max bandwidth"),
            (
                ffi::OPUS_SET_INBAND_FEC_REQUEST,
                i32::from(c.fec),
                "in-band FEC",
            ),
            (
                ffi::OPUS_SET_PACKET_LOSS_PERC_REQUEST,
                if c.fec { 100 } else { 0 },
                "packet loss",
            ),
            (ffi::OPUS_SET_SIGNAL_REQUEST, signal, "signal type"),
        ];
        for (req, value, what) in settings {
            // SAFETY: setter requests take one opus_int32 argument.
            let r = unsafe { ffi::opus_encoder_ctl(enc.handle.as_ptr(), req, value) };
            if r != ffi::OPUS_OK {
                return Err(opus_err(r, what));
            }
        }
        Ok(enc)
    }

    /// The configuration.
    pub fn config(&self) -> &OpusEncoderConfig {
        &self.config
    }

    /// Encoder lookahead in samples at 48 kHz.
    pub fn lookahead(&self) -> usize {
        let mut v: i32 = 0;
        // SAFETY: getter request with an opus_int32 out-pointer.
        let r = unsafe {
            ffi::opus_encoder_ctl(
                self.handle.as_ptr(),
                ffi::OPUS_GET_LOOKAHEAD_REQUEST,
                &mut v as *mut i32,
            )
        };
        if r == ffi::OPUS_OK {
            v.max(0) as usize
        } else {
            0
        }
    }

    /// SDC audio information for this service (see [`OpusSignalling`]).
    pub fn audio_info(&self, signalling: OpusSignalling) -> AudioInfo {
        AudioInfo::opus(signalling)
    }

    /// Encodes 20 ms of interleaved 48 kHz PCM (`960 × channels` samples, nominal ±1).
    pub fn encode(&mut self, pcm: &[f32]) -> Result<OpusDrmFrame, CodecError> {
        let expected = OPUS_FRAME_LEN * self.config.channels;
        if pcm.len() != expected {
            return Err(CodecError::InvalidInput(format!(
                "expected {expected} samples per Opus frame, got {}",
                pcm.len()
            )));
        }
        // SAFETY: `pcm` holds 960 × channels floats; `buf` holds OPUS_MAX_PACKET bytes and
        // packet_bytes <= OPUS_MAX_PACKET is passed as the limit.
        let n = unsafe {
            ffi::opus_encode_float(
                self.handle.as_ptr(),
                pcm.as_ptr(),
                OPUS_FRAME_LEN as i32,
                self.buf.as_mut_ptr(),
                self.config.packet_bytes as i32,
            )
        };
        if n < 0 {
            return Err(opus_err(n, "opus_encode_float"));
        }
        let data = self.buf[..n as usize].to_vec();
        Ok(OpusDrmFrame {
            crc: dream_opus_crc(&data),
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toc_descriptions() {
        assert_eq!(
            describe_toc(0xFC),
            "Opus CELT fullband stereo, 20 ms frames, 48 kHz"
        );
        assert_eq!(
            describe_toc(0x08),
            "Opus SILK narrowband mono, 20 ms frames, 48 kHz"
        );
        assert_eq!(
            describe_toc(0x70),
            "Opus hybrid fullband mono, 10 ms frames, 48 kHz"
        );
        assert_eq!(
            describe_toc(0x78),
            "Opus hybrid fullband mono, 20 ms frames, 48 kHz"
        );
    }
}
