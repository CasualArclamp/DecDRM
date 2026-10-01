//! Ogg (RFC 3533): pages with a CRC, carrying the packets of one or more logical
//! streams. Icecast serves Ogg Vorbis, Opus and FLAC as *chained* streams — at a track
//! change the source ends the logical stream and starts a new one with fresh headers
//! (and new comments, which carry the title) — which general-purpose demuxers often do
//! not follow; this one does (see `decode::OggDecoder`).

use std::io::{self, Read};

/// Page header flag: the first packet continues one from the previous page.
pub(crate) const CONTINUED: u8 = 0x01;
/// Page header flag: first page of a logical stream.
pub(crate) const BOS: u8 = 0x02;
/// Page header flag: last page of a logical stream.
pub(crate) const EOS: u8 = 0x04;

/// One Ogg page.
#[derive(Debug, Clone)]
pub(crate) struct Page {
    pub flags: u8,
    pub serial: u32,
    pub sequence: u32,
    /// Lacing values (segment lengths).
    pub lacing: Vec<u8>,
    pub body: Vec<u8>,
}

/// CRC-32 of Ogg pages: polynomial 0x04C11DB7, not reflected, initial value and final
/// XOR 0 (RFC 3533 §6).
pub(crate) fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, entry) in t.iter_mut().enumerate() {
            let mut r = (i as u32) << 24;
            for _ in 0..8 {
                r = if r & 0x8000_0000 != 0 { (r << 1) ^ 0x04C1_1DB7 } else { r << 1 };
            }
            *entry = r;
        }
        t
    });
    data.iter().fold(0u32, |crc, &b| (crc << 8) ^ table[((crc >> 24) as u8 ^ b) as usize])
}

/// Reads pages, checking their CRC and resynchronising on the capture pattern `OggS`
/// after damage (or when the stream is joined mid-page).
pub(crate) struct PageReader {
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
    /// Bytes skipped to find pages (damage, or joining mid-page).
    pub skipped: u64,
    /// Bytes skipped since the last page.
    run: u64,
    max_search: u64,
}

impl PageReader {
    /// A reader that gives up (an `InvalidData` error) after searching `max_search`
    /// bytes in a row without finding a page.
    pub fn new(max_search: u64) -> Self {
        PageReader { buf: Vec::new(), pos: 0, eof: false, skipped: 0, run: 0, max_search }
    }

    /// Make `n` bytes available (reading only what is missing).
    fn fill(&mut self, input: &mut dyn Read, n: usize) -> io::Result<bool> {
        while self.buf.len() - self.pos < n && !self.eof {
            if self.pos > 64 * 1024 {
                self.buf.drain(..self.pos);
                self.pos = 0;
            }
            let mut chunk = [0u8; 8192];
            let want = (n - (self.buf.len() - self.pos)).clamp(1, chunk.len());
            let k = input.read(&mut chunk[..want])?;
            if k == 0 {
                self.eof = true;
            } else {
                self.buf.extend_from_slice(&chunk[..k]);
            }
        }
        Ok(self.buf.len() - self.pos >= n)
    }

    fn skip(&mut self) -> io::Result<()> {
        let here = &self.buf[self.pos + 1..];
        let next = here.windows(4).position(|w| w == b"OggS").map_or(self.buf.len().saturating_sub(3).max(self.pos + 1), |i| self.pos + 1 + i);
        self.skipped += (next - self.pos) as u64;
        self.run += (next - self.pos) as u64;
        self.pos = next;
        if self.run > self.max_search {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("no Ogg pages found in {} kB of data", self.run / 1024)));
        }
        Ok(())
    }

    /// The next intact page; `None` at the end of the stream.
    pub fn next_page(&mut self, input: &mut dyn Read) -> io::Result<Option<Page>> {
        loop {
            if !self.fill(input, 27)? {
                return Ok(None);
            }
            let h = &self.buf[self.pos..];
            if &h[..4] != b"OggS" || h[4] != 0 {
                self.skip()?;
                continue;
            }
            let segments = usize::from(h[26]);
            if !self.fill(input, 27 + segments)? {
                return Ok(None);
            }
            let h = &self.buf[self.pos..];
            let body_len: usize = h[27..27 + segments].iter().map(|&l| usize::from(l)).sum();
            let total = 27 + segments + body_len;
            if !self.fill(input, total)? {
                return Ok(None);
            }
            let page = &self.buf[self.pos..self.pos + total];
            let stored = u32::from_le_bytes([page[22], page[23], page[24], page[25]]);
            let mut check = page.to_vec();
            check[22..26].fill(0);
            if crc32(&check) != stored {
                self.skip()?;
                continue;
            }
            let le32 = |i: usize| u32::from_le_bytes([page[i], page[i + 1], page[i + 2], page[i + 3]]);
            let out = Page {
                flags: page[5],
                serial: le32(14),
                sequence: le32(18),
                lacing: page[27..27 + segments].to_vec(),
                body: page[27 + segments..].to_vec(),
            };
            self.pos += total;
            self.run = 0;
            return Ok(Some(out));
        }
    }
}

