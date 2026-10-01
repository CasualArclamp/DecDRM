//! Plot data preparation: turns the engine's raw [`Visuals`] into ready-to-draw point
//! lists with physical axes (kHz, carrier index, ms, dB).
//!
//! This runs once per new snapshot (not once per painted frame), and is kept free of
//! egui so it can be unit-tested; `panels::plots` does the drawing.

use decdrm_core::Cplx;
use decdrm_core::fac::{ChannelParams, MscMode, SdcMode};
use decdrm_core::params::{RobustnessMode, SAMPLE_RATE, carrier_range};
use decdrm_core::rx::{ChainVisuals, DelayDoppler, PdsAxis, Visuals};
use decdrm_core::tables;
use decdrm_engine::{AudioSpectrum, Snapshot};
use std::f64::consts::PI;

/// Points of a line or scatter plot, `[x, y]`.
pub type Points = Vec<[f64; 2]>;

/// Most points drawn per constellation (the MSC of a 20 kHz mode-A signal has ~6000
/// cells per frame; more would only cost drawing time).
pub const MAX_CONSTELLATION_POINTS: usize = 8000;

/// Floor of the dB plots relative to their peak.
pub const DB_FLOOR: f64 = -60.0;

/// A spectrum ready to draw: the receiver's input or the transmitter's output.
#[derive(Debug, Clone, Default)]
pub struct SpectrumPlot {
    /// (kHz, dB).
    pub points: Points,
    /// Frequency span to show, kHz.
    pub span_khz: (f64, f64),
    /// Level span to show, dB.
    pub db_range: (f64, f64),
    /// Occupied band of the DRM signal, kHz.
    pub band_khz: Option<(f64, f64)>,
    /// DRM DC carrier, kHz.
    pub dc_khz: Option<f64>,
}

impl SpectrumPlot {
    /// `db` holds bins from `centre − span/2` to `centre + span/2` (Hz); for a real
    /// signal only the upper half is shown. `dc_hz` and `band_hz` mark the DRM signal.
    pub fn new(
        db: &[f64],
        centre_hz: f64,
        span_hz: f64,
        real: bool,
        dc_hz: Option<f64>,
        band_hz: Option<(f64, f64)>,
    ) -> Self {
        let span = if span_hz > 0.0 {
            span_hz
        } else {
            f64::from(SAMPLE_RATE)
        };
        let points = spectrum_points(db, centre_hz, span, real);
        let lo = if real {
            centre_hz
        } else {
            centre_hz - span / 2.0
        };
        Self {
            span_khz: (lo / 1e3, (centre_hz + span / 2.0) / 1e3),
            db_range: level_range(points.iter().map(|p| p[1])),
            points,
            band_khz: band_hz.map(|(a, b)| (a / 1e3, b / 1e3)),
            dc_khz: dc_hz.map(|f| f / 1e3),
        }
    }

    /// The receiver's input spectrum.
    pub fn from_visuals(v: &Visuals) -> Self {
        Self::new(
            &v.spectrum_db,
            v.spectrum_centre_hz,
            v.spectrum_span_hz,
            v.real_input,
            v.dc_hz,
            v.signal_band_hz,
        )
    }
}

/// Lowest level of the audio spectrum plot, dBFS.
pub const AUDIO_FLOOR_DB: f64 = -120.0;

/// The decoded audio's spectrum ready to draw.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioPlot {
    /// (kHz, dBFS), from 0 Hz to half the sample rate, floored at [`AUDIO_FLOOR_DB`].
    pub points: Points,
    /// Half the sample rate, kHz (the right edge of the plot).
    pub top_khz: f64,
    pub bin_hz: f64,
    pub sample_rate: u32,
    pub channels: u8,
    /// Decoder description (e.g. "HE-AAC v2 …").
    pub codec: String,
}

impl AudioPlot {
    pub fn new(s: &AudioSpectrum, codec: &str) -> Self {
        Self {
            points: s
                .db
                .iter()
                .enumerate()
                .map(|(j, &db)| [j as f64 * s.bin_hz / 1e3, db.max(AUDIO_FLOOR_DB)])
                .collect(),
            top_khz: f64::from(s.sample_rate) / 2e3,
            bin_hz: s.bin_hz,
            sample_rate: s.sample_rate,
            channels: s.channels,
            codec: codec.to_string(),
        }
    }
}

