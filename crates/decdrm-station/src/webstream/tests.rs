//! Web stream tests against servers on 127.0.0.1 (no internet): streams made here from
//! tones (`content`), served with ICY metadata, playlists, redirects, chunked coding,
//! TLS, dropped connections and errors (`server`).

mod content;
mod server;

use super::*;
use content::*;
use decdrm_codecs::AacProfile;
use server::{Server, Stream, body, redirect, silent};

/// Test settings: short timeouts, quick reconnects.
fn options(url: &str, rate: u32, channels: usize) -> WebStreamOptions {
    WebStreamOptions {
        connect_timeout: Duration::from_secs(3),
        io_timeout: Duration::from_secs(3),
        open_timeout: Duration::from_secs(10),
        backoff: vec![Duration::from_millis(50)],
        ..WebStreamOptions::new(url, rate, channels)
    }
}

/// `reads` station reads of 400 ms each.
fn read(src: &mut WebStreamSource, reads: usize) -> Vec<f32> {
    let frames = src.out_rate as usize * 2 / 5;
    let mut out = Vec::new();
    for _ in 0..reads {
        src.read(frames, &mut out).unwrap();
    }
    out
}

/// The strongest tone between `lo` and `hi` Hz (1 Hz steps) in the last second of
/// channel `ch`: its frequency, its share of that second's energy, its amplitude.
fn dominant_tone(x: &[f32], channels: usize, ch: usize, rate: u32, lo: f64, hi: f64) -> (f64, f64, f64) {
    let n = rate as usize;
    let x: Vec<f64> = x.chunks_exact(channels).map(|c| f64::from(c[ch])).collect();
    assert!(x.len() >= n, "only {} samples", x.len());
    let seg = &x[x.len() - n..];
    let power = |f: f64| {
        let w = std::f64::consts::TAU * f / f64::from(rate);
        let (c, mut s1, mut s2) = (2.0 * w.cos(), 0.0, 0.0);
        for &v in seg {
            let s0 = v + c * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        s1 * s1 + s2 * s2 - c * s1 * s2
    };
    let (mut best, mut best_p) = (lo, 0.0);
    let mut f = lo;
    while f <= hi {
        let p = power(f);
        if p > best_p {
            (best, best_p) = (f, p);
        }
        f += 1.0;
    }
    let energy: f64 = seg.iter().map(|v| v * v).sum::<f64>().max(1e-30);
    (best, 2.0 * best_p / n as f64 / energy, (4.0 * best_p).sqrt() / n as f64)
}

fn logged(lines: &[String], what: &str) -> bool {
    lines.iter().any(|l| l.contains(what))
}

/// AAC-LC, HE-AAC and HE-AAC v2 in ADTS with ICY metadata: the tone survives (mixed to
/// mono, resampled to 24 kHz), the status names the coding and the station, and the
/// titles go on air with their audio.
#[test]
fn aac_streams_with_icy_titles() {
    for (profile, rate, channels, bitrate, codec) in [
        (AacProfile::Lc, 48_000, 1, 64_000, "AAC-LC"),
        (AacProfile::HeAac, 44_100, 2, 48_000, "HE-AAC"),
        (AacProfile::HeAacV2, 48_000, 2, 32_000, "HE-AAC v2"),
    ] {
        let data = adts(profile, rate, channels, bitrate, &tone(rate, channels, 1000.0, 0.3, 4.0));
        let half = data.len() / 2;
        let mut s = Stream::new("audio/aacp", data);
        s.headers.push(("icy-name", "Test Radio".into()));
        s.headers.push(("icy-genre", "Tones".into()));
        s.headers.push(("icy-br", format!("{}", bitrate / 1000)));
        s.metaint = Some(1000);
        s.titles = vec![(0, "Artist - First".into()), (half, "Artist - Second".into())];
        s.repeat = true;
        let server = Server::start(vec![("/live", s.handler())]);
        let mut src = WebStreamSource::open(options(&server.url("/live"), 24_000, 1)).unwrap();
        let out = read(&mut src, 4);
        assert_eq!(src.title().as_deref(), Some("Artist - First"), "{codec}: 1.6 s in");
        let out = [out, read(&mut src, 5)].concat();
        let st = src.status();
        assert_eq!(st.codec.as_deref(), Some(codec));
        assert_eq!((st.station_name.as_deref(), st.genre.as_deref()), (Some("Test Radio"), Some("Tones")));
        assert_eq!((st.bitrate, st.sample_rate, st.channels), (Some(bitrate), Some(rate), Some(channels as u16)));
        assert_eq!(st.state, WebStreamState::Playing);
        assert_eq!(src.title().as_deref(), Some("Artist - Second"), "{codec}: 3.6 s in");
        let (f, share, amp) = dominant_tone(&out, 1, 0, 24_000, 800.0, 1200.0);
        assert!(f == 1000.0 && share > 0.9 && (amp - 0.3).abs() < 0.04, "{codec}: {f} Hz, {share:.3}, {amp:.3}");
        let log = src.take_log();
        assert!(logged(&log, "connected to") && logged(&log, "title: Artist - First") && logged(&log, "title: Artist - Second"), "{log:?}");
        assert!(src.describe().contains("playing"), "{}", src.describe());
    }
}

/// MP3 from a SHOUTCAST v1 server (`ICY 200 OK`): the coded spectral line comes out as
/// a tone at its frequency.
#[test]
fn mp3_from_a_shoutcast_server() {
    let line = 44; // the middle of subband 2: no alias butterflies
    let mut s = Stream::new("audio/mpeg", mp3(line, 202, 250));
    s.status = "ICY 200 OK";
    s.metaint = Some(8192);
    s.titles = vec![(0, "Shout - Cast".into())];
    let server = Server::start(vec![("/;stream.mp3", s.handler())]);
    let mut src = WebStreamSource::open(options(&server.url("/;stream.mp3"), 48_000, 1)).unwrap();
    let out = read(&mut src, 8);
    let st = src.status();
    assert_eq!((st.codec.as_deref(), st.bitrate, st.sample_rate, st.channels), (Some("MP3"), Some(64_000), Some(48_000), Some(1)));
    assert_eq!(src.title().as_deref(), Some("Shout - Cast"));
    let want = mp3_line_hz(line, 48_000);
    let (f, share, amp) = dominant_tone(&out, 1, 0, 48_000, want - 300.0, want + 300.0);
    println!("MP3 line {line}: {f} Hz (bin centre {want:.1} Hz), {share:.3} of the energy, amplitude {amp:.3}");
    assert!((f - want).abs() < 25.0 && share > 0.8, "{f} Hz, {share:.3}");
    assert_eq!(st.decode_errors, 0);
}

/// Ogg Opus as Icecast chains it: a new logical stream with new comments at a track
/// change; the title and the tone follow.
#[test]
fn chained_ogg_opus_changes_the_title() {
    let a = ogg_opus(&tone(48_000, 2, 600.0, 0.3, 2.0), 2, 11, &["ARTIST=One", "TITLE=First"]);
    let b = ogg_opus(&tone(48_000, 2, 900.0, 0.3, 2.4), 2, 12, &["ARTIST=Two", "TITLE=Second"]);
    let server = Server::start(vec![("/opus", Stream::new("application/ogg", [a, b].concat()).handler())]);
    let mut src = WebStreamSource::open(options(&server.url("/opus"), 48_000, 2)).unwrap();
    let first = read(&mut src, 4);
    assert_eq!(src.title().as_deref(), Some("One - First"));
    let (f, share, _) = dominant_tone(&first, 2, 1, 48_000, 400.0, 1100.0);
    assert!(f == 600.0 && share > 0.9, "first link: {f} Hz, {share:.3}");
    let second = read(&mut src, 6);
    assert_eq!(src.title().as_deref(), Some("Two - Second"));
    let (f, share, amp) = dominant_tone(&second, 2, 0, 48_000, 400.0, 1100.0);
    assert!(f == 900.0 && share > 0.9 && (amp - 0.3).abs() < 0.03, "second link: {f} Hz, {share:.3}, {amp:.3}");
    assert_eq!(src.status().codec.as_deref(), Some("Ogg Opus"));
}

/// Ogg Vorbis (a minimal stream made here: one coded MDCT bin on a flat floor), chained
/// with new comments: the bins come out as tones at their frequencies, the titles
/// follow, and the nominal bit rate comes from the identification header.
#[test]
fn chained_ogg_vorbis() {
    let a = ogg_vorbis(5, 230, 2.0, 21, &["ARTIST=Vor", "TITLE=Bis"]);
    let b = ogg_vorbis(9, 230, 2.4, 22, &["TITLE=Second"]);
    let server = Server::start(vec![("/vorbis", Stream::new("application/ogg", [a, b].concat()).handler())]);
    let mut src = WebStreamSource::open(options(&server.url("/vorbis"), 48_000, 1)).unwrap();
    let first = read(&mut src, 4);
    assert_eq!(src.title().as_deref(), Some("Vor - Bis"));
    let st = src.status();
    assert_eq!((st.codec.as_deref(), st.bitrate, st.sample_rate, st.channels), (Some("Ogg Vorbis mono"), Some(64_000), Some(48_000), Some(1)));
    let check = |out: &[f32], bin: usize| {
        let want = vorbis_bin_hz(bin, 48_000);
        let (f, share, amp) = dominant_tone(out, 1, 0, 48_000, want - 400.0, want + 400.0);
        println!("Vorbis bin {bin}: {f} Hz (bin centre {want:.2} Hz), {share:.3} of the energy, amplitude {amp:.3}");
        assert!((f - want).abs() < 30.0 && share > 0.7 && amp > 0.01, "bin {bin}: {f} Hz, {share:.3}, {amp:.3}");
    };
    check(&first, 5);
    let second = read(&mut src, 6);
    assert_eq!(src.title().as_deref(), Some("Second"));
    check(&second, 9);
    assert_eq!(src.status().decode_errors, 0);
}

/// FLAC: native with its header, native joined mid-way (no header), and in Ogg.
#[test]
fn flac_streams() {
    let pcm = tone(44_100, 2, 1000.0, 0.3, 3.0);
    let server = Server::start(vec![
        ("/native", Stream::new("audio/flac", flac(&pcm, 2, 44_100)).handler()),
        ("/joined", Stream::new("audio/flac", flac_frames_only(&pcm, 2, 44_100)).handler()),
        ("/ogg", Stream::new("audio/ogg", ogg_flac(&pcm, 2, 44_100, 5, &["TITLE=Lossless"])).handler()),
    ]);
    for (path, codec) in [("/native", "FLAC"), ("/joined", "FLAC"), ("/ogg", "Ogg FLAC")] {
        let mut src = WebStreamSource::open(options(&server.url(path), 24_000, 1)).unwrap();
        let out = read(&mut src, 6);
        let (f, share, amp) = dominant_tone(&out, 1, 0, 24_000, 800.0, 1200.0);
        assert!(f == 1000.0 && share > 0.95 && (amp - 0.3).abs() < 0.01, "{path}: {f} Hz, {share:.3}, {amp:.3}");
        assert_eq!(src.status().codec.as_deref(), Some(codec), "{path}");
        assert_eq!(src.status().sample_rate, Some(44_100), "{path}");
        if path == "/ogg" {
            assert_eq!(src.title().as_deref(), Some("Lossless"));
        }
    }
}

/// Playlists (M3U with a relative entry, PLS) and redirects lead to the stream; HLS is
/// refused.
#[test]
fn playlists_and_redirects() {
    let data = adts(AacProfile::Lc, 48_000, 1, 64_000, &tone(48_000, 1, 1000.0, 0.3, 3.0));
    let mut routes = vec![
        ("/live", Stream::new("audio/aac", data).handler()),
        ("/go", redirect("302 Found", "/live")),
        ("/lists/radio.m3u", body("200 OK", "audio/x-mpegurl", "#EXTM3U\r\n#EXTINF:-1,Test\r\n../go\r\n")),
        ("/hls.m3u8", body("200 OK", "application/vnd.apple.mpegurl", "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\nseg1.aac\n")),
        ("/moved", redirect("301 Moved Permanently", "/radio.pls")),
    ];
    // The PLS names the stream by its absolute URL: the port is known after starting,
    // so it is served by a handler that reads it from the request.
    routes.push((
        "/radio.pls",
        std::sync::Arc::new(|req: &server::Req, out: &mut dyn std::io::Write| {
            let host = req.header("host").unwrap_or_default().to_string();
            let text = format!("[playlist]\nNumberOfEntries=1\nFile1=http://{host}/live\nTitle1=Test\n");
            write!(out, "HTTP/1.1 200 OK\r\nContent-Type: audio/x-scpls\r\nContent-Length: {}\r\n\r\n{text}", text.len())
        }),
    ));
    let server = Server::start(routes);
    let mut src = WebStreamSource::open(options(&server.url("/lists/radio.m3u"), 24_000, 1)).unwrap();
    read(&mut src, 2);
    let log = src.take_log();
    assert!(logged(&log, "playlist") && logged(&log, "redirected to"), "{log:?}");
    assert_eq!(src.status().stream_url, Some(server.url("/live")));
    drop(src);
    let mut src = WebStreamSource::open(options(&server.url("/moved"), 24_000, 1)).unwrap();
    read(&mut src, 2);
    assert_eq!(src.status().stream_url, Some(server.url("/live")));
    let err = WebStreamSource::open(options(&server.url("/hls.m3u8"), 24_000, 1)).err().unwrap();
    assert!(err.contains("HLS"), "{err}");
}

/// Chunked transfer coding (HTTP/1.1 servers and proxies).
#[test]
fn chunked_stream() {
    let mut s = Stream::new("audio/mpeg", mp3(44, 202, 100));
    s.status = "HTTP/1.1 200 OK";
    s.chunked = true;
    let server = Server::start(vec![("/chunked", s.handler())]);
    let mut src = WebStreamSource::open(options(&server.url("/chunked"), 48_000, 1)).unwrap();
    let out = read(&mut src, 4);
    let (f, _, _) = dominant_tone(&out, 1, 0, 48_000, 1500.0, 2200.0);
    assert!((f - mp3_line_hz(44, 48_000)).abs() < 25.0, "{f} Hz");
}

/// The server drops the first connection mid-stream: silence, a reconnection, and the
/// audio goes on.
#[test]
fn reconnects_after_a_dropped_connection() {
    let data = adts(AacProfile::Lc, 48_000, 1, 64_000, &tone(48_000, 1, 1000.0, 0.3, 3.0));
    let mut s = Stream::new("audio/aac", data.clone());
    s.drop_after = Some(data.len() / 3);
    s.repeat = true;
    let server = Server::start(vec![("/live", s.handler())]);
    let mut src = WebStreamSource::open(options(&server.url("/live"), 24_000, 1)).unwrap();
    let out = read(&mut src, 8);
    let st = src.status();
    assert_eq!(st.reconnects, 1, "{st:?}");
    assert_eq!(server.hits("/live"), 2);
    let log = src.take_log();
    assert!(logged(&log, "the server ended the stream; reconnecting") && logged(&log, "reconnected to"), "{log:?}");
    let (f, share, _) = dominant_tone(&out, 1, 0, 24_000, 800.0, 1200.0);
    assert!(f == 1000.0 && share > 0.9, "after the reconnection: {f} Hz, {share:.3}");
}

/// A wrong URL, HTTP errors, unsupported or non-audio content and a silent server give
/// clear errors when opening, without hanging.
#[test]
fn clear_errors() {
    let mut speex = super::ogg::tests::OggWriter::new(3);
    speex.page(&[b"Speex   1.2\0\0\0\0\0\0\0\0\0"], 0, super::ogg::BOS);
    for i in 0..80 {
        speex.page(&[&[i as u8; 40]], 0, 0);
    }
    let server = Server::start(vec![
        ("/mp4", body("200 OK", "audio/mp4", b"\0\0\0\x20ftypM4A \0\0\0\0".to_vec())),
        ("/page", body("200 OK", "text/html; charset=utf-8", "<!DOCTYPE html><html><body>Listen live!</body></html>")),
        ("/speex", Stream::new("audio/ogg", speex.out).handler()),
        ("/nothing", Stream::new("audio/mpeg", vec![0x55; 600 * 1024]).handler()),
        ("/silent", silent()),
        ("/empty", body("200 OK", "audio/mpeg", Vec::new())),
    ]);
    let refused = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://127.0.0.1:{}/x", l.local_addr().unwrap().port())
    };
    let cases: [(String, &str); 9] = [
        (server.url("/missing"), "HTTP 404"),
        (server.url("/mp4"), "unsupported stream type audio/mp4"),
        (server.url("/page"), "web page"),
        (server.url("/speex"), "Speex"),
        (server.url("/nothing"), "MPEG audio: no frames found"),
        (server.url("/empty"), "ended the stream before any audio"),
        (refused, "cannot connect"),
        ("ftp://example.com/x".into(), "not an http:// or https:// URL"),
        (server.url("/silent"), "no data from the server"),
    ];
    for (url, want) in cases {
        let started = Instant::now();
        let err = WebStreamSource::open(options(&url, 24_000, 1)).err().unwrap_or_else(|| panic!("{url} opened"));
        assert!(err.contains(want), "{url}: \"{err}\" does not say \"{want}\"");
        assert!(started.elapsed() < Duration::from_secs(5), "{url}: {:?}", started.elapsed());
    }
    // A shorter open timeout than the server's silence.
    let opts = WebStreamOptions { open_timeout: Duration::from_millis(500), ..options(&server.url("/silent"), 24_000, 1) };
    let err = WebStreamSource::open(opts).err().unwrap();
    assert!(err.contains("no audio from") && err.contains("within"), "{err}");
}