/// Reassembles the packets of one logical stream from its pages.
pub(crate) struct Packets {
    partial: Vec<u8>,
    /// A packet is continuing onto the next page.
    open: bool,
    next_sequence: Option<u32>,
}

impl Packets {
    pub fn new() -> Self {
        Packets { partial: Vec::new(), open: false, next_sequence: None }
    }

    /// Append the complete packets of `page` (of this stream) to `out`. A lost page
    /// (sequence gap) or a continuation without its start drops the broken packet.
    pub fn push(&mut self, page: &Page, out: &mut Vec<Vec<u8>>) {
        if self.next_sequence.is_some_and(|s| s != page.sequence) {
            self.partial.clear();
            self.open = false;
        }
        self.next_sequence = Some(page.sequence.wrapping_add(1));
        // Skip the continued part if its beginning is missing.
        let mut discard = page.flags & CONTINUED != 0 && !self.open;
        if page.flags & CONTINUED == 0 && self.open {
            self.partial.clear();
            self.open = false;
        }
        let mut offset = 0;
        for &len in &page.lacing {
            let len = usize::from(len);
            if !discard {
                self.partial.extend_from_slice(&page.body[offset..offset + len]);
            }
            offset += len;
            if len < 255 {
                if !discard {
                    out.push(std::mem::take(&mut self.partial));
                }
                discard = false;
                self.open = false;
            } else {
                self.open = !discard;
            }
        }
        if discard {
            self.open = false;
        }
    }
}

/// The comment fields (`KEY=value`) of a Vorbis comment block — Vorbis' comment header
/// after its 7-byte packet header, OpusTags after its 8-byte magic, FLAC's
/// VORBIS_COMMENT block.
pub(crate) fn comments(data: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let read_u32 = |p: usize| data.get(p..p + 4).map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")) as usize);
    let Some(vendor) = read_u32(0) else { return out };
    let mut p = 4 + vendor;
    let Some(count) = read_u32(p) else { return out };
    p += 4;
    for _ in 0..count.min(1000) {
        let Some(len) = read_u32(p) else { break };
        let Some(field) = data.get(p + 4..p + 4 + len) else { break };
        p += 4 + len;
        let field = String::from_utf8_lossy(field);
        if let Some((k, v)) = field.split_once('=') {
            out.push((k.to_ascii_uppercase(), v.trim().to_string()));
        }
    }
    out
}

