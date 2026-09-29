//! DRM packet mode (ES 201 980 §6.6).
//!
//! A packet-mode MSC stream is cut into fixed-length packets:
//!
//! ```text
//! | header (1 byte)                                   | data field (n bytes) | CRC (2) |
//! | first 1 | last 1 | packet id 2 | PPI 1 | CI 3 |
//! ```
//!
//! * `n` is the SDC type 5 "packet length"; the total packet length is `n + 3`.
//! * Up to four packet ids share one stream; data units (normally MSC data groups)
//!   span one or more packets delimited by the first/last flags.
//! * PPI (padded packet indicator) = 1 means the first data-field byte gives the number
//!   of useful bytes that follow; the rest is padding. A packet with PPI = 1, first =
//!   last = 1 and zero useful bytes carries no data at all.
//! * CI (continuity index) increments modulo 8 for every packet of a packet id,
//!   including padding packets.
//! * The CRC-16 covers header and data field ([`crate::crc`]).
//!
//! The receiver side ([`PacketDemux`]) is a port of `CDataDecoder::ProcessDataInternal`
//! (Dream `DataDecoder.cpp`); the transmitter side ([`PacketEncoder`], [`PacketMux`])
//! replaces `CDataEncoder::GeneratePacket` (`DataEncoder.cpp`), whose padded packets
//! are one byte too long and filled with stale data instead of zeros.

use crate::crc::{append_crc16, crc16_check};
use crate::encoder::DataUnitSource;
use crate::error::{DataError, Result};

/// Bytes of packet header.
pub const HEADER_LEN: usize = 1;
/// Bytes of packet CRC.
pub const CRC_LEN: usize = 2;
/// Header + CRC: the total packet length is the SDC "packet length" plus this.
pub const OVERHEAD: usize = HEADER_LEN + CRC_LEN;
/// Smallest usable total packet length (one data byte).
pub const MIN_PACKET_LEN: usize = OVERHEAD + 1;
/// Largest total packet length (the SDC packet length field has 8 bits).
pub const MAX_PACKET_LEN: usize = OVERHEAD + 255;
/// Data units longer than this are discarded by the demultiplexer (memory guard).
pub const MAX_DATA_UNIT_LEN: usize = 1 << 20;

/// The one-byte packet header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PacketHeader {
    /// First packet of a data unit.
    pub first: bool,
    /// Last packet of a data unit.
    pub last: bool,
    /// Packet id 0..=3.
    pub packet_id: u8,
    /// Padded packet indicator.
    pub padded: bool,
    /// Continuity index 0..=7.
    pub continuity: u8,
}

impl PacketHeader {
    /// Decode the header byte.
    pub fn from_byte(b: u8) -> Self {
        Self {
            first: b & 0x80 != 0,
            last: b & 0x40 != 0,
            packet_id: (b >> 4) & 0x03,
            padded: b & 0x08 != 0,
            continuity: b & 0x07,
        }
    }

    /// Encode the header byte.
    pub fn to_byte(self) -> u8 {
        (u8::from(self.first) << 7)
            | (u8::from(self.last) << 6)
            | ((self.packet_id & 0x03) << 4)
            | (u8::from(self.padded) << 3)
            | (self.continuity & 0x07)
    }
}

/// A CRC-checked packet with its useful payload (padding removed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet<'a> {
    /// Decoded header.
    pub header: PacketHeader,
    /// Useful bytes of the data field.
    pub payload: &'a [u8],
}

impl<'a> Packet<'a> {
    /// Parse and CRC-check one packet; `bytes.len()` is the total packet length.
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() < MIN_PACKET_LEN {
            return Err(DataError::Truncated);
        }
        if !crc16_check(bytes) {
            return Err(DataError::CrcMismatch);
        }
        let header = PacketHeader::from_byte(bytes[0]);
        let payload = useful_payload(header, &bytes[HEADER_LEN..bytes.len() - CRC_LEN])?;
        Ok(Self { header, payload })
    }

    /// `true` for a packet that carries no useful data (PPI = 1, length 0, first = last = 1).
    pub fn is_padding(&self) -> bool {
        self.header.first && self.header.last && self.header.padded && self.payload.is_empty()
    }
}