/// The station's stop request ends a wait for a connection at once.
#[test]
fn stop_ends_the_wait() {
    let server = Server::start(vec![("/silent", silent())]);
    let opts = WebStreamOptions { io_timeout: Duration::from_secs(30), open_timeout: Duration::from_secs(30), ..options(&server.url("/silent"), 24_000, 1) };
    let stop = opts.stop.clone();
    let started = Instant::now();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        stop.stop();
    });
    let err = WebStreamSource::open(opts).err().unwrap();
    t.join().unwrap();
    assert!(err.contains("stopped"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
}

/// A station whose web stream is still connecting can be stopped through a stop handle
/// made beforehand (a GUI's Stop button while `Station::new` waits).
#[test]
fn stopping_a_connecting_station() {
    let server = Server::start(vec![("/silent", silent())]);
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = crate::StationConfig {
        output: crate::OutputSettings { file: Some("out.wav".into()), ..Default::default() },
        base_dir: Some(dir.path().to_path_buf()),
        ..Default::default()
    };
    let mut service = crate::ServiceSettings::new("Web", 0x42);
    service.audio = Some(crate::AudioSettings::new(crate::Codec::HeAac, crate::AudioInputSettings::url(server.url("/silent"))));
    cfg.services.push(service);
    let plan = cfg.validate().unwrap();
    let stop = StopHandle::default();
    let t = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            stop.stop();
        })
    };
    let started = Instant::now();
    let err = crate::Station::with_plan_and_stop(cfg, plan, stop).err().unwrap().to_string();
    t.join().unwrap();
    assert!(err.contains("stopped while connecting"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    assert!(!dir.path().join("out.wav").exists(), "no output file is left behind");
}

/// With a file output the stream paces the station: reads wait for the audio, which
/// the server sends in real time after a 0.5 s burst.
#[test]
fn file_output_is_paced_by_the_stream() {
    let data = mp3(44, 202, 150); // 3.6 s, 8000 bytes/s
    let mut s = Stream::new("audio/mpeg", data);
    s.pace = Some(8000.0);
    s.burst = 4000;
    let server = Server::start(vec![("/live", s.handler())]);
    let mut src = WebStreamSource::open(options(&server.url("/live"), 48_000, 1)).unwrap();
    let started = Instant::now();
    let out = read(&mut src, 5);
    let elapsed = started.elapsed().as_secs_f64();
    // 2 s of audio, 0.5 s of it in the burst: about 1.5 s (the decoder lags a frame).
    assert!((1.2..2.5).contains(&elapsed), "{elapsed:.2} s for 2 s of audio");
    assert_eq!(src.underruns, 0);
    assert_eq!(out.len(), 5 * 19_200);
}

/// The whole station: a web stream (AAC with ICY titles) transmitted to a file and
/// received by DecDRM's receiver. The stream's titles arrive as text messages, first in
/// the cycle with the configured message, the new title replacing the old one; the
/// tone survives HE-AAC; the status and the log describe the stream.
#[test]
fn station_sends_the_stream_titles() {
    use decdrm_engine::{InputFormat, RealChannel, ReceiverConfig, Session, SessionEvent};
    let data = adts(AacProfile::Lc, 48_000, 1, 64_000, &tone(48_000, 1, 1000.0, 0.25, 12.0));
    let mut s = Stream::new("audio/aac", data);
    s.metaint = Some(2000);
    s.titles = vec![(0, "A - 1".into()), (40_000, "B - 2".into())]; // 5 s in
    s.headers.push(("icy-name", "Relay".into()));
    s.repeat = true;
    let server = Server::start(vec![("/live", s.handler())]);
    let dir = tempfile::tempdir().unwrap();
    let toml = format!(
        r#"
        [channel]
        mode = "B"
        occupancy = 3
        interleaving = "short"
        [output]
        file = "web.wav"
        [[service]]
        label = "Web Radio"
        id = 0xD0D0B1
        [service.audio]
        codec = "he-aac"
        text = ["Hi"]
        [service.audio.input]
        url = "{}"
        "#,
        server.url("/live")
    );
    let mut cfg = crate::StationConfig::from_toml_str(&toml).unwrap();
    cfg.base_dir = Some(dir.path().to_path_buf());
    let mut station = crate::Station::new(cfg).unwrap();
    station.run_frames(30).unwrap();
    let log = station.take_log();
    assert!(logged(&log, "service 0 (\"Web Radio\"): web stream: connected to"), "{log:?}");
    let audio = station.status().services[0].audio.clone().unwrap();
    let web = audio.web_stream.unwrap();
    assert_eq!((web.codec.as_deref(), web.station_name.as_deref(), web.state), (Some("AAC-LC"), Some("Relay"), WebStreamState::Playing));
    assert!(audio.input.contains("Relay"), "{}", audio.input);
    station.finish().unwrap();

    let mut reader = decdrm_io::FileReader::open(dir.path().join("web.wav")).unwrap();
    let mut session = Session::new(ReceiverConfig { input: InputFormat::Real(RealChannel::Mix), channels: 1, ..Default::default() });
    let (mut texts, mut pcm, mut rate) = (Vec::new(), Vec::new(), 0);
    while let Some(block) = reader.read(4800).unwrap() {
        for ev in session.push(&block) {
            match ev {
                SessionEvent::Text(Some(t)) => texts.push(t),
                SessionEvent::Audio(a) => {
                    rate = a.sample_rate;
                    pcm.extend(a.samples.chunks_exact(usize::from(a.channels)).map(|c| c[0]));
                }
                _ => {}
            }
        }
    }
    println!("received texts {texts:?}");
    let first = texts.iter().position(|t| t == "A - 1").expect("the first title");
    let second = texts.iter().position(|t| t == "B - 2").expect("the second title");
    assert!(first < second && texts.iter().any(|t| t == "Hi"), "{texts:?}");
    assert!(!texts[second..].iter().any(|t| t == "A - 1"), "the old title is gone: {texts:?}");
    let (f, share, _) = dominant_tone(&pcm, 1, 0, rate, 800.0, 1200.0);
    assert!(f == 1000.0 && share > 0.8, "received audio: {f} Hz, {share:.3}");
}

/// Test certificates (a throwaway CA and a certificate for 127.0.0.1 signed by it,
/// valid until 2126), made with OpenSSL for these tests only.
mod certs {
    pub const CA: &str = "-----BEGIN CERTIFICATE-----
MIIBmDCCAT+gAwIBAgIUMbuM9gUIxjGqRMytPElWyss/tM8wCgYIKoZIzj0EAwIw
GTEXMBUGA1UEAwwORGVjRFJNIHRlc3QgQ0EwIBcNMjYxMDAxMDE0OTMzWhgPMjEy
NjA5MDcwMTQ5MzNaMBkxFzAVBgNVBAMMDkRlY0RSTSB0ZXN0IENBMFkwEwYHKoZI
zj0CAQYIKoZIzj0DAQcDQgAEoY9Q70cOJCrZVRZ4uuaBQwHBimlWN5282huhdNV1
3bQoY9HxCLyCNrNk30xq+e51Yng7hZ9CiJUW5snpfSOGeaNjMGEwHQYDVR0OBBYE
FLZbq1Z/b+138WmutTRAIKfdkAxtMB8GA1UdIwQYMBaAFLZbq1Z/b+138WmutTRA
IKfdkAxtMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgEGMAoGCCqGSM49
BAMCA0cAMEQCIAyt05ZCVJctOUUhYOO7P4Flek3l/uYI6Fvp04tUGuKMAiB7CxkE
MQQe7BG4YyfrGgz0pAI1rNs68wfuOy6P1zz80Q==
-----END CERTIFICATE-----
";
    pub const SERVER: &str = "-----BEGIN CERTIFICATE-----
MIIBxTCCAWqgAwIBAgIUKL0MPx4tViortxebk32LaV/GTQ0wCgYIKoZIzj0EAwIw
GTEXMBUGA1UEAwwORGVjRFJNIHRlc3QgQ0EwIBcNMjYxMDAxMDE0OTQyWhgPMjEy
NjA5MDcwMTQ5NDJaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDBZMBMGByqGSM49AgEG
CCqGSM49AwEHA0IABIRNZOXAPqV2cfFWfB4+8H4GVkgQSykHq9yziye6s94n2KIh
cxZOCNwJ8GCsJlZ0msnQAUWfTizMx2pV4GvDnIejgZIwgY8wDAYDVR0TAQH/BAIw
ADAOBgNVHQ8BAf8EBAMCB4AwEwYDVR0lBAwwCgYIKwYBBQUHAwEwGgYDVR0RBBMw
EYcEfwAAAYIJbG9jYWxob3N0MB0GA1UdDgQWBBQObG5/GjWK8QwQNeuRQspxH7rS
RDAfBgNVHSMEGDAWgBS2W6tWf2/td/FprrU0QCCn3ZAMbTAKBggqhkjOPQQDAgNJ
ADBGAiEAjqrDcPuleNqSdy2ac82mrs7jLpL3w4a6QcZIZEjdRaACIQDlhcxw4D9I
m8afA0gsdLDDr29EKYtCSv04+L8i6l7jbQ==
-----END CERTIFICATE-----
";
    pub const SERVER_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgLO0LIaEQlAP25mf9
4kULji84QFrKmm/Dg8Ufx2RIwwihRANCAASETWTlwD6ldnHxVnwePvB+BlZIEEsp
B6vcs4snurPeJ9iiIXMWTgjcCfBgrCZWdJrJ0AFFn04szMdqVeBrw5yH
-----END PRIVATE KEY-----
";
}

/// HTTPS: a TLS server with a certificate for 127.0.0.1; the client trusts the test CA.
/// Without that trust the certificate is refused with a clear message.
#[test]
fn https_stream() {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = CertificateDer::from_pem_slice(certs::SERVER.as_bytes()).unwrap();
    let key = PrivateKeyDer::from_pem_slice(certs::SERVER_KEY.as_bytes()).unwrap();
    let server_config = Arc::new(
        rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap(),
    );
    let data = adts(AacProfile::HeAac, 48_000, 1, 32_000, &tone(48_000, 1, 1000.0, 0.3, 3.0));
    let server = Server::start_with(vec![("/secure", Stream::new("audio/aac", data).handler())], move |tcp| {
        let conn = rustls::ServerConnection::new(server_config.clone()).unwrap();
        Box::new(rustls::StreamOwned::new(conn, tcp)) as Box<dyn server::ReadWrite>
    });
    let url = format!("https://127.0.0.1:{}/secure", server.port);
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from_pem_slice(certs::CA.as_bytes()).unwrap()).unwrap();
    let opts = WebStreamOptions { tls: Some(http::tls_config(roots).unwrap()), ..options(&url, 24_000, 1) };
    let mut src = WebStreamSource::open(opts).unwrap();
    let out = read(&mut src, 4);
    let (f, share, _) = dominant_tone(&out, 1, 0, 24_000, 800.0, 1200.0);
    assert!(f == 1000.0 && share > 0.9, "{f} Hz, {share:.3}");
    assert_eq!(src.status().codec.as_deref(), Some("HE-AAC"));
    // An empty trust store: the server's certificate is refused.
    let opts = WebStreamOptions {
        tls: Some(http::tls_config(rustls::RootCertStore::empty()).unwrap()),
        ..options(&url, 24_000, 1)
    };
    let err = WebStreamSource::open(opts).err().unwrap();
    assert!(err.contains("TLS") && err.to_lowercase().contains("certificate"), "{err}");
}

