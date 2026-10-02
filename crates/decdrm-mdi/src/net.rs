//! UDP for DCP: where to listen and where to send, in Dream's address syntax.
//!
//! Receiving (an *origin*), fields may be empty, e.g. `192.168.1.5::239.1.2.3:8000`:
//!
//! | Form | Meaning |
//! |---|---|
//! | `port` | any local address |
//! | `group:port` | join multicast group `group` — or, for a unicast address, listen on that local address only |
//! | `iface:group:port` | join `group` on the interface with address `iface` |
//! | `source:iface:group:port` | as before, and accept only packets from `source` |
//!
//! Sending (a *destination*): `port` (this computer), `host:port`, or
//! `iface:host:port` (send from the interface with address `iface`). Dream's TCP forms
//! (`-`, `t…`) are not supported.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::str::FromStr;
use std::time::Duration;

/// Where to receive UDP (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpOrigin {
    pub port: u16,
    /// Multicast group to join, or the local address to listen on.
    pub group: Option<Ipv4Addr>,
    /// Interface (by its address) to join the group on.
    pub interface: Option<Ipv4Addr>,
    /// Accept packets only from this sender.
    pub source: Option<Ipv4Addr>,
}

fn field(s: &str, what: &str) -> Result<Option<Ipv4Addr>, String> {
    if s.is_empty() {
        return Ok(None);
    }
    s.parse().map(Some).map_err(|_| format!("{what} \"{s}\" is not an IPv4 address"))
}

fn port(s: &str) -> Result<u16, String> {
    match s.parse::<u16>() {
        Ok(p) if p > 0 => Ok(p),
        _ => Err(format!("\"{s}\" is not a port number (1–65535)")),
    }
}

/// Strip an optional `udp:` / `udp://` prefix.
fn strip_scheme(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix("udp://").or_else(|| s.strip_prefix("udp:")).unwrap_or(s)
}

impl FromStr for UdpOrigin {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = strip_scheme(s).split(':').collect();
        let (source, interface, group, p) = match parts.as_slice() {
            [p] => ("", "", "", *p),
            [g, p] => ("", "", *g, *p),
            [i, g, p] => ("", *i, *g, *p),
            [s, i, g, p] => (*s, *i, *g, *p),
            _ => return Err(format!("\"{s}\" is not an address (port, group:port, interface:group:port or source:interface:group:port)")),
        };
        Ok(Self { port: port(p)?, group: field(group, "group")?, interface: field(interface, "interface")?, source: field(source, "source")? })
    }
}

impl std::fmt::Display for UdpOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let a = |x: Option<Ipv4Addr>| x.map(|a| a.to_string()).unwrap_or_default();
        match (self.source, self.interface, self.group) {
            (None, None, None) => write!(f, "{}", self.port),
            (None, None, Some(g)) => write!(f, "{g}:{}", self.port),
            (None, i, g) => write!(f, "{}:{}:{}", a(i), a(g), self.port),
            (s, i, g) => write!(f, "{}:{}:{}:{}", a(s), a(i), a(g), self.port),
        }
    }
}

/// Where to send UDP (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpDestination {
    pub addr: SocketAddrV4,
    /// Send from the interface with this address.
    pub interface: Option<Ipv4Addr>,
}

impl FromStr for UdpDestination {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = strip_scheme(s).split(':').collect();
        let (interface, host, p) = match parts.as_slice() {
            [p] => (None, Ipv4Addr::LOCALHOST, *p),
            [h, p] => (None, field(h, "host")?.ok_or("empty host")?, *p),
            [i, h, p] => (field(i, "interface")?, field(h, "host")?.ok_or("empty host")?, *p),
            _ => return Err(format!("\"{s}\" is not a destination (port, host:port or interface:host:port)")),
        };
        Ok(Self { addr: SocketAddrV4::new(host, port(p)?), interface })
    }
}

/// A UDP socket receiving DCP packets.
pub struct UdpReceiver {
    socket: UdpSocket,
    source: Option<Ipv4Addr>,
    buf: Vec<u8>,
    /// Packets dropped by the source filter.
    pub filtered: u64,
}

impl UdpReceiver {
    /// Listen as `origin` says (joining a multicast group if it names one).
    pub fn bind(origin: &UdpOrigin) -> io::Result<Self> {
        let multicast = origin.group.filter(Ipv4Addr::is_multicast);
        // A multicast group is joined on a socket bound to any address (Windows does
        // not allow binding to the group address); a unicast address is listened on.
        let local = match origin.group {
            Some(g) if !g.is_multicast() => g,
            _ => Ipv4Addr::UNSPECIFIED,
        };
        let socket = UdpSocket::bind(SocketAddrV4::new(local, origin.port))?;
        if let Some(group) = multicast {
            socket.join_multicast_v4(&group, &origin.interface.unwrap_or(Ipv4Addr::UNSPECIFIED))?;
        }
        Ok(Self { socket, source: origin.source, buf: vec![0; 65_536], filtered: 0 })
    }

