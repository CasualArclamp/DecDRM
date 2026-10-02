//! Recordings of DCP packets:
//!
//! * **file framing** (TS 102 821 annex B, what Dream writes as `.rsA` … `.rsZ` and
//!   `.ff`): a `fio_` TAG item per packet, holding an `afpf` item (the AF packet or PFT
//!   fragment) and optionally a `time` item (seconds and nanoseconds);
//! * **raw** AF packets or PFT fragments one after another;
//! * **pcap** and **pcapng** captures (Wireshark, tcpdump, Dream's `.pcap`): the UDP
//!   payloads, optionally only those to one port; IPv4 fragments are put back together
//!   (an AF packet without PFT is often larger than a network packet).
//!
//! [`DcpFileReader::open`] recognises the format by its first bytes.

use crate::af::AfPacket;
use crate::pft::PftFragment;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

/// Format of a recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// `fio_` file framing (Dream's `.rsX`, `.ff`).
    FileIo,
    RawAf,
    RawPft,
    Pcap,
    PcapNg,
}

impl FileKind {
    pub fn name(self) -> &'static str {
        match self {
            FileKind::FileIo => "RSCI/DCP file framing",
            FileKind::RawAf => "raw AF packets",
            FileKind::RawPft => "raw PFT fragments",
            FileKind::Pcap => "pcap capture",
            FileKind::PcapNg => "pcapng capture",
        }
    }
}

/// File name extensions of DCP recordings: Dream's `.rsA` … `.rsZ` (the letter is the
/// RSCI profile), `.ff`, `.af`, `.pf`, `.pft`, `.mdi`, `.dcp`, and captures.
pub fn has_recording_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else { return false };
    let lower = ext.to_ascii_lowercase();
    matches!(lower.as_str(), "ff" | "af" | "pf" | "pft" | "mdi" | "dcp" | "rsci" | "pcap" | "pcapng" | "cap")
        || (lower.len() == 3 && lower.starts_with("rs") && ext.as_bytes()[2].is_ascii_alphabetic())
}

/// One packet of a recording.
#[derive(Debug, Clone, PartialEq)]
pub struct DcpPacket {
    pub data: Vec<u8>,
    /// Time stamp, seconds (captures and file framing with a `time` item).
    pub time_s: Option<f64>,
}

/// Byte order of a capture.
#[derive(Debug, Clone, Copy)]
struct Endian {
    big: bool,
}

impl Endian {
    fn u16(self, b: &[u8]) -> u16 {
        if self.big { u16::from_be_bytes([b[0], b[1]]) } else { u16::from_le_bytes([b[0], b[1]]) }
    }
    fn u32(self, b: &[u8]) -> u32 {
        if self.big { u32::from_be_bytes([b[0], b[1], b[2], b[3]]) } else { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) }
    }
}

/// A pcapng interface: link type and time stamp units per second.
#[derive(Debug, Clone, Copy)]
struct Interface {
    link: u32,
    ticks_per_s: f64,
}

/// Reads the packets of a DCP recording.
pub struct DcpFileReader {
    kind: FileKind,
    r: BufReader<File>,
    /// Captures: only UDP datagrams to this port.
    port: Option<u16>,
    endian: Endian,
    /// pcap: link type and whether time stamps are in nanoseconds.
    link: u32,
    nanos: bool,
    /// pcapng: interfaces of the current section.
    interfaces: Vec<Interface>,
    defrag: Defragmenter,
    /// The first four bytes when they belong to the first packet (read by `open` to
    /// recognise the format).
    head: Option<[u8; 4]>,
    /// Bytes read, for progress displays.
    pub bytes_read: u64,
    /// Total size of the file.
    pub file_size: u64,
}

fn read_exact_or_eof(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "recording ends inside a packet")),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