/// With a sound-card output (the FIFO fed by hand, no worker): the start-up backlog,
/// the burst dropped beyond it, re-buffering after an underrun, skipping ahead when far
/// behind, and the clock trim once the output's queue is full.
#[test]
fn following_a_live_stream_clock() {
    let opts = WebStreamOptions { output_queue: Some(Duration::from_millis(800)), ..options("http://test/", 1000, 1) };
    let mut src = WebStreamSource::new(&opts);
    let push = |src: &WebStreamSource, seconds: f64| {
        let mut inner = src.shared.lock();
        inner.fifo.buf.extend(std::iter::repeat_n(0.5f32, (seconds * 1000.0) as usize));
        inner.written += (seconds * 1000.0) as u64;
        inner.status.state = WebStreamState::Playing;
    };
    let level = |src: &WebStreamSource| src.shared.lock().fifo.len() as f64 / 1000.0;
    // A 6 s burst: the first read keeps the start-up backlog 1.5 + 0.8 + 0.4 s.
    push(&src, 6.0);
    let mut out = Vec::new();
    src.read(400, &mut out).unwrap();
    assert!(out.iter().all(|&v| v == 0.5), "playing at once");
    assert!((level(&src) - 2.3).abs() < 0.01, "{}", level(&src));
    // This read and the next fill the output's queue (800 ms): no trim yet. The one
    // after measures the backlog: 1.5 s, on target.
    src.read(400, &mut out).unwrap();
    assert_eq!(f64::from_bits(src.shared.trim_ppm.load(Ordering::Relaxed)), 0.0);
    src.read(400, &mut out).unwrap();
    assert!((level(&src) - 1.5).abs() < 1e-9);
    assert!(src.clock.as_ref().unwrap().ppm().abs() < 1e-6);
    // Starve it: 1.1, 0.7, 0.3 s left, then an underrun and re-buffering (silence).
    for _ in 0..4 {
        src.read(400, &mut out).unwrap();
    }
    assert_eq!(src.underruns, 1);
    assert_eq!(src.status().state, WebStreamState::Buffering);
    out.clear();
    src.read(400, &mut out).unwrap();
    assert!(out.iter().all(|&v| v == 0.0), "silence while buffering");
    push(&src, 1.0);
    src.read(400, &mut out).unwrap();
    assert_eq!(src.status().state, WebStreamState::Buffering, "1.0 s is below the target");
    push(&src, 1.0);
    out.clear();
    src.read(400, &mut out).unwrap();
    assert!(out.iter().all(|&v| v == 0.5), "playing again at 2.0 s");
    assert_eq!(src.status().state, WebStreamState::Playing);
    // Far behind (a stall of the station): skip to the target.
    push(&src, 4.0);
    src.read(400, &mut out).unwrap();
    assert!((level(&src) - 1.5).abs() < 0.01, "{}", level(&src));
    let log = src.take_log();
    assert!(logged(&log, "ran dry") && logged(&log, "buffered, playing") && logged(&log, "skipped"), "{log:?}");
    // A growing backlog (the stream's clock fast) makes the trim negative.
    for _ in 0..20 {
        push(&src, 0.41);
        src.read(400, &mut out).unwrap();
    }
    assert!(src.clock.as_ref().unwrap().ppm() < -50.0, "{}", src.clock.as_ref().unwrap().ppm());
    assert!(f64::from_bits(src.shared.trim_ppm.load(Ordering::Relaxed)) < -50.0);
}

