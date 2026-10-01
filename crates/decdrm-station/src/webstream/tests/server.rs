//! A small HTTP server on 127.0.0.1 for the web stream tests: routes by path, serves
//! streams with ICY metadata, pacing, chunked coding and dropped connections, plus
//! redirects, plain bodies and a server that never answers. Each connection gets its
//! own thread.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A request as the handlers see it.
pub(crate) struct Req {
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    /// How many earlier requests had the same path (0 for the first).
    pub index: usize,
    /// Set when the server shuts down; long-running handlers return.
    pub stop: Arc<AtomicBool>,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// Writes the response to one request (any I/O error just ends the connection).
pub(crate) type Handler = Arc<dyn Fn(&Req, &mut dyn Write) -> io::Result<()> + Send + Sync>;

/// The server; stops when dropped.
pub(crate) struct Server {
    pub port: u16,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    hits: Arc<Mutex<Vec<String>>>,
}

impl Server {
    /// Serve `routes` (path → handler); unknown paths get 404.
    pub fn start(routes: Vec<(&str, Handler)>) -> Server {
        Self::start_with(routes, |s| Box::new(s) as Box<dyn ReadWrite>)
    }

    /// Like [`Self::start`], wrapping every accepted connection with `wrap` (e.g. TLS).
    pub fn start_with(
        routes: Vec<(&str, Handler)>,
        wrap: impl Fn(TcpStream) -> Box<dyn ReadWrite> + Send + Sync + 'static,
    ) -> Server {
        Self::start_on(0, routes, wrap)
    }

    /// Like [`Self::start_with`] on `port` of 127.0.0.1 (0: any free one).
    pub fn start_on(
        port: u16,
        routes: Vec<(&str, Handler)>,
        wrap: impl Fn(TcpStream) -> Box<dyn ReadWrite> + Send + Sync + 'static,
    ) -> Server {
        let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind a local port");
        let port = listener.local_addr().expect("local address").port();
        listener.set_nonblocking(true).expect("non-blocking listener");
        let routes: Arc<HashMap<String, Handler>> = Arc::new(routes.into_iter().map(|(p, h)| (p.to_string(), h)).collect());
        let stop = Arc::new(AtomicBool::new(false));
        let hits = Arc::new(Mutex::new(Vec::new()));
        let wrap = Arc::new(wrap);
        let accept = {
            let (stop, hits) = (stop.clone(), hits.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            let (routes, stop, hits, wrap) = (routes.clone(), stop.clone(), hits.clone(), wrap.clone());
                            std::thread::spawn(move || {
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                                let mut conn = wrap(stream);
                                let _ = serve(&mut *conn, &routes, &stop, &hits);
                            });
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                        Err(_) => break,
                    }
                }
            })
        };
        Server { port, stop, accept: Some(accept), hits }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Requests made for `path` so far.
    pub fn hits(&self, path: &str) -> usize {
        self.hits.lock().unwrap().iter().filter(|p| *p == path).count()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
    }
}

/// A connection: plain TCP or TLS.
pub(crate) trait ReadWrite: Read + Write + Send {}
impl<T: Read + Write + Send> ReadWrite for T {}

fn serve(conn: &mut dyn ReadWrite, routes: &HashMap<String, Handler>, stop: &Arc<AtomicBool>, hits: &Mutex<Vec<String>>) -> io::Result<()> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if conn.read(&mut byte)? == 0 || head.len() > 16 * 1024 {
            return Ok(());
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head).to_string();
    let mut lines = text.split("\r\n");
    let path = lines.next().and_then(|l| l.split(' ').nth(1)).unwrap_or("/").to_string();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let index = {
        let mut h = hits.lock().unwrap();
        let n = h.iter().filter(|p| **p == path).count();
        h.push(path.clone());
        n
    };
    let req = Req { headers, index, stop: stop.clone() };
    match routes.get(&path) {
        Some(handler) => handler(&req, conn),
        None => conn.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found"),
    }?;
    conn.flush()
}

/// A plain response.
pub(crate) fn body(status: &'static str, content_type: &'static str, body: impl Into<Vec<u8>>) -> Handler {
    let body = body.into();
    Arc::new(move |_req, out| {
        write!(out, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())?;
        out.write_all(&body)
    })
}

/// A redirect to `location`.
pub(crate) fn redirect(status: &'static str, location: impl Into<String>) -> Handler {
    let location = location.into();
    Arc::new(move |_req, out| write!(out, "HTTP/1.1 {status}\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n"))
}

/// Accepts the request and never answers (until the server stops).
pub(crate) fn silent() -> Handler {
    Arc::new(|req, _out| {
        while !req.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    })
}

