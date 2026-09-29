//! Audio super frames (ES 201 980 §5.2–§5.4): splitting the logical frame of an audio
//! stream into the coded audio frames handed to the codec, and the transmitter-side
//! inverse.
//!
//! * **AAC** (§5.4.1, tables 10/11): 5 (12 kHz) or 10 (24 kHz) frames per 400 ms. A
//!   header of `n − 1` 12-bit frame borders (plus 4 padding bits for 10 frames), one
//!   CRC byte per frame, then the frame bytes. Before V4 of the spec a stream could be
//!   split over both protection parts, and the first `num_higher_protected_bytes` of
//!   every frame were then interleaved with the CRC bytes in part A (Dream's
//!   `AACSuperFrame::parse`); V4 only allows a stream to be entirely in part A or part B,
//!   where both layouts coincide. [`AacSuperFrameFormat`] handles both.
//! * **Opus** (Dream's experimental extension, `AudioSourceEncoder.cpp`): the AAC layout
//!   with 20 frames of 20 ms and 20 explicit borders (the last frame's border too, so
//!   the header is 30 bytes); each packet's CRC byte is CRC-8 over the packet without
//!   its last byte (an off-by-one shared by Dream's encoder and decoder).
//! * **xHE-AAC** (§5.3.1): header (frame border count, bit reservoir level, CRC-8),
//!   payload, and a directory of 16-bit frame border descriptions in reverse order at
//!   the end. Audio frames (USAC access unit + CRC-16) run continuously across super
//!   frames, so [`XheAacDeframer`] / [`XheAacFramer`] keep state between calls; frame
//!   border indices 0xFFE/0xFFF place a border 2 or 1 bytes before the end of the
//!   previous payload.
//! * **EnCodec** (DecDRM's experimental extension): the whole audio super frame goes to
//!   the codec as one frame; its layout (codes, CRCs, repetition) is the
//!   `decdrm-encodec` crate's business.
//!
//! When the SDC text flag is set, the last four bytes of the logical frame carry the
//! text message (§6.5) and are not part of the audio super frame
//! ([`split_text_message`]). Dream's AAC receiver does not subtract them (they end up
//! at the end of the last AAC frame, which the decoder ignores); here they are removed,
//! as the spec and Dream's transmitter define.

use super::msc::LogicalFrame;
use super::sdc::StreamLengths;
use super::service::{AudioCodec, AudioParams};
use crate::fec::crc::{crc8, crc16};

/// Bytes of the text message at the end of an audio stream's logical frame (§6.5).
pub const TEXT_MESSAGE_BYTES: usize = 4;

/// Frames per 400 ms super frame of Dream's Opus mode (20 ms packets).
pub const OPUS_FRAMES_PER_SUPER_FRAME: usize = 20;

/// Largest xHE-AAC frame: the 6144-bit bit reservoir per channel (§5.3.1.3) for stereo.
pub const XHE_AAC_MAX_FRAME_BYTES: usize = 2 * 6144 / 8;

/// Most frame border descriptions in one xHE-AAC directory (§5.3.1: no more than 15
/// audio frames per audio super frame).
pub const XHE_AAC_MAX_BORDERS: usize = 15;

/// One coded audio frame, ready for the codec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame {
    /// AAC: the raw frame without its CRC byte. Opus: the Opus packet. xHE-AAC: the
    /// whole audio frame — USAC access unit plus its 16-bit CRC — which is what Dream
    /// hands to FDK-AAC (see [`AudioFrame::usac_access_unit`]). EnCodec: the whole audio
    /// super frame.
    pub data: Vec<u8>,
    /// AAC `aac_crc_bits` — it covers bit ranges of the frame's side information, so
    /// only the AAC decoder can check it (FDK reads it in front of the frame for
    /// `TT_DRM`) — or the Opus packet's CRC byte. `None` for xHE-AAC.
    pub crc_byte: Option<u8>,
    /// Result of the CRC check done here: Opus (Dream's CRC-8) and xHE-AAC (CRC-16 over
    /// the access unit). `None` for AAC.
    pub crc_ok: Option<bool>,
}

impl AudioFrame {
    /// An AAC or Opus frame and its CRC byte (transmitter side).
    pub fn with_crc(data: Vec<u8>, crc: u8) -> Self {
        Self { data, crc_byte: Some(crc), crc_ok: None }
    }

    /// xHE-AAC: the USAC access unit without the trailing audio frame CRC.
    pub fn usac_access_unit(&self) -> &[u8] {
        &self.data[..self.data.len().saturating_sub(2)]
    }
}

/// Header of an xHE-AAC audio super frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct XheHeader {
    pub frame_border_count: u8,
    /// Bit reservoir level (4 bits); the encoder's reservoir fill is
    /// `(level + 1) · 384 · channels` bits.
    pub bit_reservoir_level: u8,
    pub header_crc_ok: bool,
}

/// Errors while splitting or building audio super frames.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AudioError {
    #[error("audio super frame of {0} bytes is too short")]
    TooShort(usize),
    #[error("frame border {border} ({value}) is not in increasing order or beyond the payload")]
    BadBorder { border: usize, value: usize },
    #[error("frame {0} is shorter than the higher protected part")]
    FrameTooShort(usize),
    #[error("xHE-AAC directory does not match the frame border count")]
    Directory,
    #[error("xHE-AAC frame border 0xFFE/0xFFF refers to data that was not received")]
    DelayedBorder,
    #[error("xHE-AAC frame larger than the decoder buffer")]
    Overflow,
    #[error("{got} frames given, the super frame holds {want}")]
    FrameCount { got: usize, want: usize },
    #[error("frames need {need} bytes, the payload holds {have}")]
    PayloadSize { need: usize, have: usize },
    #[error("unsupported audio configuration: {0}")]
    Unsupported(&'static str),
    /// xHE-AAC transmitter: the queued audio frames do not fill the next payload (the
    /// encoder must run further ahead of the channel).
    #[error("xHE-AAC payload of {need} bytes but only {have} bytes of audio frames queued")]
    XheUnderrun { need: usize, have: usize },
    /// xHE-AAC transmitter: more frames start in one payload than a directory can list.
    #[error("more than {max} xHE-AAC frames start in one audio super frame", max = XHE_AAC_MAX_BORDERS)]
    XheTooManyFrames,
}

