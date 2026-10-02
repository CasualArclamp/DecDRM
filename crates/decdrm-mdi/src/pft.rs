//! PFT — Protection, Fragmentation and Transport (TS 102 821 §7): AF packets split into
//! fragments that fit a network packet, optionally protected by Reed–Solomon so that
//! lost fragments can be rebuilt.
//!
//! ```text
//! "PF" (2) │ Pseq (2) │ Findex (3) │ Fcount (3) │ FEC (1 bit) Addr (1 bit) Plen (14 bits)
//!   [RSk (1) │ RSz (1)]        if FEC
//!   [Source (2) │ Dest (2)]    if Addr
//! │ HCRC (2, over the header) │ payload (Plen bytes)
//! ```
//!
//! With FEC the AF packet (l bytes) is cut into c = ⌈l / k⌉ chunks of k = RSk bytes,
//! the last one padded with z = RSz zero bytes; each chunk gets 48 parity bytes
//! (RS(255, 207) shortened to k + 48, see [`crate::rs`]). The chunks one after another
//! form the RS block, which is spread over the Fcount fragments byte by byte: byte j
//! of fragment i is byte j·Fcount + i of the block (zeros past its end). A lost fragment
//! so costs each chunk only a few bytes, which the parity rebuilds as erasures.

use crate::DcpError;
use crate::rs::{PFT_MAX_K, PFT_PARITY, ReedSolomon};
use decdrm_core::fec::crc::crc16;
use std::collections::VecDeque;

/// Header bytes without FEC and addressing.
const BASE_HEADER: usize = 14;
/// Partly received packets kept at once.
const MAX_PENDING: usize = 16;
/// Sequence numbers of completed packets remembered (late fragments are dropped).
const DONE_KEPT: usize = 32;

/// One PFT fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PftFragment {
    /// Sequence number of the AF packet the fragment belongs to.
    pub pseq: u16,
    /// Index of this fragment, 0 … fcount − 1.
    pub findex: u32,
    /// Fragments of the packet.
    pub fcount: u32,
    /// Reed–Solomon protection: (RSk, RSz) — data bytes per chunk, padding bytes.
    pub fec: Option<(u8, u8)>,
    /// Addressing: (source, destination).
    pub addr: Option<(u16, u16)>,
    pub payload: Vec<u8>,
}

