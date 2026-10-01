//! A small HTTP/1.1 client for internet radio: one long `GET` over TCP or TLS (rustls,
//! with the operating system's root certificates), following redirects, with chunked
//! transfer coding, SHOUTCAST's `ICY 200 OK` status line, and reads that give up when
//! the stream stalls or the station stops.
//!
//! Why not a general-purpose HTTP library: SHOUTCAST v1 servers answer `ICY 200 OK`
//! (which HTTP parsers reject), and every blocking read must notice a stop request
//! within a fraction of a second. A radio stream needs nothing else.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};

/// Socket timeout of one read attempt: how quickly a stop request is noticed.
const POLL: Duration = Duration::from_millis(100);
/// Most bytes of a response header.
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// Most redirects followed for one request.
const MAX_REDIRECTS: usize = 10;

/// An `http://` or `https://` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Url {
    pub https: bool,
    /// Host name or address (an IPv6 address without its brackets).
    pub host: String,
    pub port: u16,
    /// Path and query, starting with `/`.
    pub path: String,
    /// `user:password` from the URL, for HTTP basic authentication.
    pub userinfo: Option<String>,
}

impl Url {
    /// Parse an absolute `http://` or `https://` URL. Spaces and non-ASCII characters in
    /// the path are percent-encoded; a fragment is dropped.
    pub fn parse(text: &str) -> Result<Url, String> {
        let s = text.trim();
        let lower = s.to_ascii_lowercase();
        let (https, rest) = if lower.starts_with("http://") {
            (false, &s[7..])
        } else if lower.starts_with("https://") {
            (true, &s[8..])
        } else {
            return Err(format!("\"{s}\" is not an http:// or https:// URL"));
        };
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        let (userinfo, hostport) = match authority.rfind('@') {
            Some(at) => (Some(authority[..at].to_string()), &authority[at + 1..]),
            None => (None, authority),
        };
        let default_port = if https { 443 } else { 80 };
        let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
            let close = v6.find(']').ok_or_else(|| format!("\"{s}\": unterminated IPv6 address"))?;
            let port = match &v6[close + 1..] {
                "" => None,
                p => Some(p.strip_prefix(':').ok_or_else(|| format!("\"{s}\": bad port"))?),
            };
            (v6[..close].to_string(), port)
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), Some(p)),
                None => (hostport.to_string(), None),
            }
        };
        if host.is_empty() {
            return Err(format!("\"{s}\" has no host name"));
        }
        let port = match port {
            None | Some("") => default_port,
            Some(p) => p.parse::<u16>().ok().filter(|&p| p > 0).ok_or_else(|| format!("\"{s}\": bad port \"{p}\""))?,
        };
        let tail = tail.split('#').next().unwrap_or_default();
        let path = match tail.chars().next() {
            None => "/".to_string(),
            Some('?') => format!("/{tail}"),
            Some(_) => tail.to_string(),
        };
        Ok(Url { https, host, port, path: encode_path(&path), userinfo })
    }

    /// `location` (a redirect target or a playlist entry: absolute, scheme-relative,
    /// absolute-path or relative) resolved against this URL.
    pub fn join(&self, location: &str) -> Result<Url, String> {
        let loc = location.trim();
        if loc.contains("://") {
            return Url::parse(loc);
        }
        if let Some(rest) = loc.strip_prefix("//") {
            return Url::parse(&format!("{}://{rest}", self.scheme()));
        }
        let path = if loc.starts_with('/') {
            loc.to_string()
        } else if loc.starts_with('?') {
            format!("{}{loc}", self.path.split('?').next().unwrap_or("/"))
        } else {
            let dir = self.path.split('?').next().unwrap_or("/");
            format!("{}{loc}", &dir[..dir.rfind('/').map_or(0, |i| i + 1)])
        };
        Ok(Url { path: encode_path(&remove_dot_segments(&path)), ..self.clone() })
    }

    fn scheme(&self) -> &'static str {
        if self.https { "https" } else { "http" }
    }

    /// The `Host` header: the host, bracketed if IPv6, with a non-default port.
    fn host_header(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == if self.https { 443 } else { 80 } { host } else { format!("{host}:{}", self.port) }
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Without the user information: it may hold a password.
        write!(f, "{}://{}{}", self.scheme(), self.host_header(), self.path)
    }
}

