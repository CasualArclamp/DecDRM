//! Decoding KCBS's EVS stream as well as it goes: the frame types its encoder gets
//! wrong are concealed ([`crate::kcbs::unreliable`]), bursts are caught by the
//! [`GuardedDecoder`], and concealed pauses get [`ComfortNoise`] instead of fading to
//! silence. The noise is measured from the pause frames themselves, decoded normally by
//! a second decoder that sees every frame (its other output is not used).

use crate::comfort::{self, ComfortNoise};
use crate::decoder::{EvsDecoder, EvsError};
use crate::guard::GuardedDecoder;
use crate::kcbs::{is_pause, unreliable};

/// The KCBS decoder (48 kHz mono output, one frame behind its input).
pub struct KcbsDecoder {
    main: GuardedDecoder,
    /// Decodes every frame normally, for measuring the pause noise.
    probe: EvsDecoder,
    noise: ComfortNoise,
    /// Whether the frame the guard hands out next is a pause.
    pending_pause: bool,
    /// Whether the last received frame was a pause (lost frames keep the state).
    in_pause: bool,
}

impl KcbsDecoder {
    pub fn new() -> Result<Self, EvsError> {
        Ok(Self {
            main: GuardedDecoder::new(comfort::RATE)?,
            probe: EvsDecoder::new(comfort::RATE)?,
            noise: ComfortNoise::new(),
            pending_pause: false,
            in_pause: false,
        })
    }

    /// Output sampling rate, Hz.
    pub fn rate(&self) -> u32 {
        comfort::RATE
    }

    /// Frames the burst guard has replaced so far.
    pub fn bursts(&self) -> u64 {
        self.main.bursts
    }

    /// Pause frames measured for the comfort noise so far.
    pub fn noise_frames(&self) -> u64 {
        self.noise.accepted
    }

    /// Decode the next 264-bit frame (`None`: lost) and return 20 ms of audio for the
    /// frame before it (empty for the very first call).
    pub fn push(&mut self, frame: Option<&[u8]>) -> Result<Vec<f32>, EvsError> {
        let probe_out = self.probe.decode(frame)?;
        if let Some(f) = frame {
            self.in_pause = is_pause(f);
            if self.in_pause {
                self.noise.observe(&probe_out);
            }
        }
        let mut out = self.main.push(frame.filter(|f| !unreliable(f)))?;
        let emitted_pause = std::mem::replace(&mut self.pending_pause, self.in_pause);
        if !out.is_empty() {
            self.noise.render(&mut out, emitted_pause);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(x: &[f32]) -> f32 {
        10.0 * (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32 + 1e-15).log10()
    }

    #[test]
    fn pauses_get_comfort_noise_instead_of_silence() {
        let mut d = KcbsDecoder::new().unwrap();
        // Inactive super-wideband frames with all-zero parameters: concealed in the main
        // decoder, which on its own would output silence.
        let mut pause = [0u8; 33];
        pause[0] = 0x74;
        let mut levels = Vec::new();
        for _ in 0..50 {
            let pcm = d.push(Some(&pause)).unwrap();
            if !pcm.is_empty() {
                levels.push(level(&pcm));
            }
        }
        let late = &levels[10..];
        assert!(late.iter().all(|&l| l > -80.0 && l < -50.0), "comfort-noise level: {late:?}");
    }
}
