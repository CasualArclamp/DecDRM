//! Text messages (ES 201 980 §6.5): up to 128 bytes of UTF-8 text carried in 4-byte
//! pieces at the end of every logical frame of an audio stream. A message consists of
//! up to 8 segments (16-bit header, up to 16 body bytes, CRC-16); each segment starts
//! with a piece of four 0xFF bytes and its last piece is zero padded.
//!
//! Port of Dream's `CTextMessageDecoder` / `CTextMessageEncoder` (`TextMessage.cpp`)
//! with these differences:
//! * a segment is decoded as soon as its last byte arrived (Dream waits for the next
//!   segment's 0xFF marker, so the last segment before the text stops is never shown);
//! * Dream's `ResetSegments` iterates over copies and never actually clears anything;
//!   here a new toggle bit or changed segment content really starts a new message;
//! * commands are recognised: "clear display" and DL Plus (ETSI TS 102 980, passed on
//!   raw — its body length is taken from field 2 as in DAB, with a CRC search as a
//!   fallback since TS 102 980 is not available here).

use crate::fec::crc::crc16;

/// Maximum bytes of a segment: header, 16 body bytes, CRC.
const MAX_SEGMENT_BYTES: usize = 2 + 16 + 2;
/// Maximum message length in bytes (8 segments of 16 bytes).
pub const MAX_MESSAGE_BYTES: usize = 8 * 16;
/// The beginning-of-segment marker.
const MARKER: [u8; 4] = [0xFF; 4];

/// A complete text message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextMessage {
    /// The message bytes (UTF-8, possibly containing the control codes 0x0A preferred
    /// line break, 0x0B end of headline, 0x1F preferred word break).
    pub bytes: Vec<u8>,
    /// Text control field of the first segment (§6.7.2: bidi, base direction,
    /// contextual and combining flags).
    pub text_control: u8,
}

impl TextMessage {
    /// The raw text (lossy UTF-8).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    /// Text for display: line breaks for 0x0A and 0x0B (end of headline), a soft
    /// hyphen for the preferred word break 0x1F.
    pub fn display_text(&self) -> String {
        self.text()
            .chars()
            .map(|c| match c {
                '\u{0A}' | '\u{0B}' => '\n',
                '\u{1F}' => '\u{AD}',
                c => c,
            })
            .collect()
    }
}

/// Something the text message decoder recognised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextEvent {
    /// A new or changed complete message.
    Message(TextMessage),
    /// "Clear display" command: remove the message from the display.
    Clear,
    /// DL Plus command (ETSI TS 102 980): fields 2 and 3 of the header and the body.
    DlPlus { field2: u8, field3: u8, body: Vec<u8> },
}

/// Text message decoder for one audio service; feed it the four text bytes of every
/// logical frame ([`crate::mux::audio::AudioSuperFrame::text`]).
#[derive(Debug, Clone, Default)]
pub struct TextMessageDecoder {
    /// Bytes of the segment being received (after its marker).
    buf: Vec<u8>,
    /// A marker was seen and the segment is not finished yet.
    in_segment: bool,
    segments: [Option<Vec<u8>>; 8],
    num_segments: Option<usize>,
    text_toggle: Option<bool>,
    command_toggle: Option<bool>,
    text_control: u8,
    current: Option<TextMessage>,
}

