//! Transmitter → packet stream → receiver round trips for every data application.

use decdrm_data::encoder::DataUnitSource;
use decdrm_data::epg::{self, EpgElement, EpgValue};
use decdrm_data::journaline::{
    JournalineBrowser, JournalineEncoder, ListItem, MenuItem, NmlBody, NmlObject,
};
use decdrm_data::mot::{MotEncoder, MotHeader, param};
use decdrm_data::slideshow::{Slide, SlideShow, SlideShowFeeder};
use decdrm_data::time::MotTime;
use decdrm_data::website::{self, Website};
use decdrm_data::{
    DataDecoder, DataEncoder, DataEvent, DataServiceConfig, RawSource, UserApplication,
};
use std::collections::HashMap;
use std::io::Write;

/// Deterministic xorshift PRNG for reproducible test data and channel errors.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

fn fake_jpeg(rng: &mut Rng, n: usize) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
    v.extend(rng.bytes(n));
    v
}

fn fake_png(rng: &mut Rng, n: usize) -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    v.extend(rng.bytes(n));
    v
}

/// Run `frames` frames from `tx` into `rx`, corrupting each packet with probability
/// `loss` (one flipped byte, which the packet CRC must catch).
fn run(
    tx: &mut DataEncoder,
    rx: &mut DataDecoder,
    frames: usize,
    stream_len: usize,
    loss: f64,
    rng: &mut Rng,
) -> Vec<DataEvent> {
    let packet_len = rx.config().packet_len.max(1);
    let mut events = Vec::new();
    for _ in 0..frames {
        let mut frame = tx.next_frame(stream_len);
        assert_eq!(frame.len(), stream_len);
        if loss > 0.0 {
            for p in frame.chunks_mut(packet_len) {
                if (rng.next() % 10_000) as f64 / 10_000.0 < loss {
                    let i = (rng.next() as usize) % p.len();
                    p[i] ^= 1 << (rng.next() % 8);
                }
            }
        }
        events.extend(
            rx.push_frame(&frame)
                .into_iter()
                .filter(|e| !matches!(e, DataEvent::Stats(_))),
        );
    }
    events
}

#[test]
fn slideshow_round_trip_with_model() {
    let mut rng = Rng(0x5EED);
    let images: Vec<(String, Vec<u8>)> = vec![
        ("studio.jpg".into(), fake_jpeg(&mut rng, 5000)),
        ("logo.png".into(), fake_png(&mut rng, 700)),
        ("weather.jpg".into(), fake_jpeg(&mut rng, 12_000)),
    ];
    let cfg = DataServiceConfig::packet(UserApplication::SlideShow, 0, 50);
    let mut feeder = SlideShowFeeder::new();
    for (name, data) in &images {
        feeder.add_image(name, data.clone()).unwrap();
    }
    let mut tx = DataEncoder::new(&cfg, Box::new(feeder)).unwrap();
    let mut rx = DataDecoder::new(cfg);
    let events = run(&mut tx, &mut rx, 120, 800, 0.0, &mut rng);
    let mut show = SlideShow::new(8);
    let mut received: HashMap<String, usize> = HashMap::new();
    for ev in &events {
        if let DataEvent::SlideShowImage {
            transport_id,
            name,
            mime,
            data,
            header,
        } = ev
        {
            let original = &images
                .iter()
                .find(|(n, _)| n == name)
                .expect("known name")
                .1;
            assert_eq!(data, original, "{name}");
            assert_eq!(
                mime,
                if name.ends_with(".png") {
                    "image/png"
                } else {
                    "image/jpeg"
                }
            );
            *received.entry(name.clone()).or_default() += 1;
            assert_eq!(
                Slide::from_mot(*transport_id, header.clone(), data.clone()).name,
                *name
            );
            assert!(show.apply(ev, None));
        }
    }
    // 120 frames x 800 bytes carry the ~18 kB cycle several times.
    assert!(
        images
            .iter()
            .all(|(n, _)| received.get(n).copied().unwrap_or(0) >= 2),
        "{received:?}"
    );
    assert_eq!(show.len(), 3); // same names replace each other
    assert_eq!(rx.stats().packets_crc_error, 0);
    assert_eq!(rx.stats().continuity_errors, 0);
}

