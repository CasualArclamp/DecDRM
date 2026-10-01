//! Print a KiwiSDR's WebSocket handshake and first messages (protocol debugging):
//! `cargo run -p decdrm-kiwi --example probe -- HOST [PORT] [PATH] [MESSAGE...]`, where
//! PATH may contain `{ts}` (default `/{ts}/SND`) and the messages follow `SET auth`.

use std::net::TcpStream;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::Message;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let host = args.first().expect("HOST").clone();
    let port: u16 = args.get(1).map_or(8073, |p| p.parse().expect("port"));
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as u32;
    let path = args.get(2).map_or("/{ts}/SND", String::as_str).replace("{ts}", &ts.to_string());
    let stream = TcpStream::connect((host.as_str(), port)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let url = format!("ws://{host}:{port}{path}");
    println!("GET {url}");
    let (mut ws, resp) = match tungstenite::client(url.as_str(), stream) {
        Ok(v) => v,
        Err(e) => return println!("handshake failed: {e:?}"),
    };
    println!("{:?} {}", resp.version(), resp.status());
    for (k, v) in resp.headers() {
        println!("  {k}: {v:?}");
    }
    ws.send(Message::text("SET auth t=kiwi p=")).unwrap();
    for m in args.iter().skip(3) {
        println!("send {m}");
        ws.send(Message::text(m.as_str())).unwrap();
    }
    let t0 = Instant::now();
    for _ in 0..30 {
        match ws.read() {
            Ok(m) => {
                let kind = match &m {
                    Message::Text(_) => "text",
                    Message::Binary(_) => "binary",
                    Message::Ping(_) => "ping",
                    Message::Pong(_) => "pong",
                    Message::Close(_) => "close",
                    Message::Frame(_) => "frame",
                };
                let data = m.into_data();
                let shown: String = String::from_utf8_lossy(&data[..data.len().min(150)]).chars().map(|c| if c.is_control() { '.' } else { c }).collect();
                println!("{:6.2}s {kind} {} bytes: {shown}", t0.elapsed().as_secs_f64(), data.len());
            }
            Err(e) => {
                println!("{:6.2}s read error: {e:?}", t0.elapsed().as_secs_f64());
                break;
            }
        }
    }
}
