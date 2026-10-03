//! Loopback tests: DecDRM transmitter → (channel simulator) → output stage (real IF
//! or I/Q `f32`) → receiver.
//!
//! * `every_layout_*`: FAC decoding, mode/occupancy detection and SDC CRCs for every
//!   valid robustness mode / spectrum occupancy combination, noiseless, over I/Q
//!   and real IF.
//! * `msc_bit_exact_*`: MSC multiplex frames recovered bit-exactly for 16-QAM,
//!   64-QAM SM, HMsym and HMmix with short and long interleaving (incl. UEP).
//! * `robustness_*`: AWGN, DRM channel models 1–4, frequency and sample-rate
//!   offsets. These print a report and only assert easy, high-SNR cases. The long
//!   sweeps (SNR sweeps, channels 5/6, other layouts) are `#[ignore]`d; run them with
//!   `cargo test -p decdrm-core --test loopback -- --ignored --nocapture`.
//! * Regression tests for receiver problems these loopbacks found (lost frames after
//!   an occupancy change, a spurious SRO estimate, slow mode A/SO0 detection).
//!
//! Use `--nocapture` to see the result tables. Everything is seeded, so results are
//! reproducible run to run.

use decdrm_core::channel::{ChannelConfig, ChannelModel, ChannelSimulator, Rng};
use decdrm_core::fac::{ChannelParams, Fac, Interleaving, MscMode, SdcMode, ServiceParams};
use decdrm_core::fec::mlc::MscProtection;
use decdrm_core::params::{ChannelLayout, RobustnessMode, SAMPLE_RATE, SAMPLES_PER_FRAME, SpectrumOccupancy};
use decdrm_core::rx::{InputFormat, MscConfig, RealChannel, Receiver, ReceiverConfig, ReceiverEvent};
use decdrm_core::tx::output::{OutputConfig, OutputStage, suggested_if_hz};
use decdrm_core::tx::{Transmitter, TxConfig};
use decdrm_core::{Cplx, Real};
use std::fmt::Write as _;

/// Seconds per transmission frame.
const FRAME_S: Real = SAMPLES_PER_FRAME as Real / SAMPLE_RATE as Real;

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
    name: String,
    tx: TxConfig,
    link: Link,
    channel: Option<ChannelConfig>,
    seconds: Real,
    /// Tell the receiver the MSC parameters (as the SDC would) and check the frames.
    decode_msc: bool,
    /// Send this FAC identity in every frame (`Transmitter::set_fixed_fac_identity`).
    fixed_identity: Option<u8>,
    /// Frames whose FAC announces two audio services instead of one.
    odd_facs: Vec<usize>,
    seed: u64,
}

impl Scenario {
    fn new(tx: TxConfig, link: Link, seconds: Real) -> Self {
        Self {
            name: layout_name(&tx),
            tx,
            link,
            channel: None,
            seconds,
            decode_msc: false,
            fixed_identity: None,
            odd_facs: Vec::new(),
            seed: 1,
        }
    }
}

/// Tracks which of the transmitted items (FACs, SDC blocks, MSC frames) the
/// receiver delivered, in order.
#[derive(Debug, Clone, Default)]
struct Matches {
    /// Transmitted index of every delivered item that matched.
    indices: Vec<usize>,
    /// Delivered items that match nothing that was sent.
    wrong: usize,
    /// A delivered item after the first match that was not the successor of the
    /// previous one (a gap, a repeat or a wrong item).
    breaks: usize,
    /// Number of matched items before each break (1 = between the first and the
    /// second item).
    break_at: Vec<usize>,
    /// Transmitter frame being pushed when each matched item arrived, minus the
    /// frame that completed it (for MSC frames: the frame that completed the
    /// multiplex frame D - 1 later, which the deinterleaver needs).
    lags: Vec<usize>,
}

impl Matches {
    fn record(&mut self, found: Option<usize>, push_frame: usize, sent_frame: impl Fn(usize) -> usize) {
        match found {
            Some(j) => {
                if self.indices.last().is_some_and(|&l| l + 1 != j) {
                    self.breaks += 1;
                    self.break_at.push(self.indices.len());
                }
                self.indices.push(j);
                self.lags.push(push_frame.saturating_sub(sent_frame(j)));
            }
            None => {
                self.wrong += 1;
                if !self.indices.is_empty() {
                    self.breaks += 1;
                    self.break_at.push(self.indices.len());
                }
            }
        }
    }

    fn count(&self) -> usize {
        self.indices.len()
    }

    fn lag_range(&self) -> String {
        match (self.lags.iter().min(), self.lags.iter().max()) {
            (Some(a), Some(b)) if a == b => format!("{a}"),
            (Some(a), Some(b)) => format!("{a}-{b}"),
            _ => "-".into(),
        }
    }
}

