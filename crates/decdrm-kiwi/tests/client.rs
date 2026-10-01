//! The client against the stand-in KiwiSDR ([`decdrm_kiwi::mock`]).

use decdrm_kiwi::mock::{MockConfig, MockEnd, MockKiwi, MockSession};
use decdrm_kiwi::{KiwiAddress, KiwiConfig, KiwiState, KiwiStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

const BLOCK: usize = 512;

fn config_for(address: &str) -> KiwiConfig {
    let mut c = KiwiConfig::new(KiwiAddress::parse(address).unwrap(), 6140.0);
    c.reconnect_delay = Duration::from_millis(100);
    c
}

fn mock(sessions: Vec<MockSession>, iq: Vec<(i16, i16)>) -> MockKiwi {
    MockKiwi::start(MockConfig { iq: Arc::new(iq), sessions, ..MockConfig::default() }).unwrap()
}

/// A complex 1 kHz tone at 12 kHz.
fn tone(n: usize) -> Vec<(i16, i16)> {
    (0..n)
        .map(|k| {
            let ph = 2.0 * std::f64::consts::PI * 1000.0 * k as f64 / 12_000.0;
            ((ph.cos() * 16_000.0) as i16, (ph.sin() * 16_000.0) as i16)
        })
        .collect()
}

/// Read until `frames` I/Q frames arrived, the stream ended (its error) or `limit` passed.
fn read_frames(s: &KiwiStream, frames: usize, limit: Duration) -> (Vec<f32>, Option<String>) {
    let deadline = Instant::now() + limit;
    let mut got = Vec::new();
    while got.len() < 2 * frames && Instant::now() < deadline {
        match s.read_blocking(frames - got.len() / 2, Duration::from_millis(200)) {
            Ok(v) => got.extend(v),
            Err(e) => return (got, Some(e.to_string())),
        }
    }
    (got, None)
}

fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    done()
}

#[test]
fn streams_iq_and_sets_up_the_receiver() {
    let iq = tone(12_000);
    let kiwi = mock(vec![MockSession::Stream { blocks: Some(25), end: MockEnd::Wait }], iq.clone());
    let s = KiwiStream::start(config_for(&kiwi.address()));
    let (got, err) = read_frames(&s, 24 * BLOCK, Duration::from_secs(10));
    assert_eq!(err, None);
    assert_eq!(got.len(), 2 * 24 * BLOCK, "25 blocks, the first one dropped");
    for (k, pair) in got.chunks(2).enumerate() {
        let (i, q) = iq[(BLOCK + k) % iq.len()];
        assert_eq!(pair, [f32::from(i) / 32768.0, f32::from(q) / 32768.0], "frame {k}");
    }
    let st = s.status();
    assert_eq!(st.state, KiwiState::Streaming);
    assert_eq!(st.sample_rate, Some(12_001.135));
    assert_eq!((st.name.as_deref(), st.location.as_deref()), (Some("Mock Kiwi"), Some("Test bench")));
    assert_eq!(st.version.as_deref(), Some("1.826"));
    assert!((st.rssi_dbm.unwrap() + 73.0).abs() < 0.05);
    assert_eq!(st.samples, (24 * BLOCK) as u64);
    let log = s.take_log();
    assert!(log.iter().any(|l| l.contains("tuned to 6140.000 kHz")), "{log:?}");
    assert!(log.iter().any(|l| l.contains("\"Mock Kiwi\", Test bench")), "{log:?}");
    s.stop_and_join();

    let cmds = kiwi.commands();
    assert_eq!(cmds[0], "SET auth t=kiwi p=");
    for expected in [
        "SET AR OK in=12000 out=48000",
        "SET ident_user=DecDRM",
        "SET mod=iq low_cut=-5000 high_cut=5000 freq=6140.000",
        "SET agc=1 hang=0 thresh=-100 slope=6 decay=1000 manGain=50",
        "SET compression=0",
    ] {
        assert!(cmds.iter().any(|c| c == expected), "missing {expected:?} in {cmds:?}");
    }
    let path = &kiwi.paths()[0];
    let ts = path.strip_prefix('/').and_then(|p| p.strip_suffix("/SND")).unwrap_or_else(|| panic!("path {path}"));
    assert!(ts.parse::<u32>().is_ok(), "path {path}");
}

