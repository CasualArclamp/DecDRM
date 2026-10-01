//! The framing of Korean Central Broadcasting Station's DRM service (KCBS, 6140 kHz;
//! worked out from recordings of 2026-09-30). It is signalled as a data service (SDC
//! type 5, user application 0x000, packet mode) but carries 3GPP EVS audio at
//! 13.2 kbit/s. Each 400 ms multiplex frame holds one MSC data group whose data field
//! (712 bytes) is
//!
//! ```text
//! bytes   0..5     first bytes of frames 0, 4, 8, 12 and 16
//! bytes   5..660   5 groups of 131 bytes, group g for frames 4g..4g+3:
//!                    frame 4g without its first byte (32 bytes),
//!                    frames 4g+1, 4g+2, 4g+3 (33 bytes each)
//! bytes 660..712   side data on a 3-frame cycle (the data group's continuity index
//!                  counts 0, 1, 2); in phase 0 a 32-bit counter, +1 per 1.2 s
//! ```
//!
//! — 20 EVS frames of 264 bits, 20 ms each. A data field is recognised when all 20
//! frames signal the same audio bandwidth in their first 5 bits
//! ([`crate::signalling`]); random data does so with a probability of about 10⁻⁸.

use crate::signalling::{BITS_13K2, Bandwidth, signalling_13k2};

/// EVS frames per data group (400 ms).
pub const FRAMES: usize = 20;
/// Bytes of one frame (264 bits).
pub const FRAME_BYTES: usize = BITS_13K2 / 8;
/// Leading bytes of the data field that hold the frames.
pub const AUDIO_BYTES: usize = 5 + 5 * GROUP_BYTES;
/// Bytes of a group of four frames minus the first byte of its first frame.
const GROUP_BYTES: usize = 4 * FRAME_BYTES - 1;
/// Longest data field accepted (KCBS sends 712 bytes).
const MAX_FIELD_BYTES: usize = 2048;

/// The 20 frames of a data field, or `None` if it is too short or too long.
pub fn frames(field: &[u8]) -> Option<[[u8; FRAME_BYTES]; FRAMES]> {
    if !(AUDIO_BYTES..=MAX_FIELD_BYTES).contains(&field.len()) {
        return None;
    }
    let mut out = [[0u8; FRAME_BYTES]; FRAMES];
    for g in 0..5 {
        let base = 5 + GROUP_BYTES * g;
        out[4 * g][0] = field[g];
        out[4 * g][1..].copy_from_slice(&field[base..base + FRAME_BYTES - 1]);
        for j in 0..3 {
            let o = base + FRAME_BYTES - 1 + FRAME_BYTES * j;
            out[4 * g + 1 + j].copy_from_slice(&field[o..o + FRAME_BYTES]);
        }
    }
    Some(out)
}

/// The audio bandwidth all frames of `field` signal, if it has this framing.
pub fn detect(field: &[u8]) -> Option<Bandwidth> {
    let frames = frames(field)?;
    let bandwidth = signalling_13k2(frames[0][0]).bandwidth;
    frames.iter().all(|f| signalling_13k2(f[0]).bandwidth == bandwidth).then_some(bandwidth)
}

/// Whether a frame is of a type KCBS's encoder writes in a form the 3GPP decoder
/// cannot read: INACTIVE, TRANSITION and low-rate MDCT frames. Found with recordings
/// of 2026-09-30: the reference decoder's bit-error checks fire on 34 % of its
/// INACTIVE and 42 % of its MDCT frames (0 % for frames of the 3GPP encoder), and its
/// TRANSITION frames decode as clipping bursts; decoders of EVS 12.0–12.2 fare no
/// better, so the encoder itself departs from the standard there. GENERIC and VOICED
/// frames, most of the speech, decode cleanly. Decoding these frames as lost (EVS
/// concealment) removes most of the glitches: clipped frames in 32 s went from 40–49
/// to 5–9.
pub fn unreliable(frame: &[u8]) -> bool {
    use crate::signalling::CoderType;
    frame.first().is_some_and(|&b| {
        matches!(signalling_13k2(b).coder_type, CoderType::Inactive | CoderType::Transition | CoderType::LowRateMdct)
    })
}

/// The inverse of [`frames`] (the first 660 bytes of a data field), for tests and
/// transmitters.
pub fn pack(frames: &[[u8; FRAME_BYTES]; FRAMES]) -> Vec<u8> {
    let mut field = vec![0u8; AUDIO_BYTES];
    for g in 0..5 {
        let base = 5 + GROUP_BYTES * g;
        field[g] = frames[4 * g][0];
        field[base..base + FRAME_BYTES - 1].copy_from_slice(&frames[4 * g][1..]);
        for j in 0..3 {
            let o = base + FRAME_BYTES - 1 + FRAME_BYTES * j;
            field[o..o + FRAME_BYTES].copy_from_slice(&frames[4 * g + 1 + j]);
        }
    }
    field
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames with a given first byte and otherwise distinct contents.
    fn test_frames(first: impl Fn(usize) -> u8) -> [[u8; FRAME_BYTES]; FRAMES] {
        let mut f = [[0u8; FRAME_BYTES]; FRAMES];
        for (i, frame) in f.iter_mut().enumerate() {
            frame[0] = first(i);
            for (k, b) in frame.iter_mut().enumerate().skip(1) {
                *b = (i * 37 + k * 11) as u8;
            }
        }
        f
    }

    #[test]
    fn layout_round_trip() {
        let f = test_frames(|i| [0x58, 0x53, 0x76, 0x61][i % 4]);
        let mut field = pack(&f);
        assert_eq!(field.len(), 660);
        // The frames' first bytes: 0, 4, .. at the front; 1 at 5 + 32.
        assert_eq!(&field[..5], &[f[0][0], f[4][0], f[8][0], f[12][0], f[16][0]]);
        assert_eq!(field[37], f[1][0]);
        field.extend_from_slice(&[0u8; 52]); // the side data
        assert_eq!(frames(&field), Some(f));
        assert_eq!(detect(&field), Some(Bandwidth::Swb));
    }

    #[test]
    fn unreliable_frame_types() {
        let frame = |first: u8| {
            let mut f = [0u8; FRAME_BYTES];
            f[0] = first;
            f
        };
        // Inactive, transition and low-rate MDCT: concealed.
        assert!(unreliable(&frame(0x76)) && unreliable(&frame(0x61)) && unreliable(&frame(0xf8)));
        // Generic and voiced: decoded.
        assert!(!unreliable(&frame(0x53)) && !unreliable(&frame(0x58)) && !unreliable(&frame(0x9b)));
        assert!(!unreliable(&[]));
    }

    #[test]
    fn detection_needs_one_bandwidth() {
        // One wideband frame among super-wideband ones.
        let mut field = pack(&test_frames(|i| if i == 7 { 0x28 } else { 0x58 }));
        field.resize(712, 0);
        assert_eq!(detect(&field), None);
        // Too short.
        assert_eq!(frames(&field[..600]), None);
        // Pseudo-random data groups are rejected.
        let mut x = 0x1234_5678u32;
        let random = (0..1000)
            .filter(|_| {
                let field: Vec<u8> = (0..712)
                    .map(|_| {
                        x ^= x << 13;
                        x ^= x >> 17;
                        x ^= x << 5;
                        x as u8
                    })
                    .collect();
                detect(&field).is_some()
            })
            .count();
        assert_eq!(random, 0);
    }
}
