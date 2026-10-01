//! A stand-in KiwiSDR for tests: it accepts WebSocket connections on 127.0.0.1, talks
//! like a Kiwi in I/Q mode (version, configuration, `audio_rate`, waiting for
//! `SET AR OK`, `sample_rate`, `SND` blocks) and records every command it receives.
//! Each connection plays the next [`MockSession`]; the last one repeats.

use crate::protocol;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::{Message, WebSocket};

/// What one connection does.
#[derive(Debug, Clone)]
pub enum MockSession {
    /// Stream I/Q: `blocks` blocks in all (`None`: until the client leaves), then end.
    Stream { blocks: Option<usize>, end: MockEnd },
    /// All `n` channels busy.
    TooBusy(u32),
    /// Refuse with this `badp` code.
    BadPassword(u8),
    /// Answer the WebSocket upgrade with an HTTP/1.0 307 to this URL (as the kiwisdr.com
    /// proxy does).
    HttpRedirect(String),
    /// Send the Kiwi's `redirect` message with this URL.
    MsgRedirect(String),
    /// Answer HTTP 404 (not a Kiwi, or a proxied Kiwi that is offline).
    NotFound,
}

/// How a streaming session ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockEnd {
    /// Drop the TCP connection without a WebSocket close (a lost connection).
    Drop,
    /// Close the WebSocket cleanly (as a Kiwi's time limit does).
    Close,
    /// Keep the connection open until the client closes it.
    Wait,
}

/// The stand-in's behaviour.
#[derive(Debug, Clone)]
pub struct MockConfig {
    /// Exact sample rate announced (`sample_rate`).
    pub sample_rate: f64,
    /// Nominal rate announced (`audio_rate`).
    pub audio_rate: u32,
    pub name: Option<String>,
    pub location: Option<String>,
    /// I/Q samples streamed, repeated as needed (silence if empty).
    pub iq: Arc<Vec<(i16, i16)>>,
    /// Samples per `SND` block.
    pub block: usize,
    pub rssi_dbm: f32,
    /// Pace blocks in real time (otherwise as fast as the client takes them).
    pub paced: bool,
    pub sessions: Vec<MockSession>,
    /// Behave like firmware that ignores one style of WebSocket path: accept the
    /// connection but never answer. `Some(true)`: typed paths (`/no_wf/<ts>/SND`);
    /// `Some(false)`: kiwiclient's `/<ts>/SND`.
    pub ignore_typed_paths: Option<bool>,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            sample_rate: 12_001.135,
            audio_rate: 12_000,
            name: Some("Mock Kiwi".into()),
            location: Some("Test bench".into()),
            iq: Arc::new(Vec::new()),
            block: 512,
            rssi_dbm: -73.0,
            paced: false,
            sessions: vec![MockSession::Stream { blocks: None, end: MockEnd::Wait }],
            ignore_typed_paths: None,
        }
    }
}

#[derive(Default)]
struct Record {
    commands: Mutex<Vec<String>>,
    paths: Mutex<Vec<String>>,
}