#[test]
fn keepalive_about_once_a_second() {
    let kiwi = MockKiwi::start(MockConfig { paced: true, ..MockConfig::default() }).unwrap();
    let s = KiwiStream::start(config_for(&kiwi.address()));
    let (got, err) = read_frames(&s, 30_000, Duration::from_secs(10));
    assert_eq!(err, None);
    assert_eq!(got.len(), 60_000, "2.5 s of signal");
    s.stop_and_join();
    let keepalives = kiwi.commands().iter().filter(|c| *c == "SET keepalive").count();
    assert!((3..=5).contains(&keepalives), "{keepalives} keepalives in 2.5 s (one with the set-up)");
}

#[test]
fn busy_refused_or_missing_kiwis_are_not_retried() {
    for (session, expect) in [
        (MockSession::TooBusy(8), "all 8 channels"),
        (MockSession::BadPassword(1), "wrong password"),
        (MockSession::NotFound, "HTTP 404"),
    ] {
        let kiwi = mock(vec![session], Vec::new());
        let s = KiwiStream::start(config_for(&kiwi.address()));
        let (got, err) = read_frames(&s, 1, Duration::from_secs(10));
        assert!(got.is_empty());
        let err = err.expect("the stream ends");
        assert!(err.contains(expect), "{err}");
        assert_eq!(s.status().state, KiwiState::Failed);
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(kiwi.connections(), 1, "{expect}: not asked again");
    }
}

#[test]
fn reconnects_after_a_lost_connection() {
    let kiwi = mock(
        vec![MockSession::Stream { blocks: Some(10), end: MockEnd::Drop }, MockSession::Stream { blocks: None, end: MockEnd::Wait }],
        tone(1200),
    );
    let s = KiwiStream::start(config_for(&kiwi.address()));
    // 9 blocks from the first connection, then more from the second.
    let (got, err) = read_frames(&s, 9 * BLOCK + 4 * BLOCK, Duration::from_secs(15));
    assert_eq!(err, None);
    assert_eq!(got.len(), 2 * 13 * BLOCK);
    assert_eq!(kiwi.connections(), 2);
    let st = s.status();
    assert_eq!((st.state, st.reconnects), (KiwiState::Streaming, 1));
    let log = s.take_log();
    assert!(log.iter().any(|l| l.contains("connection lost") && l.contains("reconnecting")), "{log:?}");
    s.stop_and_join();
}

#[test]
fn a_clean_close_is_not_retried() {
    let kiwi = mock(vec![MockSession::Stream { blocks: Some(10), end: MockEnd::Close }], tone(1200));
    let s = KiwiStream::start(config_for(&kiwi.address()));
    let (got, err) = read_frames(&s, 100 * BLOCK, Duration::from_secs(10));
    assert_eq!(got.len(), 2 * 9 * BLOCK, "everything that arrived is still read");
    assert!(err.expect("ended").contains("closed the connection"));
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(kiwi.connections(), 1);
    assert_eq!(s.status().state, KiwiState::Failed);
}

#[test]
fn follows_http_and_kiwi_redirections() {
    let target = mock(vec![MockSession::Stream { blocks: None, end: MockEnd::Wait }], tone(1200));
    for session in [
        MockSession::HttpRedirect(format!("http://{}/", target.address())),
        MockSession::MsgRedirect(format!("http://{}", target.address())),
    ] {
        let front = mock(vec![session], Vec::new());
        let s = KiwiStream::start(config_for(&front.address()));
        let (got, err) = read_frames(&s, 4 * BLOCK, Duration::from_secs(10));
        assert_eq!((got.len(), err), (2 * 4 * BLOCK, None));
        assert_eq!(s.status().address, target.address());
        assert!(s.take_log().iter().any(|l| l.contains("redirected to")));
        s.stop_and_join();
    }
    assert!(wait_for(Duration::from_secs(2), || target.connections() == 2));
}

#[test]
fn stopping_ends_the_connection_at_once() {
    let kiwi = MockKiwi::start(MockConfig { paced: true, ..MockConfig::default() }).unwrap();
    let s = KiwiStream::start(config_for(&kiwi.address()));
    let (got, _) = read_frames(&s, 2000, Duration::from_secs(10));
    assert_eq!(got.len(), 4000);
    let t = Instant::now();
    s.stop_and_join();
    assert!(t.elapsed() < Duration::from_secs(1), "stopped after {:?}", t.elapsed());
}

#[test]
fn an_unreachable_address_fails_without_retrying() {
    // A port nobody listens on any more.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let s = KiwiStream::start(config_for(&format!("127.0.0.1:{port}")));
    let (_, err) = read_frames(&s, 1, Duration::from_secs(15));
    assert!(err.expect("fails").contains("cannot connect"));
    assert_eq!(s.status().reconnects, 0);
}