impl PftFragment {
    /// Decode one fragment (`data` must hold it exactly or be longer).
    pub fn parse(data: &[u8]) -> Result<Self, DcpError> {
        if data.len() < BASE_HEADER {
            return Err(if data.starts_with(b"PF") || data.len() < 2 { DcpError::Truncated } else { DcpError::Sync("PF") });
        }
        if &data[..2] != b"PF" {
            return Err(DcpError::Sync("PF"));
        }
        let header = Self::header_len(data).ok_or(DcpError::Truncated)?;
        if data.len() < header {
            return Err(DcpError::Truncated);
        }
        if crc16(&data[..header - 2]) != u16::from_be_bytes([data[header - 2], data[header - 1]]) {
            return Err(DcpError::Crc("PFT header"));
        }
        let be24 = |b: &[u8]| (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let flags = u16::from_be_bytes([data[10], data[11]]);
        let plen = usize::from(flags & 0x3FFF);
        let fec = (flags & 0x8000 != 0).then(|| (data[12], data[13]));
        let addr = (flags & 0x4000 != 0).then(|| {
            let at = BASE_HEADER - 2 + if fec.is_some() { 2 } else { 0 };
            (u16::from_be_bytes([data[at], data[at + 1]]), u16::from_be_bytes([data[at + 2], data[at + 3]]))
        });
        let payload = data.get(header..header + plen).ok_or(DcpError::Truncated)?.to_vec();
        let fragment =
            Self { pseq: u16::from_be_bytes([data[2], data[3]]), findex: be24(&data[4..7]), fcount: be24(&data[7..10]), fec, addr, payload };
        if fragment.fcount == 0 || fragment.findex >= fragment.fcount {
            return Err(DcpError::Invalid("PFT fragment index"));
        }
        if fragment.fec.is_some_and(|(k, _)| k == 0 || usize::from(k) > PFT_MAX_K) {
            return Err(DcpError::Invalid("PFT RSk"));
        }
        Ok(fragment)
    }

    /// Header length of the fragment starting with `data` (at least 12 bytes).
    fn header_len(data: &[u8]) -> Option<usize> {
        let flags = u16::from_be_bytes([*data.get(10)?, *data.get(11)?]);
        Some(BASE_HEADER + if flags & 0x8000 != 0 { 2 } else { 0 } + if flags & 0x4000 != 0 { 4 } else { 0 })
    }

    /// Total length of the fragment that starts with `data` (for reading PFT
    /// recordings), or `None` without the "PF" sync or with too few bytes.
    pub fn total_len(data: &[u8]) -> Option<usize> {
        if !data.starts_with(b"PF") {
            return None;
        }
        let flags = u16::from_be_bytes([*data.get(10)?, *data.get(11)?]);
        Some(Self::header_len(data)? + usize::from(flags & 0x3FFF))
    }

    /// The fragment as sent.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(BASE_HEADER + 6 + self.payload.len());
        out.extend_from_slice(b"PF");
        out.extend_from_slice(&self.pseq.to_be_bytes());
        out.extend_from_slice(&self.findex.to_be_bytes()[1..]);
        out.extend_from_slice(&self.fcount.to_be_bytes()[1..]);
        let flags = (u16::from(self.fec.is_some()) << 15)
            | (u16::from(self.addr.is_some()) << 14)
            | (self.payload.len() as u16 & 0x3FFF);
        out.extend_from_slice(&flags.to_be_bytes());
        if let Some((k, z)) = self.fec {
            out.extend_from_slice(&[k, z]);
        }
        if let Some((s, d)) = self.addr {
            out.extend_from_slice(&s.to_be_bytes());
            out.extend_from_slice(&d.to_be_bytes());
        }
        let crc = crc16(&out);
        out.extend_from_slice(&crc.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }
}

/// How [`fragment`] splits an AF packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PftConfig {
    /// Largest fragment payload, bytes (e.g. 1400 for Ethernet minus headers).
    pub max_payload: usize,
    /// Reed–Solomon protection: how many fragments of a packet may be lost (TS 102 821
    /// "m"); `None`: none.
    pub fec: Option<usize>,
    /// Data bytes per Reed–Solomon chunk (TS 102 821 "k", at most 207).
    pub chunk: usize,
    /// Addressing: (source, destination).
    pub addr: Option<(u16, u16)>,
}

impl Default for PftConfig {
    fn default() -> Self {
        Self { max_payload: 1400, fec: None, chunk: PFT_MAX_K, addr: None }
    }
}

/// Split the AF packet `af` into PFT fragments (TS 102 821 §7.2.2).
pub fn fragment(af: &[u8], pseq: u16, cfg: &PftConfig) -> Vec<PftFragment> {
    let max_payload = cfg.max_payload.clamp(1, 0x3FFF);
    let (block, fec, fcount, plen) = match cfg.fec {
        Some(m) => {
            let k = cfg.chunk.clamp(1, PFT_MAX_K);
            let c = af.len().div_ceil(k).max(1);
            let z = c * k - af.len();
            let rs = ReedSolomon::pft();
            let mut block = Vec::with_capacity(c * (k + PFT_PARITY));
            for chunk in 0..c {
                let mut data: Vec<u8> = af.iter().skip(chunk * k).take(k).copied().collect();
                data.resize(k, 0);
                let parity = rs.encode(&data);
                block.extend(data);
                block.extend(parity);
            }
            // s_max = min(⌊c·p / (m + 1)⌋, MTU − h); f = ⌈|block| / s_max⌉; s = ⌈|block| / f⌉.
            let s_max = ((c * PFT_PARITY) / (m + 1)).min(max_payload).max(1);
            let f = block.len().div_ceil(s_max);
            let s = block.len().div_ceil(f);
            (block, Some((k as u8, z as u8)), f, s)
        }
        None => {
            let f = af.len().div_ceil(max_payload).max(1);
            (af.to_vec(), None, f, af.len().div_ceil(f).max(1))
        }
    };
    (0..fcount)
        .map(|i| {
            let payload: Vec<u8> = if fec.is_some() {
                (0..plen).map(|j| block.get(j * fcount + i).copied().unwrap_or(0)).collect()
            } else {
                block.iter().skip(i * plen).take(plen).copied().collect()
            };
            PftFragment { pseq, findex: i as u32, fcount: fcount as u32, fec, addr: cfg.addr, payload }
        })
        .collect()
}

