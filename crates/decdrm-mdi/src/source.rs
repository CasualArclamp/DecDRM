//! Packets in, MDI frames out: [`DcpReceiver`] takes AF packets or PFT fragments (UDP
//! payloads, recording entries), [`MdiInput`] reads them from UDP or a recording.

use crate::af::AfPacket;
use crate::file::{DcpFileReader, has_recording_extension};
use crate::mdi::MdiFrame;
use crate::net::{UdpOrigin, UdpReceiver};
use crate::pft::{PftReassembler, PftStats};
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A logical frame lasts 400 ms.
pub const FRAME: Duration = Duration::from_millis(400);

/// Counters of a [`DcpReceiver`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DcpStats {
    /// Packets received (datagrams, recording entries).
    pub packets: u64,
    /// AF packets decoded.
    pub af_packets: u64,
    /// AF packets with a bad CRC or header.
    pub af_errors: u64,
    /// Packets that are neither AF nor PFT.
    pub foreign: u64,
    /// MDI frames delivered.
    pub frames: u64,
    /// Frames dropped as repeats (a frame count seen just before).
    pub repeats: u64,
    /// Frames missing, judged by the frame count.
    pub lost: u64,
    pub pft: PftStats,
}

/// Turns DCP packets into MDI frames, dropping repeated frames and counting lost ones
/// by the logical frame count (`dlfc`), as Dream does.
#[derive(Default)]
pub struct DcpReceiver {
    pft: PftReassembler,
    last_dlfc: Option<u32>,
    pub stats: DcpStats,
}

impl DcpReceiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// One packet: an AF packet, or a PFT fragment (then the AF packet once complete).
    pub fn push_af(&mut self, data: &[u8]) -> Option<AfPacket> {
        self.stats.packets += 1;
        let af = if data.starts_with(b"PF") {
            let out = self.pft.push_bytes(data);
            self.stats.pft = self.pft.stats;
            out?
        } else if data.starts_with(b"AF") {
            data.to_vec()
        } else {
            self.stats.foreign += 1;
            return None;
        };
        match AfPacket::parse(&af) {
            Ok(p) => {
                self.stats.af_packets += 1;
                Some(p)
            }
            Err(_) => {
                self.stats.af_errors += 1;
                None
            }
        }
    }

    /// One packet; returns the MDI frame it completes, unless it repeats one just
    /// delivered.
    pub fn push(&mut self, data: &[u8]) -> Option<MdiFrame> {
        let af = self.push_af(data)?;
        let frame = MdiFrame::from_af(&af).ok()?;
        if let Some(n) = frame.dlfc
            && !self.accept(n)
        {
            self.stats.repeats += 1;
            return None;
        }
        self.stats.frames += 1;
        Some(frame)
    }

    /// Frame count logic: a repeat or a slightly older count is dropped; a much older
    /// or much newer one is a restart. Count 0 is always taken (some receivers, e.g.
    /// the Newstar DR111, send it in every frame).
    fn accept(&mut self, n: u32) -> bool {
        if n == 0 {
            self.last_dlfc = Some(0);
            return true;
        }
        let Some(last) = self.last_dlfc else {
            self.last_dlfc = Some(n);
            return true;
        };
        let ahead = n.wrapping_sub(last);
        if ahead == 0 || last.wrapping_sub(n) < 10 {
            return false;
        }
        if ahead < 1000 {
            self.stats.lost += u64::from(ahead - 1);
        }
        self.last_dlfc = Some(n);
        true
    }
}

/// Where MDI comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MdiOrigin {
    Udp(UdpOrigin),
    /// A recording; `port`: for captures only UDP datagrams to this port.
    File { path: PathBuf, port: Option<u16> },
}

impl MdiOrigin {
    /// Interpret what a user typed: a recording (an existing file, or a name with a
    /// recording extension; Dream's `file.pcap#port` picks a port) or a UDP origin in
    /// Dream's syntax ([`crate::net`]).
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("give a UDP port (or group:port) or a recording".into());
        }
        let (name, port) = match s.rsplit_once('#') {
            Some((n, p)) if p.parse::<u16>().is_ok() => (n, p.parse().ok()),
            _ => (s, None),
        };
        let path = Path::new(name);
        if path.is_file() || has_recording_extension(path) {
            return Ok(MdiOrigin::File { path: path.to_path_buf(), port });
        }
        s.parse::<UdpOrigin>().map(MdiOrigin::Udp)
    }

    /// For logs and status lines.
    pub fn describe(&self) -> String {
        match self {
            MdiOrigin::Udp(o) => match o.group {
                Some(g) if g.is_multicast() => format!("UDP multicast {g}:{}", o.port),
                Some(a) => format!("UDP {a}:{}", o.port),
                None => format!("UDP port {}", o.port),
            },
            MdiOrigin::File { path, port } => {
                let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
                match port {
                    Some(p) => format!("{name} (port {p})"),
                    None => name,
                }
            }
        }
    }
}