/// Everything the plot tabs draw, derived from one snapshot.
#[derive(Debug, Clone, Default)]
pub struct PlotData {
    /// Input spectrum.
    pub spectrum: SpectrumPlot,
    /// Spectrum of the decoded audio.
    pub audio: AudioPlot,
    /// Equalised cells of the latest complete frame / SDC block / multiplex frame: (I, Q).
    pub fac: Points,
    pub sdc: Points,
    pub msc: Points,
    /// Ideal constellation points for the signalled modulations (empty if unknown).
    pub fac_ideal: Points,
    pub sdc_ideal: Points,
    pub msc_ideal: Points,
    /// Channel magnitude: (carrier index, dB).
    pub chan_db: Points,
    /// Group delay between neighbouring carriers: (carrier index, ms).
    pub group_delay_ms: Points,
    /// Display range for the group delay, ms.
    pub group_delay_range: (f64, f64),
    /// Power delay profile relative to its peak: (delay, dB).
    pub pds: Points,
    /// `true` if the PDS x axis is in ms (else in impulse-response samples).
    pub pds_in_ms: bool,
    /// Guard interval on the PDS axis, ms.
    pub guard_ms: Option<(f64, f64)>,
    /// Estimated begin and end of the channel impulse response, ms.
    pub spread_ms: Option<(f64, f64)>,
    /// MSC SNR per carrier: (carrier index, dB).
    pub snr: Points,
    /// Carrier-index range of the current layout, for the per-carrier plots.
    pub carriers: Option<(f64, f64)>,
    /// The latest delay–Doppler map (one a second, once a few seconds are tracked).
    pub delay_doppler: Option<DelayDoppler>,
}

impl PlotData {
    pub fn from_snapshot(snap: &Snapshot) -> Self {
        let v = &snap.visuals;
        let rx = &snap.rx;
        let chain = &v.chain;
        let (chan_db, group_delay_ms) = channel_curves(chain, rx.mode);
        let pds = pds_plot(&chain.pds, chain.pds_axis.as_ref());
        let carriers = match (rx.mode, rx.occupancy) {
            (Some(m), Some(so)) => carrier_range(m, so).map(|(a, b)| (f64::from(a), f64::from(b))),
            _ => None,
        };
        let ideal = IdealPoints::new(snap.channel.as_ref());
        Self {
            spectrum: SpectrumPlot::from_visuals(v),
            audio: AudioPlot::new(&snap.audio_spectrum, &snap.audio.codec),
            fac: constellation_points(&chain.fac),
            sdc: constellation_points(&chain.sdc),
            msc: constellation_points(&chain.msc),
            fac_ideal: ideal.fac,
            sdc_ideal: ideal.sdc,
            msc_ideal: ideal.msc,
            group_delay_range: robust_range(group_delay_ms.iter().map(|p| p[1]), 2.0),
            chan_db,
            group_delay_ms,
            pds: pds.points,
            pds_in_ms: pds.in_ms,
            guard_ms: pds.guard_ms,
            spread_ms: pds.spread_ms,
            snr: chain
                .snr_profile
                .iter()
                .map(|&(k, db)| [f64::from(k), db])
                .collect(),
            carriers,
            delay_doppler: chain.delay_doppler.clone(),
        }
    }
}

/// A spectrum as (kHz, dB). Bin `j` of `db` lies at `centre − span/2 + j·span/N`;
/// for a real signal only the upper (non-negative) half is meaningful and returned.
pub fn spectrum_points(db: &[f64], centre_hz: f64, span_hz: f64, real: bool) -> Points {
    let n = db.len();
    if n == 0 {
        return Vec::new();
    }
    let df = span_hz / n as f64;
    let f0 = centre_hz - span_hz / 2.0;
    db.iter()
        .enumerate()
        .map(|(j, &v)| [(f0 + j as f64 * df) / 1e3, v])
        .filter(|p| !real || p[0] >= centre_hz / 1e3)
        .collect()
}

/// Display range for a dB trace: the bottom at the lowest value rounded down to 10 dB,
/// but at most 120 dB below the peak; the top at the next 10 dB above the peak plus
/// 3 dB and a tenth of the data's range (room for the band label above the trace,
/// also on a short plot of a deep range); at least 40 dB shown. Empty data gives
/// −100…0 dB.
pub fn level_range(values: impl Iterator<Item = f64>) -> (f64, f64) {
    let (lo, hi) = values
        .filter(|v| v.is_finite())
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(v), hi.max(v))
        });
    if lo > hi {
        return (-100.0, 0.0);
    }
    let floor = ((lo / 10.0).floor() * 10.0).max(((hi - 120.0) / 10.0).floor() * 10.0);
    let top = ((hi + 3.0 + 0.1 * (hi - floor).clamp(40.0, 120.0)) / 10.0).ceil() * 10.0;
    (floor.min(top - 40.0), top)
}

