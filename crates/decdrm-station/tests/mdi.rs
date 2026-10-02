//! MDI and RSCI input end to end: a station's frames as MDI (what a content server
//! sends, `Station::capture_mdi`) go to the engine — from a recording, and over UDP
//! with PFT fragmentation, Reed–Solomon protection, a lost fragment per packet and an
//! RSCI receiver's status items — and the engine decodes the audio, the text and the
//! services without any radio part; RCI commands go back to the "receiver".

use decdrm_engine::decdrm_mdi::file::{DcpFileWriter, FileKind};
use decdrm_engine::decdrm_mdi::net::{UdpOrigin, UdpSender};
use decdrm_engine::decdrm_mdi::pft::{PftConfig, fragment};
use decdrm_engine::decdrm_mdi::rci::{RciCommand, RciListener};
use decdrm_engine::decdrm_mdi::rsci::{ImpulseResponse, RxFlags};
use decdrm_engine::decdrm_mdi::source::MdiOrigin;
use decdrm_engine::decdrm_mdi::{MdiFrame, Protocol, RsciStatus};
use decdrm_engine::{Command, Engine, EngineConfig, EngineEvent, InputSpec, MdiSpec, Snapshot};
use decdrm_station::{Station, StationConfig};
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// `frames` frames of a mode B, 10 kHz station (AAC tone, a text message) as MDI.
fn station_mdi(frames: u64) -> Vec<MdiFrame> {
    let dir = tempfile::tempdir().unwrap();
    let toml = r#"
        [channel]
        mode = "B"
        occupancy = 3
        msc_mode = "64-QAM"
        interleaving = "short"
        [output]
        file = "rf.wav"
        [[service]]
        label = "MDI Test"
        id = 0xD0D0C9
        [service.audio]
        codec = "aac"
        core_rate = 24000
        text = ["Hello MDI"]
        input = { tone_hz = 1000.0 }
    "#;
    let mut cfg = StationConfig::from_toml_str(toml).unwrap();
    cfg.base_dir = Some(dir.path().to_path_buf());
    let mut station = Station::new(cfg).unwrap_or_else(|e| panic!("{e}"));
    station.capture_mdi(true);
    (0..frames)
        .map(|_| {
            station.transmit_frame().unwrap();
            station.last_mdi().cloned().unwrap()
        })
        .collect()
}

/// Collect the engine's log until `done` holds for a snapshot or `limit` passes.
fn run_until(engine: &Engine, log: &mut Vec<String>, limit: Duration, done: impl Fn(&Snapshot) -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(EngineEvent::Log(l)) = engine.recv_event(Duration::from_millis(20)) {
            log.push(l);
        }
        if done(&engine.snapshot()) {
            return true;
        }
    }
    false
}

/// An MDI recording (file framing, as Dream writes `.rsA`): decoded to the end.
#[test]
fn mdi_recording_through_the_engine() {
    let frames = station_mdi(25);
    assert_eq!(frames[0].fac.map(|f| f.len()), Some(9));
    assert!(frames[0].sdc.is_some() && frames[1].sdc.is_none(), "SDC in the first frame of a super frame");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("station.rsM");
    let mut w = DcpFileWriter::create(&path, FileKind::FileIo, 0).unwrap();
    for (i, f) in frames.iter().enumerate() {
        w.write_packet(&f.to_af(i as u16).to_bytes(), None).unwrap();
    }
    w.finish().unwrap();

    let origin = MdiOrigin::parse(path.to_str().unwrap()).unwrap();
    let input = InputSpec::Mdi(MdiSpec { origin, realtime: false, rci: None });
    let engine = Engine::start(EngineConfig { input, ..EngineConfig::default() });
    let mut log = Vec::new();
    assert!(run_until(&engine, &mut log, Duration::from_secs(60), |s| s.stopped), "{log:#?}");
    let snap = engine.snapshot();
    drop(engine);
    for l in &log {
        println!("{l}");
    }
    assert_eq!(snap.error, None, "{log:#?}");
    let mdi = snap.input.mdi.as_ref().expect("MDI status");
    assert_eq!(mdi.protocol.as_deref(), Some("DMDI 0.0"));
    assert_eq!((mdi.stats.frames, mdi.stats.lost), (25, 0));
    assert!(snap.services.iter().any(|s| s.label == "MDI Test"), "{:?}", snap.services);
    assert!(snap.audio.frames_ok >= 150, "{} audio frames ok", snap.audio.frames_ok);
    assert_eq!(snap.audio.frames_bad, 0);
    assert_eq!(snap.text.as_deref(), Some("Hello MDI"));
    assert_eq!(snap.rx.mode, Some(decdrm_core::params::RobustnessMode::B));
    assert!((snap.input.position_s - 10.0).abs() < 1e-9);
    assert!(snap.rx.fac_ok >= 24 && snap.rx.sdc_ok >= 8, "FAC {} SDC {}", snap.rx.fac_ok, snap.rx.sdc_ok);
}