#[test]
fn slideshow_survives_packet_errors() {
    let mut rng = Rng(42);
    let img = fake_jpeg(&mut rng, 3000);
    let cfg = DataServiceConfig::packet(UserApplication::SlideShow, 3, 60);
    let mut feeder = SlideShowFeeder::new();
    feeder.set_segment_size(256).unwrap();
    feeder.add_image("a.jpg", img.clone()).unwrap();
    let mut tx = DataEncoder::new(&cfg, Box::new(feeder)).unwrap();
    let mut rx = DataDecoder::new(cfg);
    let events = run(&mut tx, &mut rx, 150, 630, 0.01, &mut rng);
    let good: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            DataEvent::SlideShowImage { data, .. } => Some(data),
            _ => None,
        })
        .collect();
    assert!(!good.is_empty());
    assert!(
        good.iter().all(|d| **d == img),
        "a corrupted image got through"
    );
    let st = rx.stats();
    assert!(
        st.packets_crc_error > 0 && st.data_units_dropped > 0,
        "{st:?}"
    );
}

#[test]
fn website_round_trip_with_gzip_and_index() {
    let mut mot = MotEncoder::directory_mode();
    mot.set_segment_size(300).unwrap();
    mot.set_directory_index(0x01, "index.html");
    mot.add_file(
        "index.html",
        b"<html><body><a href=\"news/today.html\">news</a></body></html>".to_vec(),
    )
    .unwrap();
    mot.add_file("style.css", b"body { color: #123456; }".to_vec())
        .unwrap();
    // A gzip transport-compressed page (CompressionType = 1).
    let page = b"<p>Today: DRM data services work.</p>".repeat(30);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&page).unwrap();
    let mut header = MotHeader::for_file("news/today.html", 0);
    header.set_param(param::COMPRESSION_TYPE, vec![1]);
    mot.add_object(header, gz.finish().unwrap()).unwrap();

    let cfg = DataServiceConfig::packet(UserApplication::BroadcastWebsite, 1, 100);
    let mut tx = DataEncoder::new(&cfg, Box::new(mot)).unwrap();
    let mut rx = DataDecoder::new(cfg);
    let mut rng = Rng(7);
    let events = run(&mut tx, &mut rx, 20, 1030, 0.0, &mut rng);
    let mut site = Website::new();
    let index_events = events
        .iter()
        .filter(|e| matches!(e, DataEvent::WebsiteIndex { .. }))
        .count();
    assert_eq!(index_events, 1, "index signalled once");
    for ev in &events {
        site.apply(ev);
    }
    assert_eq!(site.len(), 3);
    assert_eq!(site.index(), Some("index.html"));
    assert_eq!(site.get("news/today.html").unwrap().data, page);
    assert_eq!(site.get("news/today.html").unwrap().mime, "text/html");
    assert_eq!(site.get("style.css").unwrap().mime, "text/css");
    assert_eq!(site.start_page().unwrap().0, "index.html");
    assert_eq!(rx.mot().unwrap().directory().unwrap().entries.len(), 3);
}

#[test]
fn journaline_round_trip_with_browser() {
    let mut enc = JournalineEncoder::new();
    enc.insert(NmlObject::menu(
        0x0000,
        "DecDRM News",
        vec![
            MenuItem::new(0x0101, "Headlines"),
            MenuItem::new(0x0102, "Results"),
            MenuItem::new(0x0199, "Soon"),
        ],
    ))
    .unwrap();
    enc.insert(NmlObject::plain_text(
        0x0101,
        "Headlines",
        "DRM data services decoded\nin pure Rust.",
    ))
    .unwrap();
    enc.insert(NmlObject::list(
        0x0102,
        "Results",
        vec![ListItem::row("A"), ListItem::cell("1"), ListItem::row("B")],
    ))
    .unwrap();
    enc.set_compression(true).unwrap();

    let cfg = DataServiceConfig::packet(UserApplication::Journaline, 2, 30);
    let mut tx = DataEncoder::new(&cfg, Box::new(enc)).unwrap();
    let mut rx = DataDecoder::new(cfg);
    let mut rng = Rng(9);
    let events = run(&mut tx, &mut rx, 10, 330, 0.0, &mut rng);
    let mut browser = JournalineBrowser::new();
    let mut updates = 0;
    for ev in &events {
        if let DataEvent::Journaline(u) = ev {
            browser.apply(u);
            updates += 1;
        }
    }
    assert_eq!(updates, 3, "each page reported once");
    assert_eq!(browser.root().unwrap().title, "DecDRM News");
    let entries = browser.menu_entries(0);
    assert_eq!(
        entries.iter().map(|e| e.available).collect::<Vec<_>>(),
        [true, true, false]
    );
    assert!(browser.follow(0));
    assert_eq!(
        browser.current().unwrap().body,
        NmlBody::PlainText("DRM data services decoded\nin pure Rust.".into())
    );
    assert!(browser.back());
    assert!(!browser.follow(2));
}