/// "Artist - Title" from comment fields (just the title without an artist; `None`
/// without a title).
pub(crate) fn title_of(fields: &[(String, String)]) -> Option<String> {
    let get = |key: &str| fields.iter().find(|(k, v)| k == key && !v.is_empty()).map(|(_, v)| v.as_str());
    let title = get("TITLE")?;
    Some(match get("ARTIST") {
        Some(artist) if !title.contains(artist) => format!("{artist} - {title}"),
        _ => title.to_string(),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Ogg pages for tests: packets of one logical stream, `granule` per audio packet.
    pub(crate) struct OggWriter {
        serial: u32,
        sequence: u32,
        pub out: Vec<u8>,
    }

    impl OggWriter {
        pub fn new(serial: u32) -> Self {
            OggWriter { serial, sequence: 0, out: Vec::new() }
        }

        /// One page holding `packets` (each must fit: < 255 lacing values in total).
        pub fn page(&mut self, packets: &[&[u8]], granule: u64, flags: u8) {
            let mut lacing = Vec::new();
            let mut body = Vec::new();
            for p in packets {
                let mut left = p.len();
                loop {
                    let l = left.min(255);
                    lacing.push(l as u8);
                    left -= l;
                    if l < 255 {
                        break;
                    }
                }
                body.extend_from_slice(p);
            }
            self.raw_page(&lacing, &body, granule, flags);
        }

        /// One page with explicit lacing values.
        pub fn raw_page(&mut self, lacing: &[u8], body: &[u8], granule: u64, flags: u8) {
            assert!(lacing.len() < 256);
            let mut page = b"OggS\0".to_vec();
            page.push(flags);
            page.extend_from_slice(&granule.to_le_bytes());
            page.extend_from_slice(&self.serial.to_le_bytes());
            page.extend_from_slice(&self.sequence.to_le_bytes());
            page.extend_from_slice(&[0; 4]);
            page.push(lacing.len() as u8);
            page.extend_from_slice(lacing);
            page.extend_from_slice(body);
            let crc = crc32(&page);
            page[22..26].copy_from_slice(&crc.to_le_bytes());
            self.out.extend_from_slice(&page);
            self.sequence += 1;
        }
    }

    /// A Vorbis comment block body with `fields`.
    pub(crate) fn comment_block(fields: &[&str]) -> Vec<u8> {
        let mut b = Vec::new();
        let vendor = b"DecDRM test";
        b.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        b.extend_from_slice(vendor);
        b.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        for f in fields {
            b.extend_from_slice(&(f.len() as u32).to_le_bytes());
            b.extend_from_slice(f.as_bytes());
        }
        b
    }

    #[test]
    fn crc_reference() {
        // The Ogg CRC is CRC-32/CKSUM without its final inversion (whose check value
        // for "123456789" is 0x765E7680).
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0x765E_7680 ^ 0xFFFF_FFFF);
    }

    /// Pages are read back with their packets (one spanning two pages); a damaged page
    /// is skipped and its packets lost; junk before the first page is skipped.
    #[test]
    fn pages_and_packets() {
        let mut w = OggWriter::new(7);
        let big = vec![0xAB; 600];
        w.page(&[b"first"], 0, BOS);
        // A 600-byte packet over two pages: lacing 255, 255 (continues), then 90.
        w.raw_page(&[255, 255], &big[..510], 0, 0);
        w.raw_page(&[90, 5], &[&big[510..], b"after"].concat(), 100, CONTINUED);
        let damaged = w.out.len();
        w.page(&[b"lost"], 200, 0);
        w.page(&[b"last"], 300, EOS);
        let mut bytes = w.out.clone();
        bytes[damaged + 28] ^= 0x55; // a body byte of the "lost" page
        let mut stream = b"junk".to_vec();
        stream.extend_from_slice(&bytes);
        let mut reader = PageReader::new(1 << 20);
        let mut packets = Packets::new();
        let mut input = &stream[..];
        let mut got = Vec::new();
        let mut serials = Vec::new();
        while let Some(page) = reader.next_page(&mut input).unwrap() {
            serials.push(page.serial);
            packets.push(&page, &mut got);
        }
        assert_eq!(serials.len(), 4, "the damaged page is dropped");
        assert_eq!(got.len(), 4, "{:?}", got.iter().map(Vec::len).collect::<Vec<_>>());
        assert_eq!(got[0], b"first");
        assert_eq!(got[1], big);
        assert_eq!(got[2], b"after");
        assert_eq!(got[3], b"last");
        assert!(reader.skipped >= 4);
    }

    #[test]
    fn comment_titles() {
        let block = comment_block(&["ARTIST=Band", "title=Song", "ALBUM=x"]);
        let fields = comments(&block);
        assert_eq!(title_of(&fields).as_deref(), Some("Band - Song"));
        assert_eq!(title_of(&comments(&comment_block(&["TITLE=Band - Song", "ARTIST=Band"]))).as_deref(), Some("Band - Song"));
        assert_eq!(title_of(&comments(&comment_block(&["ARTIST=Band"]))), None);
        assert!(comments(&[1, 2]).is_empty());
    }
}