impl DcpFileReader {
    /// Open a recording; `port`: for captures, only UDP datagrams to this port (Dream
    /// writes it as `file.pcap#port`).
    pub fn open(path: &Path, port: Option<u16>) -> io::Result<Self> {
        let file = File::open(path)?;
        let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);
        let mut r = BufReader::new(file);
        let mut head = [0u8; 4];
        if !read_exact_or_eof(&mut r, &mut head)? {
            return Err(bad("the recording is empty"));
        }
        let le = u32::from_le_bytes(head);
        let mut reader = Self {
            kind: FileKind::RawAf,
            r,
            port,
            endian: Endian { big: false },
            link: 1,
            nanos: false,
            interfaces: Vec::new(),
            defrag: Defragmenter::default(),
            head: None,
            bytes_read: 4,
            file_size,
        };
        match le {
            0xA1B2_C3D4 | 0xA1B2_3C4D | 0xD4C3_B2A1 | 0x4D3C_B2A1 => {
                reader.kind = FileKind::Pcap;
                reader.endian.big = matches!(le, 0xD4C3_B2A1 | 0x4D3C_B2A1);
                reader.nanos = matches!(le, 0xA1B2_3C4D | 0x4D3C_B2A1);
                let mut rest = [0u8; 20];
                if !read_exact_or_eof(&mut reader.r, &mut rest)? {
                    return Err(bad("truncated pcap header"));
                }
                reader.bytes_read += 20;
                reader.link = reader.endian.u32(&rest[16..20]) & 0x0FFF_FFFF;
            }
            0x0A0D_0D0A => {
                reader.kind = FileKind::PcapNg;
                // The section header: length, then the byte-order magic.
                let mut rest = [0u8; 8];
                if !read_exact_or_eof(&mut reader.r, &mut rest)? {
                    return Err(bad("truncated pcapng header"));
                }
                reader.endian.big = rest[4..8] == [0x1A, 0x2B, 0x3C, 0x4D];
                let len = reader.endian.u32(&rest[0..4]) as usize;
                reader.skip(len.checked_sub(12).ok_or_else(|| bad("bad pcapng section header"))?)?;
                reader.bytes_read += 8;
            }
            _ if &head == b"fio_" => reader.kind = FileKind::FileIo,
            _ if &head[..2] == b"AF" => reader.kind = FileKind::RawAf,
            _ if &head[..2] == b"PF" => reader.kind = FileKind::RawPft,
            _ => return Err(bad("not a DCP recording (no AF, PF, fio_, pcap or pcapng header)")),
        }
        if matches!(reader.kind, FileKind::FileIo | FileKind::RawAf | FileKind::RawPft) {
            reader.head = Some(head);
        }
        Ok(reader)
    }

    pub fn kind(&self) -> FileKind {
        self.kind
    }

    fn skip(&mut self, n: usize) -> io::Result<()> {
        io::copy(&mut (&mut self.r).take(n as u64), &mut io::sink())?;
        self.bytes_read += n as u64;
        Ok(())
    }

    /// Read `n` bytes (`Ok(None)` at the end of the file).
    fn take(&mut self, n: usize) -> io::Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; n];
        if !read_exact_or_eof(&mut self.r, &mut buf)? {
            return Ok(None);
        }
        self.bytes_read += n as u64;
        Ok(Some(buf))
    }

    /// The next packet (`Ok(None)` at the end of the recording).
    pub fn next_packet(&mut self) -> io::Result<Option<DcpPacket>> {
        match self.kind {
            FileKind::FileIo => self.next_file_io(),
            FileKind::RawAf => self.next_raw(true),
            FileKind::RawPft => self.next_raw(false),
            FileKind::Pcap => self.next_pcap(),
            FileKind::PcapNg => self.next_pcapng(),
        }
    }

    /// The first bytes, read by `open` to recognise the format.
    fn start(&mut self, n: usize) -> io::Result<Option<Vec<u8>>> {
        match self.head.take() {
            Some(h) => {
                let mut v = h.to_vec();
                if n > 4 {
                    match self.take(n - 4)? {
                        Some(rest) => v.extend(rest),
                        None => return Err(bad("recording ends inside a packet")),
                    }
                }
                Ok(Some(v))
            }
            None => self.take(n),
        }
    }

    fn next_raw(&mut self, af: bool) -> io::Result<Option<DcpPacket>> {
        // Enough of the header to know the length: AF 6 bytes, PFT 12.
        let Some(mut buf) = self.start(if af { 6 } else { 12 })? else { return Ok(None) };
        let total = if af {
            AfPacket::total_len(&buf).ok_or_else(|| bad("lost AF packet sync"))?
        } else {
            let mut h = buf.clone();
            h.resize(12, 0);
            // The header length depends on the flags in bytes 10–11.
            PftFragment::total_len(&h).ok_or_else(|| bad("lost PFT fragment sync"))?
        };
        let more = total.checked_sub(buf.len()).ok_or_else(|| bad("bad packet length"))?;
        match self.take(more)? {
            Some(rest) => buf.extend(rest),
            None => return Err(bad("recording ends inside a packet")),
        }
        Ok(Some(DcpPacket { data: buf, time_s: None }))
    }

    fn next_file_io(&mut self) -> io::Result<Option<DcpPacket>> {
        loop {
            let Some(head) = self.start(8)? else { return Ok(None) };
            let len = (u32::from_be_bytes([head[4], head[5], head[6], head[7]]) as usize).div_ceil(8);
            let body = self.take(len)?.ok_or_else(|| bad("recording ends inside a fio_ item"))?;
            if &head[..4] != b"fio_" {
                continue;
            }
            let mut time_s = None;
            let mut data = None;
            for item in crate::tag::parse_tag_packet(&body).map_err(|_| bad("bad fio_ item"))? {
                match &item.name {
                    b"time" if item.value.len() >= 8 => {
                        let v = &item.value;
                        let s = u32::from_be_bytes([v[0], v[1], v[2], v[3]]);
                        let ns = u32::from_be_bytes([v[4], v[5], v[6], v[7]]);
                        time_s = Some(f64::from(s) + f64::from(ns) * 1e-9);
                    }
                    b"afpf" => data = Some(item.value),
                    _ => {}
                }
            }
            if let Some(data) = data {
                return Ok(Some(DcpPacket { data, time_s }));
            }
        }
    }

    fn next_pcap(&mut self) -> io::Result<Option<DcpPacket>> {
        loop {
            let Some(h) = self.take(16)? else { return Ok(None) };
            let e = self.endian;
            let (sec, frac, incl) = (e.u32(&h[0..4]), e.u32(&h[4..8]), e.u32(&h[8..12]) as usize);
            if incl > 1 << 20 {
                return Err(bad("implausible pcap record length"));
            }
            let frame = self.take(incl)?.ok_or_else(|| bad("capture ends inside a packet"))?;
            let time = f64::from(sec) + f64::from(frac) * if self.nanos { 1e-9 } else { 1e-6 };
            if let Some(data) = self.udp_payload(self.link, &frame) {
                return Ok(Some(DcpPacket { data, time_s: Some(time) }));
            }
        }
    }

    fn next_pcapng(&mut self) -> io::Result<Option<DcpPacket>> {
        loop {
            let Some(h) = self.take(8)? else { return Ok(None) };
            let mut e = self.endian;
            let raw_type = e.u32(&h[0..4]);
            if raw_type == 0x0A0D_0D0A {
                // A new section, possibly in the other byte order.
                let magic = self.take(4)?.ok_or_else(|| bad("truncated pcapng section"))?;
                self.endian.big = magic == [0x1A, 0x2B, 0x3C, 0x4D];
                e = self.endian;
                let len = e.u32(&h[4..8]) as usize;
                self.interfaces.clear();
                self.skip(len.checked_sub(12).ok_or_else(|| bad("bad pcapng section header"))?)?;
                continue;
            }
            let len = e.u32(&h[4..8]) as usize;
            if !(12..=1 << 24).contains(&len) {
                return Err(bad("bad pcapng block length"));
            }
            let body = self.take(len - 8)?.ok_or_else(|| bad("capture ends inside a block"))?;
            let body = &body[..len - 12];
            match raw_type {
                // Interface description: link type, then options (if_tsresol = 9).
                1 if body.len() >= 8 => {
                    let link = u32::from(e.u16(&body[0..2]));
                    let mut ticks = 1e6;
                    let mut pos = 8;
                    while pos + 4 <= body.len() {
                        let (code, olen) = (e.u16(&body[pos..pos + 2]), usize::from(e.u16(&body[pos + 2..pos + 4])));
                        if code == 0 {
                            break;
                        }
                        if code == 9 && olen >= 1 && pos + 4 < body.len() {
                            let r = body[pos + 4];
                            ticks = if r & 0x80 != 0 { 2f64.powi(i32::from(r & 0x7F)) } else { 10f64.powi(i32::from(r)) };
                        }
                        pos += 4 + olen.div_ceil(4) * 4;
                    }
                    self.interfaces.push(Interface { link, ticks_per_s: ticks });
                }
                // Enhanced packet.
                6 if body.len() >= 20 => {
                    let iface = self.interfaces.get(e.u32(&body[0..4]) as usize).copied().unwrap_or(Interface { link: 1, ticks_per_s: 1e6 });
                    let ts = (u64::from(e.u32(&body[4..8])) << 32) | u64::from(e.u32(&body[8..12]));
                    let caplen = (e.u32(&body[12..16]) as usize).min(body.len() - 20);
                    if let Some(data) = self.udp_payload(iface.link, &body[20..20 + caplen]) {
                        return Ok(Some(DcpPacket { data, time_s: Some(ts as f64 / iface.ticks_per_s) }));
                    }
                }
                // Simple packet (interface 0, no time stamp).
                3 if body.len() >= 4 => {
                    let link = self.interfaces.first().map_or(1, |i| i.link);
                    if let Some(data) = self.udp_payload(link, &body[4..]) {
                        return Ok(Some(DcpPacket { data, time_s: None }));
                    }
                }
                // Obsolete packet block.
                2 if body.len() >= 20 => {
                    let iface = self.interfaces.get(usize::from(e.u16(&body[0..2]))).copied().unwrap_or(Interface { link: 1, ticks_per_s: 1e6 });
                    let ts = (u64::from(e.u32(&body[4..8])) << 32) | u64::from(e.u32(&body[8..12]));
                    let caplen = (e.u32(&body[12..16]) as usize).min(body.len() - 20);
                    if let Some(data) = self.udp_payload(iface.link, &body[20..20 + caplen]) {
                        return Ok(Some(DcpPacket { data, time_s: Some(ts as f64 / iface.ticks_per_s) }));
                    }
                }
                _ => {}
            }
        }
    }

    /// The UDP payload of a captured frame of link type `link`, if it is UDP (to the
    /// wanted port) and complete — IPv4 fragments are collected first.
    fn udp_payload(&mut self, link: u32, frame: &[u8]) -> Option<Vec<u8>> {
        let ip = match link {
            // NULL / LOOP: a 4-byte address family (host order / big-endian).
            0 | 108 => frame.get(4..)?,
            // Ethernet, with optional VLAN tags.
            1 => {
                let mut pos = 12;
                let mut ethertype = u16::from_be_bytes([*frame.get(pos)?, *frame.get(pos + 1)?]);
                while ethertype == 0x8100 || ethertype == 0x88A8 {
                    pos += 4;
                    ethertype = u16::from_be_bytes([*frame.get(pos)?, *frame.get(pos + 1)?]);
                }
                if ethertype != 0x0800 && ethertype != 0x86DD {
                    return None;
                }
                frame.get(pos + 2..)?
            }
            // Raw IP.
            12 | 14 | 101 | 228 | 229 => frame,
            // Linux cooked captures v1 and v2.
            113 => frame.get(16..)?,
            276 => frame.get(20..)?,
            _ => return None,
        };
        let datagram = match ip.first()? >> 4 {
            4 => self.defrag.push(ip)?,
            6 => {
                // IPv6 without extension headers.
                if *ip.get(6)? != 17 {
                    return None;
                }
                let plen = usize::from(u16::from_be_bytes([*ip.get(4)?, *ip.get(5)?]));
                ip.get(40..40 + plen)?.to_vec()
            }
            _ => return None,
        };
        // UDP header: source port, destination port, length, checksum.
        let dest = u16::from_be_bytes([*datagram.get(2)?, *datagram.get(3)?]);
        if self.port.is_some_and(|p| p != dest) {
            return None;
        }
        let len = usize::from(u16::from_be_bytes([*datagram.get(4)?, *datagram.get(5)?]));
        Some(datagram.get(8..len.clamp(8, datagram.len()))?.to_vec())
    }
}