// ---------------------------------------------------------------------------------
// Text message bytes
// ---------------------------------------------------------------------------------

/// Split an audio stream's logical frame into the audio super frame and, when the text
/// flag is set, the four text message bytes at its end (§6.5).
pub fn split_text_message(logical_frame: &[u8], text_flag: bool) -> (&[u8], Option<[u8; 4]>) {
    if !text_flag || logical_frame.len() < TEXT_MESSAGE_BYTES {
        return (logical_frame, None);
    }
    let (audio, text) = logical_frame.split_at(logical_frame.len() - TEXT_MESSAGE_BYTES);
    // `try_into` converts the 4-byte slice into an array; it cannot fail here.
    (audio, text.try_into().ok())
}

/// Write a text message piece into the last four bytes of a logical frame.
pub fn insert_text_message(logical_frame: &mut [u8], piece: [u8; 4]) {
    if let Some(start) = logical_frame.len().checked_sub(TEXT_MESSAGE_BYTES) {
        logical_frame[start..].copy_from_slice(&piece);
    }
}

// ---------------------------------------------------------------------------------
// AAC / Opus super frames
// ---------------------------------------------------------------------------------

/// Layout of an AAC (or Dream-Opus) audio super frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AacSuperFrameFormat {
    /// Audio frames per super frame.
    pub num_frames: usize,
    /// Frame borders in the header: `num_frames − 1` for AAC (the last length is
    /// implicit), `num_frames` for Opus.
    pub num_borders: usize,
    /// Bytes of every frame carried in front of its CRC byte (non-zero only for a
    /// stream with both a part A and a part B, pre-V4 UEP; see the module docs).
    pub higher_protected_bytes: usize,
}

impl AacSuperFrameFormat {
    /// AAC with `num_frames` (5 or 10) frames in a stream of the given lengths.
    pub fn aac(num_frames: usize, stream: StreamLengths) -> Self {
        Self::with_borders(num_frames, num_frames.saturating_sub(1), stream)
    }

    /// Dream's Opus framing in a stream of the given lengths.
    pub fn opus(stream: StreamLengths) -> Self {
        Self::with_borders(OPUS_FRAMES_PER_SUPER_FRAME, OPUS_FRAMES_PER_SUPER_FRAME, stream)
    }

    fn with_borders(num_frames: usize, num_borders: usize, stream: StreamLengths) -> Self {
        let header = header_bytes(num_borders);
        // Dream: (lengthPartA − headerBytes − numFrames) / numFrames when part A is used.
        let higher_protected_bytes = if stream.part_a > 0 && stream.part_b > 0 && num_frames > 0 {
            stream.part_a.saturating_sub(header + num_frames) / num_frames
        } else {
            0
        };
        Self { num_frames, num_borders, higher_protected_bytes }
    }

    /// Header length in bytes: 12 bits per border, padded to whole bytes.
    pub fn header_bytes(&self) -> usize {
        header_bytes(self.num_borders)
    }

    /// Bytes available for the frames in a super frame of `super_frame_len` bytes
    /// (`audio_payload_length`: minus header and CRC bytes).
    pub fn payload_len(&self, super_frame_len: usize) -> Option<usize> {
        super_frame_len.checked_sub(self.header_bytes() + self.num_frames)
    }
}

fn header_bytes(num_borders: usize) -> usize {
    (12 * num_borders).div_ceil(8)
}

/// Read the `k`-th 12-bit value of a byte slice (MSB first).
fn read12(bytes: &[u8], k: usize) -> usize {
    let bit = 12 * k;
    let b = |i: usize| usize::from(bytes.get(i).copied().unwrap_or(0));
    let v = (b(bit / 8) << 16) | (b(bit / 8 + 1) << 8) | b(bit / 8 + 2);
    (v >> (12 - bit % 8)) & 0xFFF
}

/// CRC byte of a Dream Opus packet: DRM CRC-8 over all bytes but the last one.
pub fn dream_opus_crc(packet: &[u8]) -> u8 {
    crc8(&packet[..packet.len().saturating_sub(1)])
}

/// Split an AAC (or Dream-Opus) audio super frame into its frames. Frames of an Opus
/// super frame get their CRC checked (`crc_ok`).
pub fn parse_aac_super_frame(sf: &[u8], fmt: &AacSuperFrameFormat) -> Result<Vec<AudioFrame>, AudioError> {
    let n = fmt.num_frames;
    let payload = fmt.payload_len(sf.len()).filter(|_| n > 0).ok_or(AudioError::TooShort(sf.len()))?;
    let implicit_last = fmt.num_borders < n;
    let mut lengths = Vec::with_capacity(n);
    let mut prev = 0;
    for k in 0..fmt.num_borders.min(n) {
        let mut border = read12(sf, k);
        if border < prev {
            // Table 11 note 2: borders above 4095 are sent modulo 4096.
            border += 4096;
        }
        // With an implicit last frame, every border must leave it at least one byte
        // (Dream: running sum < audio_payload_length).
        let too_far = if implicit_last { border >= payload } else { border > payload };
        if border < prev || too_far {
            return Err(AudioError::BadBorder { border: k, value: border });
        }
        lengths.push(border - prev);
        prev = border;
    }
    if implicit_last {
        lengths.push(payload - prev);
    }
    let hp = fmt.higher_protected_bytes;
    if let Some(f) = lengths.iter().position(|&l| l < hp) {
        return Err(AudioError::FrameTooShort(f));
    }
    let mut pos = fmt.header_bytes();
    let mut frames = Vec::with_capacity(n);
    for _ in 0..n {
        let data = sf[pos..pos + hp].to_vec();
        pos += hp;
        frames.push(AudioFrame { data, crc_byte: Some(sf[pos]), crc_ok: None });
        pos += 1;
    }
    for (f, &len) in frames.iter_mut().zip(&lengths) {
        f.data.extend_from_slice(&sf[pos..pos + len - hp]);
        pos += len - hp;
    }
    if !implicit_last {
        // Dream-Opus: the CRC can be checked here.
        for f in &mut frames {
            f.crc_ok = Some(!f.data.is_empty() && f.crc_byte == Some(dream_opus_crc(&f.data)));
        }
    }
    Ok(frames)
}

