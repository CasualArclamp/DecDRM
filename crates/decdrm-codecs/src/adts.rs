//! AAC in ADTS frames (ISO/IEC 13818-7 §6.2, ISO/IEC 14496-3 §1.A.2), the transport of
//! AAC internet radio streams, through FDK-AAC:
//!
//! * [`FdkAdtsDecoder`] decodes AAC-LC, HE-AAC (SBR) and HE-AAC v2 (SBR + parametric
//!   stereo). ADTS signals SBR and PS only *implicitly* — the header says AAC-LC at the
//!   core rate — so FDK finds them in the payload and then delivers twice the core rate
//!   (and stereo for PS). Multichannel streams are downmixed to stereo.
//! * [`FdkAdtsEncoder`] produces such streams (the station's web stream tests serve them
//!   to the web stream input).
//!
//! See [`crate::fdk`] for why the wrappers are `Send` but not `Sync`.

use std::ptr::NonNull;

use decdrm_fdk_sys as ffi;

use crate::fdk::{AacProfile, AacStreamInfo, dec_err, enc_err, stream_info_from};
use crate::{CodecError, PcmFrame};

/// Decoder output buffer in samples (all channels): an HE-AAC frame of 2048 samples × 2
/// channels with ample room (FDK limits the output to two channels here).
const OUT_CAPACITY: usize = 2048 * 8;

/// AAC decoder for ADTS streams (FDK-AAC with transport `TT_MP4_ADTS`).
pub struct FdkAdtsDecoder {
    handle: NonNull<ffi::AAC_DECODER_INSTANCE>,
    input: Vec<u8>,
    out: Vec<i16>,
}

// SAFETY: exclusively owned FDK instance without thread affinity (see crate::fdk docs).
unsafe impl Send for FdkAdtsDecoder {}

impl Drop for FdkAdtsDecoder {
    fn drop(&mut self) {
        // SAFETY: the handle came from aacDecoder_Open and is closed exactly once here.
        unsafe { ffi::aacDecoder_Close(self.handle.as_ptr()) }
    }
}

impl FdkAdtsDecoder {
    /// A decoder with at most two output channels.
    pub fn new() -> Result<Self, CodecError> {
        // SAFETY: plain constructor call; NULL is handled below.
        let raw = unsafe { ffi::aacDecoder_Open(ffi::TT_MP4_ADTS, 1) };
        let handle = NonNull::new(raw).ok_or(CodecError::Fdk {
            code: 0,
            name: "aacDecoder_Open failed",
            context: "open",
        })?;
        let dec = Self {
            handle,
            input: Vec::with_capacity(8192),
            out: vec![0; OUT_CAPACITY],
        };
        // SAFETY: valid handle; plain integer parameter.
        let err = unsafe {
            ffi::aacDecoder_SetParam(dec.handle.as_ptr(), ffi::AAC_PCM_MAX_OUTPUT_CHANNELS, 2)
        };
        if err != ffi::AAC_DEC_OK {
            return Err(dec_err(err, "limiting output channels"));
        }
        Ok(dec)
    }

    /// Decodes ADTS bytes — whole frames (header included), as a framer cuts them, or
    /// any pieces of the stream — and returns the frames FDK completed. FDK may hold a
    /// frame back until the next one arrives. A damaged frame yields FDK's concealment
    /// (`concealed = true`); bytes it cannot synchronise to are skipped.
    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<PcmFrame>, CodecError> {
        let mut frames = Vec::new();
        self.input.clear();
        self.input.extend_from_slice(data);
        let mut offset = 0;
        while offset < self.input.len() {
            let size = (self.input.len() - offset) as u32;
            let mut valid = size;
            // SAFETY: the pointer and size describe the live tail of `input`; FDK copies
            // from it and reports in `valid` how many bytes it did not take.
            let mut ptr = self.input[offset..].as_mut_ptr();
            let err = unsafe { ffi::aacDecoder_Fill(self.handle.as_ptr(), &mut ptr, &size, &mut valid) };
            if err != ffi::AAC_DEC_OK {
                return Err(dec_err(err, "filling input"));
            }
            let taken = (size - valid.min(size)) as usize;
            offset += taken;
            let decoded = self.drain(&mut frames)?;
            if taken == 0 && decoded == 0 {
                // FDK's input buffer is full and nothing decodes: drop what it holds.
                // SAFETY: valid handle; documented to flush the transport buffer.
                unsafe { ffi::aacDecoder_SetParam(self.handle.as_ptr(), ffi::AAC_TPDEC_CLEAR_BUFFER, 1) };
                return Err(CodecError::CorruptFrame("ADTS input does not decode".into()));
            }
        }
        Ok(frames)
    }