    /// The local address (the port, when 0 was asked for).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// The next packet and its sender, waiting up to `wait` (`Ok(None)` if nothing
    /// came).
    pub fn recv(&mut self, wait: Duration) -> io::Result<Option<(Vec<u8>, SocketAddr)>> {
        self.socket.set_read_timeout(Some(wait.max(Duration::from_millis(1))))?;
        loop {
            match self.socket.recv_from(&mut self.buf) {
                Ok((n, from)) => {
                    if let (Some(want), SocketAddr::V4(f)) = (self.source, from)
                        && *f.ip() != want
                    {
                        self.filtered += 1;
                        continue;
                    }
                    return Ok(Some((self.buf[..n].to_vec(), from)));
                }
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => return Ok(None),
                // Windows reports an ICMP "port unreachable" for an earlier send as an
                // error on the next receive; it says nothing about this socket.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

/// A UDP socket sending to one destination.
pub struct UdpSender {
    socket: UdpSocket,
    dest: SocketAddrV4,
}

impl UdpSender {
    pub fn new(dest: &UdpDestination) -> io::Result<Self> {
        let socket = UdpSocket::bind(SocketAddrV4::new(dest.interface.unwrap_or(Ipv4Addr::UNSPECIFIED), 0))?;
        if dest.addr.ip().is_multicast() {
            socket.set_multicast_ttl_v4(16)?;
        }
        Ok(Self { socket, dest: dest.addr })
    }

    pub fn send(&self, data: &[u8]) -> io::Result<()> {
        self.socket.send_to(data, self.dest).map(|_| ())
    }

    pub fn destination(&self) -> SocketAddrV4 {
        self.dest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        let o: UdpOrigin = "8000".parse().unwrap();
        assert_eq!(o, UdpOrigin { port: 8000, group: None, interface: None, source: None });
        let o: UdpOrigin = "udp://239.1.2.3:8000".parse().unwrap();
        assert_eq!(o.group, Some(Ipv4Addr::new(239, 1, 2, 3)));
        let o: UdpOrigin = "192.168.1.5::239.1.2.3:8000".parse().unwrap();
        assert_eq!((o.source, o.interface), (Some(Ipv4Addr::new(192, 168, 1, 5)), None));
        assert_eq!(o.to_string(), "192.168.1.5::239.1.2.3:8000");
        let o: UdpOrigin = "10.0.0.2:239.1.2.3:9000".parse().unwrap();
        assert_eq!(o.to_string(), "10.0.0.2:239.1.2.3:9000");
        assert!("0".parse::<UdpOrigin>().is_err());
        assert!("a:b:c:d:e".parse::<UdpOrigin>().is_err());
        assert!("x.y:80".parse::<UdpOrigin>().unwrap_err().contains("group"));
    }

    #[test]
    fn destinations() {
        let d: UdpDestination = "9000".parse().unwrap();
        assert_eq!(d.addr, SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9000));
        let d: UdpDestination = "10.0.0.1:10.0.0.9:9000".parse().unwrap();
        assert_eq!((d.interface, d.addr.ip()), (Some(Ipv4Addr::new(10, 0, 0, 1)), &Ipv4Addr::new(10, 0, 0, 9)));
        assert!(":9000".parse::<UdpDestination>().is_err());
    }

    /// A packet to this computer arrives; nothing more within the wait; the source
    /// filter drops packets from other senders.
    #[test]
    fn send_and_receive_locally() {
        let origin = UdpOrigin { port: 0, group: Some(Ipv4Addr::LOCALHOST), interface: None, source: None };
        let mut rx = UdpReceiver::bind(&origin).unwrap();
        let port = rx.local_addr().unwrap().port();
        let tx = UdpSender::new(&format!("127.0.0.1:{port}").parse().unwrap()).unwrap();
        tx.send(b"AF hello").unwrap();
        let (data, from) = rx.recv(Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(data, b"AF hello");
        assert_eq!(from.ip(), std::net::IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(rx.recv(Duration::from_millis(20)).unwrap(), None);

        let mut filtered = UdpReceiver::bind(&UdpOrigin { source: Some(Ipv4Addr::new(10, 9, 8, 7)), ..origin }).unwrap();
        let port = filtered.local_addr().unwrap().port();
        UdpSender::new(&format!("127.0.0.1:{port}").parse().unwrap()).unwrap().send(b"x").unwrap();
        assert_eq!(filtered.recv(Duration::from_millis(200)).unwrap(), None);
        assert_eq!(filtered.filtered, 1);
    }
}
