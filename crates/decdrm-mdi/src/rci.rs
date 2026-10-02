//! RCI — the control half of RSCI (TS 102 349): commands to a receiver, sent as TAG
//! items in an AF packet whose `*ptr` names the protocol "RSCI".
//!
//! | Item | Command |
//! |---|---|
//! | `cact` | activate ('1') or deactivate ('0') the receiver |
//! | `cfre` | tune to this frequency, Hz (32 bits) |
//! | `cdmo` | demodulation mode: "drm_", "am__", "fm__" … (4 characters) |
//! | `crec` | start/stop a recording: kind ("st" RSCI, "iq" I/Q), profile, '1'/'0' |
//! | `cpro` | send RSCI in this profile ('A'–'D', 'Q', 'M') |
//! | `cser` | decode this service (Short Id, 1 byte) |
//!
//! Dream sends `cfre` and `cdmo` to the receiver whose RSCI it shows, and obeys `cact`,
//! `cfre`, `cdmo`, `crec` and `cpro` itself.

use crate::af::AfPacket;
use crate::mdi::Protocol;
use crate::net::{UdpDestination, UdpOrigin, UdpReceiver, UdpSender};
use crate::source::DcpReceiver;
use crate::tag::{TagItem, build_tag_packet, parse_tag_packet};
use std::io;
use std::net::{SocketAddr, SocketAddrV4};
use std::time::Duration;

/// One RCI command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RciCommand {
    Activate(bool),
    /// Frequency, Hz.
    Frequency(u32),
    /// "drm_", "am__", "fm__", ….
    Demodulation(String),
    Recording { kind: [u8; 2], profile: u8, on: bool },
    Profile(char),
    Service(u8),
}

impl RciCommand {
    /// The command as a TAG item.
    pub fn to_item(&self) -> TagItem {
        match self {
            RciCommand::Activate(on) => TagItem::new(b"cact", vec![if *on { b'1' } else { b'0' }]),
            RciCommand::Frequency(hz) => TagItem::new(b"cfre", hz.to_be_bytes().to_vec()),
            RciCommand::Demodulation(m) => {
                let mut v = m.as_bytes().to_vec();
                v.resize(4, b'_');
                TagItem::new(b"cdmo", v)
            }
            RciCommand::Recording { kind, profile, on } => {
                TagItem::new(b"crec", vec![kind[0], kind[1], *profile, if *on { b'1' } else { b'0' }])
            }
            RciCommand::Profile(p) => TagItem::new(b"cpro", vec![*p as u8]),
            RciCommand::Service(s) => TagItem::new(b"cser", vec![*s]),
        }
    }

    /// The command an item carries, if it is one.
    pub fn from_item(item: &TagItem) -> Option<Self> {
        let v = &item.value[..(item.bits as usize / 8).min(item.value.len())];
        Some(match &item.name {
            b"cact" => RciCommand::Activate(*v.first()? == b'1'),
            b"cfre" if v.len() >= 4 => RciCommand::Frequency(u32::from_be_bytes([v[0], v[1], v[2], v[3]])),
            b"cdmo" if !v.is_empty() => RciCommand::Demodulation(String::from_utf8_lossy(v).into_owned()),
            b"crec" if v.len() >= 4 => RciCommand::Recording { kind: [v[0], v[1]], profile: v[2], on: v[3] == b'1' },
            b"cpro" => RciCommand::Profile(*v.first()? as char),
            b"cser" => RciCommand::Service(*v.first()?),
            _ => return None,
        })
    }

    /// For logs: "tune to 6030.000 kHz", ….
    pub fn describe(&self) -> String {
        match self {
            RciCommand::Activate(on) => if *on { "activate" } else { "deactivate" }.into(),
            RciCommand::Frequency(hz) => format!("tune to {:.3} kHz", f64::from(*hz) / 1000.0),
            RciCommand::Demodulation(m) => format!("demodulate {}", m.trim_end_matches('_')),
            RciCommand::Recording { kind, profile, on } => format!(
                "{} {} recording (profile {})",
                if *on { "start" } else { "stop" },
                String::from_utf8_lossy(kind),
                *profile as char
            ),
            RciCommand::Profile(p) => format!("RSCI profile {p}"),
            RciCommand::Service(s) => format!("select service {s}"),
        }
    }
}