    /// Decodes every frame FDK has buffered; returns how many.
    fn drain(&mut self, frames: &mut Vec<PcmFrame>) -> Result<usize, CodecError> {
        let mut n = 0;
        // A frame per call; the bound only guards against a decoder that never asks for
        // more input.
        for _ in 0..64 {
            // SAFETY: `out` is a live buffer of OUT_CAPACITY samples, the size passed.
            let err = unsafe {
                ffi::aacDecoder_DecodeFrame(self.handle.as_ptr(), self.out.as_mut_ptr(), OUT_CAPACITY as i32, 0)
            };
            if err == ffi::AAC_DEC_NOT_ENOUGH_BITS {
                break;
            }
            if err == ffi::AAC_DEC_TRANSPORT_SYNC_ERROR {
                // FDK skipped bytes looking for the next header; try again.
                continue;
            }
            if !ffi::IS_OUTPUT_VALID(err) {
                return Err(dec_err(err, "decoding"));
            }
            let Some(si) = self.stream_info() else { break };
            let total = usize::from(si.channels) * si.frame_size;
            if si.channels == 0 || total == 0 || total > OUT_CAPACITY {
                break;
            }
            frames.push(PcmFrame {
                sample_rate: si.sample_rate,
                channels: si.channels,
                samples: self.out[..total].iter().map(|&s| f32::from(s) / 32768.0).collect(),
                concealed: err != ffi::AAC_DEC_OK,
            });
            n += 1;
        }
        Ok(n)
    }

    /// Stream information (after the first decoded frame).
    pub fn stream_info(&self) -> Option<AacStreamInfo> {
        // SAFETY: valid handle; the returned pointer points into the instance and is only
        // copied immediately, while `self` is borrowed.
        let p = unsafe { ffi::aacDecoder_GetStreamInfo(self.handle.as_ptr()) };
        if p.is_null() {
            return None;
        }
        // SAFETY: non-null pointer to a CStreamInfo inside the live instance.
        let si = unsafe { *p };
        (si.sampleRate > 0).then(|| stream_info_from(&si))
    }

    /// The coding: `"AAC-LC"`, `"HE-AAC"` or `"HE-AAC v2"` (after the first decoded frame).
    pub fn coding(&self) -> Option<&'static str> {
        let si = self.stream_info()?;
        // Parametric stereo: a mono core decoded to stereo.
        let ps = si.ps || (si.channels == 2 && si.core_channels == 1);
        let sbr = si.sbr || si.sample_rate > si.core_sample_rate;
        Some(match (sbr, ps) {
            (true, true) => "HE-AAC v2",
            (true, false) => "HE-AAC",
            _ => "AAC-LC",
        })
    }
}

/// ADTS AAC encoder (FDK-AAC with transport `TT_MP4_ADTS`, constant bit rate, implicit
/// SBR/PS signalling as broadcasters' streams have it).
pub struct FdkAdtsEncoder {
    handle: NonNull<ffi::AACENCODER>,
    input: Vec<i16>,
    output: Vec<u8>,
    frame_len: usize,
    channels: usize,
}

// SAFETY: as for the decoder — exclusively owned instance without thread affinity.
unsafe impl Send for FdkAdtsEncoder {}

impl Drop for FdkAdtsEncoder {
    fn drop(&mut self) {
        let mut h = self.handle.as_ptr();
        // SAFETY: the handle came from aacEncOpen and is closed exactly once here.
        unsafe { ffi::aacEncClose(&mut h) };
    }
}