/// An IPv4 datagram being put together: (source, destination, identification).
type DatagramKey = ([u8; 4], [u8; 4], u16);
/// Its fragments by offset, and the total length once the last fragment has been seen.
type Fragments = (Vec<(usize, Vec<u8>)>, Option<usize>);

/// IPv4 datagrams of UDP put back together from their fragments.
#[derive(Default)]
struct Defragmenter {
    pending: HashMap<DatagramKey, Fragments>,
}

impl Defragmenter {
    /// The UDP datagram (header included) of an IPv4 packet, once complete.
    fn push(&mut self, ip: &[u8]) -> Option<Vec<u8>> {
        let ihl = usize::from(ip.first()? & 0x0F) * 4;
        let total = usize::from(u16::from_be_bytes([*ip.get(2)?, *ip.get(3)?]));
        if *ip.get(9)? != 17 || ihl < 20 || total < ihl {
            return None;
        }
        let payload = ip.get(ihl..total.min(ip.len()))?;
        let flags = u16::from_be_bytes([ip[6], ip[7]]);
        let more = flags & 0x2000 != 0;
        let offset = usize::from(flags & 0x1FFF) * 8;
        if !more && offset == 0 {
            return Some(payload.to_vec());
        }
        let key = ([ip[12], ip[13], ip[14], ip[15]], [ip[16], ip[17], ip[18], ip[19]], u16::from_be_bytes([ip[4], ip[5]]));
        if self.pending.len() > 64 {
            self.pending.clear();
        }
        let entry = self.pending.entry(key).or_default();
        if !entry.0.iter().any(|(o, _)| *o == offset) {
            entry.0.push((offset, payload.to_vec()));
        }
        if !more {
            entry.1 = Some(offset + payload.len());
        }
        let len = entry.1?;
        entry.0.sort_by_key(|(o, _)| *o);
        let mut have = 0;
        for (o, p) in &entry.0 {
            if *o > have {
                return None;
            }
            have = have.max(o + p.len());
        }
        if have < len {
            return None;
        }
        let mut out = vec![0u8; len];
        for (o, p) in &entry.0 {
            let end = (o + p.len()).min(len);
            out[*o..end].copy_from_slice(&p[..end - o]);
        }
        self.pending.remove(&key);
        Some(out)
    }
}