/// A packet being collected.
struct Partial {
    pseq: u16,
    fcount: u32,
    fec: Option<(u8, u8)>,
    plen: usize,
    fragments: Vec<Option<Vec<u8>>>,
    received: usize,
}

/// Counters of a [`PftReassembler`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PftStats {
    pub fragments: u64,
    /// AF packets put together.
    pub packets: u64,
    /// Packets rebuilt by Reed–Solomon although fragments were missing.
    pub recovered: u64,
    /// Bytes Reed–Solomon corrected (lost fragments and errors).
    pub corrected_bytes: u64,
    /// Packets given up (too many fragments lost).
    pub lost: u64,
    /// Fragments ignored: bad header, other destination, inconsistent, late.
    pub ignored: u64,
}

/// Collects PFT fragments and returns the AF packets they carry.
pub struct PftReassembler {
    /// Accept only fragments for this destination address (`None`: any).
    dest: Option<u16>,
    pending: VecDeque<Partial>,
    done: VecDeque<u16>,
    rs: ReedSolomon,
    pub stats: PftStats,
}

impl Default for PftReassembler {
    fn default() -> Self {
        Self::new(None)
    }
}

impl PftReassembler {
    /// `dest`: accept only fragments addressed to it (fragments without addressing
    /// are always accepted).
    pub fn new(dest: Option<u16>) -> Self {
        Self { dest, pending: VecDeque::new(), done: VecDeque::new(), rs: ReedSolomon::pft(), stats: PftStats::default() }
    }

    /// Take one received fragment (raw bytes); returns the AF packet once it is
    /// complete — or rebuilt, if enough fragments arrived for the Reed–Solomon code.
    pub fn push_bytes(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        match PftFragment::parse(data) {
            Ok(f) => self.push(f),
            Err(_) => {
                self.stats.ignored += 1;
                None
            }
        }
    }

    /// Take one decoded fragment (see [`Self::push_bytes`]).
    pub fn push(&mut self, f: PftFragment) -> Option<Vec<u8>> {
        self.stats.fragments += 1;
        if let (Some(want), Some((_, dest))) = (self.dest, f.addr)
            && want != dest
        {
            self.stats.ignored += 1;
            return None;
        }
        if self.done.contains(&f.pseq) {
            // A late copy of a packet already put together.
            return None;
        }
        if f.fcount == 1 && f.fec.is_none() {
            self.finish(f.pseq);
            self.stats.packets += 1;
            return Some(f.payload);
        }
        let pos = match self.pending.iter().position(|p| p.pseq == f.pseq) {
            Some(pos) => pos,
            None => {
                if self.pending.len() == MAX_PENDING {
                    self.pending.pop_front();
                    self.stats.lost += 1;
                }
                self.pending.push_back(Partial {
                    pseq: f.pseq,
                    fcount: f.fcount,
                    fec: f.fec,
                    plen: f.payload.len(),
                    fragments: vec![None; f.fcount as usize],
                    received: 0,
                });
                self.pending.len() - 1
            }
        };
        let p = &mut self.pending[pos];
        let consistent = p.fcount == f.fcount && p.fec == f.fec && (p.fec.is_none() || p.plen == f.payload.len());
        if !consistent {
            self.stats.ignored += 1;
            return None;
        }
        let slot = &mut p.fragments[f.findex as usize];
        if slot.is_some() {
            return None;
        }
        *slot = Some(f.payload);
        p.received += 1;
        let result = match p.fec {
            None if p.received == p.fcount as usize => {
                Some(p.fragments.iter().flat_map(|s| s.as_deref().unwrap_or_default().iter().copied()).collect())
            }
            None => None,
            Some(_) => Self::try_fec(p, &self.rs, &mut self.stats),
        };
        if result.is_some() {
            self.pending.remove(pos);
            self.finish(f.pseq);
            self.stats.packets += 1;
        }
        result
    }