fn sample_schedule() -> EpgElement {
    let start = MotTime::from_ymd_hms(2026, 9, 29, 18, 0, 0);
    let prog = |id: u32, name: &str, t: MotTime, secs: u16| {
        EpgElement::new("programme")
            .attr("shortId", EpgValue::U24(id))
            .attr("version", EpgValue::U16(1))
            .child(EpgElement::new("mediumName").text(name))
            .child(
                EpgElement::new("location").child(
                    EpgElement::new("time")
                        .attr("time", EpgValue::Time(t))
                        .attr("duration", EpgValue::Duration(secs)),
                ),
            )
            .child(
                EpgElement::new("genre")
                    .attr_str("href", "urn:tva:metadata:cs:ContentCS:2005:3.1.1"),
            )
    };
    EpgElement::new("epg")
        .attr("system", EpgValue::Enum("DRM".into()))
        .child(
            EpgElement::new("schedule")
                .attr("version", EpgValue::U16(5))
                .child(EpgElement::new("scope").attr("startTime", EpgValue::Time(start)))
                .child(prog(1, "Evening News", start, 1800))
                .child(prog(
                    2,
                    "Jazz & Blues",
                    MotTime::from_unix(start.to_unix() + 1800),
                    3600,
                )),
        )
}

#[test]
fn epg_round_trip_through_mot_directory() {
    let tree = sample_schedule();
    let binary = epg::encode(&tree).unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&binary).unwrap();
    let start = MotTime::from_ymd_hms(2026, 9, 29, 0, 0, 0);
    let mut mot = MotEncoder::directory_mode();
    mot.add_object(
        epg::mot_header(1, "", Some(start), None, Some(0x00E1C2)),
        gz.finish().unwrap(),
    )
    .unwrap();
    // A logo travels in the same carousel and comes out as a plain MOT object.
    mot.add_file("logo.png", vec![0x89, b'P', b'N', b'G'])
        .unwrap();

    let cfg = DataServiceConfig::packet(UserApplication::Epg, 0, 80);
    let mut tx = DataEncoder::new(&cfg, Box::new(mot)).unwrap();
    let mut rx = DataDecoder::new(cfg);
    let mut rng = Rng(11);
    let events = run(&mut tx, &mut rx, 6, 830, 0.0, &mut rng);
    let epgs: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            DataEvent::Epg { name, xml } => Some((name.clone(), xml.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(epgs.len(), 1);
    assert_eq!(epgs[0].0, "20260929e1c2P.EHB");
    assert_eq!(epgs[0].1, tree.to_xml());
    assert!(
        epgs[0]
            .1
            .contains("<mediumName>Jazz &amp; Blues</mediumName>")
    );
    assert!(
        epgs[0]
            .1
            .contains(r#"<time time="2026-09-29T18:30:00Z" duration="PT1H"/>"#),
        "{}",
        epgs[0].1
    );
    assert!(events.iter().any(|e| matches!(e, DataEvent::MotObject { header, .. } if header.content_name().as_deref() == Some("logo.png"))));
}

#[test]
fn tpeg_raw_round_trip() {
    let mut src = RawSource::new();
    let frames: Vec<Vec<u8>> = (0..5u8)
        .map(|i| vec![i; 40 + usize::from(i) * 17])
        .collect();
    for f in &frames {
        src.push_general_data(f.clone());
    }
    let cfg = DataServiceConfig::packet(UserApplication::Tpeg, 1, 24);
    let mut tx = DataEncoder::new(&cfg, Box::new(src)).unwrap();
    let mut rx = DataDecoder::new(cfg);
    let mut rng = Rng(3);
    let events = run(&mut tx, &mut rx, 4, 270, 0.0, &mut rng);
    let raw: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            DataEvent::Raw {
                user_app_id: 4,
                data_group,
            } => Some(data_group.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(raw.len(), frames.len());
    for (unit, original) in raw.iter().zip(&frames) {
        assert_eq!(
            &decdrm_data::datagroup::DataGroup::parse(unit).unwrap().data,
            original
        );
    }
    // Once the queue is empty the stream is filled with padding packets.
    assert!(rx.stats().padding_packets > 0);
}

#[test]
fn two_services_share_one_stream() {
    let mut rng = Rng(99);
    let img = fake_png(&mut rng, 900);
    let mut feeder = SlideShowFeeder::new();
    feeder.add_image("shared.png", img.clone()).unwrap();
    let mut news = JournalineEncoder::new();
    news.insert(NmlObject::title_only(0, "Breaking")).unwrap();

    let slides_cfg = DataServiceConfig::packet(UserApplication::SlideShow, 0, 40);
    let news_cfg = DataServiceConfig {
        packet_id: 1,
        ..DataServiceConfig::packet(UserApplication::Journaline, 1, 40)
    };
    let mut tx = DataEncoder::new(&slides_cfg, Box::new(feeder)).unwrap();
    tx.add_packet_channel(1, true, Box::new(news)).unwrap();
    let mut rx_slides = DataDecoder::new(slides_cfg);
    let mut rx_news = DataDecoder::new(news_cfg);
    let (mut slides, mut pages) = (0, 0);
    // One slide transmission is ~26 packets and the two services alternate packets.
    for _ in 0..30 {
        let frame = tx.next_frame(430);
        for ev in rx_slides.push_frame(&frame) {
            match ev {
                DataEvent::SlideShowImage { data, .. } => {
                    assert_eq!(data, img);
                    slides += 1;
                }
                DataEvent::Journaline(_) => panic!("wrong service"),
                _ => {}
            }
        }
        for ev in rx_news.push_frame(&frame) {
            match ev {
                DataEvent::Journaline(u) => {
                    assert_eq!(u.object.title, "Breaking");
                    pages += 1;
                }
                DataEvent::SlideShowImage { .. } => panic!("wrong service"),
                _ => {}
            }
        }
    }
    assert!(slides > 1);
    assert_eq!(pages, 1);
}

#[test]
fn stream_mode_round_trip() {
    let cfg = DataServiceConfig::stream(UserApplication::Other(0x3FF));
    let mut src = RawSource::new();
    src.push_data_unit((0..=255).collect());
    let mut tx = DataEncoder::new(&cfg, Box::new(src)).unwrap();
    let mut rx = DataDecoder::new(cfg);
    let mut got = Vec::new();
    for _ in 0..3 {
        for ev in rx.push_frame(&tx.next_frame(100)) {
            if let DataEvent::StreamData { user_app_id, data } = ev {
                assert_eq!(user_app_id, 0x3FF);
                got.extend(data);
            }
        }
    }
    assert_eq!(&got[..256], (0..=255).collect::<Vec<u8>>().as_slice());
    assert!(got[256..].iter().all(|&b| b == 0));
}

#[test]
fn feeders_load_directories() {
    let dir = std::env::temp_dir().join(format!("decdrm-data-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("site/img")).unwrap();
    std::fs::create_dir_all(dir.join("slides")).unwrap();
    let mut rng = Rng(5);
    std::fs::write(dir.join("slides/b.png"), fake_png(&mut rng, 50)).unwrap();
    std::fs::write(dir.join("slides/a.jpg"), fake_jpeg(&mut rng, 50)).unwrap();
    std::fs::write(dir.join("slides/notes.txt"), b"ignored").unwrap();
    std::fs::write(dir.join("site/index.html"), b"<html/>").unwrap();
    std::fs::write(dir.join("site/img/x.png"), fake_png(&mut rng, 10)).unwrap();

    let mut feeder = SlideShowFeeder::from_dir(dir.join("slides")).unwrap();
    assert_eq!(feeder.len(), 2);
    let site = website::encoder_from_dir(dir.join("site"), Some("index.html")).unwrap();
    let names: Vec<_> = site
        .objects()
        .iter()
        .map(|o| o.header.content_name().unwrap())
        .collect();
    assert_eq!(names, ["img/x.png", "index.html"]);
    assert_eq!(site.directory().best_index().as_deref(), Some("index.html"));
    // The first slide in name order goes out first.
    let mut dec = decdrm_data::mot::MotDecoder::new();
    let first = std::iter::from_fn(|| feeder.next_data_unit())
        .take(50)
        .flat_map(|g| dec.push_data_unit(&g))
        .find_map(|o| match o {
            decdrm_data::mot::MotOutput::Object(obj) => Some(obj.name()),
            _ => None,
        });
    assert_eq!(first.as_deref(), Some("a.jpg"));
    let _ = std::fs::remove_dir_all(&dir);
}