/// Equalised cells as (I, Q) points, thinned to at most
/// [`MAX_CONSTELLATION_POINTS`] by taking every n-th cell.
pub fn constellation_points(cells: &[Cplx]) -> Points {
    let step = cells.len().div_ceil(MAX_CONSTELLATION_POINTS).max(1);
    cells
        .iter()
        .step_by(step)
        .filter(|c| c.re.is_finite() && c.im.is_finite())
        .map(|c| [c.re, c.im])
        .collect()
}

/// Per-axis amplitude levels of the SDC constellation (ES 201 980 §7.4, the same
/// tables the demapper uses).
pub fn sdc_levels(mode: SdcMode) -> &'static [f64] {
    match mode {
        SdcMode::Qam4 => &tables::QAM4,
        SdcMode::Qam16 => &tables::QAM16,
    }
}

/// Per-axis amplitude levels of the MSC constellation. The hierarchical 64-QAM
/// variants only map bits differently: their points are those of standard 64-QAM.
pub fn msc_levels(mode: MscMode) -> &'static [f64] {
    match mode {
        MscMode::Qam16Sm => &tables::QAM16,
        MscMode::Qam64Sm | MscMode::Qam64HmSym | MscMode::Qam64HmMix => &tables::QAM64_SM,
    }
}

/// The ideal points of a square constellation: every combination of the per-axis
/// levels as (I, Q).
pub fn ideal_points(levels: &[f64]) -> Points {
    levels
        .iter()
        .flat_map(|&i| levels.iter().map(move |&q| [i, q]))
        .collect()
}

/// Ideal points of the three channels: the FAC is always 4-QAM, the SDC and MSC
/// modulations come from the latest FAC (none before it).
struct IdealPoints {
    fac: Points,
    sdc: Points,
    msc: Points,
}

impl IdealPoints {
    fn new(channel: Option<&ChannelParams>) -> Self {
        Self {
            fac: ideal_points(&tables::QAM4),
            sdc: channel.map_or_else(Vec::new, |c| ideal_points(sdc_levels(c.sdc_mode))),
            msc: channel.map_or_else(Vec::new, |c| ideal_points(msc_levels(c.msc_mode))),
        }
    }
}

/// Channel magnitude |H|² in dB per carrier, and the group delay
/// τ = −Δφ / (2π·Δf) between neighbouring carriers in ms (plotted half-way between
/// them). The group delay needs the carrier spacing, i.e. the robustness mode.
pub fn channel_curves(chain: &ChainVisuals, mode: Option<RobustnessMode>) -> (Points, Points) {
    let k = |c: usize| f64::from(chain.kmin) + c as f64;
    let mag = chain
        .chan
        .iter()
        .enumerate()
        .map(|(c, h)| [k(c), 10.0 * h.norm_sqr().max(1e-12).log10()])
        .collect();
    let gd = match mode {
        Some(m) => {
            let df = m.carrier_spacing();
            chain
                .chan
                .windows(2)
                .enumerate()
                .map(|(c, w)| {
                    let dphi = (w[1] * w[0].conj()).arg();
                    [k(c) + 0.5, -dphi / (2.0 * PI * df) * 1e3]
                })
                .collect()
        }
        None => Vec::new(),
    };
    (mag, gd)
}

/// Display range that ignores outliers: the 5th…95th percentile, widened by 20 % and
/// to at least `min_span`. Empty data gives `(-min_span/2, min_span/2)`.
pub fn robust_range(values: impl Iterator<Item = f64>, min_span: f64) -> (f64, f64) {
    let mut v: Vec<f64> = values.filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return (-min_span / 2.0, min_span / 2.0);
    }
    v.sort_by(f64::total_cmp);
    let pick = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    let (lo, hi) = (pick(0.05), pick(0.95));
    let mid = (lo + hi) / 2.0;
    let half = ((hi - lo) * 1.2).max(min_span) / 2.0;
    (mid - half, mid + half)
}