/// Build an AAC (or Dream-Opus) audio super frame of `len` bytes from already coded
/// frames and their CRC bytes (transmitter side, Dream's
/// `CAudioSourceEncoderImplementation::ProcessDataInternal`).
///
/// For AAC the last frame's length is implicit, so the frames must fill the payload
/// exactly — pad the frames beforehand (between core and SBR data, as an SBR frame is
/// read from both ends); Dream-Opus frames may leave zero padding at the end.
pub fn build_aac_super_frame(
    frames: &[AudioFrame],
    fmt: &AacSuperFrameFormat,
    len: usize,
) -> Result<Vec<u8>, AudioError> {
    let n = fmt.num_frames;
    if frames.len() != n || n == 0 {
        return Err(AudioError::FrameCount { got: frames.len(), want: n });
    }
    let payload = fmt.payload_len(len).ok_or(AudioError::TooShort(len))?;
    let total: usize = frames.iter().map(|f| f.data.len()).sum();
    let implicit_last = fmt.num_borders < n;
    if (implicit_last && total != payload) || total > payload {
        return Err(AudioError::PayloadSize { need: total, have: payload });
    }
    if implicit_last && frames[n - 1].data.is_empty() {
        return Err(AudioError::FrameTooShort(n - 1));
    }
    let hp = fmt.higher_protected_bytes;
    if let Some(f) = frames.iter().position(|f| f.data.len() < hp) {
        return Err(AudioError::FrameTooShort(f));
    }
    let mut w = crate::bits::BitWriter::new();
    let mut border = 0usize;
    for f in frames.iter().take(fmt.num_borders) {
        border += f.data.len();
        w.write((border & 0xFFF) as u32, 12);
    }
    if fmt.num_borders % 2 == 1 {
        w.write(0, 4);
    }
    let mut sf = crate::bits::pack(&w.into_bits());
    for f in frames {
        sf.extend_from_slice(&f.data[..hp]);
        sf.push(f.crc_byte.unwrap_or(0));
    }
    for f in frames {
        sf.extend_from_slice(&f.data[hp..]);
    }
    sf.resize(len, 0);
    Ok(sf)
}

// ---------------------------------------------------------------------------------
// xHE-AAC
// ---------------------------------------------------------------------------------

/// Check the audio frame CRC of an xHE-AAC frame (access unit + CRC-16, §5.3.1.2).
pub fn xhe_frame_crc_ok(frame: &[u8]) -> bool {
    frame.len() >= 2 && {
        let (au, crc) = frame.split_at(frame.len() - 2);
        crc16(au) == u16::from_be_bytes([crc[0], crc[1]])
    }
}

/// Stateful xHE-AAC audio super frame parser (port of Dream's `XHEAACSuperFrame`).
///
/// Differences to Dream: the frame that was already running when reception started
/// (or after an error) is dropped instead of being passed on incomplete, errors reset
/// the carried-over bytes, a failed header CRC falls back to the count repeated in the
/// directory (§5.3.1.1 note), and each frame's CRC-16 is checked.
#[derive(Debug, Clone, Default)]
pub struct XheAacDeframer {
    /// Payload bytes since the last frame border: the beginning of the frame in
    /// progress, which may span several super frames.
    pending: Vec<u8>,
    /// `pending` starts at a frame border.
    synced: bool,
}

impl XheAacDeframer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop the carried-over bytes (after loss of reception or reconfiguration).
    pub fn reset(&mut self) {
        self.pending.clear();
        self.synced = false;
    }

    /// Parse one audio super frame (the logical frame without text message bytes) and
    /// return the audio frames completed by it.
    pub fn push(&mut self, sf: &[u8]) -> Result<(XheHeader, Vec<AudioFrame>), AudioError> {
        let r = self.push_inner(sf);
        if r.is_err() {
            self.reset();
        }
        r
    }

    fn push_inner(&mut self, sf: &[u8]) -> Result<(XheHeader, Vec<AudioFrame>), AudioError> {
        if sf.len() < 2 {
            return Err(AudioError::TooShort(sf.len()));
        }
        let header_crc_ok = crc8(&sf[..1]) == sf[1];
        // Without a good header CRC, take the count from the last directory element.
        let count = if header_crc_ok { sf[0] >> 4 } else { sf[sf.len() - 1] & 0x0F };
        let header = XheHeader { frame_border_count: count, bit_reservoir_level: sf[0] & 0x0F, header_crc_ok };
        let n = usize::from(count);
        let dir_start = sf.len().checked_sub(2 * n).filter(|&d| d >= 2).ok_or(AudioError::TooShort(sf.len()))?;
        // Element j of the directory describes border n − 1 − j.
        let mut index = vec![0usize; n];
        for j in 0..n {
            let e = u16::from_be_bytes([sf[dir_start + 2 * j], sf[dir_start + 2 * j + 1]]);
            if e & 0x0F != u16::from(count) {
                return Err(AudioError::Directory);
            }
            index[n - 1 - j] = usize::from(e >> 4);
        }
        let payload = &sf[2..dir_start];
        let start = self.pending.len();
        let mut borders = Vec::with_capacity(n);
        for (i, &x) in index.iter().enumerate() {
            let pos = match x {
                0xFFE | 0xFFF if i == 0 => {
                    let back = if x == 0xFFE { 2 } else { 1 };
                    match start.checked_sub(back) {
                        Some(p) => p,
                        // The previous payload was not received: skip this border.
                        None if !self.synced => continue,
                        None => return Err(AudioError::DelayedBorder),
                    }
                }
                _ if x < payload.len() => start + x,
                _ => return Err(AudioError::BadBorder { border: i, value: x }),
            };
            if borders.last().is_some_and(|&b| pos <= b) {
                return Err(AudioError::BadBorder { border: i, value: x });
            }
            borders.push(pos);
        }
        self.pending.extend_from_slice(payload);
        let mut frames = Vec::with_capacity(borders.len());
        let mut prev = 0;
        for &b in &borders {
            if self.synced {
                let data = self.pending[prev..b].to_vec();
                let crc_ok = Some(xhe_frame_crc_ok(&data));
                frames.push(AudioFrame { data, crc_byte: None, crc_ok });
            }
            // The first border ends the partial frame from before tune-in.
            self.synced = true;
            prev = b;
        }
        self.pending.drain(..prev);
        if !self.synced {
            // Only the last two bytes can matter (a delayed 0xFFE/0xFFF border).
            let keep = self.pending.len().saturating_sub(2);
            self.pending.drain(..keep);
        } else if self.pending.len() > 4 * XHE_AAC_MAX_FRAME_BYTES {
            return Err(AudioError::Overflow);
        }
        Ok((header, frames))
    }
}

