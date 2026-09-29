//! Loopback tests: DecDRM transmitter → (channel simulator) → output stage (real IF
//! or I/Q `f32`) → receiver.
//!
//! * `every_layout_*`: FAC, mode/occupancy detection and SDC CRCs for every valid
//!   robustness mode / spectrum occupancy combination, noiseless.
//! * `msc_bit_exact_*`: MSC multiplex frames recovered bit-exactly for 16-QAM,
//!   64-QAM SM, HMsym and HMmix with short and long interleaving.
//! * `robustness_*`: AWGN, DRM channel models, frequency and sample-rate offsets.
//!   These print a report and only assert the easy cases; run with
//!   `cargo test -p decdrm-core --test loopback -- --nocapture` to see the tables.
//!   The long sweeps are `#[ignore]`d (`-- --ignored` runs them).
//!
//! Everything is seeded, so results are reproducible run to run.

use decdrm_core::channel::{ChannelConfig, ChannelModel, ChannelSimulator, Rng};
use decdrm_core::fac::{ChannelParams, Fac, Interleaving, MscMode, SdcMode, ServiceParams};
use decdrm_core::fec::mlc::MscProtection;
use decdrm_core::params::{ChannelLayout, RobustnessMode, SAMPLE_RATE, SAMPLES_PER_FRAME, SpectrumOccupancy};
use decdrm_core::rx::{InputFormat, MscConfig, RealChannel, Receiver, ReceiverConfig, ReceiverEvent};
use decdrm_core::tx::output::{OutputConfig, OutputFormat, OutputStage, suggested_if_hz};
use decdrm_core::tx::{Transmitter, TxConfig};
use decdrm_core::{Cplx, Real};
use std::fmt::Write as _;

/// How the transmitter output reaches the receiver.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Link {
    /// Real IF at the layout's suggested IF (12 kHz, or ~7 kHz for 18/20 kHz).
    RealIf,
    /// I/Q with the DC carrier at `offset_hz`.
    Iq(Real),
}

impl Link {
    fn name(self) -> String {
        match self {
            Link::RealIf => "real".into(),
            Link::Iq(0.0) => "I/Q".into(),
            Link::Iq(f) => format!("I/Q{f:+.0}"),
        }
    }
}

/// One loopback run.
#[derive(Debug, Clone)]
struct Scenario {
    tx: TxConfig,
    link: Link,
    channel: Option<ChannelConfig>,
    seconds: Real,
    /// Tell the receiver the MSC parameters (as the SDC would) and check the frames.
    decode_msc: bool,
    seed: u64,
}

impl Scenario {
    fn new(tx: TxConfig, link: Link, seconds: Real) -> Self {
        Self { tx, link, channel: None, seconds, decode_msc: false, seed: 1 }
    }
}

/// What the receiver made of it.
#[derive(Debug, Clone, Default)]
struct Outcome {
    frames: usize,
    signal_found: Option<(Real, bool)>,
    modes: Vec<RobustnessMode>,
    restarts: usize,
    fac_ok: usize,
    fac_bad: usize,
    /// CRC-valid FACs that differ from every FAC sent.
    fac_wrong: usize,
    /// Time of the first good FAC, s.
    first_fac_s: Option<Real>,
    occupancies: Vec<u8>,
    sdc_ok: usize,
    sdc_bad: usize,
    /// CRC-valid SDC blocks whose content was never sent.
    sdc_wrong: usize,
    msc_frames: usize,
    msc_matched: usize,
    /// Matched frames form one run of consecutive transmitted frames.
    msc_consecutive: bool,
    /// Transmitter frames between sending a multiplex frame and its decoding event.
    msc_lags: Vec<usize>,
    /// Receiver status at the end.
    snr_db: Option<Real>,
    sro_hz: Real,
    dc_hz: Option<Real>,
    clipped: u64,
}

fn test_fac(rng: &mut Rng) -> Fac {
    Fac {
        channel: ChannelParams {
            enhancement: false,
            frame_index: 0,
            afs_valid: true,
            // The transmitter overwrites occupancy, interleaving and the modes.
            occupancy: SpectrumOccupancy::SO_3,
            interleaving: Interleaving::Long,
            msc_mode: MscMode::Qam64Sm,
            sdc_mode: SdcMode::Qam16,
            num_audio: 1,
            num_data: 0,
            reconfiguration_index: 0,
            toggle: rng.bit() == 1,
        },
        service: ServiceParams {
            service_id: (rng.next_u64() & 0xFF_FFFF) as u32,
            short_id: 0,
            audio_ca: false,
            language: rng.below(16) as u8,
            is_data: false,
            descriptor: rng.below(32) as u8,
            data_ca: false,
        },
    }
}