/// `path` without `.` and `..` segments (RFC 3986 §5.2.4); the query is kept as it is.
fn remove_dot_segments(path: &str) -> String {
    let (path, query) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path, None),
    };
    let mut out: Vec<&str> = Vec::new();
    let segments: Vec<&str> = path.split('/').collect();
    for (i, seg) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        match *seg {
            "." => {
                if last {
                    out.push("");
                }
            }
            ".." => {
                if out.len() > 1 {
                    out.pop();
                }
                if last {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    let mut joined = out.join("/");
    if !joined.starts_with('/') {
        joined.insert(0, '/');
    }
    match query {
        Some(q) => format!("{joined}?{q}"),
        None => joined,
    }
}

/// Percent-encode the bytes a request line cannot carry (controls, space, non-ASCII).
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        if b <= b' ' || b >= 0x7F {
            out.push_str(&format!("%{b:02X}"));
        } else {
            out.push(char::from(b));
        }
    }
    out
}

/// Base64 (RFC 4648) for the `Authorization` header.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// What went wrong with a request.
#[derive(Debug)]
pub(crate) enum HttpError {
    /// The stop flag was set.
    Stopped,
    /// The server answered with an error status.
    Status { code: u16, reason: String, url: String },
    /// Anything else (resolving, connecting, TLS, protocol, timeouts), as text.
    Failed(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::Stopped => write!(f, "stopped"),
            HttpError::Status { code, reason, url } => write!(f, "{url}: HTTP {code} {reason}"),
            HttpError::Failed(s) => f.write_str(s),
        }
    }
}

/// Settings of a request.
#[derive(Clone)]
pub(crate) struct Request {
    /// Set by the owner to abandon the request (and any read of its body).
    pub stop: Arc<AtomicBool>,
    /// TLS configuration (default: [`system_tls`]).
    pub tls: Option<Arc<rustls::ClientConfig>>,
    /// Ask for interleaved ICY metadata (`Icy-MetaData: 1`).
    pub icy_metadata: bool,
    /// Limit for resolving the host name and for connecting.
    pub connect_timeout: Duration,
    /// Longest wait for the response header and, later, for more body data.
    pub io_timeout: Duration,
}