enum Transport {
    Udp(UdpReceiver),
    File(DcpFileReader),
}

/// What [`MdiInput::read`] got.
#[derive(Debug, Clone, PartialEq)]
pub enum MdiRead {
    Frame(Box<MdiFrame>),
    /// Nothing within the wait (UDP), or the next frame is not due yet (paced file).
    Idle,
    /// The recording has ended.
    End,
}

/// MDI frames from UDP or a recording. A recording is read at 400 ms per frame
/// (`realtime`), as a live source sends, or as fast as it is asked for.
pub struct MdiInput {
    origin: MdiOrigin,
    transport: Transport,
    pub receiver: DcpReceiver,
    realtime: bool,
    /// Paced recordings: when frame 0 was due, and frames handed out since.
    clock: Option<Instant>,
    paced: u64,
    pending: Option<MdiFrame>,
    /// UDP: the sender of the last packet.
    pub last_sender: Option<SocketAddr>,
}

impl MdiInput {
    pub fn open(origin: MdiOrigin, realtime: bool) -> io::Result<Self> {
        let transport = match &origin {
            MdiOrigin::Udp(o) => Transport::Udp(UdpReceiver::bind(o)?),
            MdiOrigin::File { path, port } => Transport::File(DcpFileReader::open(path, *port)?),
        };
        Ok(Self { origin, transport, receiver: DcpReceiver::new(), realtime, clock: None, paced: 0, pending: None, last_sender: None })
    }

    pub fn origin(&self) -> &MdiOrigin {
        &self.origin
    }

    pub fn is_file(&self) -> bool {
        matches!(self.transport, Transport::File(_))
    }

    /// The local UDP address (for a port chosen by the system).
    pub fn local_addr(&self) -> Option<SocketAddr> {
        match &self.transport {
            Transport::Udp(u) => u.local_addr().ok(),
            Transport::File(_) => None,
        }
    }

    /// Recordings: (bytes read, file size).
    pub fn progress(&self) -> Option<(u64, u64)> {
        match &self.transport {
            Transport::File(f) => Some((f.bytes_read, f.file_size)),
            Transport::Udp(_) => None,
        }
    }

    /// The next MDI frame, waiting up to `wait` for UDP packets or for a paced
    /// recording's next frame to be due.
    pub fn read(&mut self, wait: Duration) -> io::Result<MdiRead> {
        let deadline = Instant::now() + wait;
        let frame = match self.pending.take() {
            Some(f) => f,
            None => match self.next_frame(deadline)? {
                Some(f) => f,
                None if self.is_file() => return Ok(MdiRead::End),
                None => return Ok(MdiRead::Idle),
            },
        };
        if self.realtime && self.is_file() {
            let start = *self.clock.get_or_insert_with(Instant::now);
            let due = start + FRAME * u32::try_from(self.paced).unwrap_or(u32::MAX);
            let now = Instant::now();
            if due > now {
                if due > deadline {
                    std::thread::sleep(deadline.saturating_duration_since(now));
                    self.pending = Some(frame);
                    return Ok(MdiRead::Idle);
                }
                std::thread::sleep(due - now);
            }
            self.paced += 1;
        }
        Ok(MdiRead::Frame(Box::new(frame)))
    }