fn receiver_for(link: Link) -> Receiver {
    let cfg = match link {
        Link::RealIf => ReceiverConfig { input: InputFormat::Real(RealChannel::Mix), channels: 1, ..Default::default() },
        Link::Iq(_) => ReceiverConfig { input: InputFormat::Iq { swap: false }, channels: 2, ..Default::default() },
    };
    Receiver::new(cfg)
}

fn output_for(layout: ChannelLayout, link: Link) -> OutputStage {
    let cfg = match link {
        Link::RealIf => OutputConfig::real(suggested_if_hz(layout)),
        Link::Iq(f) => OutputConfig::iq(f),
    };
    OutputStage::new(layout, cfg).expect("signal fits the output band")
}

fn run(sc: &Scenario) -> Outcome {
    let mut tx = Transmitter::new(sc.tx).expect("valid transmitter configuration");
    let layout = tx.layout();
    let mut out_stage = output_for(layout, sc.link);
    let mut chan = sc.channel.clone().map(|c| ChannelSimulator::new(layout, c));
    let mut rx = receiver_for(sc.link);
    if sc.decode_msc {
        rx.set_msc_config(Some(MscConfig {
            mode: sc.tx.msc_mode,
            protection: sc.tx.protection,
            part_a_bytes: sc.tx.part_a_bytes,
            interleaving: sc.tx.interleaving,
        }));
    }
    let mut rng = Rng::new(sc.seed);
    let frames = (sc.seconds * Real::from(SAMPLE_RATE) / SAMPLES_PER_FRAME as Real).ceil() as usize;
    let cap = tx.msc_capacity();
    let mut o = Outcome { frames, msc_consecutive: true, ..Default::default() };
    let mut sent_fac: Vec<Fac> = Vec::new();
    let mut sent_sdc: Vec<Vec<u8>> = Vec::new();
    let mut sent_msc: Vec<Vec<u8>> = Vec::new();
    let mut last_match: Option<usize> = None;
    let mut chan_out: Vec<Cplx> = Vec::new();
    let mut pcm: Vec<f32> = Vec::new();

    for f in 0..frames {
        let fac = test_fac(&mut rng);
        let msc = rng.bits(cap.total_bits());
        let sdc = (tx.frame_index() == 0).then(|| rng.bytes(tx.sdc_capacity_bytes()));
        sent_fac.push(tx.fac_for_next_frame(&fac));
        let baseband = tx.transmit_frame(&fac, &msc, sdc.as_deref()).expect("transmit");
        sent_msc.push(msc);
        if let Some(s) = sdc {
            sent_sdc.push(s);
        }

        pcm.clear();
        match chan.as_mut() {
            Some(c) => {
                chan_out.clear();
                c.process(&baseband, &mut chan_out);
                out_stage.process(&chan_out, &mut pcm);
            }
            None => out_stage.process(&baseband, &mut pcm),
        }

        let t = (f + 1) as Real * SAMPLES_PER_FRAME as Real / Real::from(SAMPLE_RATE);
        for ev in rx.push(&pcm) {
            match ev {
                ReceiverEvent::SignalFound { dc_hz, inverted } => o.signal_found = Some((dc_hz, inverted)),
                ReceiverEvent::ModeDetected(m) => o.modes.push(m),
                ReceiverEvent::Restarted => o.restarts += 1,
                ReceiverEvent::Fac(fac) => {
                    o.fac_ok += 1;
                    o.first_fac_s.get_or_insert(t);
                    let so = fac.channel.occupancy.value();
                    if !o.occupancies.contains(&so) {
                        o.occupancies.push(so);
                    }
                    if !sent_fac.contains(&fac) {
                        o.fac_wrong += 1;
                    }
                }
                ReceiverEvent::FacError => o.fac_bad += 1,
                ReceiverEvent::Sdc(b) => {
                    if b.crc_ok {
                        o.sdc_ok += 1;
                        if b.afs_index != sc.tx.afs_index || !sent_sdc.contains(&b.data) {
                            o.sdc_wrong += 1;
                        }
                    } else {
                        o.sdc_bad += 1;
                    }
                }
                ReceiverEvent::Msc(m) => {
                    o.msc_frames += 1;
                    let mut all = m.vspp.clone();
                    all.extend_from_slice(&m.bits);
                    if let Some(j) = sent_msc.iter().position(|s| *s == all) {
                        o.msc_matched += 1;
                        o.msc_lags.push(f - j);
                        if last_match.is_some_and(|l| l + 1 != j) {
                            o.msc_consecutive = false;
                        }
                        last_match = Some(j);
                    } else if last_match.is_some() {
                        // A miss after the first match breaks the run.
                        o.msc_consecutive = false;
                    }
                }
                // Any events added to the receiver later are irrelevant here.
                #[allow(unreachable_patterns)]
                _ => {}
            }
        }
    }
    let st = rx.status();
    o.snr_db = st.snr_db;
    o.sro_hz = st.sro_hz;
    o.dc_hz = st.dc_frequency_hz;
    o.clipped = out_stage.clipped_samples();
    o
}