/// Prepared power delay profile.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PdsPlot {
    pub points: Points,
    pub in_ms: bool,
    pub guard_ms: Option<(f64, f64)>,
    pub spread_ms: Option<(f64, f64)>,
}

/// The averaged power delay profile (linear power, ordered by delay) in dB relative
/// to its peak, floored at [`DB_FLOOR`]. With the engine's [`PdsAxis`] the x axis is
/// the delay in ms (`start_ms + i·step_ms`) and the guard interval and estimated
/// delay spread come along; without it (or with a malformed one) it is the index.
pub fn pds_plot(pds: &[f64], axis: Option<&PdsAxis>) -> PdsPlot {
    let peak = pds
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .fold(0.0_f64, f64::max);
    if pds.is_empty() || peak <= 0.0 {
        return PdsPlot::default();
    }
    let db = |p: f64| (10.0 * (p / peak).max(1e-30).log10()).max(DB_FLOOR);
    let finite = |a: f64, b: f64| (a.is_finite() && b.is_finite()).then_some((a, b));
    match axis.filter(|a| a.step_ms.is_finite() && a.step_ms > 0.0 && a.start_ms.is_finite()) {
        Some(a) => PdsPlot {
            points: pds
                .iter()
                .enumerate()
                .map(|(i, &p)| [a.start_ms + i as f64 * a.step_ms, db(p)])
                .collect(),
            in_ms: true,
            guard_ms: finite(a.guard_ms.0, a.guard_ms.1),
            spread_ms: finite(a.pds_begin_ms, a.pds_end_ms),
        },
        None => PdsPlot {
            points: pds
                .iter()
                .enumerate()
                .map(|(i, &p)| [i as f64, db(p)])
                .collect(),
            ..PdsPlot::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdrm_core::fac::Interleaving;
    use decdrm_core::params::SpectrumOccupancy;
    use decdrm_core::rx::RxStatus;

    fn visuals(n: usize, real: bool) -> Visuals {
        Visuals {
            spectrum_db: (0..n).map(|j| -(j as f64)).collect(),
            spectrum_centre_hz: 0.0,
            spectrum_span_hz: 48_000.0,
            real_input: real,
            ..Default::default()
        }
    }

    #[test]
    fn spectrum_axis() {
        let s = SpectrumPlot::from_visuals(&visuals(8, false));
        let p = &s.points;
        assert_eq!(p.len(), 8);
        assert_eq!(p[0], [-24.0, 0.0]);
        assert_eq!(p[4], [0.0, -4.0], "bin N/2 is 0 Hz");
        assert_eq!(p[7][0], 18.0);
        assert_eq!(s.span_khz, (-24.0, 24.0));

        let s = SpectrumPlot::from_visuals(&visuals(8, true));
        assert_eq!(s.points.len(), 4, "real input: upper half only");
        assert_eq!(s.points[0][0], 0.0);
        assert_eq!(s.span_khz, (0.0, 24.0));
        let empty = SpectrumPlot::from_visuals(&Visuals::default());
        assert!(empty.points.is_empty());
        assert_eq!(empty.span_khz, (-24.0, 24.0), "an unset span means 48 kHz");

        // The transmitter's spectrum with its markers.
        let tx = SpectrumPlot::new(
            &[-50.0; 16],
            0.0,
            48_000.0,
            true,
            Some(12_000.0),
            Some((7_000.0, 17_000.0)),
        );
        assert_eq!(tx.points.len(), 8);
        assert_eq!((tx.dc_khz, tx.band_khz), (Some(12.0), Some((7.0, 17.0))));
        assert_eq!(tx.db_range, (-80.0, -40.0), "at least 40 dB shown");
    }

    #[test]
    fn audio_axis() {
        let s = AudioSpectrum {
            db: vec![-10.0, -200.0, -30.0],
            bin_hz: 11.71875,
            sample_rate: 24_000,
            channels: 2,
        };
        let a = AudioPlot::new(&s, "HE-AAC");
        assert_eq!(
            a.points,
            vec![
                [0.0, -10.0],
                [0.01171875, AUDIO_FLOOR_DB],
                [0.0234375, -30.0]
            ]
        );
        assert_eq!((a.top_khz, a.channels), (12.0, 2));
        assert_eq!(a.codec, "HE-AAC");
        assert!(
            AudioPlot::new(&AudioSpectrum::default(), "")
                .points
                .is_empty()
        );
    }

    #[test]
    fn level_ranges() {
        // 45 dB of data: 3 + 4.5 dB above the peak, up to the next 10 dB.
        assert_eq!(
            level_range([-35.0, -80.0, -62.5].into_iter()),
            (-80.0, -20.0)
        );
        assert_eq!(level_range(std::iter::empty()), (-100.0, 0.0));
        // Very deep noise floors are cut at 120 dB below the peak (then 15 dB above it) …
        assert_eq!(level_range([-3.0, -300.0].into_iter()), (-130.0, 20.0));
        // … and at least 40 dB are always shown (non-finite values are ignored).
        assert_eq!(
            level_range([f64::NEG_INFINITY, -20.0].into_iter()),
            (-50.0, -10.0)
        );
    }

    #[test]
    fn constellation_thinning() {
        let cells: Vec<Cplx> = (0..20_000).map(|i| Cplx::new(i as f64, -1.0)).collect();
        let p = constellation_points(&cells);
        assert!(p.len() <= MAX_CONSTELLATION_POINTS && p.len() >= MAX_CONSTELLATION_POINTS / 2);
        assert_eq!(p[1], [3.0, -1.0], "every third cell");
        let few = constellation_points(&[Cplx::new(0.5, 0.5), Cplx::new(f64::NAN, 0.0)]);
        assert_eq!(few, vec![[0.5, 0.5]], "non-finite cells are dropped");
    }

    fn channel(sdc_mode: SdcMode, msc_mode: MscMode) -> ChannelParams {
        ChannelParams {
            enhancement: false,
            frame_index: 0,
            afs_valid: true,
            occupancy: SpectrumOccupancy::SO_3,
            interleaving: Interleaving::Long,
            msc_mode,
            sdc_mode,
            num_audio: 1,
            num_data: 0,
            reconfiguration_index: 0,
            toggle: false,
        }
    }

    #[test]
    fn ideal_constellations() {
        let qam4 = ideal_points(&tables::QAM4);
        assert_eq!(qam4.len(), 4);
        assert!(
            qam4.iter()
                .all(|p| (p[0].abs() - 0.5f64.sqrt()).abs() < 1e-9)
        );
        // Unit mean power, as the equaliser output.
        for mode in [MscMode::Qam16Sm, MscMode::Qam64Sm, MscMode::Qam64HmMix] {
            let pts = ideal_points(msc_levels(mode));
            let power =
                pts.iter().map(|p| p[0] * p[0] + p[1] * p[1]).sum::<f64>() / pts.len() as f64;
            assert!((power - 1.0).abs() < 1e-6, "{mode:?}: {power}");
        }
        assert_eq!(ideal_points(msc_levels(MscMode::Qam64HmSym)).len(), 64);
        assert_eq!(ideal_points(sdc_levels(SdcMode::Qam16)).len(), 16);

        let none = IdealPoints::new(None);
        assert_eq!(none.fac.len(), 4, "the FAC is always 4-QAM");
        assert!(none.sdc.is_empty() && none.msc.is_empty());
        let c = channel(SdcMode::Qam4, MscMode::Qam16Sm);
        let known = IdealPoints::new(Some(&c));
        assert_eq!((known.sdc.len(), known.msc.len()), (4, 16));
    }

    #[test]
    fn channel_magnitude_and_group_delay() {
        // A pure delay of 1 ms: H(k) = exp(−j2π·k·Δf·τ) → flat 0 dB, group delay 1 ms.
        let mode = RobustnessMode::B;
        let df = mode.carrier_spacing();
        let tau = 1e-3;
        let chain = ChainVisuals {
            kmin: -5,
            chan: (0..11)
                .map(|c| Cplx::from_polar(1.0, -2.0 * PI * (c as f64 - 5.0) * df * tau))
                .collect(),
            ..Default::default()
        };
        let (mag, gd) = channel_curves(&chain, Some(mode));
        assert_eq!(mag.len(), 11);
        assert_eq!(mag[0][0], -5.0);
        assert!(mag.iter().all(|p| p[1].abs() < 1e-9));
        assert_eq!(gd.len(), 10);
        assert_eq!(gd[0][0], -4.5);
        assert!(gd.iter().all(|p| (p[1] - 1.0).abs() < 1e-9), "{gd:?}");
        assert!(channel_curves(&chain, None).1.is_empty());
    }

    #[test]
    fn robust_ranges_ignore_outliers() {
        let mut v: Vec<f64> = vec![1.0; 98];
        v.push(1000.0);
        v.push(-1000.0);
        let (lo, hi) = robust_range(v.into_iter(), 2.0);
        assert_eq!((lo, hi), (0.0, 2.0));
        let (lo, hi) = robust_range((0..=100).map(f64::from), 1.0);
        assert!((lo - (50.0 - 54.0)).abs() < 1e-9 && (hi - (50.0 + 54.0)).abs() < 1e-9);
        assert_eq!(robust_range(std::iter::empty(), 4.0), (-2.0, 2.0));
    }

    fn axis() -> PdsAxis {
        PdsAxis {
            start_ms: -2.5,
            step_ms: 0.1,
            guard_ms: (0.0, 5.33),
            pds_begin_ms: -0.2,
            pds_end_ms: 3.1,
        }
    }

    #[test]
    fn pds_axis_and_db() {
        let mut pds = vec![1e-9; 100];
        pds[25] = 4.0; // main path at −2.5 + 25·0.1 = 0 ms
        pds[45] = 0.4; // echo at 2 ms, 10 dB down
        let p = pds_plot(&pds, Some(&axis()));
        assert!(p.in_ms);
        assert_eq!(p.points.len(), 100);
        let at = |delay: f64| {
            p.points
                .iter()
                .find(|q| (q[0] - delay).abs() < 1e-9)
                .map(|q| q[1])
        };
        assert_eq!(at(0.0), Some(0.0), "peak at 0 ms, 0 dB");
        assert!((at(2.0).unwrap() + 10.0).abs() < 1e-9, "echo at −10 dB");
        assert_eq!(p.points[0][0], -2.5);
        assert_eq!(p.points.iter().map(|q| q[1]).fold(0.0, f64::min), DB_FLOOR);
        assert_eq!(p.guard_ms, Some((0.0, 5.33)));
        assert_eq!(p.spread_ms, Some((-0.2, 3.1)));

        // No (or a malformed) axis: raw index axis without markers.
        let raw = pds_plot(&pds, None);
        assert!(!raw.in_ms && raw.guard_ms.is_none() && raw.spread_ms.is_none());
        assert_eq!(raw.points[25], [25.0, 0.0]);
        let bad = PdsAxis {
            step_ms: 0.0,
            ..axis()
        };
        assert!(!pds_plot(&pds, Some(&bad)).in_ms);
        let nan_spread = PdsAxis {
            pds_end_ms: f64::NAN,
            ..axis()
        };
        assert_eq!(pds_plot(&pds, Some(&nan_spread)).spread_ms, None);
        assert_eq!(pds_plot(&[0.0; 4], None), PdsPlot::default());
    }

    #[test]
    fn from_snapshot_combines_everything() {
        let mut snap = Snapshot {
            rx: RxStatus {
                mode: Some(RobustnessMode::B),
                occupancy: Some(SpectrumOccupancy::SO_3),
                ..Default::default()
            },
            visuals: visuals(16, true),
            channel: Some(channel(SdcMode::Qam16, MscMode::Qam64Sm)),
            ..Default::default()
        };
        snap.visuals.dc_hz = Some(12_000.0);
        snap.visuals.signal_band_hz = Some((7_148.4, 16_851.6));
        snap.visuals.chain.snr_profile = vec![(-103, 20.0), (103, 18.0)];
        snap.visuals.chain.pds = vec![1.0, 2.0];
        snap.visuals.chain.pds_axis = Some(axis());
        let d = PlotData::from_snapshot(&snap);
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert_eq!(d.spectrum.points.len(), 8);
        assert_eq!(d.spectrum.dc_khz, Some(12.0));
        let (lo, hi) = d.spectrum.band_khz.unwrap();
        assert!(close(lo, 7.1484) && close(hi, 16.8516), "{lo} {hi}");
        assert_eq!(d.carriers, Some((-103.0, 103.0)));
        assert_eq!(d.snr, vec![[-103.0, 20.0], [103.0, 18.0]]);
        assert!(d.fac.is_empty());
        assert_eq!((d.sdc_ideal.len(), d.msc_ideal.len()), (16, 64));
        assert!(d.pds_in_ms && d.guard_ms.is_some() && d.spread_ms.is_some());
        assert!(
            close(d.pds[1][0], -2.4) && d.pds[1][1] == 0.0,
            "{:?}",
            d.pds[1]
        );
    }
}