/// In real time, as with a sound-card output: a server that sends a 3 s burst and then
/// paces the stream; reads every 400 ms as a sound card would ask. The start-up backlog
/// is reached at once, the burst beyond it dropped, and the backlog then stays near its
/// target without underruns. (15 s.)
#[test]
#[ignore = "real time, 15 s"]
fn live_stream_with_a_sound_card_output() {
    let data = mp3(44, 202, 1000); // 24 s, 8000 bytes/s
    let mut s = Stream::new("audio/mpeg", data);
    s.pace = Some(8000.0);
    s.burst = 24_000;
    let server = Server::start(vec![("/live", s.handler())]);
    let opts = WebStreamOptions { output_queue: Some(Duration::from_millis(800)), ..options(&server.url("/live"), 24_000, 1) };
    let mut src = WebStreamSource::open(opts).unwrap();
    let mut out = Vec::new();
    let mut levels = Vec::new();
    // The first read waits for the start-up backlog; it and the next two fill the
    // output's queue at once; the card then plays, asking for 400 ms every 400 ms.
    src.read(9600, &mut out).unwrap();
    levels.push(src.status().buffer_s);
    let started = Instant::now();
    for i in 1..36u32 {
        let due = Duration::from_millis(400) * i.saturating_sub(2);
        if let Some(wait) = due.checked_sub(started.elapsed()) {
            std::thread::sleep(wait);
        }
        src.read(9600, &mut out).unwrap();
        levels.push(src.status().buffer_s);
    }
    println!("backlog after each read: {levels:.2?}");
    println!("{:?}", src.take_log());
    let st = src.status();
    assert_eq!((st.underruns, st.state), (0, WebStreamState::Playing), "{st:?}");
    assert!(levels[3..].iter().all(|l| (1.35..1.7).contains(l)), "on target: {levels:.2?}");
    assert!(src.drift_ppm().is_some_and(|p| p.abs() < 1000.0));
}