fn layout_name(tx: &TxConfig) -> String {
    format!("{}/SO{}", tx.mode, tx.occupancy.value())
}

fn fmt_opt(v: Option<Real>, prec: usize) -> String {
    v.map_or("-".into(), |x| format!("{x:.prec$}"))
}

fn header() -> String {
    format!(
        "{:<8} {:<8} {:>9} {:>10} {:>6} {:>8} {:>10} {:>9} {:>9} {:>7} {:>7}",
        "layout", "link", "signal", "modes", "FAC s", "FAC ok", "bad/wrong", "SDC ok", "bad/wrong", "SNR dB", "restart"
    )
}

fn row(sc: &Scenario, o: &Outcome) -> String {
    let modes: String = o.modes.iter().map(|m| m.to_string()).collect::<Vec<_>>().join("");
    format!(
        "{:<8} {:<8} {:>9} {:>10} {:>6} {:>5}/{:<2} {:>6}/{:<3} {:>9} {:>5}/{:<3} {:>7} {:>7}",
        layout_name(&sc.tx),
        sc.link.name(),
        o.signal_found.map_or("no".into(), |(f, inv)| format!("{f:.0}{}", if inv { "i" } else { "" })),
        if modes.is_empty() { "-".into() } else { modes },
        fmt_opt(o.first_fac_s, 1),
        o.fac_ok,
        o.frames,
        o.fac_bad,
        o.fac_wrong,
        o.sdc_ok,
        o.sdc_bad,
        o.sdc_wrong,
        fmt_opt(o.snr_db, 1),
        o.restarts,
    )
}

/// Problems of a basic (noiseless) FAC/SDC loopback; empty if it passed.
fn check_basic(sc: &Scenario, o: &Outcome) -> Vec<String> {
    let mut p = Vec::new();
    if o.signal_found.is_none() {
        p.push("never found the signal".to_string());
    }
    if !o.modes.contains(&sc.tx.mode) {
        p.push(format!("mode {} never detected (got {:?})", sc.tx.mode, o.modes));
    }
    if o.fac_ok == 0 {
        p.push("no FAC decoded".into());
    } else if o.fac_ok + 8 < o.frames {
        p.push(format!("only {} of {} FACs decoded", o.fac_ok, o.frames));
    }
    if o.fac_bad > 2 {
        p.push(format!("{} FAC CRC errors", o.fac_bad));
    }
    if o.fac_wrong > 0 {
        p.push(format!("{} FACs with wrong content", o.fac_wrong));
    }
    if o.occupancies.iter().any(|&so| so != sc.tx.occupancy.value()) {
        p.push(format!("wrong occupancy in FAC: {:?}", o.occupancies));
    }
    if o.sdc_ok < 2 {
        p.push(format!("only {} SDC blocks with good CRC", o.sdc_ok));
    }
    if o.sdc_bad > 0 {
        p.push(format!("{} SDC CRC errors", o.sdc_bad));
    }
    if o.sdc_wrong > 0 {
        p.push(format!("{} SDC blocks with wrong content", o.sdc_wrong));
    }
    if o.restarts > 0 {
        p.push(format!("{} restarts", o.restarts));
    }
    p
}

fn all_layouts() -> Vec<ChannelLayout> {
    let mut v = Vec::new();
    for mode in RobustnessMode::ALL {
        for so in SpectrumOccupancy::ALL {
            if let Some(l) = ChannelLayout::new(mode, so) {
                v.push(l);
            }
        }
    }
    v
}

