//! Small helpers for rearranging interleaved multi-channel buffers.
//!
//! The receiver wants either one real channel or an I/Q pair; recordings and sound cards
//! deliver whatever they deliver. These functions bridge the two. They never panic: a trailing
//! partial frame is ignored and out-of-range channel indices yield empty/zero output.

/// Copies channel `channel` out of an interleaved buffer (e.g. the left channel of a stereo
/// recording that carries a real IF signal on one side only).
///
/// Returns an empty vector if `channel >= channels`.
pub fn extract_channel(interleaved: &[f32], channels: usize, channel: usize) -> Vec<f32> {
    if channels == 0 || channel >= channels {
        return Vec::new();
    }
    interleaved.chunks_exact(channels).map(|frame| frame[channel]).collect()
}

/// Averages all channels of each frame into one (e.g. a stereo line-in that carries the same
/// IF signal on both sides).
pub fn downmix_to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    match channels {
        0 => Vec::new(),
        1 => interleaved.to_vec(),
        _ => {
            let scale = 1.0 / channels as f32;
            interleaved
                .chunks_exact(channels)
                .map(|frame| frame.iter().sum::<f32>() * scale)
                .collect()
        }
    }
}

/// Converts an interleaved buffer from `in_channels` to `out_channels`, appending to `out`.
///
/// * same count: copied unchanged;
/// * mono input: the single channel goes to the first two outputs (front left/right) — the
///   mono → stereo upmix for mono programmes; any further channels of a surround device
///   (centre, LFE, rears) are left silent;
/// * mono output: all input channels are averaged;
/// * otherwise: the first `min(in, out)` channels are copied and any extra output channels
///   are filled with silence (e.g. stereo on an 8-channel headset or HDMI output).
pub fn remap_channels_into(
    input: &[f32],
    in_channels: usize,
    out_channels: usize,
    out: &mut Vec<f32>,
) {
    if in_channels == 0 || out_channels == 0 {
        return;
    }
    let frames = input.len() / in_channels;
    out.reserve(frames * out_channels);
    if in_channels == out_channels {
        out.extend_from_slice(&input[..frames * in_channels]);
    } else if in_channels == 1 {
        let fronts = out_channels.min(2);
        for &s in &input[..frames] {
            out.extend(std::iter::repeat_n(s, fronts));
            out.extend(std::iter::repeat_n(0.0, out_channels - fronts));
        }
    } else if out_channels == 1 {
        let scale = 1.0 / in_channels as f32;
        out.extend(
            input
                .chunks_exact(in_channels)
                .map(|frame| frame.iter().sum::<f32>() * scale),
        );
    } else {
        let common = in_channels.min(out_channels);
        for frame in input.chunks_exact(in_channels) {
            out.extend_from_slice(&frame[..common]);
            out.extend(std::iter::repeat_n(0.0, out_channels - common));
        }
    }
}

/// Allocating version of [`remap_channels_into`].
pub fn remap_channels(input: &[f32], in_channels: usize, out_channels: usize) -> Vec<f32> {
    let mut out = Vec::new();
    remap_channels_into(input, in_channels, out_channels, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_and_downmix() {
        let st = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(extract_channel(&st, 2, 0), vec![1.0, 3.0]);
        assert_eq!(extract_channel(&st, 2, 1), vec![2.0, 4.0]);
        assert!(extract_channel(&st, 2, 2).is_empty());
        assert!(extract_channel(&st, 0, 0).is_empty());
        assert_eq!(downmix_to_mono(&st, 2), vec![1.5, 3.5]);
        assert_eq!(downmix_to_mono(&st, 1), st.to_vec());
    }

    #[test]
    fn remap() {
        assert_eq!(remap_channels(&[1.0, 2.0], 1, 2), vec![1.0, 1.0, 2.0, 2.0]);
        assert_eq!(remap_channels(&[1.0], 1, 4), vec![1.0, 1.0, 0.0, 0.0]);
        assert_eq!(remap_channels(&[1.0, 3.0], 2, 1), vec![2.0]);
        assert_eq!(remap_channels(&[1.0, 2.0], 2, 3), vec![1.0, 2.0, 0.0]);
        assert_eq!(remap_channels(&[1.0, 2.0, 3.0], 3, 2), vec![1.0, 2.0]);
        assert_eq!(remap_channels(&[1.0, 2.0, 9.0], 2, 2), vec![1.0, 2.0]);
        assert!(remap_channels(&[1.0], 0, 2).is_empty());
    }
}