/// For trying the CLI or GUI by hand: serves an endless MP3 stream (a 1.85 kHz tone)
/// with ICY titles changing every 4 s at http://127.0.0.1:PORT/live (PORT from
/// `DECDRM_TEST_PORT`, default 8790) for `DECDRM_TEST_SECONDS` (default 60); the first
/// connection drops after 20 s to show a reconnection.
#[test]
#[ignore = "a server for manual tests"]
fn manual_test_server() {
    let env = |name: &str, default: u64| std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
    let mut s = Stream::new("audio/mpeg", mp3(44, 202, 2500)); // 60 s at 8000 bytes/s
    s.status = "ICY 200 OK";
    s.headers.push(("icy-name", "DecDRM test stream".into()));
    s.headers.push(("icy-br", "64".into()));
    s.metaint = Some(8000);
    s.titles = (0..15).map(|i| (i * 32_000, format!("Test Artist - Song {}", i + 1))).collect();
    s.pace = Some(8000.0);
    s.burst = 32_000;
    s.repeat = true;
    s.drop_after = Some(160_000);
    let server = Server::start_on(env("DECDRM_TEST_PORT", 8790) as u16, vec![("/live", s.handler())], |s| Box::new(s) as Box<dyn server::ReadWrite>);
    println!("serving {}", server.url("/live"));
    std::thread::sleep(Duration::from_secs(env("DECDRM_TEST_SECONDS", 60)));
}