/// Shortest and longest xHE-AAC audio super frame [`XheAacFramer`] builds. The upper
/// limit keeps every payload index below the special values 0xFFE/0xFFF of the 12-bit
/// frame border index; it is the spec's maximum anyway (163 920 bit/s in robustness mode
/// E, whose super frames last 200 ms: 4 098 bytes).
const XHE_FRAMER_LEN: std::ops::RangeInclusive<usize> = 8..=4098;

/// xHE-AAC audio super frame builder (transmitter side, §5.3.1).
///
/// Queue the encoder's USAC access units with their bit reservoir levels
/// ([`XheAacFramer::push_access_unit`]) and take one super frame of constant size per
/// logical frame ([`XheAacFramer::next_super_frame`]):
///
/// * **Header level** (§5.3.1.1): the bit reservoir level of the first frame that starts
///   in the payload, or — when none does — of the frame in progress. A frame that starts
///   in the last two payload bytes counts as starting in that super frame although its
///   border is only signalled in the next one (0xFFE/0xFFF, §5.3.1.3).
/// * **No padding.** The payload is taken from queued frames only: padding bytes would
///   become part of the frame in progress and break its CRC, and §5.3.1.3 puts padding
///   inside the access units (the encoder's fill element). The transmitter therefore runs
///   the encoder ahead of the channel — `decdrm_codecs::XheAacEncoder` keeps its output at
///   or above the channel rate, so encoding the audio up to the end of the super frame
///   plus two frames is enough — and [`XheAacFramer::ready`] tells whether the queue
///   fills the next payload. If it does not, `next_super_frame` fails with
///   [`AudioError::XheUnderrun`].
/// * **At most [`XHE_AAC_MAX_BORDERS`] borders per directory.** More frame starts in one
///   payload fail with [`AudioError::XheTooManyFrames`]; the encoder rules them out with
///   frames (access unit plus CRC) of at least ⌈len/15⌉ bytes.
///
/// A failed call changes nothing. Super frames are 8 to 4 098 bytes long.
#[derive(Debug, Clone, Default)]
pub struct XheAacFramer {
    /// Audio frame bytes (access units + CRCs) not yet placed into a super frame.
    pending: Vec<u8>,
    /// Frame starts not yet signalled: position in `pending` and the frame's bit
    /// reservoir level.
    starts: Vec<(usize, u8)>,
    /// Border that did not fit into the previous directory (0xFFE or 0xFFF).
    delayed: Option<u16>,
    /// Bit reservoir level of the frame in progress at the start of `pending` (the last
    /// frame that started in an earlier payload).
    current_level: u8,
}

/// How the queue maps onto the next xHE-AAC super frame.
#[derive(Debug, Clone, Copy)]
struct XheLayout {
    /// Frame starts listed in the directory (besides a delayed border).
    signalled: usize,
    /// Frame border count: directory elements.
    count: usize,
    payload_len: usize,
    /// A start in the last two payload bytes, signalled in the next directory.
    next_delayed: Option<u16>,
}

