//! A burst guard around [`EvsDecoder`]: frames whose decoded audio clips or jumps far
//! above the previous frame are replaced by concealment. Needed for KCBS, whose
//! encoder writes some frames the 3GPP decoder turns into loud bursts (see
//! [`crate::kcbs::unreliable`] for the frame types concealed up front).
//!
//! The output runs one frame (20 ms) behind the input, so that the frame before a
//! burst, which often caused it (a bad frame corrupts the decoder state and the burst
//! appears one frame later), can be concealed too. The decoder state cannot be copied
//! (the reference decoder keeps heap objects behind it), so on a burst a fresh decoder
//! replays the last [`HISTORY`] frames with the earlier decisions, then decodes the
//! two frames as lost and takes over. EVS converges within a few frames, and decoding
//! costs well under a millisecond per frame, so a replay is cheap.

use crate::decoder::{EvsDecoder, EvsError};
use std::collections::VecDeque;

/// Frames replayed into a fresh decoder (0.5 s).
pub const HISTORY: usize = 25;
/// A frame whose peak reaches this (of 1.0) counts as a burst: EVS speech hardly gets
/// there, the bursts clip.
const CLIP: f32 = 0.92;
/// ... as does one more than this many dB above the previous frame's level, if it is
/// also loud (peak above [`LOUD`]).
const JUMP_DB: f32 = 15.0;
const LOUD: f32 = 0.35;

/// One frame of input as decided: `None` = decoded as lost.
type Decided = Option<Vec<u8>>;

/// The burst-guarded decoder (mono).
pub struct GuardedDecoder {
    decoder: EvsDecoder,
    rate: u32,
    /// The last frames as decided, newest last (the pending frame included).
    history: VecDeque<Decided>,
    /// The decoded audio of the newest frame, not yet handed out.
    pending: Option<Vec<f32>>,
    /// RMS level (dB) of the frame before the pending one.
    prev_db: f32,
    /// Frames replaced because of bursts.
    pub bursts: u64,
}

impl GuardedDecoder {
    pub fn new(output_rate: u32) -> Result<Self, EvsError> {
        Ok(Self {
            decoder: EvsDecoder::new(output_rate)?,
            rate: output_rate,
            history: VecDeque::with_capacity(HISTORY + 1),
            pending: None,
            prev_db: -120.0,
            bursts: 0,
        })
    }

    /// Output sampling rate, Hz.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Decode the next frame (`None` = lost or to be concealed) and return the audio of
    /// the frame before it (empty for the very first call), 20 ms of mono samples.
    pub fn push(&mut self, frame: Option<&[u8]>) -> Result<Vec<f32>, EvsError> {
        let mut out = self.decoder.decode(frame)?;
        self.remember(frame.map(<[u8]>::to_vec));
        if frame.is_some() && self.is_burst(&out) {
            // Conceal this frame and the pending one with a replayed decoder.
            let (prev, cur) = self.replay_concealing_last_two()?;
            self.bursts += 1;
            if self.pending.is_some() {
                self.pending = Some(prev);
            }
            out = cur;
        }
        let emitted = self.pending.replace(out).unwrap_or_default();
        if !emitted.is_empty() {
            self.prev_db = level_db(&emitted);
        }
        Ok(emitted)
    }

    fn remember(&mut self, frame: Decided) {
        if self.history.len() == HISTORY + 1 {
            self.history.pop_front();
        }
        self.history.push_back(frame);
    }

    /// Whether `out`, the newest frame's audio, is a burst: clipping, or loud and far
    /// above the frame before it (the pending one, or the last emitted one).
    fn is_burst(&self, out: &[f32]) -> bool {
        let peak = out.iter().fold(0f32, |m, s| m.max(s.abs()));
        let before = self.pending.as_deref().map_or(self.prev_db, level_db);
        peak >= CLIP || (peak >= LOUD && level_db(out) - before > JUMP_DB)
    }

    /// A fresh decoder replays the history but the last two frames, then decodes those
    /// two as lost; it replaces the current decoder. Returns its audio for the two.
    fn replay_concealing_last_two(&mut self) -> Result<(Vec<f32>, Vec<f32>), EvsError> {
        let mut fresh = EvsDecoder::new(self.rate)?;
        let n = self.history.len();
        for f in self.history.iter().take(n.saturating_sub(2)) {
            fresh.decode(f.as_deref())?;
        }
        let prev = fresh.decode(None)?;
        let cur = fresh.decode(None)?;
        for k in n.saturating_sub(2)..n {
            self.history[k] = None;
        }
        self.decoder = fresh;
        Ok((prev, cur))
    }
}

/// RMS level in dB (full scale 1.0).
fn level_db(x: &[f32]) -> f32 {
    if x.is_empty() {
        return -120.0;
    }
    let ms = x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32;
    10.0 * (ms + 1e-12).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_runs_one_frame_behind() {
        let mut g = GuardedDecoder::new(48_000).unwrap();
        let mut frame = [0u8; 33];
        frame[0] = 0x74; // inactive, super-wideband
        assert!(g.push(Some(&frame)).unwrap().is_empty(), "nothing before the first frame");
        for _ in 0..30 {
            let pcm = g.push(Some(&frame)).unwrap();
            assert_eq!(pcm.len(), 960);
            assert!(pcm.iter().all(|s| s.is_finite()));
        }
        assert_eq!(g.push(None).unwrap().len(), 960);
        assert!(g.history.len() <= HISTORY + 1);
    }

    #[test]
    fn levels() {
        assert!((level_db(&[1.0; 10]) - 0.0).abs() < 1e-3);
        assert!((level_db(&[0.1; 10]) + 20.0).abs() < 1e-3);
        assert_eq!(level_db(&[]), -120.0);
    }
}