/// Run every layout over `link` and fail with a table if any combination fails.
fn every_layout(link: Link) {
    let mut report = String::new();
    let mut failures = Vec::new();
    writeln!(report, "{}", header()).unwrap();
    for (i, l) in all_layouts().into_iter().enumerate() {
        let tx = TxConfig {
            mode: l.mode,
            occupancy: l.occupancy,
            // Alternate the SDC constellation and interleaving across layouts.
            sdc_mode: if i % 2 == 0 { SdcMode::Qam16 } else { SdcMode::Qam4 },
            interleaving: if i % 3 == 0 { Interleaving::Short } else { Interleaving::Long },
            afs_index: (i % 16) as u8,
            ..Default::default()
        };
        let sc = Scenario { seed: 100 + i as u64, ..Scenario::new(tx, link, 6.0) };
        let o = run(&sc);
        writeln!(report, "{}", row(&sc, &o)).unwrap();
        let problems = check_basic(&sc, &o);
        if !problems.is_empty() {
            failures.push(format!("{} over {}: {}", layout_name(&sc.tx), link.name(), problems.join("; ")));
        }
    }
    println!("{report}");
    assert!(failures.is_empty(), "\n{report}\nfailing combinations:\n  {}\n", failures.join("\n  "));
}

#[test]
fn every_layout_iq() {
    every_layout(Link::Iq(0.0));
}

#[test]
fn every_layout_real_if() {
    every_layout(Link::RealIf);
}

#[test]
#[ignore]
fn dbg_a_so0() {
    for link in [Link::Iq(0.0), Link::RealIf] {
        for (m, so) in [(RobustnessMode::A, SpectrumOccupancy::SO_0), (RobustnessMode::A, SpectrumOccupancy::SO_1), (RobustnessMode::B, SpectrumOccupancy::SO_0), (RobustnessMode::A, SpectrumOccupancy::SO_2)] {
            let mut firsts = Vec::new();
            for seed in 0..12u64 {
                let tx = TxConfig { mode: m, occupancy: so, ..Default::default() };
                let sc = Scenario { seed: 1000 + seed, ..Scenario::new(tx, link, 12.0) };
                let o = run(&sc);
                firsts.push(format!("{}({}/{} bad{} r{})", fmt_opt(o.first_fac_s, 1), o.fac_ok, o.frames, o.fac_bad, o.restarts));
            }
            println!("{m}/SO{} {}: first FAC s: {}", so.value(), link.name(), firsts.join(" "));
        }
    }
}

#[test]
#[ignore]
fn dbg_timeline() {
    let seed: u64 = std::env::var("DBG_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(1001);
    let so: u8 = std::env::var("DBG_SO").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let mode = match std::env::var("DBG_MODE").as_deref() { Ok("B") => RobustnessMode::B, Ok("C") => RobustnessMode::C, Ok("D") => RobustnessMode::D, _ => RobustnessMode::A };
    let txc = TxConfig { mode, occupancy: SpectrumOccupancy::new(so).unwrap(), ..Default::default() };
    let mut tx = Transmitter::new(txc).unwrap();
    let layout = tx.layout();
    let mut out_stage = output_for(layout, Link::Iq(0.0));
    let mut rx = receiver_for(Link::Iq(0.0));
    let mut rng = Rng::new(seed);
    let cap = tx.msc_capacity();
    for f in 0..15 {
        let fac = test_fac(&mut rng);
        let msc = rng.bits(cap.total_bits());
        let sdc = (tx.frame_index() == 0).then(|| rng.bytes(tx.sdc_capacity_bytes()));
        let bb = tx.transmit_frame(&fac, &msc, sdc.as_deref()).unwrap();
        let mut pcm = Vec::new();
        out_stage.process(&bb, &mut pcm);
        for chunk in pcm.chunks(2 * 1920) {
            for ev in rx.push(chunk) {
                let s = match ev { ReceiverEvent::Fac(f) => format!("FAC frame {}", f.channel.frame_index), e => format!("{e:?}").chars().take(100).collect() };
                println!("  frame {f:2}: {s}");
            }
        }
        let st = rx.status();
        println!("frame {f:2} end: state {:?} mode {:?} so {:?} fsync {:?} dc {:?} scores {:?}", st.state, st.mode, st.occupancy.map(|o| o.value()), st.frame_sync, st.dc_frequency_hz, rx.mode_scores());
    }
}