/// Writes DCP recordings (for tests, and for keeping what was received).
pub struct DcpFileWriter {
    kind: FileKind,
    w: io::BufWriter<File>,
    /// pcap: UDP destination port written in the headers.
    port: u16,
    ip_id: u16,
}

impl DcpFileWriter {
    /// Create `path` in format `kind` (file framing, raw or pcap; pcapng is read only).
    pub fn create(path: &Path, kind: FileKind, port: u16) -> io::Result<Self> {
        if kind == FileKind::PcapNg {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "pcapng is read only"));
        }
        let mut w = io::BufWriter::new(File::create(path)?);
        if kind == FileKind::Pcap {
            use io::Write;
            // Little-endian, microseconds, version 2.4, Ethernet.
            let mut h = Vec::with_capacity(24);
            h.extend(0xA1B2_C3D4u32.to_le_bytes());
            h.extend(2u16.to_le_bytes());
            h.extend(4u16.to_le_bytes());
            h.extend([0u8; 8]);
            h.extend(65_535u32.to_le_bytes());
            h.extend(1u32.to_le_bytes());
            w.write_all(&h)?;
        }
        Ok(Self { kind, w, port, ip_id: 0 })
    }

    /// Append one packet (an AF packet or PFT fragment) received at `time_s`.
    pub fn write_packet(&mut self, data: &[u8], time_s: Option<f64>) -> io::Result<()> {
        use io::Write;
        match self.kind {
            FileKind::RawAf | FileKind::RawPft => self.w.write_all(data),
            FileKind::FileIo => {
                let mut items = Vec::new();
                if let Some(t) = time_s {
                    let s = t.floor();
                    let mut v = (s as u32).to_be_bytes().to_vec();
                    v.extend((((t - s) * 1e9).round() as u32).min(999_999_999).to_be_bytes());
                    items.push(crate::tag::TagItem::new(b"time", v));
                }
                items.push(crate::tag::TagItem::new(b"afpf", data.to_vec()));
                let body = crate::tag::build_tag_packet(&items);
                self.w.write_all(b"fio_")?;
                self.w.write_all(&(body.len() as u32 * 8).to_be_bytes())?;
                self.w.write_all(&body)
            }
            FileKind::Pcap => {
                let frames = ethernet_udp(data, self.port, self.ip_id, 1500);
                self.ip_id = self.ip_id.wrapping_add(1);
                let t = time_s.unwrap_or(0.0);
                for f in frames {
                    let mut h = Vec::with_capacity(16);
                    h.extend((t.floor() as u32).to_le_bytes());
                    h.extend((((t - t.floor()) * 1e6).round() as u32).min(999_999).to_le_bytes());
                    h.extend((f.len() as u32).to_le_bytes());
                    h.extend((f.len() as u32).to_le_bytes());
                    self.w.write_all(&h)?;
                    self.w.write_all(&f)?;
                }
                Ok(())
            }
            FileKind::PcapNg => unreachable!("checked in create"),
        }
    }

    /// Flush and close.
    pub fn finish(mut self) -> io::Result<()> {
        use io::Write;
        self.w.flush()
    }
}