/// A file (a response with a length) is neither trimmed nor skipped.
#[test]
fn a_file_has_no_clock() {
    let opts = WebStreamOptions { output_queue: Some(Duration::from_millis(800)), ..options("http://test/", 1000, 1) };
    let mut src = WebStreamSource::new(&opts);
    {
        let mut inner = src.shared.lock();
        inner.on_demand = true;
        inner.fifo.buf.extend(std::iter::repeat_n(0.5f32, 6000));
        inner.written = 6000;
    }
    let mut out = Vec::new();
    for _ in 0..5 {
        src.read(400, &mut out).unwrap();
    }
    assert_eq!(src.shared.lock().fifo.len(), 4000, "nothing dropped");
    assert_eq!(f64::from_bits(src.shared.trim_ppm.load(Ordering::Relaxed)), 0.0);
}

/// Titles go on air when the audio they arrived with is read.
#[test]
fn titles_follow_the_audio() {
    let opts = options("http://test/", 1000, 1);
    let mut src = WebStreamSource::new(&opts);
    {
        let mut inner = src.shared.lock();
        inner.status.state = WebStreamState::Playing;
        inner.titles.push_back((0, Some("First".into())));
        inner.titles.push_back((1000, Some("Second".into())));
        inner.titles.push_back((1500, None));
        inner.fifo.buf.extend(std::iter::repeat_n(0.1f32, 3000));
        inner.written = 3000;
    }
    let mut out = Vec::new();
    src.read(400, &mut out).unwrap();
    assert_eq!(src.title().as_deref(), Some("First"));
    src.read(400, &mut out).unwrap();
    assert_eq!(src.title().as_deref(), Some("First"), "800 samples read");
    src.read(400, &mut out).unwrap();
    assert_eq!(src.title().as_deref(), Some("Second"), "1200 samples read");
    src.read(400, &mut out).unwrap();
    assert_eq!(src.title(), None, "cleared at 1500");
    let log = src.take_log();
    assert!(logged(&log, "title: First") && logged(&log, "title: Second"), "{log:?}");
}