/// What an RSCI receiver adds to each frame.
fn rsci_status() -> RsciStatus {
    RsciStatus {
        profile: Some('A'),
        signal_dbuv: Some(38.5),
        flags: Some(RxFlags::default()),
        wmer_fac_db: Some(21.0),
        wmer_msc_db: Some(18.5),
        mer_db: Some(19.5),
        doppler_hz: Some(0.75),
        delay: vec![(95, 1.5), (99, 2.25)],
        psd_db: Some((0..85).map(|i| if (11..74).contains(&i) { -40.0 } else { -80.0 }).collect()),
        impulse_response: Some(ImpulseResponse { start_ms: -2.0, end_ms: 8.0, db: (0..41).map(|i| if i == 8 { 0.0 } else { -40.0 }).collect() }),
        frequency_hz: Some(6_030_000),
        receiver_info: Some("Test receiver".into()),
        ..RsciStatus::default()
    }
}

/// RSCI over UDP in PFT fragments with Reed–Solomon, one fragment lost per packet: the
/// frames are rebuilt and decoded, the receiver's status shows, and RCI commands go
/// back to it.
#[test]
fn rsci_over_udp_with_losses_and_rci() {
    let mut frames = station_mdi(30);
    for f in &mut frames {
        f.protocol = Some(Protocol::RSCI);
        f.rsci = rsci_status();
    }
    let rci_origin = UdpOrigin { port: 0, group: Some(Ipv4Addr::LOCALHOST), interface: None, source: None };
    let mut rci = RciListener::bind(&rci_origin).unwrap();
    let rci_port = rci.local_addr().unwrap().port();
    let origin = MdiOrigin::Udp(UdpOrigin { port: 0, group: Some(Ipv4Addr::LOCALHOST), interface: None, source: None });
    let input = InputSpec::Mdi(MdiSpec { origin, realtime: false, rci: Some(format!("127.0.0.1:{rci_port}").parse().unwrap()) });
    let engine = Engine::start(EngineConfig { input, ..EngineConfig::default() });
    let mut log = Vec::new();
    let listening = |s: &Snapshot| s.input.mdi.as_ref().is_some_and(|m| m.local.is_some());
    assert!(run_until(&engine, &mut log, Duration::from_secs(10), listening), "{log:#?}");
    let local = engine.snapshot().input.mdi.unwrap().local.unwrap();
    let port: u16 = local.rsplit(':').next().unwrap().parse().unwrap();

    // The "receiver": every frame in fragments with FEC; the second fragment is lost.
    let sender = std::thread::spawn(move || {
        let tx = UdpSender::new(&format!("127.0.0.1:{port}").parse().unwrap()).unwrap();
        let cfg = PftConfig { max_payload: 600, fec: Some(2), ..PftConfig::default() };
        for (n, f) in frames.iter().enumerate() {
            for (i, frag) in fragment(&f.to_af(n as u16).to_bytes(), n as u16, &cfg).iter().enumerate() {
                if i != 1 {
                    tx.send(&frag.to_bytes()).unwrap();
                }
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    });
    let decoded = |s: &Snapshot| s.audio.frames_ok >= 100 && s.text.is_some();
    assert!(run_until(&engine, &mut log, Duration::from_secs(60), decoded), "{log:#?}");
    sender.join().unwrap();

    let snap = engine.snapshot();
    let mdi = snap.input.mdi.clone().unwrap();
    assert_eq!(mdi.protocol.as_deref(), Some("RSCI 3.0"));
    assert!(mdi.stats.pft.recovered >= 20, "{:?}", mdi.stats);
    assert_eq!(mdi.rsci.profile, Some('A'));
    assert_eq!(mdi.rsci.receiver_info.as_deref(), Some("Test receiver"));
    assert_eq!((snap.rx.wmer_db, snap.rx.mer_db, snap.rx.fac_mer_db), (Some(18.5), Some(19.5), Some(21.0)));
    assert_eq!((snap.rx.doppler_hz, snap.rx.delay_ms), (0.75, 1.5));
    assert_eq!(snap.rx.state, decdrm_core::rx::RxState::Locked);
    assert_eq!(snap.visuals.spectrum_db.len(), 85);
    assert_eq!(snap.visuals.dc_hz, Some(0.0));
    assert!((snap.visuals.spectrum_centre_hz - (-7875.0 + 85.0 * 187.5 / 2.0)).abs() < 1e-9);
    assert_eq!(snap.visuals.chain.pds.len(), 41);
    assert!((snap.visuals.chain.pds_axis.unwrap().step_ms - 0.25).abs() < 1e-12);
    assert_eq!(snap.text.as_deref(), Some("Hello MDI"));

    // RCI: retune and select a service.
    engine.command(Command::Tune(7325.0));
    engine.command(Command::SelectService(0));
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got = Vec::new();
    while got.len() < 2 && Instant::now() < deadline {
        got.extend(rci.poll(Duration::from_millis(100)).unwrap().into_iter().map(|(c, _)| c));
    }
    drop(engine);
    assert_eq!(got, [RciCommand::Frequency(7_325_000), RciCommand::Service(0)]);
}