impl TextMessageDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything (service change).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// The last complete message.
    pub fn message(&self) -> Option<&TextMessage> {
        self.current.as_ref()
    }

    /// Process the text message piece of one logical frame.
    pub fn push(&mut self, piece: [u8; 4]) -> Option<TextEvent> {
        if piece == MARKER {
            // An unfinished segment gets a last chance (commands of unknown length).
            let ev = if self.in_segment { self.finish(true) } else { None };
            self.buf.clear();
            self.in_segment = true;
            return ev;
        }
        if !self.in_segment {
            return None;
        }
        let room = MAX_SEGMENT_BYTES.saturating_sub(self.buf.len()).min(4);
        self.buf.extend_from_slice(&piece[..room]);
        self.finish(false)
    }

    /// Decode the buffered segment once complete. `last_chance`: no more bytes will come.
    fn finish(&mut self, last_chance: bool) -> Option<TextEvent> {
        if self.buf.len() < 4 {
            if last_chance {
                self.in_segment = false;
            }
            return None;
        }
        let (b0, b1) = (self.buf[0], self.buf[1]);
        let command = b0 & 0x10 != 0;
        let (field1, field2) = (usize::from(b0 & 0x0F), usize::from(b1 >> 4));
        // Expected body length: text = field 1 + 1; clear display = 0; DL Plus =
        // field 2 + 1 (DAB DLS convention); other commands unknown.
        let expected = match (command, field1) {
            (false, _) => Some(field1 + 1),
            (true, 1) => Some(0),
            (true, 2) => Some(field2 + 1),
            _ => None,
        };
        let full = self.buf.len() >= MAX_SEGMENT_BYTES || last_chance;
        let body_len = match expected {
            Some(l) if self.buf.len() >= 2 + l + 2 && crc_ok(&self.buf, l) => Some(l),
            Some(l) if self.buf.len() >= 2 + l + 2 && !command => {
                // A text segment with a bad CRC is dropped.
                self.in_segment = false;
                return None;
            }
            _ if command && full => (0..=16).find(|&l| crc_ok(&self.buf, l)),
            _ if full => None,
            _ => return None, // wait for more pieces
        };
        self.in_segment = false;
        let body_len = body_len?;
        let body = self.buf[2..2 + body_len].to_vec();
        if command { self.command(b0, b1, body) } else { self.text_segment(b0, b1, body) }
    }

    fn command(&mut self, b0: u8, b1: u8, body: Vec<u8>) -> Option<TextEvent> {
        let toggle = b0 & 0x80 != 0;
        if self.command_toggle == Some(toggle) {
            return None; // repetition of the last command
        }
        self.command_toggle = Some(toggle);
        match b0 & 0x0F {
            1 => {
                self.clear_segments();
                self.current = None;
                Some(TextEvent::Clear)
            }
            2 => Some(TextEvent::DlPlus { field2: b1 >> 4, field3: b1 & 0x0F, body }),
            _ => None,
        }
    }

    fn text_segment(&mut self, b0: u8, b1: u8, body: Vec<u8>) -> Option<TextEvent> {
        let toggle = b0 & 0x80 != 0;
        let first = b0 & 0x40 != 0;
        let last = b0 & 0x20 != 0;
        let index = if first { 0 } else { usize::from((b1 >> 4) & 7) };
        if !first && index == 0 {
            return None; // SegNum 0 is reserved
        }
        if self.text_toggle != Some(toggle) {
            // A different message.
            self.clear_segments();
            self.text_toggle = Some(toggle);
        } else if self.segments[index].as_ref().is_some_and(|s| *s != body) {
            // Same toggle but different content: also a new message (Dream).
            self.clear_segments();
        }
        if first {
            self.text_control = b1 & 0x0F;
        }
        self.segments[index] = Some(body);
        if last {
            self.num_segments = Some(index + 1);
        }
        let n = self.num_segments?;
        if !self.segments[..n].iter().all(Option::is_some) {
            return None;
        }
        // The first `flatten` skips `None` slots (all are `Some` here), the second
        // walks the bytes of each segment.
        let bytes: Vec<u8> = self.segments[..n].iter().flatten().flatten().copied().collect();
        let msg = TextMessage { bytes, text_control: self.text_control };
        if self.current.as_ref() == Some(&msg) {
            return None;
        }
        self.current = Some(msg.clone());
        Some(TextEvent::Message(msg))
    }

    fn clear_segments(&mut self) {
        self.segments = Default::default();
        self.num_segments = None;
    }
}

/// CRC-16 of the header and `body_len` body bytes matches the following two bytes.
fn crc_ok(seg: &[u8], body_len: usize) -> bool {
    let end = 2 + body_len;
    seg.len() >= end + 2 && crc16(&seg[..end]) == u16::from_be_bytes([seg[end], seg[end + 1]])
}