impl XheAacFramer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue one USAC access unit together with the encoder's bit reservoir level after
    /// encoding it (the 4-bit header value, §5.3.1.3); the audio frame CRC-16 is appended
    /// here.
    pub fn push_access_unit(&mut self, au: &[u8], bit_reservoir_level: u8) {
        self.starts.push((self.pending.len(), bit_reservoir_level & 0x0F));
        self.pending.extend_from_slice(au);
        self.pending.extend_from_slice(&crc16(au).to_be_bytes());
    }

    /// Bytes queued but not yet transmitted.
    pub fn pending_bytes(&self) -> usize {
        self.pending.len()
    }

    /// Whether the queued frames fill the payload of the next super frame of `len` bytes.
    pub fn ready(&self, len: usize) -> bool {
        self.pending.len() >= self.layout(len).payload_len
    }

    fn layout(&self, len: usize) -> XheLayout {
        let delayed = usize::from(self.delayed.is_some());
        let payload_for = |elements: usize| len.saturating_sub(2 + 2 * elements);
        // Signal every frame start of which at least one byte fits into the payload that
        // remains once its directory element is added (§5.3.1.3).
        let mut m = 0;
        while delayed + m < XHE_AAC_MAX_BORDERS
            && self.starts.get(m).is_some_and(|&(s, _)| s < payload_for(delayed + m + 1))
        {
            m += 1;
        }
        let count = delayed + m;
        let payload_len = payload_for(count);
        // A start in this payload whose element does not fit lies in its last two bytes:
        // the next directory signals it.
        let next_delayed = match self.starts.get(m) {
            Some(&(s, _)) if s < payload_len => match payload_len - s {
                1 => Some(0xFFF),
                2 => Some(0xFFE),
                _ => None,
            },
            _ => None,
        };
        XheLayout { signalled: m, count, payload_len, next_delayed }
    }

    /// Build the next super frame of `len` bytes (see the type docs). Fails, changing
    /// nothing, when the queue does not fill the payload, when more frames start in it
    /// than a directory can list, or for a length outside 8–4 098 bytes.
    pub fn next_super_frame(&mut self, len: usize) -> Result<Vec<u8>, AudioError> {
        if len < *XHE_FRAMER_LEN.start() {
            return Err(AudioError::TooShort(len));
        }
        if len > *XHE_FRAMER_LEN.end() {
            return Err(AudioError::Unsupported("xHE-AAC audio super frame longer than 4098 bytes"));
        }
        let XheLayout { signalled, count, payload_len, next_delayed } = self.layout(len);
        if self.pending.len() < payload_len {
            return Err(AudioError::XheUnderrun { need: payload_len, have: self.pending.len() });
        }
        // Every start in the payload must be signalled now or in the next directory.
        let in_payload = self.starts.iter().take_while(|&&(s, _)| s < payload_len).count();
        if in_payload > signalled + usize::from(next_delayed.is_some()) {
            return Err(AudioError::XheTooManyFrames);
        }
        let level = match self.starts.first() {
            Some(&(s, level)) if s < payload_len => level,
            _ => self.current_level,
        };

        let b0 = ((count as u8) << 4) | level;
        let mut sf = Vec::with_capacity(len);
        sf.extend_from_slice(&[b0, crc8(&[b0])]);
        sf.extend_from_slice(&self.pending[..payload_len]);
        // Border 0 (the delayed one, if any) is the last directory element.
        let borders = self.delayed.into_iter().chain(self.starts[..signalled].iter().map(|&(s, _)| s as u16));
        let elements: Vec<u16> = borders.map(|b| (b << 4) | count as u16).collect();
        for e in elements.iter().rev() {
            sf.extend_from_slice(&e.to_be_bytes());
        }
        debug_assert_eq!(sf.len(), len);

        if let Some(&(_, level)) = self.starts[..in_payload].last() {
            self.current_level = level;
        }
        self.pending.drain(..payload_len);
        self.starts.drain(..in_payload);
        for (s, _) in &mut self.starts {
            *s -= payload_len;
        }
        self.delayed = next_delayed;
        Ok(sf)
    }
}

// ---------------------------------------------------------------------------------
// Per-service deframer
// ---------------------------------------------------------------------------------

/// Everything extracted from one logical frame of an audio stream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AudioSuperFrame {
    pub frames: Vec<AudioFrame>,
    /// Why the super frame could not be split (a corrupt header); the decoder should
    /// conceal [`AudioSuperFrame::nominal_frames`] frames then.
    pub error: Option<AudioError>,
    /// Frames per super frame for AAC (5/10), Opus (20) and EnCodec (1: the whole super
    /// frame); `None` for xHE-AAC.
    pub nominal_frames: Option<usize>,
    /// Text message bytes of this logical frame (text flag set).
    pub text: Option<[u8; 4]>,
    /// xHE-AAC super frame header.
    pub xhe: Option<XheHeader>,
}

#[derive(Debug, Clone)]
enum Deframer {
    Aac(AacSuperFrameFormat),
    Xhe(XheAacDeframer),
    /// The super frame is one codec frame (EnCodec).
    Whole,
}

/// Splits the logical frames of one audio service into audio frames and text message
/// pieces, according to its SDC audio parameters and stream lengths.
#[derive(Debug, Clone)]
pub struct AudioDeframer {
    deframer: Deframer,
    text_flag: bool,
    nominal_frames: Option<usize>,
}

impl AudioDeframer {
    /// `stream` = lengths of the stream carrying the audio (from the multiplex
    /// description).
    pub fn new(params: &AudioParams, stream: StreamLengths) -> Result<Self, AudioError> {
        let (deframer, nominal_frames) = match params.codec {
            AudioCodec::Aac => {
                let n = params
                    .aac_frames_per_super_frame()
                    .ok_or(AudioError::Unsupported("AAC sampling rate not allowed in robustness modes A-D"))?;
                (Deframer::Aac(AacSuperFrameFormat::aac(n, stream)), Some(n))
            }
            AudioCodec::Opus => (Deframer::Aac(AacSuperFrameFormat::opus(stream)), Some(OPUS_FRAMES_PER_SUPER_FRAME)),
            AudioCodec::XheAac => (Deframer::Xhe(XheAacDeframer::new()), None),
            AudioCodec::Encodec => (Deframer::Whole, Some(1)),
            AudioCodec::Reserved => return Err(AudioError::Unsupported("reserved audio coding")),
        };
        Ok(Self { deframer, text_flag: params.text_flag, nominal_frames })
    }

    /// Forget state carried between super frames (xHE-AAC).
    pub fn reset(&mut self) {
        if let Deframer::Xhe(x) = &mut self.deframer {
            x.reset();
        }
    }

    /// Process the logical frame (part A + part B bytes) of one multiplex frame.
    pub fn push(&mut self, logical_frame: &[u8]) -> AudioSuperFrame {
        let (sf, text) = split_text_message(logical_frame, self.text_flag);
        let mut out = AudioSuperFrame { nominal_frames: self.nominal_frames, text, ..Default::default() };
        let result = match &mut self.deframer {
            Deframer::Aac(fmt) => parse_aac_super_frame(sf, fmt),
            Deframer::Xhe(x) => x.push(sf).map(|(h, frames)| {
                out.xhe = Some(h);
                frames
            }),
            Deframer::Whole => Ok(vec![AudioFrame { data: sf.to_vec(), crc_byte: None, crc_ok: None }]),
        };
        match result {
            Ok(frames) => out.frames = frames,
            Err(e) => out.error = Some(e),
        }
        out
    }