/// Apply the padded-packet indicator to a data field.
fn useful_payload(header: PacketHeader, field: &[u8]) -> Result<&[u8]> {
    if !header.padded {
        return Ok(field);
    }
    let n = usize::from(field[0]);
    if n > field.len() - 1 {
        return Err(DataError::Malformed("padded packet length"));
    }
    Ok(&field[1..1 + n])
}

/// Build a packet of `packet_len` total bytes. The PPI is derived from the payload
/// length (`header.padded` is ignored): a short payload is preceded by its length and
/// followed by zero padding.
pub fn build_packet(header: PacketHeader, payload: &[u8], packet_len: usize) -> Result<Vec<u8>> {
    if !(MIN_PACKET_LEN..=MAX_PACKET_LEN).contains(&packet_len) {
        return Err(DataError::Config("packet length"));
    }
    let field_len = packet_len - OVERHEAD;
    let padded = payload.len() < field_len;
    if payload.len() > field_len {
        return Err(DataError::OutOfRange("packet payload"));
    }
    let mut out = Vec::with_capacity(packet_len);
    out.push(PacketHeader { padded, ..header }.to_byte());
    if padded {
        out.push(payload.len() as u8);
    }
    out.extend_from_slice(payload);
    out.resize(packet_len - CRC_LEN, 0);
    append_crc16(&mut out);
    Ok(out)
}

/// Counters for one packet id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PacketIdStats {
    /// CRC-good packets with this id (including padding packets).
    pub packets: u64,
    /// Continuity-index jumps.
    pub continuity_errors: u64,
    /// Padding packets (no useful data).
    pub padding_packets: u64,
    /// Complete data units delivered.
    pub data_units: u64,
    /// Data units abandoned because packets were lost or inconsistent.
    pub data_units_dropped: u64,
}

/// Counters for a whole packet stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PacketStats {
    /// Packets whose CRC was correct.
    pub packets_ok: u64,
    /// Packets whose CRC was wrong (their packet id is unknown).
    pub packets_crc_error: u64,
    /// CRC-good packets with an impossible PPI length byte.
    pub packets_malformed: u64,
    /// Per packet id counters.
    pub per_id: [PacketIdStats; 4],
}

/// A complete data unit recovered from the packet stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataUnit {
    /// Packet id it was carried on.
    pub packet_id: u8,
    /// The reassembled bytes (normally one MSC data group).
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
struct Channel {
    last_ci: Option<u8>,
    buf: Vec<u8>,
    /// A first packet has been seen and no packet since was lost.
    ok: bool,
    /// A data unit is in progress (first seen, last not yet).
    started: bool,
}

/// Receiver: splits each frame's stream bytes into packets, checks CRC and continuity
/// and reassembles data units for all four packet ids.
#[derive(Debug, Clone)]
pub struct PacketDemux {
    packet_len: usize,
    channels: [Channel; 4],
    stats: PacketStats,
}

impl PacketDemux {
    /// `packet_len` is the *total* packet length (SDC packet length + 3).
    pub fn new(packet_len: usize) -> Result<Self> {
        if !(MIN_PACKET_LEN..=MAX_PACKET_LEN).contains(&packet_len) {
            return Err(DataError::Config("packet length"));
        }
        Ok(Self {
            packet_len,
            channels: Default::default(),
            stats: PacketStats::default(),
        })
    }

    /// Total packet length in bytes.
    pub fn packet_len(&self) -> usize {
        self.packet_len
    }

    /// Cumulative counters.
    pub fn stats(&self) -> &PacketStats {
        &self.stats
    }

    /// Forget partially received data units and continuity state (keeps counters).
    pub fn reset(&mut self) {
        self.channels = Default::default();
    }

    /// Feed one multiplex frame's worth of stream bytes. Trailing bytes that do not
    /// form a whole packet are ignored, as in Dream.
    pub fn push_frame(&mut self, stream: &[u8]) -> Vec<DataUnit> {
        // Rust note: `chunks_exact` yields only full-length chunks; the remainder is
        // simply not visited.
        stream
            .chunks_exact(self.packet_len)
            .filter_map(|p| self.push_packet(p))
            .collect()
    }