/// Text message encoder (transmitter side, Dream's `CTextMessageEncoder`): cycles
/// through the configured messages; call [`TextMessageEncoder::next_piece`] once per
/// logical frame. With several messages the toggle bit changes from one to the next.
/// [`TextMessageEncoder::set_messages`] replaces the cycle while it runs (e.g. with a
/// web stream's current title).
#[derive(Debug, Clone, Default)]
pub struct TextMessageEncoder {
    messages: Vec<Vec<u8>>,
    /// Segments of the current message, each with its marker and zero padding to a
    /// multiple of four bytes.
    segments: Vec<Vec<u8>>,
    segment: usize,
    pos: usize,
    message: usize,
    toggle: bool,
    /// Messages replacing the cycle at the next segment boundary.
    pending: Option<Vec<Vec<u8>>>,
}

/// `text` truncated to [`MAX_MESSAGE_BYTES`] at a character boundary; `None` if empty.
fn message_bytes(text: &str) -> Option<Vec<u8>> {
    let mut end = text.len().min(MAX_MESSAGE_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (end > 0).then(|| text.as_bytes()[..end].to_vec())
}

impl TextMessageEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a message to the cycle (truncated to 128 bytes at a character boundary;
    /// empty messages are ignored).
    pub fn add_message(&mut self, text: &str) {
        let Some(bytes) = message_bytes(text) else { return };
        self.messages.push(bytes);
        if self.messages.len() == 1 {
            self.start_message(0);
        }
    }

    /// Remove all messages; the encoder then sends 0x00 bytes.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Replace the cycle with `texts` (truncated as by [`Self::add_message`]; empty ones
    /// are ignored, an empty list stops the text). The segment being sent is finished
    /// first, so receivers never see a broken one; then the first new message starts
    /// with the toggle bit inverted, which tells receivers that a new message begins
    /// (ES 201 980 §6.5.2). A second call before that boundary replaces the first.
    pub fn set_messages<S: AsRef<str>>(&mut self, texts: &[S]) {
        self.pending = Some(texts.iter().filter_map(|t| message_bytes(t.as_ref())).collect());
        if self.pos == 0 {
            self.apply_pending();
        }
    }

    /// Switch to the pending messages (at a segment boundary).
    fn apply_pending(&mut self) {
        let Some(messages) = self.pending.take() else { return };
        self.messages = messages;
        self.toggle = !self.toggle;
        if self.messages.is_empty() {
            self.segments.clear();
            self.segment = 0;
            self.pos = 0;
        } else {
            self.start_message(0);
        }
    }

    /// The four text bytes for the next logical frame.
    pub fn next_piece(&mut self) -> [u8; 4] {
        if self.pos == 0 {
            self.apply_pending();
        }
        // `let … else`: bind `seg` if there is a segment, otherwise leave the function.
        let Some(seg) = self.segments.get(self.segment) else { return [0; 4] };
        let mut piece = [0u8; 4];
        piece.copy_from_slice(&seg[self.pos..self.pos + 4]);
        self.pos += 4;
        if self.pos >= seg.len() {
            self.pos = 0;
            self.segment += 1;
            if self.segment == self.segments.len() {
                let next = (self.message + 1) % self.messages.len();
                if self.messages.len() > 1 {
                    self.toggle = !self.toggle;
                }
                self.start_message(next);
            }
        }
        piece
    }

    fn start_message(&mut self, index: usize) {
        self.message = index;
        self.segment = 0;
        self.pos = 0;
        self.segments = segments(&self.messages[index], self.toggle);
    }
}

/// Split a message into transmitted segments (Dream's `CTextMessage::SetText`).
fn segments(text: &[u8], toggle: bool) -> Vec<Vec<u8>> {
    let chunks: Vec<&[u8]> = text.chunks(16).collect();
    let n = chunks.len();
    chunks
        .iter()
        .enumerate()
        .map(|(i, body)| {
            let first = i == 0;
            let last = i + 1 == n;
            let b0 = (u8::from(toggle) << 7) | (u8::from(first) << 6) | (u8::from(last) << 5) | (body.len() as u8 - 1);
            // Field 2: "1111" for the first segment, else rfa + SegNum; field 3: text
            // control field (0 = plain left-to-right text) or rfa.
            let b1 = if first { 0xF0 } else { (i as u8 & 7) << 4 };
            let mut seg = MARKER.to_vec();
            seg.extend_from_slice(&[b0, b1]);
            seg.extend_from_slice(body);
            let crc = crc16(&seg[4..]);
            seg.extend_from_slice(&crc.to_be_bytes());
            seg.resize(seg.len().div_ceil(4) * 4, 0);
            seg
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(enc: &mut TextMessageEncoder, dec: &mut TextMessageDecoder, pieces: usize) -> Vec<TextEvent> {
        (0..pieces).filter_map(|_| dec.push(enc.next_piece())).collect()
    }

    #[test]
    fn roundtrip_single_and_multi_segment() {
        for text in [
            "Hi",
            "Exactly sixteen!",
            "DecDRM text message test: äöü €, 日本語 and a long tail to fill several segments.",
        ] {
            let mut enc = TextMessageEncoder::new();
            enc.add_message(text);
            let mut dec = TextMessageDecoder::new();
            let ev = run(&mut enc, &mut dec, 200);
            // Reported once although it is repeated.
            assert_eq!(ev.len(), 1, "{text}: {ev:?}");
            let TextEvent::Message(m) = &ev[0] else { panic!("{ev:?}") };
            assert_eq!(m.text(), text);
            assert_eq!(dec.message().unwrap().text(), text);
        }
    }

    #[test]
    fn truncation_at_char_boundary() {
        let text = "€".repeat(50); // 150 bytes
        let mut enc = TextMessageEncoder::new();
        enc.add_message(&text);
        let mut dec = TextMessageDecoder::new();
        run(&mut enc, &mut dec, 200);
        let got = dec.message().unwrap().text();
        assert_eq!(got, "€".repeat(42)); // 126 bytes
    }

    #[test]
    fn several_messages_and_mid_stream_start() {
        let mut enc = TextMessageEncoder::new();
        enc.add_message("First message, a bit longer than one segment");
        enc.add_message("Second");
        let mut dec = TextMessageDecoder::new();
        // Start in the middle of the first message.
        for _ in 0..3 {
            enc.next_piece();
        }
        let ev = run(&mut enc, &mut dec, 120);
        let texts: Vec<String> = ev
            .iter()
            .map(|e| match e {
                TextEvent::Message(m) => m.text(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert!(texts.len() >= 3, "{texts:?}");
        assert!(texts.iter().any(|t| t == "Second"));
        assert!(texts.iter().any(|t| t == "First message, a bit longer than one segment"));
        // Alternating messages.
        assert!(texts.windows(2).all(|w| w[0] != w[1]));
    }

    #[test]
    fn corrupted_segment_is_ignored() {
        let mut enc = TextMessageEncoder::new();
        enc.add_message("Robust");
        let mut dec = TextMessageDecoder::new();
        let mut first = enc.next_piece(); // marker
        assert_eq!(first, MARKER);
        dec.push(first);
        first = enc.next_piece();
        first[2] ^= 0x20; // body byte
        assert_eq!(dec.push(first), None);
        for _ in 0..2 {
            assert_eq!(dec.push(enc.next_piece()), None);
        }
        assert!(dec.message().is_none());
        // The repetition gets through.
        let ev = run(&mut enc, &mut dec, 8);
        assert_eq!(ev.len(), 1);
    }

    #[test]
    fn last_segment_without_following_marker() {
        // Dream only decodes a segment when the next marker arrives; here the zeros
        // sent after the text stops must not prevent decoding.
        let seg = segments(b"End", false);
        let mut dec = TextMessageDecoder::new();
        let mut ev = None;
        for c in seg[0].chunks(4) {
            ev = ev.or(dec.push(c.try_into().unwrap()));
        }
        for _ in 0..5 {
            assert_eq!(dec.push([0; 4]), None);
        }
        assert_eq!(ev, Some(TextEvent::Message(TextMessage { bytes: b"End".to_vec(), text_control: 0 })));
    }

    #[test]
    fn commands() {
        // Clear display: toggle 1, first + last, command flag, command 0001, field 2/3 0.
        let mut seg = MARKER.to_vec();
        seg.extend_from_slice(&[0x80 | 0x40 | 0x20 | 0x10 | 0x01, 0x00]);
        let crc = crc16(&seg[4..6]);
        seg.extend_from_slice(&crc.to_be_bytes());
        let mut dec = TextMessageDecoder::new();
        let mut enc = TextMessageEncoder::new();
        enc.add_message("to be cleared");
        run(&mut enc, &mut dec, 20);
        assert!(dec.message().is_some());
        let mut ev = Vec::new();
        for _ in 0..2 {
            for c in seg.chunks(4) {
                ev.extend(dec.push(c.try_into().unwrap()));
            }
        }
        assert_eq!(ev, vec![TextEvent::Clear]); // the repetition is not reported again
        assert!(dec.message().is_none());
        // DL Plus: body length from field 2.
        let mut seg = MARKER.to_vec();
        seg.extend_from_slice(&[0x60 | 0x10 | 0x02, 0x20]);
        seg.extend_from_slice(&[0xAB, 0xCD, 0xEF]);
        let crc = crc16(&seg[4..9]);
        seg.extend_from_slice(&crc.to_be_bytes());
        seg.resize(12, 0);
        let ev: Vec<TextEvent> = seg.chunks(4).filter_map(|c| dec.push(c.try_into().unwrap())).collect();
        assert_eq!(ev, vec![TextEvent::DlPlus { field2: 2, field3: 0, body: vec![0xAB, 0xCD, 0xEF] }]);
    }

    #[test]
    fn display_text() {
        let m = TextMessage { bytes: b"Headline\x0bBody\x1ftext\x0aend".to_vec(), text_control: 0 };
        assert_eq!(m.display_text(), "Headline\nBody\u{AD}text\nend");
    }

    #[test]
    fn empty_encoder_sends_zeros() {
        let mut enc = TextMessageEncoder::new();
        assert_eq!(enc.next_piece(), [0; 4]);
        enc.add_message("");
        assert_eq!(enc.next_piece(), [0; 4]);
    }

    /// A replaced cycle starts at the next segment boundary with the toggle bit
    /// inverted, and the decoder reports the new message.
    #[test]
    fn replacing_the_messages() {
        let mut enc = TextMessageEncoder::new();
        let mut dec = TextMessageDecoder::new();
        // Nothing yet, then a title: it starts at once.
        enc.set_messages(&["Artist - First song"]);
        let ev = run(&mut enc, &mut dec, 40);
        assert_eq!(ev, vec![TextEvent::Message(TextMessage { bytes: b"Artist - First song".to_vec(), text_control: 0 })]);
        let toggle = enc.toggle;
        // Mid-segment: the current segment is finished before the switch.
        while enc.pos == 0 {
            dec.push(enc.next_piece());
        }
        enc.set_messages(&["Artist - Second song", "Static message"]);
        assert_ne!(enc.pos, 0, "switch deferred to the boundary");
        while enc.pos != 0 {
            assert_eq!(dec.push(enc.next_piece()), None);
        }
        let marker = enc.next_piece();
        assert_eq!(marker, MARKER, "the new message starts with a segment");
        assert_ne!(enc.toggle, toggle, "the toggle bit changed");
        dec.push(marker);
        let mut seen = Vec::new();
        for _ in 0..200 {
            if let Some(TextEvent::Message(m)) = dec.push(enc.next_piece()) {
                seen.push(m.text());
            }
        }
        assert_eq!(seen[0], "Artist - Second song", "{seen:?}");
        assert!(seen.iter().any(|t| t == "Static message"));
        assert!(seen.iter().all(|t| t != "Artist - First song"), "{seen:?}");
        // An empty list stops the text after the current segment.
        enc.set_messages::<&str>(&[]);
        for _ in 0..12 {
            enc.next_piece();
        }
        assert_eq!(enc.next_piece(), [0; 4]);
    }
}