/// A running stand-in; dropping it stops accepting connections.
pub struct MockKiwi {
    port: u16,
    record: Arc<Record>,
    connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl MockKiwi {
    pub fn start(cfg: MockConfig) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let record = Arc::new(Record::default());
        let connections = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (rec, count, halt) = (Arc::clone(&record), Arc::clone(&connections), Arc::clone(&stop));
        std::thread::Builder::new().name("mock-kiwi".into()).spawn(move || {
            while !halt.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((s, _)) => {
                        let _ = s.set_nonblocking(false);
                        let k = count.fetch_add(1, Ordering::SeqCst);
                        let session = cfg.sessions[k.min(cfg.sessions.len().saturating_sub(1))].clone();
                        let (cfg, rec, halt) = (cfg.clone(), Arc::clone(&rec), Arc::clone(&halt));
                        std::thread::spawn(move || serve(s, &session, &cfg, &rec, &halt));
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                    Err(_) => break,
                }
            }
        })?;
        Ok(Self { port, record, connections, stop })
    }

    /// `127.0.0.1:port`, as a KiwiSDR address.
    pub fn address(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// Every text message the clients sent, in order.
    pub fn commands(&self) -> Vec<String> {
        self.record.commands.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The WebSocket paths the clients asked for.
    pub fn paths(&self) -> Vec<String> {
        self.record.paths.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Connections accepted so far.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

impl Drop for MockKiwi {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Read an HTTP request's head (to answer it without a WebSocket).
fn read_head(s: &mut TcpStream) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") && head.len() < 16_384 {
        match s.read(&mut b) {
            Ok(1) => head.push(b[0]),
            _ => break,
        }
    }
}

fn send_msg(ws: &mut WebSocket<TcpStream>, text: &str) -> bool {
    ws.send(Message::binary(format!("MSG {text}").into_bytes())).is_ok()
}

/// Read messages until the client has gone (recording its commands).
fn drain(ws: &mut WebSocket<TcpStream>, rec: &Record) {
    let _ = ws.get_mut().set_read_timeout(Some(Duration::from_secs(5)));
    while let Ok(m) = ws.read() {
        if let Message::Text(t) = m {
            rec.commands.lock().unwrap_or_else(|p| p.into_inner()).push(t.to_string());
        }
    }
}

fn serve(mut s: TcpStream, session: &MockSession, cfg: &MockConfig, rec: &Record, halt: &AtomicBool) {
    match session {
        MockSession::HttpRedirect(url) => {
            read_head(&mut s);
            let _ = write!(s, "HTTP/1.0 307 Temporary Redirect\r\nContent-Type: text/html\r\nLocation: {url}\r\nServer: frp secondary redirect\r\n\r\n307 Temporary Redirect");
            return;
        }
        MockSession::NotFound => {
            read_head(&mut s);
            let _ = write!(s, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return;
        }
        _ => {}
    }
    let path_log = &rec.paths;
    let mut path = String::new();
    // The signature is tungstenite's handshake callback.
    #[allow(clippy::result_large_err)]
    let callback = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        path = req.uri().path().to_string();
        path_log.lock().unwrap_or_else(|p| p.into_inner()).push(path.clone());
        Ok(resp)
    };
    let Ok(mut ws) = tungstenite::accept_hdr(s, callback) else { return };
    let record = |t: &str| rec.commands.lock().unwrap_or_else(|p| p.into_inner()).push(t.to_string());
    let typed = path.trim_start_matches('/').split('/').count() > 2;
    if cfg.ignore_typed_paths == Some(typed) {
        // Accepted, then silence (but the client's messages are still read).
        return drain(&mut ws, rec);
    }
    // The client authenticates first.
    match ws.read() {
        Ok(Message::Text(t)) => record(&t),
        _ => return,
    }
    if !send_msg(&mut ws, "version_maj=1 version_min=826") {
        return;
    }
    if cfg.name.is_some() || cfg.location.is_some() {
        let json = serde_json::json!({
            "rx_name": cfg.name.clone().unwrap_or_default(),
            "rx_location": cfg.location.clone().unwrap_or_default(),
            "rx_gps": "(-34.9, 138.6)",
        });
        send_msg(&mut ws, &format!("load_cfg={}", protocol::percent_encode(&json.to_string())));
    }
    let (blocks, end) = match session {
        MockSession::TooBusy(n) => {
            send_msg(&mut ws, &format!("too_busy={n}"));
            let _ = ws.close(None);
            return drain(&mut ws, rec);
        }
        MockSession::BadPassword(code) => {
            send_msg(&mut ws, &format!("badp={code}"));
            let _ = ws.close(None);
            return drain(&mut ws, rec);
        }
        MockSession::MsgRedirect(url) => {
            send_msg(&mut ws, &format!("redirect={}", protocol::percent_encode(url)));
            let _ = ws.close(None);
            return drain(&mut ws, rec);
        }
        MockSession::Stream { blocks, end } => (*blocks, *end),
        MockSession::HttpRedirect(_) | MockSession::NotFound => unreachable!("answered above"),
    };
    send_msg(&mut ws, &format!("audio_rate={}", cfg.audio_rate));
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => {
                record(&t);
                if t.starts_with("SET AR OK") {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => return,
        }
    }
    send_msg(&mut ws, &format!("sample_rate={:.3}", cfg.sample_rate));
    // From here on, poll for commands between blocks.
    let _ = ws.get_mut().set_read_timeout(Some(Duration::from_millis(1)));
    let started = Instant::now();
    let (mut seq, mut pos, mut sent) = (0u32, 0usize, 0usize);
    loop {
        if halt.load(Ordering::SeqCst) {
            return;
        }
        loop {
            match ws.read() {
                Ok(Message::Text(t)) => record(&t),
                Ok(Message::Close(_)) => return drain(&mut ws, rec),
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
                Err(_) => return,
            }
        }
        if blocks.is_some_and(|n| seq as usize >= n) {
            break;
        }
        let block: Vec<(i16, i16)> = (0..cfg.block)
            .map(|_| {
                let v = cfg.iq.get(pos % cfg.iq.len().max(1)).copied().unwrap_or((0, 0));
                pos += 1;
                v
            })
            .collect();
        if ws.send(Message::binary(protocol::snd_message(seq, cfg.rssi_dbm, &block, 0))).is_err() {
            return;
        }
        seq += 1;
        sent += cfg.block;
        if cfg.paced {
            let due = started + Duration::from_secs_f64(sent as f64 / cfg.sample_rate);
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
    }
    match end {
        // Dropping the socket without a close frame looks like a lost connection.
        MockEnd::Drop => drop(ws),
        MockEnd::Close => {
            let _ = ws.close(None);
            drain(&mut ws, rec);
        }
        MockEnd::Wait => drain(&mut ws, rec),
    }
}