    /// Feed a single packet (`packet.len()` must be the configured packet length).
    pub fn push_packet(&mut self, packet: &[u8]) -> Option<DataUnit> {
        if packet.len() != self.packet_len || !crc16_check(packet) {
            self.stats.packets_crc_error += 1;
            return None;
        }
        self.stats.packets_ok += 1;
        let header = PacketHeader::from_byte(packet[0]);
        let id = usize::from(header.packet_id);
        let ch = &mut self.channels[id];
        let st = &mut self.stats.per_id[id];
        st.packets += 1;

        // ES 201 980: the CI "shall increment by one modulo-8 for each packet with this
        // packet Id". A jump means packets were lost: the unit in progress is broken.
        if let Some(prev) = ch.last_ci
            && (prev + 1) & 7 != header.continuity
        {
            st.continuity_errors += 1;
            ch.ok = false;
        }
        ch.last_ci = Some(header.continuity);

        let payload = match useful_payload(header, &packet[HEADER_LEN..packet.len() - CRC_LEN]) {
            Ok(p) => p,
            Err(_) => {
                self.stats.packets_malformed += 1;
                ch.ok = false;
                return None;
            }
        };

        if header.first && header.last && header.padded && payload.is_empty() {
            st.padding_packets += 1;
            if ch.started {
                st.data_units_dropped += 1;
            }
            ch.started = false;
            ch.buf.clear();
            return None;
        }

        if header.first {
            if ch.started {
                // The previous unit never got its last packet.
                st.data_units_dropped += 1;
            }
            ch.buf.clear();
            ch.ok = true;
            ch.started = true;
        }
        if ch.started {
            ch.buf.extend_from_slice(payload);
            if ch.buf.len() > MAX_DATA_UNIT_LEN {
                st.data_units_dropped += 1;
                ch.started = false;
                ch.ok = false;
                ch.buf.clear();
                return None;
            }
        }
        if !header.last {
            return None;
        }
        // Last packet of a unit: deliver if the whole unit arrived intact.
        let complete = ch.started && ch.ok && !ch.buf.is_empty();
        ch.started = false;
        ch.ok = false;
        if complete {
            st.data_units += 1;
            // Rust note: `std::mem::take` moves the buffer out and leaves an empty `Vec`
            // behind, avoiding a copy.
            Some(DataUnit {
                packet_id: header.packet_id,
                data: std::mem::take(&mut ch.buf),
            })
        } else {
            st.data_units_dropped += 1;
            ch.buf.clear();
            None
        }
    }
}

/// Transmitter: cuts data units into packets for one packet id.
#[derive(Debug, Clone)]
pub struct PacketEncoder {
    packet_id: u8,
    packet_len: usize,
    single_packet_units: bool,
    continuity: u8,
    unit: Vec<u8>,
    offset: usize,
    oversize_dropped: u64,
}

impl PacketEncoder {
    /// `packet_len` is the total packet length. With `data_unit_indicator == false`
    /// (SDC "single packets") every data unit must fit into one packet; longer units
    /// are dropped and counted in [`Self::oversize_dropped`].
    pub fn new(packet_id: u8, packet_len: usize, data_unit_indicator: bool) -> Result<Self> {
        if packet_id > 3 {
            return Err(DataError::Config("packet id"));
        }
        if !(MIN_PACKET_LEN..=MAX_PACKET_LEN).contains(&packet_len) {
            return Err(DataError::Config("packet length"));
        }
        Ok(Self {
            packet_id,
            packet_len,
            single_packet_units: !data_unit_indicator,
            continuity: 0,
            unit: Vec::new(),
            offset: 0,
            oversize_dropped: 0,
        })
    }

    /// Packet id this encoder writes.
    pub fn packet_id(&self) -> u8 {
        self.packet_id
    }

    /// Number of data units dropped because they did not fit a single packet.
    pub fn oversize_dropped(&self) -> u64 {
        self.oversize_dropped
    }

    fn field_len(&self) -> usize {
        self.packet_len - OVERHEAD
    }