    fn finish(&mut self, pseq: u16) {
        self.done.push_back(pseq);
        if self.done.len() > DONE_KEPT {
            self.done.pop_front();
        }
    }

    /// Rebuild the AF packet of `p` if every RS chunk lost at most 48 bytes.
    fn try_fec(p: &Partial, rs: &ReedSolomon, stats: &mut PftStats) -> Option<Vec<u8>> {
        let (k, z) = p.fec?;
        let (k, z) = (usize::from(k), usize::from(z));
        let n = k + PFT_PARITY;
        let f = p.fcount as usize;
        // c = ⌊Fcount·Plen / (k + 48)⌋ chunks in the RS block.
        let c = f * p.plen / n;
        if c == 0 || c * k < z {
            return None;
        }
        let missing = p.fcount as usize - p.received;
        // Erasures per chunk: the block bytes that lie in missing fragments.
        let mut erasures: Vec<Vec<usize>> = vec![Vec::new(); c];
        if missing > 0 {
            for (i, frag) in p.fragments.iter().enumerate() {
                if frag.is_some() {
                    continue;
                }
                for j in 0..p.plen {
                    let ix = j * f + i;
                    if ix < c * n {
                        erasures[ix / n].push(ix % n);
                    }
                }
            }
            if erasures.iter().any(|e| e.len() > PFT_PARITY) {
                return None;
            }
        }
        let mut af = Vec::with_capacity(c * k);
        let mut corrected = 0u64;
        for (chunk, erased) in erasures.iter().enumerate() {
            let mut word: Vec<u8> = (0..n)
                .map(|b| {
                    let ix = chunk * n + b;
                    p.fragments[ix % f].as_ref().map_or(0, |frag| frag[ix / f])
                })
                .collect();
            match rs.decode(&mut word, erased) {
                Ok(fixed) => corrected += fixed as u64,
                // All fragments here, yet uncorrectable: deliver as received and let
                // the AF CRC judge; with fragments missing there is nothing to deliver.
                Err(_) if missing == 0 => {}
                Err(_) => {
                    stats.lost += 1;
                    return None;
                }
            }
            af.extend_from_slice(&word[..k]);
        }
        af.truncate(c * k - z);
        stats.corrected_bytes += corrected;
        if missing > 0 {
            stats.recovered += 1;
        }
        Some(af)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn af_packet(len: usize) -> Vec<u8> {
        let payload: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
        crate::af::AfPacket::new(0x0102, payload).to_bytes()
    }

    #[test]
    fn header_round_trip() {
        let f = PftFragment { pseq: 7, findex: 2, fcount: 5, fec: Some((180, 12)), addr: Some((0x1234, 0xABCD)), payload: vec![1, 2, 3] };
        let bytes = f.to_bytes();
        assert_eq!(bytes.len(), 14 + 2 + 4 + 3);
        assert_eq!(PftFragment::total_len(&bytes), Some(bytes.len()));
        assert_eq!(PftFragment::parse(&bytes).unwrap(), f);
        let plain = PftFragment { fec: None, addr: None, ..f.clone() };
        let bytes = plain.to_bytes();
        assert_eq!(bytes.len(), 14 + 3);
        assert_eq!(PftFragment::parse(&bytes).unwrap(), plain);
        let mut bad = bytes.clone();
        bad[5] ^= 1;
        assert_eq!(PftFragment::parse(&bad), Err(DcpError::Crc("PFT header")));
    }

    /// Without FEC every fragment is needed, in any order; duplicates do no harm.
    #[test]
    fn plain_fragments_reassemble() {
        let af = af_packet(3000);
        let frags = fragment(&af, 1, &PftConfig { max_payload: 1000, ..PftConfig::default() });
        assert_eq!(frags.len(), 4);
        let mut r = PftReassembler::default();
        assert_eq!(r.push(frags[3].clone()), None);
        assert_eq!(r.push(frags[0].clone()), None);
        assert_eq!(r.push(frags[0].clone()), None);
        assert_eq!(r.push(frags[2].clone()), None);
        assert_eq!(r.push(frags[1].clone()).as_deref(), Some(&af[..]));
        assert_eq!(r.push(frags[1].clone()), None, "a late copy is dropped");
        // A packet that fits one fragment.
        let small = af_packet(50);
        let one = fragment(&small, 2, &PftConfig::default());
        assert_eq!(one.len(), 1);
        assert_eq!(r.push_bytes(&one[0].to_bytes()).as_deref(), Some(&small[..]));
        assert_eq!(r.stats.packets, 2);
    }

    /// With FEC the packet is rebuilt from fewer fragments: up to m may be lost.
    #[test]
    fn fec_rebuilds_lost_fragments() {
        let af = af_packet(4000);
        for m in [1usize, 2, 4] {
            let cfg = PftConfig { max_payload: 600, fec: Some(m), ..PftConfig::default() };
            let frags = fragment(&af, 9, &cfg);
            let (k, z) = frags[0].fec.unwrap();
            assert_eq!(usize::from(k), 207);
            let c = af.len().div_ceil(207);
            assert_eq!(usize::from(z), c * 207 - af.len());
            assert!(frags.iter().all(|f| f.payload.len() == frags[0].payload.len()));
            // Lose m fragments spread over the packet: still rebuilt.
            let step = frags.len() / m;
            let lost: Vec<usize> = (0..m).map(|i| i * step).collect();
            let mut r = PftReassembler::default();
            let mut out = None;
            for (i, f) in frags.iter().enumerate() {
                if !lost.contains(&i)
                    && let Some(p) = r.push(f.clone())
                {
                    out = Some(p);
                }
            }
            assert_eq!(out.as_deref(), Some(&af[..]), "m={m}, {} fragments", frags.len());
            assert_eq!(r.stats.recovered, 1);
            assert!(r.stats.corrected_bytes > 0);
        }
    }

    /// Losing more fragments than the code can rebuild: nothing comes out.
    #[test]
    fn fec_gives_up_beyond_its_strength() {
        let af = af_packet(2000);
        let frags = fragment(&af, 3, &PftConfig { max_payload: 400, fec: Some(1), ..PftConfig::default() });
        let mut r = PftReassembler::default();
        let out: Vec<Vec<u8>> = frags.iter().skip(3).filter_map(|f| r.push(f.clone())).collect();
        assert!(out.is_empty());
    }

    /// Fragments for another destination are ignored.
    #[test]
    fn destination_filter() {
        let af = af_packet(100);
        let frags = fragment(&af, 4, &PftConfig { addr: Some((1, 2)), ..PftConfig::default() });
        let mut other = PftReassembler::new(Some(3));
        assert_eq!(other.push(frags[0].clone()), None);
        assert_eq!(other.stats.ignored, 1);
        let mut mine = PftReassembler::new(Some(2));
        assert_eq!(mine.push(frags[0].clone()).as_deref(), Some(&af[..]));
    }
}