impl FdkAdtsEncoder {
    /// An encoder for `channels` (1 or 2; HE-AAC v2 needs 2) of PCM at `sample_rate` Hz
    /// (with SBR the core runs at half of it) at `bitrate` bit/s.
    pub fn new(profile: AacProfile, sample_rate: u32, channels: usize, bitrate: u32) -> Result<Self, CodecError> {
        if !(1..=2).contains(&channels) || (profile == AacProfile::HeAacV2 && channels != 2) {
            return Err(CodecError::InvalidConfig(format!("{channels} channels for {profile:?}")));
        }
        let mut raw: ffi::HANDLE_AACENCODER = std::ptr::null_mut();
        // SAFETY: valid out-pointer; 0 = allocate all encoder modules.
        let err = unsafe { ffi::aacEncOpen(&mut raw, 0, channels as u32) };
        let handle = match NonNull::new(raw) {
            Some(h) if err == ffi::AACENC_OK => h,
            _ => return Err(enc_err(err, "aacEncOpen")),
        };
        let mut enc = Self { handle, input: Vec::new(), output: Vec::new(), frame_len: 0, channels };
        let aot = match profile {
            AacProfile::Lc => ffi::AOT_AAC_LC,
            AacProfile::HeAac => ffi::AOT_SBR,
            AacProfile::HeAacV2 => ffi::AOT_PS,
        };
        let mode = if channels == 2 { ffi::MODE_2 } else { ffi::MODE_1 };
        let params: [(ffi::AACENC_PARAM, u32, &'static str); 7] = [
            (ffi::AACENC_AOT, aot as u32, "AOT"),
            (ffi::AACENC_SAMPLERATE, sample_rate, "sample rate"),
            (ffi::AACENC_CHANNELMODE, mode as u32, "channel mode"),
            (ffi::AACENC_TRANSMUX, ffi::TT_MP4_ADTS as u32, "ADTS transport"),
            (ffi::AACENC_SIGNALING_MODE, 0, "implicit signalling"),
            (ffi::AACENC_BITRATE, bitrate, "bit rate"),
            (ffi::AACENC_AFTERBURNER, 1, "afterburner"),
        ];
        for (param, value, what) in params {
            // SAFETY: valid handle; integer parameters.
            let err = unsafe { ffi::aacEncoder_SetParam(enc.handle.as_ptr(), param, value) };
            if err != ffi::AACENC_OK {
                return Err(enc_err(err, what));
            }
        }
        // SAFETY: a call with NULL buffers initialises the encoder (aacenc_lib.h).
        let err = unsafe {
            ffi::aacEncEncode(
                enc.handle.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
            )
        };
        if err != ffi::AACENC_OK {
            return Err(enc_err(err, "initialising (check bit rate / sample rate)"));
        }
        // SAFETY: zeroed POD out-struct filled by aacEncInfo.
        let mut info: ffi::AACENC_InfoStruct = unsafe { std::mem::zeroed() };
        // SAFETY: valid handle and out-pointer.
        let err = unsafe { ffi::aacEncInfo(enc.handle.as_ptr(), &mut info) };
        if err != ffi::AACENC_OK {
            return Err(enc_err(err, "aacEncInfo"));
        }
        enc.frame_len = info.frameLength as usize;
        enc.input = vec![0; enc.frame_len * channels];
        enc.output = vec![0; (info.maxOutBufBytes as usize).max(8192)];
        Ok(enc)
    }

    /// Input samples per channel per [`Self::encode`] call (1024, or 2048 with SBR).
    pub fn frame_len(&self) -> usize {
        self.frame_len
    }

    /// Input channels.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Encodes one frame of interleaved PCM (`frame_len() × channels()` samples, nominal
    /// range ±1) and returns the ADTS frame — empty while the encoder's delay line fills.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>, CodecError> {
        if pcm.len() != self.input.len() {
            return Err(CodecError::InvalidInput(format!(
                "expected {} samples per frame, got {}",
                self.input.len(),
                pcm.len()
            )));
        }
        for (d, &s) in self.input.iter_mut().zip(pcm) {
            *d = (s * 32768.0).round().clamp(-32768.0, 32767.0) as i16;
        }
        let mut in_ptr = self.input.as_mut_ptr().cast::<std::ffi::c_void>();
        let mut in_id = ffi::IN_AUDIO_DATA;
        let mut in_size = (self.input.len() * 2) as i32;
        let mut in_el = 2i32;
        let in_desc = ffi::AACENC_BufDesc {
            numBufs: 1,
            bufs: &mut in_ptr,
            bufferIdentifiers: &mut in_id,
            bufSizes: &mut in_size,
            bufElSizes: &mut in_el,
        };
        let mut out_ptr = self.output.as_mut_ptr().cast::<std::ffi::c_void>();
        let mut out_id = ffi::OUT_BITSTREAM_DATA;
        let mut out_size = self.output.len() as i32;
        let mut out_el = 1i32;
        let out_desc = ffi::AACENC_BufDesc {
            numBufs: 1,
            bufs: &mut out_ptr,
            bufferIdentifiers: &mut out_id,
            bufSizes: &mut out_size,
            bufElSizes: &mut out_el,
        };
        let in_args = ffi::AACENC_InArgs { numInSamples: self.input.len() as i32, numAncBytes: 0 };
        let mut out_args = ffi::AACENC_OutArgs::default();
        // SAFETY: the descriptors point at live locals and buffers whose sizes are given in
        // bytes as the API requires; FDK does not retain the pointers.
        let err = unsafe { ffi::aacEncEncode(self.handle.as_ptr(), &in_desc, &out_desc, &in_args, &mut out_args) };
        if err != ffi::AACENC_OK {
            return Err(enc_err(err, "encoding"));
        }
        let n = out_args.numOutBytes.max(0) as usize;
        Ok(self.output[..n.min(self.output.len())].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tone through the ADTS encoder and decoder: every profile comes back at the
    /// right rate and channel count, with the tone's frequency and level.
    #[test]
    fn adts_round_trip() {
        for (profile, rate, channels, bitrate, want) in [
            (AacProfile::Lc, 48_000, 1, 64_000, "AAC-LC"),
            (AacProfile::HeAac, 44_100, 2, 48_000, "HE-AAC"),
            (AacProfile::HeAacV2, 48_000, 2, 32_000, "HE-AAC v2"),
        ] {
            let mut enc = FdkAdtsEncoder::new(profile, rate, channels, bitrate).unwrap();
            let mut dec = FdkAdtsDecoder::new().unwrap();
            let n = enc.frame_len();
            let (freq, amp) = (1000.0, 0.3);
            let mut phase = 0.0f64;
            let mut decoded = Vec::new();
            let mut out_rate = 0;
            let mut out_ch = 0;
            for _ in 0..80 {
                let mut pcm = Vec::with_capacity(n * channels);
                for _ in 0..n {
                    let v = amp * phase.sin() as f32;
                    phase += std::f64::consts::TAU * freq / f64::from(rate);
                    pcm.extend(std::iter::repeat_n(v, channels));
                }
                let adts = enc.encode(&pcm).unwrap();
                if adts.is_empty() {
                    continue;
                }
                assert_eq!(adts[0], 0xFF, "ADTS sync");
                for f in dec.decode(&adts).unwrap() {
                    out_rate = f.sample_rate;
                    out_ch = usize::from(f.channels);
                    decoded.extend(f.samples.chunks_exact(out_ch).map(|c| c[0]));
                }
            }
            assert_eq!(dec.coding(), Some(want), "{profile:?}");
            assert_eq!((out_rate, out_ch), (rate, channels), "{profile:?}");
            // The last 0.5 s: RMS of a sine of amplitude 0.3 is 0.212.
            let tail = &decoded[decoded.len() - rate as usize / 2..];
            let rms = (tail.iter().map(|v| v * v).sum::<f32>() / tail.len() as f32).sqrt();
            assert!((rms - 0.212).abs() < 0.03, "{profile:?}: RMS {rms}");
            // Zero crossings → frequency.
            let crossings = tail.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
            assert!((crossings as i64 - 500).abs() <= 3, "{profile:?}: {crossings} cycles in 0.5 s");
        }
    }

    /// Garbage between frames is skipped; the decoder carries on.
    #[test]
    fn adts_resynchronises() {
        let mut enc = FdkAdtsEncoder::new(AacProfile::Lc, 48_000, 1, 64_000).unwrap();
        let mut dec = FdkAdtsDecoder::new().unwrap();
        let pcm = vec![0.1f32; enc.frame_len()];
        let mut got = 0;
        for i in 0..40 {
            let mut adts = enc.encode(&pcm).unwrap();
            if i == 20 {
                adts.splice(0..0, [0x12, 0x34, 0x56, 0x78, 0x9A]);
            }
            got += dec.decode(&adts).map(|f| f.len()).unwrap_or(0);
        }
        assert!(got >= 30, "{got} frames decoded");
    }
}