    /// `true` while a data unit is partially sent.
    pub fn is_busy(&self) -> bool {
        self.offset < self.unit.len()
    }

    /// Make sure a unit is loaded; `false` if the source has nothing to send.
    fn fill(&mut self, source: &mut dyn DataUnitSource) -> bool {
        // Bounded so that a misbehaving source returning only unusable units cannot
        // hang the transmitter.
        for _ in 0..64 {
            if self.is_busy() {
                return true;
            }
            match source.next_data_unit() {
                None => return false,
                Some(unit) if unit.is_empty() => {}
                Some(unit) if self.single_packet_units && unit.len() > self.field_len() => {
                    self.oversize_dropped += 1;
                }
                Some(unit) => {
                    self.unit = unit;
                    self.offset = 0;
                }
            }
        }
        self.is_busy()
    }

    /// Next packet carrying data pulled from `source`, or `None` if the source is idle.
    ///
    /// Rust note: `&mut dyn DataUnitSource` is a *trait object*: any type implementing
    /// the trait can be passed, and the call is dispatched at run time (like a C++
    /// virtual call).
    pub fn next_data_packet(&mut self, source: &mut dyn DataUnitSource) -> Option<Vec<u8>> {
        if !self.fill(source) {
            return None;
        }
        let remaining = self.unit.len() - self.offset;
        let first = self.offset == 0;
        let (take, last) = if remaining <= self.field_len() {
            (remaining, true)
        } else {
            (self.field_len(), false)
        };
        let header = PacketHeader {
            first,
            last,
            packet_id: self.packet_id,
            padded: false,
            continuity: self.continuity,
        };
        let packet = build_packet(
            header,
            &self.unit[self.offset..self.offset + take],
            self.packet_len,
        )
        .expect("payload length checked above");
        self.continuity = (self.continuity + 1) & 7;
        self.offset += take;
        if last {
            self.unit.clear();
            self.offset = 0;
        }
        Some(packet)
    }

    /// A packet without useful data (first = last = PPI = 1, length byte 0). The CI is
    /// incremented, as ES 201 980 requires for these empty packets.
    pub fn padding_packet(&mut self) -> Vec<u8> {
        let header = PacketHeader {
            first: true,
            last: true,
            packet_id: self.packet_id,
            padded: true,
            continuity: self.continuity,
        };
        self.continuity = (self.continuity + 1) & 7;
        build_packet(header, &[], self.packet_len).expect("valid packet length")
    }
}

struct MuxChannel {
    encoder: PacketEncoder,
    source: Box<dyn DataUnitSource + Send>,
}

/// Transmitter: multiplexes up to four packet ids (each with its own data source) into
/// one packet-mode stream, round-robin per packet, inserting padding packets when all
/// sources are idle.
pub struct PacketMux {
    packet_len: usize,
    channels: Vec<MuxChannel>,
    next: usize,
    idle: PacketEncoder,
}