    /// [`Self::push`] for a demultiplexed [`LogicalFrame`].
    pub fn push_frame(&mut self, frame: &LogicalFrame) -> AudioSuperFrame {
        self.push(&frame.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::service::AudioMode;

    fn pseudo(n: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9) | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    /// Frame lengths summing to `total`, varying around the mean.
    fn lengths(n: usize, total: usize, seed: u32) -> Vec<usize> {
        let r = pseudo(n, seed);
        let mut l: Vec<usize> = r.iter().map(|&v| total / n - 8 + usize::from(v % 16)).collect();
        let s: usize = l[..n - 1].iter().sum();
        l[n - 1] = total - s;
        l
    }

    #[test]
    fn aac_roundtrip_all_layouts() {
        for (n, part_a, part_b) in [(10, 0, 1000), (5, 0, 520), (10, 120, 900), (5, 60, 400), (10, 700, 0)] {
            let stream = StreamLengths { part_a, part_b };
            let fmt = AacSuperFrameFormat::aac(n, stream);
            assert_eq!(fmt.header_bytes(), if n == 10 { 14 } else { 6 });
            if part_a > 0 && part_b > 0 {
                assert_eq!(fmt.higher_protected_bytes, (part_a - fmt.header_bytes() - n) / n);
            } else {
                assert_eq!(fmt.higher_protected_bytes, 0);
            }
            let len = stream.total();
            let payload = fmt.payload_len(len).unwrap();
            let frames: Vec<AudioFrame> = lengths(n, payload, n as u32 + part_a as u32)
                .iter()
                .enumerate()
                .map(|(i, &l)| AudioFrame::with_crc(pseudo(l, i as u32), i as u8 * 17))
                .collect();
            let sf = build_aac_super_frame(&frames, &fmt, len).unwrap();
            assert_eq!(sf.len(), len);
            assert_eq!(parse_aac_super_frame(&sf, &fmt).unwrap(), frames);
        }
    }

    #[test]
    fn aac_layout_matches_table_10_and_dream() {
        // EEP, 5 frames: header of four 12-bit borders, 5 CRC bytes, then the frames.
        let frames: Vec<AudioFrame> =
            (0..5).map(|i| AudioFrame::with_crc(vec![0xA0 + i; 3 + usize::from(i)], i)).collect();
        let fmt = AacSuperFrameFormat::aac(5, StreamLengths { part_a: 0, part_b: 6 + 5 + 25 });
        let sf = build_aac_super_frame(&frames, &fmt, 36).unwrap();
        assert_eq!(&sf[..6], &[0x00, 0x30, 0x07, 0x00, 0xC0, 0x12]); // borders 3, 7, 12, 18
        assert_eq!(&sf[6..11], &[0, 1, 2, 3, 4]);
        assert_eq!(&sf[11..14], &[0xA0; 3]);
        // UEP split stream (Dream): per frame the higher protected bytes, then its CRC.
        let stream = StreamLengths { part_a: 6 + 5 + 10, part_b: 15 };
        let fmt = AacSuperFrameFormat::aac(5, stream);
        assert_eq!(fmt.higher_protected_bytes, 2);
        let sf = build_aac_super_frame(&frames, &fmt, 36).unwrap();
        assert_eq!(&sf[6..12], &[0xA0, 0xA0, 0, 0xA1, 0xA1, 1]);
    }

    #[test]
    fn aac_border_wraparound_and_errors() {
        // A payload beyond 4095 bytes needs the modulo-4096 border rule.
        let fmt = AacSuperFrameFormat::aac(10, StreamLengths { part_a: 0, part_b: 5000 });
        let payload = fmt.payload_len(5000).unwrap();
        let frames: Vec<AudioFrame> =
            lengths(10, payload, 3).iter().map(|&l| AudioFrame::with_crc(pseudo(l, l as u32), 1)).collect();
        let sf = build_aac_super_frame(&frames, &fmt, 5000).unwrap();
        assert_eq!(parse_aac_super_frame(&sf, &fmt).unwrap(), frames);
        // Decreasing borders, a border past the payload, a truncated frame: errors, no
        // panics.
        let fmt = AacSuperFrameFormat::aac(5, StreamLengths { part_a: 0, part_b: 100 });
        let mut bad = vec![0u8; 100];
        bad[0] = 0xFF; // first border 0xFF0 > payload
        assert!(matches!(parse_aac_super_frame(&bad, &fmt), Err(AudioError::BadBorder { .. })));
        assert!(parse_aac_super_frame(&[0; 8], &fmt).is_err());
        for len in 0..40 {
            for seed in 0..20 {
                let _ = parse_aac_super_frame(&pseudo(len, seed), &fmt);
            }
        }
        // Builder: frames must fill the AAC payload exactly.
        let short: Vec<AudioFrame> = (0..5).map(|_| AudioFrame::with_crc(vec![1; 10], 0)).collect();
        assert!(matches!(build_aac_super_frame(&short, &fmt, 100), Err(AudioError::PayloadSize { .. })));
    }

    #[test]
    fn opus_roundtrip_and_crc() {
        let stream = StreamLengths { part_a: 0, part_b: 1200 };
        let fmt = AacSuperFrameFormat::opus(stream);
        assert_eq!(fmt.header_bytes(), 30);
        let frames: Vec<AudioFrame> = (0..20)
            .map(|i| {
                let p = pseudo(40 + i, i as u32);
                let crc = dream_opus_crc(&p);
                AudioFrame::with_crc(p, crc)
            })
            .collect();
        let sf = build_aac_super_frame(&frames, &fmt, 1200).unwrap();
        let parsed = parse_aac_super_frame(&sf, &fmt).unwrap();
        assert_eq!(parsed.len(), 20);
        for (p, f) in parsed.iter().zip(&frames) {
            assert_eq!((&p.data, p.crc_byte, p.crc_ok), (&f.data, f.crc_byte, Some(true)));
        }
        // A corrupted byte (not the last one of a packet) fails the CRC.
        let mut bad = sf.clone();
        bad[30 + 20] ^= 1;
        let parsed = parse_aac_super_frame(&bad, &fmt).unwrap();
        assert_eq!(parsed[0].crc_ok, Some(false));
        // Via the per-service deframer with text message bytes at the end.
        let params = AudioParams::new(0, AudioCodec::Opus, false, AudioMode::Stereo, 48_000, true, vec![]);
        let mut d = AudioDeframer::new(&params, StreamLengths { part_a: 0, part_b: 1204 }).unwrap();
        let mut lf = sf;
        lf.extend_from_slice(b"TEXT");
        let out = d.push(&lf);
        assert_eq!(out.text, Some(*b"TEXT"));
        assert_eq!(out.frames.len(), 20);
        assert_eq!(out.nominal_frames, Some(20));
    }

    /// The header level §5.3.1.1 asks for, from the stream offsets of the frame starts
    /// (offset, level) and the payload `[from, to)` in the same byte stream: the first
    /// frame starting in the payload, else the frame in progress.
    fn expected_level(starts: &[(usize, u8)], from: usize, to: usize) -> u8 {
        let first = starts.iter().find(|&&(s, _)| (from..to).contains(&s));
        let running = starts.iter().rev().find(|&&(s, _)| s < from);
        first.or(running).map_or(0, |&(_, level)| level)
    }

    #[test]
    fn xhe_roundtrip_with_carry_over() {
        // Frames of irregular sizes, super frames of 150 bytes and less: frames span
        // super frames, and borders fall into the last payload bytes (0xFFE / 0xFFF).
        for sf_len in [150usize, 97, 61, 400] {
            let aus: Vec<Vec<u8>> = (0..300).map(|i| pseudo(20 + (i * 7919) % 130, i as u32)).collect();
            let level = |i: usize| (i * 7 % 16) as u8;
            let mut framer = XheAacFramer::new();
            let mut deframer = XheAacDeframer::new();
            let mut got = Vec::new();
            let mut delayed_seen = 0;
            let mut next_au = 0;
            // Stream offset of every frame start, and of the next payload.
            let mut starts = Vec::new();
            let (mut pushed, mut sent) = (0, 0);
            for _ in 0..400 {
                // Keep the encoder a little ahead of the framer.
                while framer.pending_bytes() < 2 * sf_len && next_au < aus.len() {
                    framer.push_access_unit(&aus[next_au], level(next_au));
                    starts.push((pushed, level(next_au)));
                    pushed += aus[next_au].len() + 2;
                    next_au += 1;
                }
                if !framer.ready(sf_len) {
                    assert_eq!(next_au, aus.len(), "ready once the queue holds a super frame");
                    assert!(matches!(framer.next_super_frame(sf_len), Err(AudioError::XheUnderrun { .. })));
                    break;
                }
                let sf = framer.next_super_frame(sf_len).unwrap();
                assert_eq!(sf.len(), sf_len);
                let n = usize::from(sf[0] >> 4);
                if n > 0 && u16::from_be_bytes([sf[sf_len - 2], sf[sf_len - 1]]) >> 4 >= 0xFFE {
                    delayed_seen += 1;
                }
                let (h, frames) = deframer.push(&sf).unwrap();
                assert!(h.header_crc_ok);
                let payload = sf_len - 2 - 2 * n;
                assert_eq!(h.bit_reservoir_level, expected_level(&starts, sent, sent + payload), "at {sent}");
                sent += payload;
                got.extend(frames);
            }
            // The first frame is complete too because the deframer saw it from the start.
            assert!(got.len() > 50, "only {} frames", got.len());
            for (i, f) in got.iter().enumerate() {
                assert_eq!(f.crc_ok, Some(true), "frame {i}");
                assert_eq!(f.usac_access_unit(), aus[i].as_slice(), "frame {i} (sf_len {sf_len})");
            }
            if sf_len < 100 {
                assert!(delayed_seen > 0, "no delayed borders exercised for {sf_len}");
            }
        }
    }

    /// The framer never pads: a short queue is an error that changes nothing, and so is
    /// a payload with more frame starts than a directory can list.
    #[test]
    fn xhe_framer_refuses_to_pad_or_overfill() {
        let mut framer = XheAacFramer::new();
        assert!(!framer.ready(100));
        assert_eq!(framer.next_super_frame(100), Err(AudioError::XheUnderrun { need: 98, have: 0 }));
        framer.push_access_unit(&[5; 48], 5);
        // One frame start: 2 header bytes, one directory element, 96 payload bytes.
        assert!(!framer.ready(100));
        assert_eq!(framer.next_super_frame(100), Err(AudioError::XheUnderrun { need: 96, have: 50 }));
        assert_eq!(framer.pending_bytes(), 50, "a failed call changes nothing");
        framer.push_access_unit(&[7; 48], 7);
        framer.push_access_unit(&[9; 48], 9);
        assert!(framer.ready(100));
        let sf = framer.next_super_frame(100).unwrap();
        assert_eq!((sf[0] >> 4, sf[0] & 15), (2, 5), "two borders (0 and 50), level of the first frame");
        assert!(matches!(framer.next_super_frame(4), Err(AudioError::TooShort(4))));
        assert!(matches!(framer.next_super_frame(5000), Err(AudioError::Unsupported(_))));

        // Sixteen 3-byte frames (one-byte access units) start in one 100-byte payload.
        let mut framer = XheAacFramer::new();
        for _ in 0..16 {
            framer.push_access_unit(&[1], 0);
        }
        framer.push_access_unit(&[0; 100], 0);
        assert!(framer.ready(100));
        assert_eq!(framer.next_super_frame(100), Err(AudioError::XheTooManyFrames));
        assert_eq!(framer.pending_bytes(), 16 * 3 + 102);
        // Fifteen fit: fourteen small frames and a large one reaching beyond the payload.
        let mut framer = XheAacFramer::new();
        for _ in 0..14 {
            framer.push_access_unit(&[1], 0);
        }
        framer.push_access_unit(&[0; 100], 0);
        let sf = framer.next_super_frame(100).unwrap();
        assert_eq!(sf[0] >> 4, 15);
        let (_, frames) = XheAacDeframer::new().push(&sf).unwrap();
        assert_eq!(frames.len(), 14, "the 15th frame is still in progress");
    }

    /// §5.3.1.1: the header carries the level of the first frame starting in the payload
    /// — also one whose border is delayed to the next super frame — or else of the frame
    /// in progress, which may be the one of a delayed border.
    #[test]
    fn xhe_header_level_rules() {
        // Frames (access unit + CRC) A 0..20, B 20..50, C 50..72, D 72..77, E 77..109 of
        // the payload byte stream, with levels 1, 4, 9, 12, 2.
        let mut framer = XheAacFramer::new();
        for (i, (au_len, level)) in [(18, 1), (28, 4), (20, 9), (3, 12), (30, 2)].into_iter().enumerate() {
            framer.push_access_unit(&pseudo(au_len, i as u32), level);
        }
        let mut deframer = XheAacDeframer::new();
        let mut decoded = Vec::new();
        // 16-byte super frames: 14 payload bytes without a border, 12 with one.
        let mut next = |framer: &mut XheAacFramer| {
            let sf = framer.next_super_frame(16).unwrap();
            let (h, frames) = deframer.push(&sf).unwrap();
            let delayed = h.frame_border_count > 0 && u16::from_be_bytes([sf[14], sf[15]]) >> 4 >= 0xFFE;
            decoded.extend(frames.iter().map(|f| f.data.len()));
            (h.frame_border_count, h.bit_reservoir_level, delayed)
        };
        // Payload 0..12: A starts in it.
        assert_eq!(next(&mut framer), (1, 1, false));
        // 12..24: B starts at 20 while A is in progress: B's level.
        assert_eq!(next(&mut framer), (1, 4, false));
        // 24..38: no start, B in progress.
        assert_eq!(next(&mut framer), (0, 4, false));
        // 38..52: C starts at 50, in the last two bytes — its border is delayed, but it
        // starts here, so the level is C's.
        assert_eq!(next(&mut framer), (0, 9, false));
        // 52..64: the delayed border (0xFFE) ends B; no start of its own: C, in progress.
        assert_eq!(next(&mut framer), (1, 9, true));
        // 64..76: D starts at 72 while C is in progress.
        assert_eq!(next(&mut framer), (1, 12, false));
        // 76..88: E starts at 77.
        assert_eq!(next(&mut framer), (1, 2, false));
        // 88..102: E in progress; then only 7 bytes are left.
        assert_eq!(next(&mut framer), (0, 2, false));
        assert!(!framer.ready(16));
        assert_eq!(framer.next_super_frame(16), Err(AudioError::XheUnderrun { need: 14, have: 7 }));
        assert_eq!(decoded, [20, 30, 22, 5], "A, B, C, D complete, E in progress");
    }

    #[test]
    fn xhe_tune_in_mid_stream_and_errors() {
        let aus: Vec<Vec<u8>> = (0..60).map(|i| pseudo(30 + (i * 37) % 50, 100 + i as u32)).collect();
        let mut framer = XheAacFramer::new();
        for au in &aus {
            framer.push_access_unit(au, 3);
        }
        let sfs: Vec<Vec<u8>> = (0..12).map(|_| framer.next_super_frame(120).unwrap()).collect();
        // Start at the third super frame: the first (partial) frame is dropped, all
        // following frames are complete and correct.
        let mut d = XheAacDeframer::new();
        let mut got = Vec::new();
        for sf in &sfs[2..] {
            got.extend(d.push(sf).unwrap().1);
        }
        assert!(!got.is_empty());
        assert!(got.iter().all(|f| f.crc_ok == Some(true)));
        let first = aus.iter().position(|a| a.as_slice() == got[0].usac_access_unit()).expect("frame boundary");
        for (k, f) in got.iter().enumerate() {
            assert_eq!(f.usac_access_unit(), aus[first + k].as_slice());
        }
        // A corrupted directory is an error and resets the carry-over.
        let mut bad = sfs[5].clone();
        let l = bad.len();
        bad[l - 1] ^= 0x01;
        let mut d = XheAacDeframer::new();
        d.push(&sfs[4]).unwrap();
        assert!(d.push(&bad).is_err());
        // Garbage never panics.
        for len in 0..64 {
            for seed in 0..50 {
                let _ = d.push(&pseudo(len, seed));
            }
        }
    }

    #[test]
    fn xhe_header_crc_fallback() {
        let mut framer = XheAacFramer::new();
        framer.push_access_unit(&[1, 2, 3, 4, 5, 6, 7, 8], 1);
        framer.push_access_unit(&[9; 20], 2);
        framer.push_access_unit(&[7; 20], 3);
        let mut sf = framer.next_super_frame(40).unwrap();
        let count = sf[0] >> 4;
        sf[1] ^= 0xFF; // break the header CRC; the directory still gives the count
        let mut d = XheAacDeframer::new();
        let (h, _) = d.push(&sf).unwrap();
        assert!(!h.header_crc_ok);
        assert_eq!(h.frame_border_count, count);
    }

    #[test]
    fn encodec_super_frame_passed_whole() {
        let config = vec![0x00, b'E', b'N', b'C', b'1', 0x48];
        let params = AudioParams::new(0, AudioCodec::Encodec, false, AudioMode::Mono, 24_000, true, config);
        let mut d = AudioDeframer::new(&params, StreamLengths { part_a: 0, part_b: 104 }).unwrap();
        let mut lf = pseudo(100, 9);
        lf.extend_from_slice(b"TEXT");
        let out = d.push(&lf);
        assert_eq!((out.text, out.nominal_frames, out.error), (Some(*b"TEXT"), Some(1), None));
        assert_eq!(out.frames, vec![AudioFrame { data: lf[..100].to_vec(), crc_byte: None, crc_ok: None }]);
    }

    #[test]
    fn text_split() {
        let lf = [1u8, 2, 3, 4, 5, 6];
        assert_eq!(split_text_message(&lf, true), (&lf[..2], Some([3, 4, 5, 6])));
        assert_eq!(split_text_message(&lf, false), (&lf[..], None));
        let mut lf = lf;
        insert_text_message(&mut lf, *b"abcd");
        assert_eq!(&lf[2..], b"abcd");
    }
}