/// Ethernet frames carrying `payload` in a UDP datagram to `port` (127.0.0.1 to
/// 127.0.0.1), IPv4-fragmented to `mtu`.
fn ethernet_udp(payload: &[u8], port: u16, id: u16, mtu: usize) -> Vec<Vec<u8>> {
    let mut udp = Vec::with_capacity(8 + payload.len());
    udp.extend(50_000u16.to_be_bytes());
    udp.extend(port.to_be_bytes());
    udp.extend(((8 + payload.len()) as u16).to_be_bytes());
    udp.extend([0, 0]);
    udp.extend_from_slice(payload);
    let per = (mtu - 20) / 8 * 8;
    udp.chunks(per)
        .enumerate()
        .map(|(i, chunk)| {
            let more = (i + 1) * per < udp.len();
            let mut f = vec![0u8; 12];
            f.extend(0x0800u16.to_be_bytes());
            let total = 20 + chunk.len();
            f.extend([0x45, 0]);
            f.extend((total as u16).to_be_bytes());
            f.extend(id.to_be_bytes());
            f.extend(((u16::from(more) << 13) | ((i * per / 8) as u16)).to_be_bytes());
            f.extend([64, 17, 0, 0, 127, 0, 0, 1, 127, 0, 0, 1]);
            f.extend_from_slice(chunk);
            f
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pft::{PftConfig, fragment};

    fn packets() -> Vec<Vec<u8>> {
        (0..5u16).map(|i| AfPacket::new(i, vec![i as u8; 100 + 900 * usize::from(i)]).to_bytes()).collect()
    }

    fn round_trip(kind: FileKind, data: &[Vec<u8>]) -> Vec<DcpPacket> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rec.bin");
        let mut w = DcpFileWriter::create(&path, kind, 60_000).unwrap();
        for (i, d) in data.iter().enumerate() {
            w.write_packet(d, Some(10.0 + 0.4 * i as f64)).unwrap();
        }
        w.finish().unwrap();
        let mut r = DcpFileReader::open(&path, None).unwrap();
        assert_eq!(r.kind(), kind);
        let mut out = Vec::new();
        while let Some(p) = r.next_packet().unwrap() {
            out.push(p);
        }
        out
    }

    #[test]
    fn raw_and_framed_recordings() {
        let af = packets();
        let raw: Vec<Vec<u8>> = round_trip(FileKind::RawAf, &af).into_iter().map(|p| p.data).collect();
        assert_eq!(raw, af);
        let framed = round_trip(FileKind::FileIo, &af);
        assert_eq!(framed.iter().map(|p| p.data.clone()).collect::<Vec<_>>(), af);
        assert!((framed[2].time_s.unwrap() - 10.8).abs() < 1e-6);
        let cfg = PftConfig { max_payload: 700, fec: Some(1), ..PftConfig::default() };
        let pft: Vec<Vec<u8>> = af.iter().flat_map(|a| fragment(a, 0, &cfg)).map(|f| f.to_bytes()).collect();
        let raw_pft: Vec<Vec<u8>> = round_trip(FileKind::RawPft, &pft).into_iter().map(|p| p.data).collect();
        assert_eq!(raw_pft, pft);
    }

    /// pcap: UDP payloads come back whole although the larger ones were IPv4-fragmented;
    /// the port filter drops other datagrams.
    #[test]
    fn pcap_capture_with_ip_fragments() {
        let af = packets();
        let got = round_trip(FileKind::Pcap, &af);
        assert_eq!(got.iter().map(|p| p.data.clone()).collect::<Vec<_>>(), af);
        assert!((got[1].time_s.unwrap() - 10.4).abs() < 1e-5);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rec.pcap");
        let mut w = DcpFileWriter::create(&path, FileKind::Pcap, 60_000).unwrap();
        w.write_packet(&af[0], None).unwrap();
        w.finish().unwrap();
        assert!(DcpFileReader::open(&path, Some(60_001)).unwrap().next_packet().unwrap().is_none());
        assert!(DcpFileReader::open(&path, Some(60_000)).unwrap().next_packet().unwrap().is_some());
    }

    /// pcapng: a section header, an interface with nanosecond time stamps, an enhanced
    /// packet block.
    #[test]
    fn pcapng_capture() {
        let af = &packets()[0];
        let frame = ethernet_udp(af, 9998, 1, 1500).remove(0);
        let block = |ty: u32, body: &[u8]| {
            let len = 12 + body.len().div_ceil(4) * 4;
            let mut b = ty.to_le_bytes().to_vec();
            b.extend((len as u32).to_le_bytes());
            b.extend_from_slice(body);
            b.resize(len - 4, 0);
            b.extend((len as u32).to_le_bytes());
            b
        };
        let shb = [0x4D, 0x3C, 0x2B, 0x1A, 1, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        let mut file = block(0x0A0D_0D0A, &shb);
        let mut idb = vec![1, 0, 0, 0, 0, 0, 1, 0];
        idb.extend([9, 0, 1, 0, 9, 0, 0, 0, 0, 0, 0, 0]);
        file.extend(block(1, &idb));
        let ts: u64 = 2_500_000_000;
        let mut epb = 0u32.to_le_bytes().to_vec();
        epb.extend(((ts >> 32) as u32).to_le_bytes());
        epb.extend((ts as u32).to_le_bytes());
        epb.extend((frame.len() as u32).to_le_bytes());
        epb.extend((frame.len() as u32).to_le_bytes());
        epb.extend(&frame);
        file.extend(block(6, &epb));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rec.pcapng");
        std::fs::write(&path, file).unwrap();
        let mut r = DcpFileReader::open(&path, None).unwrap();
        assert_eq!(r.kind(), FileKind::PcapNg);
        let p = r.next_packet().unwrap().unwrap();
        assert_eq!(&p.data, af);
        assert!((p.time_s.unwrap() - 2.5).abs() < 1e-9);
        assert!(r.next_packet().unwrap().is_none());
    }

    #[test]
    fn recognises_extensions_and_rejects_others() {
        for name in ["a.rsA", "b.rsd", "c.pcap", "d.pcapng", "e.ff", "f.mdi"] {
            assert!(has_recording_extension(Path::new(name)), "{name}");
        }
        for name in ["a.wav", "b.flac", "c.rs", "d.rs1"] {
            assert!(!has_recording_extension(Path::new(name)), "{name}");
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.wav");
        std::fs::write(&path, b"RIFF....WAVE").unwrap();
        assert!(DcpFileReader::open(&path, None).is_err());
    }
}