impl std::fmt::Debug for PacketMux {
    /// Rust note: written by hand because the boxed sources are trait objects, which
    /// do not implement `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PacketMux")
            .field("packet_len", &self.packet_len)
            .field(
                "channels",
                &self.channels.iter().map(|c| &c.encoder).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl PacketMux {
    /// Create an empty multiplexer for `packet_len`-byte packets (total length).
    pub fn new(packet_len: usize) -> Result<Self> {
        Ok(Self {
            packet_len,
            channels: Vec::new(),
            next: 0,
            idle: PacketEncoder::new(0, packet_len, true)?,
        })
    }

    /// Total packet length in bytes.
    pub fn packet_len(&self) -> usize {
        self.packet_len
    }

    /// Add a data source on `packet_id`.
    ///
    /// Rust note: `Box<dyn DataUnitSource + Send>` is an owned, heap-allocated trait
    /// object; `+ Send` promises it may be moved to another thread (e.g. the
    /// transmitter thread).
    pub fn add_channel(
        &mut self,
        packet_id: u8,
        data_unit_indicator: bool,
        source: Box<dyn DataUnitSource + Send>,
    ) -> Result<()> {
        if self
            .channels
            .iter()
            .any(|c| c.encoder.packet_id() == packet_id)
        {
            return Err(DataError::Config("duplicate packet id"));
        }
        let encoder = PacketEncoder::new(packet_id, self.packet_len, data_unit_indicator)?;
        if self.channels.is_empty() {
            // Padding packets go out on the first channel's packet id.
            self.idle = PacketEncoder::new(packet_id, self.packet_len, true)?;
        }
        self.channels.push(MuxChannel { encoder, source });
        Ok(())
    }

    /// Produce the next packet.
    pub fn next_packet(&mut self) -> Vec<u8> {
        let n = self.channels.len();
        for k in 0..n {
            let idx = (self.next + k) % n;
            let ch = &mut self.channels[idx];
            if let Some(packet) = ch.encoder.next_data_packet(ch.source.as_mut()) {
                self.next = (idx + 1) % n;
                return packet;
            }
        }
        // Nothing to send. A padding packet must continue the CI sequence of its packet
        // id, so use that channel's encoder when it is idle.
        match self
            .channels
            .iter_mut()
            .find(|c| c.encoder.packet_id() == self.idle.packet_id())
        {
            Some(ch) if !ch.encoder.is_busy() => ch.encoder.padding_packet(),
            _ => self.idle.padding_packet(),
        }
    }

    /// Produce one frame of `frame_len` stream bytes: as many whole packets as fit,
    /// followed by zero bytes if `frame_len` is not a multiple of the packet length.
    pub fn next_frame(&mut self, frame_len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(frame_len);
        for _ in 0..frame_len / self.packet_len {
            out.extend_from_slice(&self.next_packet());
        }
        out.resize(frame_len, 0);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn header_byte_round_trip() {
        for b in 0..=255u8 {
            assert_eq!(PacketHeader::from_byte(b).to_byte(), b);
        }
        let h = PacketHeader::from_byte(0b1101_1101);
        assert!(h.first && h.last && h.padded);
        assert_eq!((h.packet_id, h.continuity), (1, 5));
    }

    #[test]
    fn build_and_parse_padded_packet() {
        let h = PacketHeader {
            first: true,
            last: true,
            packet_id: 2,
            padded: false,
            continuity: 3,
        };
        let p = build_packet(h, b"abc", 10).unwrap();
        assert_eq!(p.len(), 10);
        // header, length byte, 3 useful bytes, 2 zero padding bytes, CRC
        assert_eq!(&p[..7], &[0b1110_1011, 3, b'a', b'b', b'c', 0, 0]);
        let parsed = Packet::parse(&p).unwrap();
        assert_eq!(parsed.payload, b"abc");
        assert!(parsed.header.padded);
        // A full payload is not padded.
        let full = build_packet(h, b"1234567", 10).unwrap();
        let parsed = Packet::parse(&full).unwrap();
        assert!(!parsed.header.padded);
        assert_eq!(parsed.payload, b"1234567");
        assert!(build_packet(h, b"12345678", 10).is_err());
    }

    #[test]
    fn corrupted_packet_is_rejected() {
        let h = PacketHeader {
            first: true,
            last: true,
            ..Default::default()
        };
        let mut p = build_packet(h, b"hello", 12).unwrap();
        p[4] ^= 0x01;
        assert_eq!(Packet::parse(&p), Err(DataError::CrcMismatch));
    }

    #[test]
    fn padded_length_larger_than_field_is_malformed() {
        let h = PacketHeader {
            first: true,
            last: true,
            padded: true,
            ..Default::default()
        };
        let mut p = vec![h.to_byte(), 9, 0, 0, 0];
        append_crc16(&mut p);
        assert_eq!(
            Packet::parse(&p),
            Err(DataError::Malformed("padded packet length"))
        );
        let mut demux = PacketDemux::new(p.len()).unwrap();
        assert!(demux.push_packet(&p).is_none());
        assert_eq!(demux.stats().packets_malformed, 1);
    }

    fn packets_for(unit: &[u8], id: u8, packet_len: usize, ci_start: u8) -> Vec<Vec<u8>> {
        let mut enc = PacketEncoder::new(id, packet_len, true).unwrap();
        enc.continuity = ci_start;
        let mut q: VecDeque<Vec<u8>> = VecDeque::from([unit.to_vec()]);
        let mut out = Vec::new();
        while let Some(p) = enc.next_data_packet(&mut q) {
            out.push(p);
        }
        out
    }

    #[test]
    fn multi_packet_unit_reassembles() {
        let unit: Vec<u8> = (0..100).collect();
        let packets = packets_for(&unit, 1, 23, 6);
        assert_eq!(packets.len(), 5); // 20 data bytes per packet
        let mut demux = PacketDemux::new(23).unwrap();
        let stream: Vec<u8> = packets.concat();
        let units = demux.push_frame(&stream);
        assert_eq!(
            units,
            vec![DataUnit {
                packet_id: 1,
                data: unit
            }]
        );
        assert_eq!(demux.stats().per_id[1].continuity_errors, 0);
    }

    #[test]
    fn lost_packet_drops_unit_and_next_unit_recovers() {
        let unit_a: Vec<u8> = (0..60).collect();
        let unit_b: Vec<u8> = (100..130).collect();
        let mut enc = PacketEncoder::new(0, 13, true).unwrap();
        let mut q: VecDeque<Vec<u8>> = VecDeque::from([unit_a, unit_b.clone()]);
        let mut packets = Vec::new();
        while let Some(p) = enc.next_data_packet(&mut q) {
            packets.push(p);
        }
        // unit A = 6 packets of 10 bytes, unit B = 3 packets.
        assert_eq!(packets.len(), 9);
        packets.remove(2);
        let mut demux = PacketDemux::new(13).unwrap();
        let units = demux.push_frame(&packets.concat());
        assert_eq!(
            units,
            vec![DataUnit {
                packet_id: 0,
                data: unit_b
            }]
        );
        let st = &demux.stats().per_id[0];
        assert_eq!(st.continuity_errors, 1);
        assert_eq!(st.data_units_dropped, 1);
    }

    #[test]
    fn crc_error_inside_unit_drops_it() {
        let unit: Vec<u8> = (0..30).collect();
        let mut packets = packets_for(&unit, 3, 13, 0);
        packets[1][5] ^= 0xFF;
        let mut demux = PacketDemux::new(13).unwrap();
        assert!(demux.push_frame(&packets.concat()).is_empty());
        assert_eq!(demux.stats().packets_crc_error, 1);
        assert_eq!(demux.stats().per_id[3].data_units_dropped, 1);
    }

    #[test]
    fn interleaved_packet_ids_and_padding() {
        let a: Vec<u8> = (0..25).collect();
        let b: Vec<u8> = (50..80).collect();
        let mut mux = PacketMux::new(13).unwrap();
        mux.add_channel(0, true, Box::new(VecDeque::from([a.clone()])))
            .unwrap();
        mux.add_channel(2, true, Box::new(VecDeque::from([b.clone()])))
            .unwrap();
        // 3 + 3 data packets, then padding; 130 bytes = 10 packets exactly.
        let frame = mux.next_frame(135);
        assert_eq!(frame.len(), 135);
        let mut demux = PacketDemux::new(13).unwrap();
        let mut units = demux.push_frame(&frame);
        units.sort_by_key(|u| u.packet_id);
        assert_eq!(
            units,
            vec![
                DataUnit {
                    packet_id: 0,
                    data: a
                },
                DataUnit {
                    packet_id: 2,
                    data: b
                }
            ]
        );
        let st = demux.stats();
        assert_eq!(st.per_id[0].padding_packets, 4);
        assert_eq!(st.per_id[0].continuity_errors, 0);
        assert_eq!(st.packets_crc_error, 0);
    }

    #[test]
    fn single_packet_mode_drops_oversize_units() {
        let mut enc = PacketEncoder::new(0, 8, false).unwrap();
        let mut q: VecDeque<Vec<u8>> = VecDeque::from([vec![1; 9], vec![2; 5]]);
        let p = enc.next_data_packet(&mut q).unwrap();
        assert_eq!(Packet::parse(&p).unwrap().payload, &[2; 5]);
        assert_eq!(enc.oversize_dropped(), 1);
    }
}