/// What the receiver made of it.
#[derive(Debug, Clone, Default)]
struct Outcome {
    frames: usize,
    signal_found: Option<(Real, bool)>,
    modes: Vec<RobustnessMode>,
    restarts: usize,
    fac: Matches,
    fac_bad: usize,
    /// FAC CRC failures after the first good FAC.
    fac_bad_after_lock: usize,
    /// Pushed frame during which the first good FAC arrived.
    first_fac_frame: Option<usize>,
    occupancies: Vec<u8>,
    sdc: Matches,
    sdc_bad: usize,
    /// SDC CRC failures after the first good SDC block.
    sdc_bad_after_ok: usize,
    msc: Matches,
    msc_frames: usize,
    /// MSC frames the receiver flagged as incomplete (long interleaver filling).
    msc_incomplete: usize,
    /// MSC frames flagged complete that match nothing that was sent.
    msc_complete_wrong: usize,
    /// `ReceiverEvent::FrameIdentity` reports (trusted or not), in order.
    frame_identity: Vec<bool>,
    /// Receiver status at the end.
    snr_db: Option<Real>,
    sro_hz: Real,
    dc_hz: Option<Real>,
    clipped: u64,
}

impl Outcome {
    fn first_fac_s(&self) -> Option<Real> {
        self.first_fac_frame.map(|f| (f + 1) as Real * FRAME_S)
    }
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
            // Random 24-bit service ID: makes every frame's FAC unique.
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

/// Transmitter frame that carries the end of multiplex frame `j` (multiplex frame
/// 3s+0 ends in transmission frame 3s+1, 3s+1 and 3s+2 end in 3s+2).
fn mux_end_frame(j: usize) -> usize {
    if j.is_multiple_of(3) { j + 1 } else { j - j % 3 + 2 }
}

fn run(sc: &Scenario) -> Outcome {
    let mut tx = Transmitter::new(sc.tx).expect("valid transmitter configuration");
    tx.set_fixed_fac_identity(sc.fixed_identity);
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
    let depth = tx.interleaver_depth();
    let mut rng = Rng::new(sc.seed);
    let frames = (sc.seconds / FRAME_S).ceil() as usize;
    let cap = tx.msc_capacity();
    let mut o = Outcome { frames, ..Default::default() };
    let mut sent_fac: Vec<Fac> = Vec::new();
    let mut sent_sdc: Vec<Vec<u8>> = Vec::new();
    let mut sent_msc: Vec<Vec<u8>> = Vec::new();
    let mut chan_out: Vec<Cplx> = Vec::new();
    let mut pcm: Vec<f32> = Vec::new();

    for f in 0..frames {
        let mut fac = test_fac(&mut rng);
        if sc.odd_facs.contains(&f) {
            fac.channel.num_audio = 2;
        }
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

        for ev in rx.push(&pcm) {
            match ev {
                ReceiverEvent::SignalFound { dc_hz, inverted } => o.signal_found = Some((dc_hz, inverted)),
                ReceiverEvent::ModeDetected(m) => o.modes.push(m),
                ReceiverEvent::Restarted => o.restarts += 1,
                ReceiverEvent::FrameIdentity { trusted } => o.frame_identity.push(trusted),
                ReceiverEvent::Fac(fac) => {
                    o.first_fac_frame.get_or_insert(f);
                    let so = fac.channel.occupancy.value();
                    if !o.occupancies.contains(&so) {
                        o.occupancies.push(so);
                    }
                    o.fac.record(sent_fac.iter().position(|s| *s == fac), f, |j| j);
                }
                ReceiverEvent::FacError => {
                    o.fac_bad += 1;
                    if o.first_fac_frame.is_some() {
                        o.fac_bad_after_lock += 1;
                    }
                }
                ReceiverEvent::Sdc(b) => {
                    if b.crc_ok {
                        let found = (b.afs_index == sc.tx.afs_index)
                            .then(|| sent_sdc.iter().position(|s| *s == b.data))
                            .flatten();
                        o.sdc.record(found, f, |j| 3 * j);
                    } else {
                        o.sdc_bad += 1;
                        if o.sdc.count() > 0 {
                            o.sdc_bad_after_ok += 1;
                        }
                    }
                }
                ReceiverEvent::Msc(m) => {
                    o.msc_frames += 1;
                    let mut all = m.vspp.clone();
                    all.extend_from_slice(&m.bits);
                    let found = sent_msc.iter().position(|s| *s == all);
                    if !m.complete {
                        o.msc_incomplete += 1;
                    } else if found.is_none() {
                        o.msc_complete_wrong += 1;
                    }
                    o.msc.record(found, f, |j| mux_end_frame(j + depth - 1));
                }
                // Events the receiver may add later are irrelevant here.
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
        "{:<22} {:<5} {:>7} {:>6} {:>6} {:>8} {:>7} {:>7} {:>7} {:>9} {:>6} {:>7} {:>7} {:>3}",
        "scenario",
        "link",
        "DC Hz",
        "modes",
        "FAC s",
        "FAC ok",
        "bad",
        "SDC ok",
        "bad",
        "MSC ok",
        "lag",
        "SNR",
        "SRO Hz",
        "rst"
    )
}

fn row(sc: &Scenario, o: &Outcome) -> String {
    let mut modes: String = o.modes.iter().map(|m| m.to_string()).collect();
    if modes.is_empty() {
        modes = "-".into();
    }
    format!(
        "{:<22} {:<5} {:>7} {:>6} {:>6} {:>5}/{:<2} {:>3}/{:<3} {:>7} {:>3}/{:<3} {:>4}/{:<4} {:>6} {:>7} {:>7.2} {:>3}",
        sc.name,
        sc.link.name(),
        o.signal_found.map_or("no".into(), |(f, inv)| format!("{f:.0}{}", if inv { "i" } else { "" })),
        modes,
        fmt_opt(o.first_fac_s(), 1),
        o.fac.count(),
        o.frames,
        o.fac_bad,
        o.fac_bad_after_lock,
        o.sdc.count(),
        o.sdc_bad,
        o.sdc_bad_after_ok,
        o.msc.count(),
        o.msc_frames,
        o.msc.lag_range(),
        fmt_opt(o.snr_db, 1),
        o.sro_hz,
        o.restarts,
    )
}

/// Longest acceptable time to the first FAC in a clean channel, s.
const MAX_ACQUISITION_S: Real = 4.5;

/// Problems of a noiseless FAC/SDC loopback; empty if it passed.
fn check_clean(sc: &Scenario, o: &Outcome) -> Vec<String> {
    let mut p = Vec::new();
    match o.signal_found {
        None => p.push("never found the signal".to_string()),
        Some((_, true)) => p.push("found the signal spectrally inverted".to_string()),
        _ => {}
    }
    if !o.modes.contains(&sc.tx.mode) {
        p.push(format!("mode {} never detected", sc.tx.mode));
    }
    if o.modes.iter().any(|&m| m != sc.tx.mode) {
        p.push(format!("wrong mode detected ({:?})", o.modes));
    }
    match o.first_fac_s() {
        None => p.push("no FAC decoded".into()),
        Some(t) if t > MAX_ACQUISITION_S => p.push(format!("first FAC only after {t:.1} s")),
        _ => {}
    }
    if o.fac.wrong > 0 {
        p.push(format!("{} FACs with wrong content", o.fac.wrong));
    }
    if o.fac.breaks > 0 {
        p.push(format!("FAC sequence broken after {:?} FACs", o.fac.break_at));
    }
    // Every frame after the first good FAC must decode (the last one or two frames may
    // still be in the receiver's pipeline at the end).
    if let Some(&last) = o.fac.indices.last()
        && last + 3 < o.frames
    {
        p.push(format!("FACs stopped after frame {last}"));
    }
    if o.fac_bad_after_lock > 0 {
        p.push(format!("{} FAC CRC errors after lock", o.fac_bad_after_lock));
    }
    if o.occupancies.iter().any(|&so| so != sc.tx.occupancy.value()) {
        p.push(format!("wrong occupancy in FAC: {:?}", o.occupancies));
    }
    if o.sdc.count() < 2 {
        p.push(format!("only {} SDC blocks with good CRC", o.sdc.count()));
    }
    if o.sdc.wrong > 0 || o.sdc.breaks > 0 {
        p.push(format!("{} SDC blocks with wrong content, {} gaps", o.sdc.wrong, o.sdc.breaks));
    }
    if o.sdc_bad_after_ok > 0 {
        p.push(format!("{} SDC CRC errors after the first good block", o.sdc_bad_after_ok));
    }
    if o.restarts > 0 {
        p.push(format!("{} restarts", o.restarts));
    }
    if !o.frame_identity.is_empty() {
        p.push(format!("FAC identity trust changed: {:?}", o.frame_identity));
    }
    // The default level leaves 15 dB of headroom; an OFDM peak beyond that is rare
    // but not an error (one in ~10⁶ samples).
    let samples = (o.frames * SAMPLES_PER_FRAME * 2) as u64;
    if o.clipped > samples / 100_000 {
        p.push(format!("{} output samples clipped", o.clipped));
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

/// Run scenarios, print a table and return it with the failures found by `check`.
fn run_all(title: &str, scenarios: &[Scenario], check: impl Fn(&Scenario, &Outcome) -> Vec<String>) -> (String, Vec<String>) {
    let mut report = String::new();
    let mut failures = Vec::new();
    writeln!(report, "\n{title}\n{}", header()).unwrap();
    for sc in scenarios {
        let o = run(sc);
        writeln!(report, "{}", row(sc, &o)).unwrap();
        let problems = check(sc, &o);
        if !problems.is_empty() {
            failures.push(format!("{} over {}: {}", sc.name, sc.link.name(), problems.join("; ")));
        }
    }
    println!("{report}");
    if !failures.is_empty() {
        println!("failing:\n  {}", failures.join("\n  "));
    }
    (report, failures)
}

/// Every layout over `link`; fails with the table if any combination fails.
fn every_layout(link: Link) {
    let scenarios: Vec<Scenario> = all_layouts()
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            let tx = TxConfig {
                mode: l.mode,
                occupancy: l.occupancy,
                // Alternate the SDC constellation and interleaving across layouts.
                sdc_mode: if i % 2 == 0 { SdcMode::Qam16 } else { SdcMode::Qam4 },
                interleaving: if i % 3 == 0 { Interleaving::Short } else { Interleaving::Long },
                afs_index: (i % 16) as u8,
                ..Default::default()
            };
            Scenario { seed: 100 + i as u64, ..Scenario::new(tx, link, 8.0) }
        })
        .collect();
    let (report, failures) = run_all(&format!("Every layout, noiseless, {}", link.name()), &scenarios, check_clean);
    assert!(failures.is_empty(), "{report}\nfailing combinations:\n  {}\n", failures.join("\n  "));
}

#[test]
fn every_layout_iq() {
    every_layout(Link::Iq(0.0));
}

#[test]
fn every_layout_real_if() {
    every_layout(Link::RealIf);
}

/// I/Q with the DC carrier away from 0 Hz (as Dream's I/Q output, which keeps its
/// 6 kHz IF).
#[test]
fn every_layout_iq_offset() {
    every_layout(Link::Iq(6000.0));
}

// ---------------------------------------------------------------------------------
// MSC bit-exact loopback
// ---------------------------------------------------------------------------------

fn msc_scenario(msc_mode: MscMode, interleaving: Interleaving, part_a_bytes: usize, hierarchical: usize) -> Scenario {
    let tx = TxConfig {
        mode: RobustnessMode::B,
        occupancy: SpectrumOccupancy::SO_3,
        msc_mode,
        interleaving,
        protection: MscProtection { part_a: 0, part_b: 1, hierarchical },
        part_a_bytes,
        ..Default::default()
    };
    let name = format!(
        "{:?} {} A={}",
        msc_mode,
        if interleaving == Interleaving::Long { "long" } else { "short" },
        part_a_bytes
    );
    Scenario { name, decode_msc: true, seed: 7 + part_a_bytes as u64, ..Scenario::new(tx, Link::Iq(0.0), 12.0) }
}

/// Problems of a noiseless MSC loopback.
fn check_msc(sc: &Scenario, o: &Outcome) -> Vec<String> {
    let mut p = check_clean(sc, o);
    let depth = if sc.tx.interleaving == Interleaving::Long { 5 } else { 1 };
    // After acquisition (≤ 4.5 s), super-frame alignment and the interleaver filling
    // there must be at least this many good frames in 12 s.
    let want = if depth == 5 { 12 } else { 16 };
    if o.msc.count() < want {
        p.push(format!("only {} of {} MSC frames bit-exact (want ≥ {want})", o.msc.count(), o.msc_frames));
    }
    if o.msc.breaks > 0 {
        p.push(format!("{} breaks in the MSC frame sequence", o.msc.breaks));
    }
    // In a clean channel every frame the receiver calls complete must be exact.
    if o.msc_complete_wrong > 0 {
        p.push(format!("{} MSC frames flagged complete but wrong", o.msc_complete_wrong));
    }
    // Multiplex frame j can be decoded once multiplex frame j + D − 1 has been
    // received; the event must come in the transmitter frame that completes it or,
    // through the receiver's pipeline delay, the next one.
    if o.msc.lags.iter().any(|&l| l > 1) {
        p.push(format!("MSC frames arrive {} frames after the interleaver allows (expected 0-1)", o.msc.lag_range()));
    }
    if let Some(&last) = o.msc.indices.last()
        && last + depth + 3 < o.frames
    {
        p.push(format!("MSC frames stopped after multiplex frame {last}"));
    }
    p
}

fn msc_bit_exact(interleaving: Interleaving) {
    let scenarios = vec![
        msc_scenario(MscMode::Qam16Sm, interleaving, 0, 0),
        msc_scenario(MscMode::Qam16Sm, interleaving, 60, 0),
        msc_scenario(MscMode::Qam64Sm, interleaving, 0, 0),
        msc_scenario(MscMode::Qam64Sm, interleaving, 90, 0),
        msc_scenario(MscMode::Qam64HmSym, interleaving, 0, 1),
        msc_scenario(MscMode::Qam64HmSym, interleaving, 50, 2),
        msc_scenario(MscMode::Qam64HmMix, interleaving, 0, 0),
        msc_scenario(MscMode::Qam64HmMix, interleaving, 40, 3),
    ];
    let title = format!("MSC bit-exact loopback, B/SO3 I/Q, {interleaving:?} interleaving");
    let (report, failures) = run_all(&title, &scenarios, check_msc);
    assert!(failures.is_empty(), "{report}\nfailing:\n  {}\n", failures.join("\n  "));
}

#[test]
fn msc_bit_exact_short_interleaving() {
    msc_bit_exact(Interleaving::Short);
}

#[test]
fn msc_bit_exact_long_interleaving() {
    msc_bit_exact(Interleaving::Long);
}

/// Problems of a loopback whose FAC identity never changes.
fn check_fixed_identity(_sc: &Scenario, o: &Outcome) -> Vec<String> {
    let mut p = Vec::new();
    if o.first_fac_s().is_none_or(|t| t > MAX_ACQUISITION_S) {
        p.push(format!("first FAC at {:?} s", o.first_fac_s()));
    }
    if o.fac.wrong > 0 || o.fac.breaks > 0 || o.fac_bad_after_lock > 0 {
        p.push(format!("FACs: {} wrong, {} gaps, {} CRC errors after lock", o.fac.wrong, o.fac.breaks, o.fac_bad_after_lock));
    }
    // Until the receiver stops trusting the identity (a few frames) it may look for the
    // SDC in the wrong frames.
    if o.sdc_bad > 4 {
        p.push(format!("{} SDC CRC errors", o.sdc_bad));
    }
    if o.sdc.count() < 4 || o.sdc.wrong > 0 || o.sdc.breaks > 0 {
        p.push(format!("{} SDC blocks good, {} wrong, {} gaps", o.sdc.count(), o.sdc.wrong, o.sdc.breaks));
    }
    if o.msc.count() < 12 || o.msc.breaks > 0 || o.msc_complete_wrong > 0 {
        p.push(format!(
            "{} of {} MSC frames bit-exact (want ≥ 12), {} gaps, {} complete but wrong",
            o.msc.count(),
            o.msc_frames,
            o.msc.breaks,
            o.msc_complete_wrong
        ));
    }
    if o.restarts > 0 {
        p.push(format!("{} restarts", o.restarts));
    }
    if o.frame_identity != [false] {
        p.push(format!("FAC identity trust changed {:?} (want once to untrusted)", o.frame_identity));
    }
    p
}

/// A transmitter that sends the same FAC identity in every frame instead of counting
/// through the super frame (0, 1, 2): one on 1557 kHz always says "third frame"
/// (2026-10). The receiver finds the super frame start from the SDC's CRC instead and
/// decodes SDC and MSC as from any station. Mode B, 9 kHz, 16-QAM MSC, 4-QAM SDC, short
/// interleaving, like that station.
#[test]
fn fixed_fac_identity() {
    let scenarios: Vec<Scenario> = (0..4u8)
        .map(|identity| {
            let tx = TxConfig {
                occupancy: SpectrumOccupancy::SO_2,
                msc_mode: MscMode::Qam16Sm,
                sdc_mode: SdcMode::Qam4,
                interleaving: Interleaving::Short,
                ..Default::default()
            };
            Scenario {
                name: format!("identity {identity}"),
                decode_msc: true,
                fixed_identity: Some(identity),
                seed: 40 + u64::from(identity),
                ..Scenario::new(tx, Link::Iq(0.0), 12.0)
            }
        })
        .collect();
    let (report, failures) = run_all("Fixed FAC identity, B/SO2 I/Q", &scenarios, check_fixed_identity);
    assert!(failures.is_empty(), "{report}\nfailing:\n  {}\n", failures.join("\n  "));
}

/// 64-QAM MSC over every layout (real IF), a slower complement to the B/SO3 tests.
#[test]
#[ignore = "long: 16 layouts × 12 s with MSC decoding"]
fn msc_bit_exact_every_layout() {
    let scenarios: Vec<Scenario> = all_layouts()
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            let msc_mode = [MscMode::Qam64Sm, MscMode::Qam16Sm, MscMode::Qam64HmMix, MscMode::Qam64HmSym][i % 4];
            let tx = TxConfig {
                mode: l.mode,
                occupancy: l.occupancy,
                msc_mode,
                interleaving: if i % 2 == 0 { Interleaving::Long } else { Interleaving::Short },
                ..Default::default()
            };
            let name = format!("{} {:?}", layout_name(&tx), msc_mode);
            Scenario { name, decode_msc: true, seed: 300 + i as u64, ..Scenario::new(tx, Link::RealIf, 12.0) }
        })
        .collect();
    let (report, failures) = run_all("MSC bit-exact loopback, every layout, real IF", &scenarios, check_msc);
    assert!(failures.is_empty(), "{report}\nfailing:\n  {}\n", failures.join("\n  "));
}

// ---------------------------------------------------------------------------------
// Robustness smoke tests
// ---------------------------------------------------------------------------------

/// Mode B / 10 kHz, 64-QAM (protection level 1) or 16-QAM, long interleaving,
/// through a channel.
fn impaired(name: &str, tx: TxConfig, link: Link, channel: ChannelConfig, seconds: Real) -> Scenario {
    Scenario {
        name: name.to_string(),
        channel: Some(channel),
        decode_msc: true,
        seed: 11,
        ..Scenario::new(tx, link, seconds)
    }
}

fn b_so3(msc_mode: MscMode) -> TxConfig {
    TxConfig { msc_mode, ..Default::default() }
}

/// Assert only that FACs and MSC frames flow after acquisition.
fn check_decodes(_sc: &Scenario, o: &Outcome) -> Vec<String> {
    let mut p = Vec::new();
    if o.first_fac_s().is_none_or(|t| t > 6.0) {
        p.push(format!("first FAC at {:?} s", o.first_fac_s()));
    }
    if o.fac.count() + 20 < o.frames {
        p.push(format!("only {} of {} FACs", o.fac.count(), o.frames));
    }
    if o.msc.count() < 5 {
        p.push(format!("only {} MSC frames bit-exact", o.msc.count()));
    }
    p
}

fn no_check(_: &Scenario, _: &Outcome) -> Vec<String> {
    Vec::new()
}

#[test]
fn robustness_awgn() {
    let scenarios: Vec<Scenario> = [30.0, 20.0, 16.0, 13.0, 10.0, 7.0]
        .into_iter()
        .map(|snr| impaired(&format!("AWGN {snr} dB 64-QAM"), b_so3(MscMode::Qam64Sm), Link::Iq(0.0), ChannelConfig::awgn(snr), 15.0))
        .collect();
    // Only the 30 dB case is asserted.
    let (report, failures) = run_all("AWGN, B/SO3, 64-QAM SM prot. 1, long interleaving", &scenarios, |sc, o| {
        if sc.name.starts_with("AWGN 30") { check_decodes(sc, o) } else { Vec::new() }
    });
    assert!(failures.is_empty(), "{report}\n{}", failures.join("\n"));
}

#[test]
fn robustness_channel_models_1_to_4() {
    let scenarios: Vec<Scenario> = (1..=4)
        .map(|n| {
            let ch = ChannelConfig { seed: 20 + u64::from(n), ..ChannelConfig::drm(n, 25.0).unwrap() };
            let name = format!("ch{n} {} 25 dB", ChannelModel::drm_name(n));
            impaired(&name, b_so3(MscMode::Qam16Sm), Link::Iq(0.0), ch, 20.0)
        })
        .collect();
    // Only channel 1 (AWGN at 25 dB) is asserted.
    let (report, failures) = run_all("DRM channels 1-4, B/SO3, 16-QAM prot. 1, long interleaving", &scenarios, |sc, o| {
        if sc.name.starts_with("ch1 ") { check_decodes(sc, o) } else { Vec::new() }
    });
    assert!(failures.is_empty(), "{report}\n{}", failures.join("\n"));
}

#[test]
fn robustness_offsets() {
    let tx = b_so3(MscMode::Qam64Sm);
    let with = |freq: Real, ppm: Real| ChannelConfig {
        freq_offset_hz: freq,
        sample_rate_offset_ppm: ppm,
        ..ChannelConfig::awgn(30.0)
    };
    let scenarios = vec![
        impaired("+40 Hz, 30 dB", tx, Link::Iq(0.0), with(40.0, 0.0), 15.0),
        impaired("-123.4 Hz, 30 dB", tx, Link::RealIf, with(-123.4, 0.0), 15.0),
        impaired("+50 ppm, 30 dB", tx, Link::Iq(0.0), with(0.0, 50.0), 20.0),
        impaired("-50 ppm, 30 dB", tx, Link::RealIf, with(0.0, -50.0), 20.0),
        impaired("+40 Hz +50 ppm, 30 dB", tx, Link::Iq(0.0), with(40.0, 50.0), 20.0),
    ];
    let (report, failures) = run_all("Frequency / sample-rate offsets, B/SO3, 64-QAM, AWGN 30 dB", &scenarios, check_decodes);
    assert!(failures.is_empty(), "{report}\n{}", failures.join("\n"));
}

/// SNR sweeps for FAC/SDC/MSC thresholds per mode.
#[test]
#[ignore = "long: SNR sweep over several layouts"]
fn robustness_awgn_sweep() {
    let mut scenarios = Vec::new();
    for (mode, so) in [
        (RobustnessMode::A, SpectrumOccupancy::SO_0),
        (RobustnessMode::A, SpectrumOccupancy::SO_3),
        (RobustnessMode::B, SpectrumOccupancy::SO_3),
        (RobustnessMode::C, SpectrumOccupancy::SO_3),
        (RobustnessMode::D, SpectrumOccupancy::SO_5),
    ] {
        for snr in [25.0, 16.0, 12.0, 9.0, 6.0, 3.0] {
            let tx = TxConfig { mode, occupancy: so, msc_mode: MscMode::Qam16Sm, ..Default::default() };
            let name = format!("{} {snr} dB 16-QAM", layout_name(&tx));
            scenarios.push(impaired(&name, tx, Link::RealIf, ChannelConfig::awgn(snr), 20.0));
        }
    }
    run_all("AWGN sweep, 16-QAM prot. 1, long interleaving, real IF", &scenarios, no_check);
}

/// The harsher channel models with the robustness modes meant for them.
#[test]
#[ignore = "long: fading channels 3-6 with modes B-D"]
fn robustness_channel_models_5_6() {
    let mut scenarios = Vec::new();
    for (n, mode, so) in [
        (3, RobustnessMode::B, SpectrumOccupancy::SO_3),
        (4, RobustnessMode::B, SpectrumOccupancy::SO_3),
        (5, RobustnessMode::C, SpectrumOccupancy::SO_3),
        (5, RobustnessMode::D, SpectrumOccupancy::SO_3),
        (6, RobustnessMode::D, SpectrumOccupancy::SO_3),
    ] {
        for snr in [30.0, 20.0] {
            let tx = TxConfig { mode, occupancy: so, msc_mode: MscMode::Qam16Sm, ..Default::default() };
            let ch = ChannelConfig { seed: 40 + u64::from(n), ..ChannelConfig::drm(n, snr).unwrap() };
            let name = format!("ch{n} {} {snr} dB", layout_name(&tx));
            scenarios.push(impaired(&name, tx, Link::Iq(0.0), ch, 30.0));
        }
    }
    run_all("DRM channels 3-6, 16-QAM prot. 1, long interleaving", &scenarios, no_check);
}

// ---------------------------------------------------------------------------------
// Regression tests for receiver problems found with these loopbacks.
// ---------------------------------------------------------------------------------

/// A single FAC block announcing another configuration (two audio services instead of
/// one), as a corrupted block that passes its 8-bit CRC by chance would: the receiver
/// passes on the FACs around it but not it. Two in a row are a reconfiguration, passed
/// on from the second, and so is the return to the old configuration.
#[test]
fn single_odd_fac_not_passed_on() {
    let run_with = |odd: Vec<usize>| {
        let sc = Scenario { odd_facs: odd, seed: 5, ..Scenario::new(TxConfig::default(), Link::Iq(0.0), 8.0) };
        let o = run(&sc);
        assert!(o.fac.indices.first().is_some_and(|&j| j < 8), "first FAC {:?}", o.fac.indices.first());
        assert_eq!(o.fac.wrong, 0);
        o.fac.indices
    };
    let got = run_with(vec![12]);
    assert!(!got.contains(&12) && got.contains(&11) && got.contains(&13), "FACs passed on: {got:?}");
    let got = run_with(vec![12, 13]);
    assert!(!got.contains(&12) && got.contains(&13), "a confirmed reconfiguration: {got:?}");
    assert!(!got.contains(&14) && got.contains(&15), "and back: {got:?}");
}

/// The receiver starts with an SO3 layout; when the first FAC announces another
/// occupancy, `SymbolChain::reconfigure` rebuilds the channel estimator. It used to
/// lose the old estimator's delay line, and with it symbol 0 of the next frame: the
/// next FAC and the SDC block of the next super frame went missing. The recent
/// windows are now replayed through the new layout.
#[test]
fn no_frame_lost_at_occupancy_change() {
    let mut failures = Vec::new();
    for so in [0, 1, 2, 4, 5] {
        let tx = TxConfig { occupancy: SpectrumOccupancy::new(so).unwrap(), ..Default::default() };
        let sc = Scenario { seed: 3, ..Scenario::new(tx, Link::Iq(0.0), 8.0) };
        let o = run(&sc);
        let first_fac = o.fac.indices.first().copied();
        // The first super frame starting after the first decoded FAC.
        let want_sdc = first_fac.map(|j| j / 3 + 1);
        if o.fac.breaks > 0 || o.sdc.indices.first().copied() != want_sdc {
            failures.push(format!(
                "B/SO{so}: FAC breaks after {:?} FACs (first FAC frame {first_fac:?}); first SDC block {:?}, expected {want_sdc:?}",
                o.fac.break_at,
                o.sdc.indices.first()
            ));
        }
    }
    assert!(failures.is_empty(), "\n  {}", failures.join("\n  "));
}

/// The sample-rate-offset acquisition (`rx/chanest/track.rs`, end of the 4 s
/// acquisition) used to measure the drift of the *integer* index of the strongest
/// impulse-response bin (Dream's method). One bin is ≈ 5 samples in mode B/SO3, so
/// a single bin flip within the 4 s window read as ≈ 1.3 Hz (28 ppm). The peak is
/// now interpolated and the drift fitted by least squares.
#[test]
fn no_spurious_sro_on_clean_signal() {
    let tx = TxConfig { interleaving: Interleaving::Short, ..Default::default() };
    let sc = Scenario { name: "B/SO3 64-QAM short".into(), decode_msc: true, seed: 7, ..Scenario::new(tx, Link::Iq(0.0), 12.0) };
    let o = run(&sc);
    println!("{}\n{}", header(), row(&sc, &o));
    assert!(o.sro_hz.abs() < 0.3, "SRO estimate {:.3} Hz on a signal without sample-rate offset", o.sro_hz);
}

/// Robustness-mode detection for mode A with 4.5 kHz occupancy is marginal: the
/// best/second-best score ratio of a clean A/SO0 signal over 16 symbols is only
/// ~4–7 (mode A's guard is 1/10 of the symbol, and the other modes' scores are
/// noisy for a narrow signal). With Dream's threshold of 8 the first FAC took
/// 2.0–4.0 s (other mode A layouts: 2.0 s). A longer observation with a lower
/// threshold now covers weak or narrow signals.
#[test]
fn fast_mode_a_so0_acquisition() {
    let mut frames = Vec::new();
    for seed in 0..12 {
        let tx = TxConfig { mode: RobustnessMode::A, occupancy: SpectrumOccupancy::SO_0, ..Default::default() };
        let sc = Scenario { seed: 1000 + seed, ..Scenario::new(tx, Link::Iq(0.0), 8.0) };
        frames.push(run(&sc).first_fac_frame);
    }
    let times: Vec<String> = frames.iter().map(|f| f.map_or("-".into(), |f| format!("{:.1}", (f + 1) as Real * FRAME_S))).collect();
    println!("A/SO0 first FAC after (s): {}", times.join(" "));
    // 2.4 s = the FAC decoded while the sixth frame is pushed.
    assert!(frames.iter().all(|f| f.is_some_and(|f| f < 6)), "first FAC after (s): {}", times.join(" "));
}

/// A virtual audio cable carries the ±1 LSB dither of an idle player until the SDR
/// audio starts, and the dither's spectral lines can pass the pilot test of the
/// frequency search (seen live: a false DC carrier at 13914 Hz, then 4 s until the
/// no-FAC timeout restarted the search). The receiver now drops an acquisition that
/// no FAC has confirmed as soon as the input power rises by 10 dB. Here a faint fake
/// pilot pattern (DC 5 kHz, −100 dBFS) precedes the real signal (DC 12 kHz).
#[test]
fn acquisition_dropped_when_a_signal_appears() {
    let fs = Real::from(SAMPLE_RATE);
    let mut rx = receiver_for(Link::RealIf);
    let mut rng = Rng::new(11);
    let mut found: Vec<(Real, Real)> = Vec::new();
    let mut restarts: Vec<Real> = Vec::new();
    let mut first_fac: Option<Real> = None;
    let mut pushed = 0usize;
    let mut feed = |rx: &mut Receiver, pcm: &[f32], pushed: &mut usize| {
        *pushed += pcm.len();
        let t = *pushed as Real / fs;
        for ev in rx.push(pcm) {
            match ev {
                ReceiverEvent::SignalFound { dc_hz, .. } => found.push((t, dc_hz)),
                ReceiverEvent::Restarted => restarts.push(t),
                ReceiverEvent::Fac(_) => {
                    first_fac.get_or_insert(t);
                }
                _ => {}
            }
        }
    };

    let lead: Vec<f32> = (0..(1.5 * fs) as usize)
        .map(|n| {
            let t = n as Real / fs;
            let tones: Real = [5750.0, 7250.0, 8000.0].iter().map(|f| (std::f64::consts::TAU * f * t).sin()).sum();
            (1e-5 * tones + 1e-6 * rng.gaussian()) as f32
        })
        .collect();
    for chunk in lead.chunks(4800) {
        feed(&mut rx, chunk, &mut pushed);
    }
    let onset = pushed as Real / fs;

    let mut tx = Transmitter::new(TxConfig::default()).expect("valid transmitter configuration");
    let mut out_stage = output_for(tx.layout(), Link::RealIf);
    let cap = tx.msc_capacity();
    let mut pcm = Vec::new();
    for _ in 0..15 {
        let fac = test_fac(&mut rng);
        let msc = rng.bits(cap.total_bits());
        let sdc = (tx.frame_index() == 0).then(|| rng.bytes(tx.sdc_capacity_bytes()));
        let baseband = tx.transmit_frame(&fac, &msc, sdc.as_deref()).expect("transmit");
        pcm.clear();
        out_stage.process(&baseband, &mut pcm);
        feed(&mut rx, &pcm, &mut pushed);
    }

    let summary = format!("signal found {found:?}, restarts {restarts:?}, first FAC {first_fac:?}, onset {onset} s");
    println!("{summary}");
    assert!(found.first().is_some_and(|&(t, dc)| t <= onset && (dc - 5000.0).abs() < 10.0), "no false acquisition: {summary}");
    assert!(restarts.first().is_some_and(|&t| t > onset && t < onset + 0.5), "{summary}");
    assert!(found.iter().any(|&(t, dc)| t > onset && (dc - 12000.0).abs() < 10.0), "{summary}");
    assert!(first_fac.is_some_and(|t| t < onset + 3.2), "{summary}");
}