/// An AF packet carrying `commands` (with the RSCI `*ptr`).
pub fn control_packet(commands: &[RciCommand], seq: u16) -> AfPacket {
    let p = Protocol::RSCI;
    let mut ptr = p.name.to_vec();
    ptr.extend_from_slice(&p.major.to_be_bytes());
    ptr.extend_from_slice(&p.minor.to_be_bytes());
    let mut items = vec![TagItem::new(b"*ptr", ptr)];
    items.extend(commands.iter().map(RciCommand::to_item));
    AfPacket::new(seq, build_tag_packet(&items))
}

/// The commands in a received AF packet (other items are ignored).
pub fn parse_control(af: &AfPacket) -> Vec<RciCommand> {
    parse_tag_packet(&af.payload).map(|items| items.iter().filter_map(RciCommand::from_item).collect()).unwrap_or_default()
}

/// Sends RCI commands to a receiver (the one whose RSCI is being shown).
pub struct RciSender {
    sender: UdpSender,
    seq: u16,
}

impl RciSender {
    pub fn new(dest: &UdpDestination) -> io::Result<Self> {
        Ok(Self { sender: UdpSender::new(dest)?, seq: 0 })
    }

    /// Send `commands` in one AF packet.
    pub fn send(&mut self, commands: &[RciCommand]) -> io::Result<()> {
        let packet = control_packet(commands, self.seq);
        self.seq = self.seq.wrapping_add(1);
        self.sender.send(&packet.to_bytes())
    }

    pub fn destination(&self) -> SocketAddrV4 {
        self.sender.destination()
    }
}

/// Receives RCI commands: DecDRM controlled from elsewhere (AF packets or PFT
/// fragments).
pub struct RciListener {
    rx: UdpReceiver,
    dcp: DcpReceiver,
}

impl RciListener {
    pub fn bind(origin: &UdpOrigin) -> io::Result<Self> {
        Ok(Self { rx: UdpReceiver::bind(origin)?, dcp: DcpReceiver::new() })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.rx.local_addr()
    }

    /// The commands that arrived, waiting up to `wait` for the first packet; with
    /// their senders.
    pub fn poll(&mut self, wait: Duration) -> io::Result<Vec<(RciCommand, SocketAddr)>> {
        let mut out = Vec::new();
        let mut wait = wait;
        while let Some((data, from)) = self.rx.recv(wait)? {
            if let Some(af) = self.dcp.push_af(&data) {
                out.extend(parse_control(&af).into_iter().map(|c| (c, from)));
            }
            // Then only what is already waiting.
            wait = Duration::from_millis(1);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_round_trip() {
        let cmds = vec![
            RciCommand::Activate(true),
            RciCommand::Frequency(6_030_000),
            RciCommand::Demodulation("drm_".into()),
            RciCommand::Recording { kind: *b"st", profile: b'A', on: true },
            RciCommand::Profile('D'),
            RciCommand::Service(2),
        ];
        let af = control_packet(&cmds, 9);
        let back = parse_control(&AfPacket::parse(&af.to_bytes()).unwrap());
        assert_eq!(back, cmds);
        assert_eq!(cmds[1].describe(), "tune to 6030.000 kHz");
        assert_eq!(RciCommand::Demodulation("am".into()).to_item().value, b"am__");
        assert_eq!(cmds[3].describe(), "start st recording (profile A)");
    }

    /// Commands sent to a listener on this computer arrive with their sender.
    #[test]
    fn send_and_listen() {
        let origin = UdpOrigin { port: 0, group: Some(std::net::Ipv4Addr::LOCALHOST), interface: None, source: None };
        let mut listener = RciListener::bind(&origin).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut sender = RciSender::new(&format!("127.0.0.1:{port}").parse().unwrap()).unwrap();
        sender.send(&[RciCommand::Frequency(7_325_000), RciCommand::Service(1)]).unwrap();
        let got = listener.poll(Duration::from_secs(5)).unwrap();
        let cmds: Vec<RciCommand> = got.iter().map(|(c, _)| c.clone()).collect();
        assert_eq!(cmds, [RciCommand::Frequency(7_325_000), RciCommand::Service(1)]);
        assert!(listener.poll(Duration::from_millis(10)).unwrap().is_empty());
    }
}