/// A stream response (see the fields).
#[derive(Clone)]
pub(crate) struct Stream {
    /// Status line, e.g. `HTTP/1.0 200 OK` or SHOUTCAST's `ICY 200 OK`.
    pub status: &'static str,
    /// Headers besides the ICY metaint (`Content-Type`, `icy-name`, ...).
    pub headers: Vec<(&'static str, String)>,
    pub data: Arc<Vec<u8>>,
    /// Interleave ICY metadata every this many bytes when the client asks for it.
    pub metaint: Option<usize>,
    /// Titles from these audio byte offsets on.
    pub titles: Vec<(usize, String)>,
    /// First connection only: close after this many audio bytes.
    pub drop_after: Option<usize>,
    /// Bytes per second after the first `burst` bytes (none: as fast as possible).
    pub pace: Option<f64>,
    pub burst: usize,
    /// Chunked transfer coding (HTTP/1.1).
    pub chunked: bool,
    /// Start over at the end of `data` (an endless stream).
    pub repeat: bool,
}

impl Stream {
    pub fn new(content_type: &str, data: Vec<u8>) -> Stream {
        Stream {
            status: "HTTP/1.0 200 OK",
            headers: vec![("Content-Type", content_type.to_string())],
            data: Arc::new(data),
            metaint: None,
            titles: Vec::new(),
            drop_after: None,
            pace: None,
            burst: 0,
            chunked: false,
            repeat: false,
        }
    }

    pub fn handler(self) -> Handler {
        Arc::new(move |req, out| self.serve(req, out))
    }

    fn serve(&self, req: &Req, out: &mut dyn Write) -> io::Result<()> {
        let metaint = self.metaint.filter(|_| req.header("icy-metadata") == Some("1"));
        let mut head = format!("{}\r\n", self.status);
        for (name, value) in &self.headers {
            head += &format!("{name}: {value}\r\n");
        }
        if let Some(n) = metaint {
            head += &format!("icy-metaint: {n}\r\n");
        }
        if self.chunked {
            head += "Transfer-Encoding: chunked\r\n";
        }
        head += "\r\n";
        out.write_all(head.as_bytes())?;
        let mut writer = BodyWriter { out, chunked: self.chunked };
        let started = Instant::now();
        let (mut sent, mut until_meta) = (0usize, metaint.unwrap_or(usize::MAX));
        let mut last_title: Option<&str> = None;
        let mut repeats_left = 1;
        let total = if self.repeat { usize::MAX } else { self.data.len() };
        while sent < total && !req.stop.load(Ordering::Relaxed) {
            if req.index == 0 && self.drop_after.is_some_and(|d| sent >= d) {
                return Ok(()); // the connection drops
            }
            if until_meta == 0 {
                // The title in force; sent when it changes (and once more after that).
                let title = self.titles.iter().rev().find(|(at, _)| *at <= sent).map(|(_, t)| t.as_str());
                let block = if title != last_title || repeats_left > 0 {
                    if title != last_title {
                        repeats_left = 1;
                    } else {
                        repeats_left -= 1;
                    }
                    last_title = title;
                    format!("StreamTitle='{}';StreamUrl='';", title.unwrap_or(""))
                } else {
                    String::new()
                };
                let mut meta = block.into_bytes();
                let blocks = meta.len().div_ceil(16);
                meta.resize(blocks * 16, 0);
                let mut piece = vec![blocks as u8];
                piece.extend_from_slice(&meta);
                writer.write(&piece)?;
                until_meta = metaint.unwrap_or(usize::MAX);
            }
            let pos = sent % self.data.len();
            let mut n = (self.data.len() - pos).min(until_meta).min(4096).min(total - sent);
            if let Some(d) = self.drop_after.filter(|_| req.index == 0) {
                n = n.min(d - sent);
            }
            if let Some(rate) = self.pace
                && sent >= self.burst
            {
                // Pace: wait until this chunk is due.
                n = n.min((rate / 50.0).max(1.0) as usize);
                let due = Duration::from_secs_f64((sent - self.burst) as f64 / rate);
                let now = started.elapsed();
                if due > now {
                    std::thread::sleep(due - now);
                }
            }
            writer.write(&self.data[pos..pos + n])?;
            sent += n;
            until_meta = until_meta.saturating_sub(n);
        }
        writer.finish()
    }
}

/// Writes body bytes, chunked or not.
struct BodyWriter<'a> {
    out: &'a mut dyn Write,
    chunked: bool,
}

impl BodyWriter<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if self.chunked {
            write!(self.out, "{:x}\r\n", data.len())?;
            self.out.write_all(data)?;
            self.out.write_all(b"\r\n")
        } else {
            self.out.write_all(data)
        }
    }

    fn finish(&mut self) -> io::Result<()> {
        if self.chunked {
            self.out.write_all(b"0\r\n\r\n")?;
        }
        self.out.flush()
    }
}