    /// The next complete frame: UDP until `deadline` (`None` then), a recording until
    /// it ends (`None` then).
    fn next_frame(&mut self, deadline: Instant) -> io::Result<Option<MdiFrame>> {
        loop {
            let packet = match &mut self.transport {
                Transport::Udp(u) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    match u.recv(left)? {
                        Some((data, from)) => {
                            self.last_sender = Some(from);
                            data
                        }
                        None => return Ok(None),
                    }
                }
                Transport::File(f) => match f.next_packet()? {
                    Some(p) => p.data,
                    None => return Ok(None),
                },
            };
            if let Some(frame) = self.receiver.push(&packet) {
                return Ok(Some(frame));
            }
            if matches!(self.transport, Transport::Udp(_)) && Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::{DcpFileWriter, FileKind};
    use crate::mdi::Protocol;
    use crate::net::UdpSender;
    use crate::pft::{PftConfig, fragment};

    fn frame(n: u32) -> MdiFrame {
        MdiFrame { protocol: Some(Protocol::MDI), dlfc: Some(n), robustness: Some(1), streams: [Some(vec![n as u8; 3000]), None, None, None], ..MdiFrame::default() }
    }

    #[test]
    fn frame_counts() {
        let mut r = DcpReceiver::new();
        let bytes = |n: u32| frame(n).to_af(n as u16).to_bytes();
        assert!(r.push(&bytes(5)).is_some());
        assert!(r.push(&bytes(5)).is_none(), "a repeat");
        assert!(r.push(&bytes(6)).is_some());
        assert!(r.push(&bytes(4)).is_none(), "slightly older");
        assert!(r.push(&bytes(9)).is_some());
        assert_eq!((r.stats.lost, r.stats.repeats, r.stats.frames), (2, 2, 3));
        assert!(r.push(&bytes(100_000)).is_some(), "a restart");
        assert_eq!(r.stats.lost, 2);
        assert!(r.push(b"hello").is_none());
        assert_eq!(r.stats.foreign, 1);
        let mut bad = bytes(100_001);
        bad[20] ^= 1;
        assert!(r.push(&bad).is_none());
        assert_eq!(r.stats.af_errors, 1);
    }

    /// PFT fragments with FEC, one lost per packet: every frame still arrives.
    #[test]
    fn pft_with_losses() {
        let mut r = DcpReceiver::new();
        let cfg = PftConfig { max_payload: 800, fec: Some(1), ..PftConfig::default() };
        let mut got = 0;
        for n in 1..=5u32 {
            let frags = fragment(&frame(n).to_af(n as u16).to_bytes(), n as u16, &cfg);
            for (i, f) in frags.iter().enumerate() {
                if i == 1 {
                    continue;
                }
                if let Some(fr) = r.push(&f.to_bytes()) {
                    assert_eq!(fr.dlfc, Some(n));
                    got += 1;
                }
            }
        }
        assert_eq!(got, 5);
        assert_eq!(r.stats.pft.recovered, 5);
    }

    #[test]
    fn origins() {
        assert_eq!(MdiOrigin::parse("8000").unwrap().describe(), "UDP port 8000");
        assert_eq!(MdiOrigin::parse("239.1.2.3:8000").unwrap().describe(), "UDP multicast 239.1.2.3:8000");
        match MdiOrigin::parse("capture.pcap#9998").unwrap() {
            MdiOrigin::File { path, port } => assert_eq!((path, port), (PathBuf::from("capture.pcap"), Some(9998))),
            other => panic!("{other:?}"),
        }
        assert!(MdiOrigin::parse("").is_err());
        assert!(MdiOrigin::parse("nonsense").is_err());
    }

    /// A paced recording hands out a frame per 400 ms; an unpaced one at once.
    #[test]
    fn recordings_paced_and_unpaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rec.rsA");
        let mut w = DcpFileWriter::create(&path, FileKind::FileIo, 0).unwrap();
        for n in 1..=3u32 {
            w.write_packet(&frame(n).to_af(n as u16).to_bytes(), None).unwrap();
        }
        w.finish().unwrap();
        let origin = MdiOrigin::parse(path.to_str().unwrap()).unwrap();
        let mut fast = MdiInput::open(origin.clone(), false).unwrap();
        let t0 = Instant::now();
        let mut n = 0;
        while let MdiRead::Frame(_) = fast.read(Duration::from_millis(10)).unwrap() {
            n += 1;
        }
        assert_eq!(n, 3);
        assert!(t0.elapsed() < Duration::from_millis(300));
        let mut paced = MdiInput::open(origin, true).unwrap();
        let t0 = Instant::now();
        let mut frames = 0;
        loop {
            match paced.read(Duration::from_millis(100)).unwrap() {
                MdiRead::Frame(_) => frames += 1,
                MdiRead::Idle => {}
                MdiRead::End => break,
            }
        }
        assert_eq!(frames, 3);
        assert!(t0.elapsed() >= Duration::from_millis(790), "{:?}", t0.elapsed());
    }

    /// Frames over UDP on this computer.
    #[test]
    fn frames_over_udp() {
        let origin = MdiOrigin::Udp(UdpOrigin { port: 0, group: Some(std::net::Ipv4Addr::LOCALHOST), interface: None, source: None });
        let mut input = MdiInput::open(origin, false).unwrap();
        let port = input.local_addr().unwrap().port();
        let tx = UdpSender::new(&format!("127.0.0.1:{port}").parse().unwrap()).unwrap();
        assert_eq!(input.read(Duration::from_millis(20)).unwrap(), MdiRead::Idle);
        let cfg = PftConfig { max_payload: 1400, ..PftConfig::default() };
        for f in fragment(&frame(7).to_af(7).to_bytes(), 7, &cfg) {
            tx.send(&f.to_bytes()).unwrap();
        }
        match input.read(Duration::from_secs(5)).unwrap() {
            MdiRead::Frame(f) => assert_eq!(f.dlfc, Some(7)),
            other => panic!("{other:?}"),
        }
        assert!(input.last_sender.is_some());
    }
}