/// A response whose status was 2xx, with its body still to be read.
pub(crate) struct Response {
    /// The URL that answered (after redirects).
    pub url: Url,
    /// Header names in lower case, values as sent (UTF-8, or Latin-1 when not UTF-8).
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

impl Response {
    /// The first header called `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// `GET url`, following redirects. Statuses of 400 and above are errors.
pub(crate) fn get(url: &Url, req: &Request) -> Result<Response, HttpError> {
    let mut url = url.clone();
    for _ in 0..=MAX_REDIRECTS {
        let (status, reason, headers, reader) = request(&url, req)?;
        let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
        match status {
            200..=299 => {
                let body = Body::new(reader, &headers);
                return Ok(Response { url, headers, body });
            }
            301 | 302 | 303 | 307 | 308 => {
                let location = header("location").ok_or_else(|| {
                    HttpError::Failed(format!("{url}: redirect ({status}) without a Location header"))
                })?;
                url = url.join(&location).map_err(HttpError::Failed)?;
            }
            _ => return Err(HttpError::Status { code: status, reason, url: url.to_string() }),
        }
    }
    Err(HttpError::Failed(format!("{url}: more than {MAX_REDIRECTS} redirects")))
}

type Head = (u16, String, Vec<(String, String)>, BufReader<Conn>);

/// One request/response exchange (no redirects).
fn request(url: &Url, req: &Request) -> Result<Head, HttpError> {
    let tcp = connect(url, req)?;
    let mut conn = if url.https {
        let config = match &req.tls {
            Some(c) => c.clone(),
            None => system_tls().map_err(HttpError::Failed)?,
        };
        let name = rustls::pki_types::ServerName::try_from(url.host.clone())
            .map_err(|e| HttpError::Failed(format!("{url}: {e}")))?;
        let tls = rustls::ClientConnection::new(config, name).map_err(|e| HttpError::Failed(format!("TLS: {e}")))?;
        Conn::Tls(Box::new(rustls::StreamOwned::new(tls, tcp)))
    } else {
        Conn::Plain(tcp)
    };
    let mut head = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: DecDRM/{}\r\nAccept: */*\r\n",
        url.path,
        url.host_header(),
        env!("CARGO_PKG_VERSION")
    );
    if req.icy_metadata {
        head.push_str("Icy-MetaData: 1\r\n");
    }
    if let Some(user) = &url.userinfo {
        head.push_str(&format!("Authorization: Basic {}\r\n", base64(user.as_bytes())));
    }
    head.push_str("Connection: close\r\n\r\n");
    conn.write_all(head.as_bytes()).and_then(|()| conn.flush()).map_err(|e| io_error(url, e))?;
    let mut reader = BufReader::with_capacity(16 * 1024, conn);
    let (status, reason) = read_status_line(&mut reader).map_err(|e| io_error(url, e))?;
    let headers = read_headers(&mut reader).map_err(|e| io_error(url, e))?;
    Ok((status, reason, headers, reader))
}

/// An I/O error of a request as an [`HttpError`].
fn io_error(url: &Url, e: io::Error) -> HttpError {
    if is_stopped(&e) {
        return HttpError::Stopped;
    }
    HttpError::Failed(format!("{url}: {}", describe_io(&e)))
}

/// Text for an I/O error, naming TLS problems as such.
pub(crate) fn describe_io(e: &io::Error) -> String {
    match e.get_ref().and_then(|inner| inner.downcast_ref::<rustls::Error>()) {
        Some(tls) => format!("TLS: {tls}"),
        None => e.to_string(),
    }
}

/// The error reads return after a stop request. It has a type of its own because std's
/// read helpers (`read_exact`, `read_until`, ...) retry errors of kind `Interrupted`,
/// which would spin instead of stopping.
#[derive(Debug)]
struct StopRequested;

impl fmt::Display for StopRequested {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("stopped")
    }
}

impl std::error::Error for StopRequested {}

/// Whether `e` is the error a read returns after a stop request.
pub(crate) fn is_stopped(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<StopRequested>())
}

fn read_line(reader: &mut BufReader<Conn>, total: &mut usize) -> io::Result<String> {
    let mut line = Vec::new();
    // `take` bounds the line, so a server sending endless bytes without a newline
    // cannot exhaust the memory.
    let n = reader.by_ref().take((MAX_HEADER_BYTES - *total) as u64 + 1).read_until(b'\n', &mut line)?;
    *total += n;
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the server closed the connection"));
    }
    if *total > MAX_HEADER_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "response header too long"));
    }
    while line.last().is_some_and(|&b| b == b'\n' || b == b'\r') {
        line.pop();
    }
    Ok(text(&line))
}

/// `HTTP/1.x 200 OK` or SHOUTCAST's `ICY 200 OK`.
fn read_status_line(reader: &mut BufReader<Conn>) -> io::Result<(u16, String)> {
    let mut total = 0;
    let line = read_line(reader, &mut total)?;
    let mut parts = line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    let code = parts.next().and_then(|c| c.trim().parse::<u16>().ok());
    match code {
        Some(code) if version.starts_with("HTTP/") || version == "ICY" => {
            Ok((code, parts.next().unwrap_or_default().trim().to_string()))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("not an HTTP response: \"{}\"", line.chars().take(60).collect::<String>()),
        )),
    }
}

fn read_headers(reader: &mut BufReader<Conn>) -> io::Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    let mut total = 0;
    loop {
        let line = read_line(reader, &mut total)?;
        if line.is_empty() {
            return Ok(headers);
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
}

/// UTF-8 text, or Latin-1 when the bytes are not UTF-8 (as old servers send).
pub(crate) fn text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| char::from(b)).collect(),
    }
}

/// Resolve and connect, within the connect timeout and interruptible by the stop flag.
fn connect(url: &Url, req: &Request) -> Result<Tcp, HttpError> {
    let addrs = resolve(&url.host, url.port, req)?;
    let mut last = None;
    for addr in addrs {
        if req.stop.load(Ordering::Relaxed) {
            return Err(HttpError::Stopped);
        }
        match TcpStream::connect_timeout(&addr, req.connect_timeout) {
            Ok(stream) => {
                let setup = stream
                    .set_read_timeout(Some(POLL))
                    .and_then(|()| stream.set_write_timeout(Some(POLL)))
                    .and_then(|()| stream.set_nodelay(true));
                setup.map_err(|e| HttpError::Failed(format!("{url}: {e}")))?;
                return Ok(Tcp { stream, stop: req.stop.clone(), timeout: req.io_timeout });
            }
            Err(e) => last = Some(e),
        }
    }
    Err(HttpError::Failed(match last {
        Some(e) => format!("cannot connect to {}:{}: {e}", url.host, url.port),
        None => format!("{} has no address", url.host),
    }))
}

/// The host's addresses. Name lookups have no timeout of their own, so they run on a
/// helper thread that is abandoned if it takes too long.
fn resolve(host: &str, port: u16, req: &Request) -> Result<Vec<SocketAddr>, HttpError> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let (tx, rx) = mpsc::channel();
    let name = format!("{host}:{port}");
    std::thread::Builder::new()
        .name("decdrm-dns".into())
        .spawn(move || {
            let _ = tx.send(name.to_socket_addrs().map(Iterator::collect::<Vec<_>>));
        })
        .map_err(|e| HttpError::Failed(format!("cannot start the name lookup: {e}")))?;
    let deadline = Instant::now() + req.connect_timeout;
    loop {
        if req.stop.load(Ordering::Relaxed) {
            return Err(HttpError::Stopped);
        }
        match rx.recv_timeout(POLL) {
            Ok(Ok(addrs)) if !addrs.is_empty() => return Ok(addrs),
            Ok(Ok(_)) => return Err(HttpError::Failed(format!("{host} has no address"))),
            Ok(Err(e)) => return Err(HttpError::Failed(format!("cannot resolve {host}: {e}"))),
            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
            Err(_) => return Err(HttpError::Failed(format!("cannot resolve {host}: no answer"))),
        }
    }
}

/// The TLS client configuration with the system's trusted roots (loaded once).
pub(crate) fn system_tls() -> Result<Arc<rustls::ClientConfig>, String> {
    // Rust note: `OnceLock` runs the closure on first use only, even across threads.
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let found = rustls_native_certs::load_native_certs();
            let mut roots = rustls::RootCertStore::empty();
            roots.add_parsable_certificates(found.certs);
            if roots.is_empty() {
                return Err("HTTPS: no trusted root certificates found on this system".to_string());
            }
            tls_config(roots)
        })
        .clone()
}

/// A TLS client configuration (ring crypto provider) trusting `roots`.
pub(crate) fn tls_config(roots: rustls::RootCertStore) -> Result<Arc<rustls::ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// A TCP connection whose reads poll the stop flag and give up after `timeout` without
/// data.
struct Tcp {
    stream: TcpStream,
    stop: Arc<AtomicBool>,
    timeout: Duration,
}

impl Tcp {
    /// Retry `op` while the socket times out, until data, the stop flag or `timeout`.
    fn retry<T>(&mut self, mut op: impl FnMut(&mut TcpStream) -> io::Result<T>) -> io::Result<T> {
        let start = Instant::now();
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Err(io::Error::other(StopRequested));
            }
            match op(&mut self.stream) {
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    if start.elapsed() >= self.timeout {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("no data from the server for {:.0} s", self.timeout.as_secs_f64()),
                        ));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                other => return other,
            }
        }
    }
}

impl Read for Tcp {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.retry(|s| s.read(buf))
    }
}

impl Write for Tcp {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.retry(|s| s.write(buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// Plain or TLS connection.
enum Conn {
    Plain(Tcp),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, Tcp>>),
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(t) => t.read(buf),
            Conn::Tls(t) => match t.read(buf) {
                // Many servers close without TLS's close_notify; for a stream that is
                // just its end.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
                other => other,
            },
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(t) => t.write(buf),
            Conn::Tls(t) => t.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Plain(t) => t.flush(),
            Conn::Tls(t) => t.flush(),
        }
    }
}

/// The body of a response: until the connection closes, `Content-Length` bytes, or
/// chunked.
pub(crate) struct Body {
    reader: BufReader<Conn>,
    framing: Framing,
}

enum Framing {
    UntilClose,
    Length(u64),
    /// Bytes left in the current chunk (`None` before the first chunk header), and
    /// whether the last chunk has been read.
    Chunked { left: Option<u64>, finished: bool },
}

impl Body {
    fn new(reader: BufReader<Conn>, headers: &[(String, String)]) -> Body {
        let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.to_ascii_lowercase());
        let framing = if header("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
            Framing::Chunked { left: None, finished: false }
        } else if let Some(n) = header("content-length").and_then(|v| v.trim().parse::<u64>().ok()) {
            Framing::Length(n)
        } else {
            Framing::UntilClose
        };
        Body { reader, framing }
    }
}

/// The size in a chunk's header line (RFC 9112 §7.1), 0 for the last chunk.
fn chunk_size(reader: &mut BufReader<Conn>) -> io::Result<u64> {
    let mut total = 0;
    let line = read_line(reader, &mut total)?;
    let hex = line.split(';').next().unwrap_or_default().trim();
    u64::from_str_radix(hex, 16)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("bad chunk header \"{hex}\"")))
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        // Rust note: `self.framing` and `self.reader` are distinct fields, so the arms
        // may change one while holding a mutable reference into the other.
        match &mut self.framing {
            Framing::UntilClose => self.reader.read(buf),
            Framing::Length(left) => {
                if *left == 0 {
                    return Ok(0);
                }
                let n = buf.len().min(usize::try_from(*left).unwrap_or(usize::MAX));
                let got = self.reader.read(&mut buf[..n])?;
                *left -= got as u64;
                Ok(got)
            }
            Framing::Chunked { left, finished } => {
                if *finished {
                    return Ok(0);
                }
                if matches!(*left, None | Some(0)) {
                    if left.is_some() {
                        // The line break after the previous chunk's data.
                        let mut total = 0;
                        read_line(&mut self.reader, &mut total)?;
                    }
                    let size = chunk_size(&mut self.reader)?;
                    if size == 0 {
                        *finished = true;
                        return Ok(0);
                    }
                    *left = Some(size);
                }
                let remaining = left.unwrap_or(0);
                let n = buf.len().min(usize::try_from(remaining).unwrap_or(usize::MAX));
                let got = self.reader.read(&mut buf[..n])?;
                if got == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the server closed a chunked response early"));
                }
                *left = Some(remaining - got as u64);
                Ok(got)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing() {
        let u = Url::parse("http://example.com:8000/live?x=1#frag").unwrap();
        assert_eq!((u.https, u.host.as_str(), u.port, u.path.as_str()), (false, "example.com", 8000, "/live?x=1"));
        assert_eq!(u.to_string(), "http://example.com:8000/live?x=1");
        let u = Url::parse("HTTPS://user:pw@[::1]/a b").unwrap();
        assert_eq!((u.https, u.host.as_str(), u.port, u.path.as_str()), (true, "::1", 443, "/a%20b"));
        assert_eq!(u.userinfo.as_deref(), Some("user:pw"));
        assert_eq!(u.to_string(), "https://[::1]/a%20b", "no password in the text");
        assert_eq!(Url::parse("http://host").unwrap().path, "/");
        assert_eq!(Url::parse("http://host?q").unwrap().path, "/?q");
        for bad in ["ftp://host/", "http://", "http://:80/", "http://host:0/", "http://host:x/", "host/stream"] {
            assert!(Url::parse(bad).is_err(), "{bad}");
        }
        assert!(Url::parse("rtsp://x/").unwrap_err().contains("http://"));
    }

    #[test]
    fn url_joining() {
        let base = Url::parse("https://radio.example:8443/lists/station.pls?id=3").unwrap();
        assert_eq!(base.join("http://other/s").unwrap().to_string(), "http://other/s");
        assert_eq!(base.join("//cdn.example/live").unwrap().to_string(), "https://cdn.example/live");
        assert_eq!(base.join("/mount").unwrap().to_string(), "https://radio.example:8443/mount");
        assert_eq!(base.join("hi.mp3").unwrap().to_string(), "https://radio.example:8443/lists/hi.mp3");
        assert_eq!(base.join("?id=4").unwrap().to_string(), "https://radio.example:8443/lists/station.pls?id=4");
        assert_eq!(base.join("../live").unwrap().path, "/live");
        assert_eq!(base.join("./a/../b/./c?x=../y").unwrap().path, "/lists/b/c?x=../y");
        assert_eq!(base.join("../../..").unwrap().path, "/");
        assert_eq!(base.join("/a/b/..").unwrap().path, "/a/");
    }

    #[test]
    fn base64_encoding() {
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b""), "");
    }

    #[test]
    fn latin1_fallback() {
        assert_eq!(text("Café".as_bytes()), "Café");
        assert_eq!(text(b"Caf\xe9"), "Café");
    }
}
