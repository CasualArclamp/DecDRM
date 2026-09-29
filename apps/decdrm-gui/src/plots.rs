//! Plot data preparation: turns the engine's raw [`Visuals`] and [`RxStatus`] into
//! ready-to-draw point lists with physical axes (kHz, carrier index, ms, dB).
//!
//! This runs once per fetched snapshot (not once per painted frame), and is kept free
//! of egui so it can be unit-tested; `panels::plots` does the drawing.

use decdrm_core::Cplx;
use decdrm_core::params::{RobustnessMode, SAMPLE_RATE, SpectrumOccupancy, carrier_range};
use decdrm_core::rx::{ChainVisuals, RxStatus, Visuals};
use decdrm_core::tables::scattered_pilots;
use decdrm_engine::Snapshot;
use std::f64::consts::PI;

/// Points of a line or scatter plot, `[x, y]`.
pub type Points = Vec<[f64; 2]>;

/// Most points drawn per constellation (the MSC of a 20 kHz mode-A signal has ~6000
/// cells per frame; more would only cost drawing time).
pub const MAX_CONSTELLATION_POINTS: usize = 8000;

/// Floor of the dB plots relative to their peak.
pub const DB_FLOOR: f64 = -60.0;

/// Everything the plot tabs draw, derived from one snapshot.
#[derive(Debug, Clone, Default)]
pub struct PlotData {
    /// Input spectrum: (kHz, dB).
    pub spectrum: Points,
    /// Frequency span to show, kHz.
    pub spectrum_khz: (f64, f64),
    /// Level span to show, dB.
    pub spectrum_db: (f64, f64),
    /// Occupied band of the DRM signal in the input spectrum, kHz.
    pub band_khz: Option<(f64, f64)>,
    /// DRM DC carrier in the input spectrum, kHz.
    pub dc_khz: Option<f64>,
    /// Equalised cells: (I, Q).
    pub fac: Points,
    pub sdc: Points,
    pub msc: Points,
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
    /// Guard interval (0 … Tg) on the PDS axis, ms.
    pub guard_ms: Option<f64>,
    /// MSC SNR per carrier: (carrier index, dB).
    pub snr: Points,
    /// Carrier-index range of the current layout, for the per-carrier plots.
    pub carriers: Option<(f64, f64)>,
}

impl PlotData {
    pub fn from_snapshot(snap: &Snapshot) -> Self {
        let v = &snap.visuals;
        let rx = &snap.rx;
        let spectrum = spectrum_points(v);
        let chain = &v.chain;
        let (chan_db, group_delay_ms) = channel_curves(chain, rx.mode);
        let pds = pds_plot(&chain.pds, rx.mode, rx.occupancy);
        let carriers = match (rx.mode, rx.occupancy) {
            (Some(m), Some(so)) => carrier_range(m, so).map(|(a, b)| (f64::from(a), f64::from(b))),
            _ => None,
        };
        Self {
            spectrum_khz: spectrum_span_khz(v),
            spectrum_db: level_range(spectrum.iter().map(|p| p[1])),
            spectrum,
            band_khz: drm_band_hz(rx).map(|(a, b)| (a / 1e3, b / 1e3)),
            dc_khz: display_dc_hz(rx).map(|f| f / 1e3),
            fac: constellation_points(&chain.fac),
            sdc: constellation_points(&chain.sdc),
            msc: constellation_points(&chain.msc),
            group_delay_range: robust_range(group_delay_ms.iter().map(|p| p[1]), 2.0),
            chan_db,
            group_delay_ms,
            pds: pds.points,
            pds_in_ms: pds.in_ms,
            guard_ms: pds.guard_ms,
            snr: chain
                .snr_profile
                .iter()
                .map(|&(k, db)| [f64::from(k), db])
                .collect(),
            carriers,
        }
    }
}

/// The spectrum as (kHz, dB). Bin `j` of `spectrum_db` lies at
/// `centre − span/2 + j·span/N`; for a real input only the upper (non-negative)
/// half is meaningful and returned.
pub fn spectrum_points(v: &Visuals) -> Points {
    let n = v.spectrum_db.len();
    if n == 0 {
        return Vec::new();
    }
    let span = if v.spectrum_span_hz > 0.0 {
        v.spectrum_span_hz
    } else {
        f64::from(SAMPLE_RATE)
    };
    let df = span / n as f64;
    let f0 = v.spectrum_centre_hz - span / 2.0;
    v.spectrum_db
        .iter()
        .enumerate()
        .map(|(j, &db)| [(f0 + j as f64 * df) / 1e3, db])
        .filter(|p| !v.real_input || p[0] >= v.spectrum_centre_hz / 1e3)
        .collect()
}

/// Frequency span of the spectrum plot, kHz.
pub fn spectrum_span_khz(v: &Visuals) -> (f64, f64) {
    let span = if v.spectrum_span_hz > 0.0 {
        v.spectrum_span_hz
    } else {
        f64::from(SAMPLE_RATE)
    };
    let lo = if v.real_input {
        v.spectrum_centre_hz
    } else {
        v.spectrum_centre_hz - span / 2.0
    };
    (lo / 1e3, (v.spectrum_centre_hz + span / 2.0) / 1e3)
}

/// Display range for a dB trace: the top at the next 10 dB above the peak (+3 dB
/// headroom), the bottom at the lowest value rounded down to 10 dB but showing between
/// 40 and 120 dB. Empty data gives −100…0 dB.
pub fn level_range(values: impl Iterator<Item = f64>) -> (f64, f64) {
    let (lo, hi) = values
        .filter(|v| v.is_finite())
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(v), hi.max(v))
        });
    if lo > hi {
        return (-100.0, 0.0);
    }
    let top = ((hi + 3.0) / 10.0).ceil() * 10.0;
    let bottom = ((lo / 10.0).floor() * 10.0).clamp(top - 120.0, top - 40.0);
    (bottom, top)
}

/// Frequency of the DRM DC carrier in the displayed input spectrum.
///
/// For a spectrally inverted signal the receiver conjugates the input before mixing,
/// so `RxStatus::dc_frequency_hz` is reported in that conjugated domain (negated).
/// TODO(engine): report the DC frequency in the (unconjugated) input spectrum, or
/// publish the occupied band edges in `Visuals`, so the GUI need not undo this.
pub fn display_dc_hz(rx: &RxStatus) -> Option<f64> {
    rx.dc_frequency_hz.map(|f| if rx.inverted { -f } else { f })
}

/// Occupied band (lowest carrier − ½ spacing … highest carrier + ½ spacing) in the
/// displayed input spectrum, Hz. Needs the robustness mode and spectrum occupancy.
pub fn drm_band_hz(rx: &RxStatus) -> Option<(f64, f64)> {
    let dc = display_dc_hz(rx)?;
    let (mode, so) = (rx.mode?, rx.occupancy?);
    let (kmin, kmax) = carrier_range(mode, so)?;
    let df = mode.carrier_spacing();
    let lo = f64::from(kmin) * df - df / 2.0;
    let hi = f64::from(kmax) * df + df / 2.0;
    // An inverted spectrum has its carriers in mirrored order.
    Some(if rx.inverted {
        (dc - hi, dc - lo)
    } else {
        (dc + lo, dc + hi)
    })
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

/// Holds the last complete set of cells of a channel whose cells the engine collects
/// progressively: the FAC cells of a frame and the SDC cells of a super frame are
/// cleared at the frame / super-frame start and then filled symbol by symbol, so a
/// snapshot often catches a partial (or empty) set and the plot would flicker. A fresh
/// set replaces the held one when it is at least as large, or when the held one has
/// been kept for `max_age` updates (so a lost signal still clears the plot).
///
/// TODO(engine): publish only complete FAC/SDC sets (double-buffer them the way
/// `ChainVisuals::msc` already is); this helper then becomes a no-op.
#[derive(Debug, Clone, Default)]
pub struct HeldPoints {
    points: Points,
    age: u32,
    max_age: u32,
}

impl HeldPoints {
    pub fn new(max_age: u32) -> Self {
        Self {
            points: Vec::new(),
            age: 0,
            max_age,
        }
    }

    /// Offer a fresh set; returns the set to draw.
    pub fn update(&mut self, fresh: Points) -> Points {
        if fresh.len() >= self.points.len() || self.age >= self.max_age {
            self.points = fresh;
            self.age = 0;
        } else {
            self.age += 1;
        }
        self.points.clone()
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
    pub guard_ms: Option<f64>,
}

/// Geometry of the impulse-response estimate for a layout, mirroring the channel
/// estimator's `PdsTracker` (Dream `CTimeSyncTrack`): `num_pil` pilots spaced `x`
/// carriers apart (after time interpolation) give `num_pil` impulse-response samples
/// of Tu / (num_pil·x) each.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PdsGeometry {
    pub num_pil: usize,
    /// Impulse-response sample spacing, ms.
    pub step_ms: f64,
    /// Guard interval Tg, ms.
    pub guard_ms: f64,
    /// Index of the raw profile that is drawn first (the most negative delay), as in
    /// Dream's display rotation: the start of the rotated view lies half-way between
    /// the end of the guard interval and the end of the profile.
    pub rotation: usize,
}

impl PdsGeometry {
    pub fn new(mode: RobustnessMode, so: SpectrumOccupancy) -> Option<Self> {
        let (kmin, kmax) = carrier_range(mode, so)?;
        let n_car = (kmax - kmin + 1) as usize;
        let x = scattered_pilots(mode).freq_int;
        let num_pil = (n_car - 1) / x + 1;
        let tu_ms = mode.fft_size() as f64 / f64::from(SAMPLE_RATE) * 1e3;
        let (gn, gd) = mode.guard_ratio();
        let guard_ir = n_car as f64 * gn as f64 / gd as f64;
        let st_po_rot = if guard_ir as usize > num_pil {
            num_pil
        } else {
            (guard_ir + ((num_pil as f64 - guard_ir) / 2.0).ceil() + 1.0) as usize
        };
        Some(Self {
            num_pil,
            step_ms: tu_ms / (num_pil * x) as f64,
            guard_ms: mode.guard_len() as f64 / f64::from(SAMPLE_RATE) * 1e3,
            rotation: (st_po_rot - 1) % num_pil,
        })
    }
}

/// The averaged power delay profile in dB relative to its peak (floored at
/// [`DB_FLOOR`]). With a known layout the axis is the delay in ms, raw sample `r`
/// shown at `(r − num_pil)·step` when it lies at or beyond the display rotation
/// (a pre-echo) and at `r·step` otherwise; else it is the raw sample index.
///
/// TODO(engine): the engine could publish this axis (Dream's `GetAvPoDeSp` returns
/// scale, guard-interval and PDS begin/end markers) instead of the GUI re-deriving the
/// estimator's geometry.
pub fn pds_plot(
    pds: &[f64],
    mode: Option<RobustnessMode>,
    so: Option<SpectrumOccupancy>,
) -> PdsPlot {
    let peak = pds
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .fold(0.0_f64, f64::max);
    if pds.is_empty() || peak <= 0.0 {
        return PdsPlot::default();
    }
    let db = |p: f64| (10.0 * (p / peak).max(1e-30).log10()).max(DB_FLOOR);
    let geometry = match (mode, so) {
        (Some(m), Some(s)) => PdsGeometry::new(m, s).filter(|g| g.num_pil == pds.len()),
        _ => None,
    };
    match geometry {
        Some(g) => {
            let n = g.num_pil;
            let points = (0..n)
                .map(|i| {
                    let r = (g.rotation + i) % n;
                    let delay = i as f64 + g.rotation as f64 - n as f64;
                    [delay * g.step_ms, db(pds[r])]
                })
                .collect();
            PdsPlot {
                points,
                in_ms: true,
                guard_ms: Some(g.guard_ms),
            }
        }
        None => PdsPlot {
            points: pds
                .iter()
                .enumerate()
                .map(|(i, &p)| [i as f64, db(p)])
                .collect(),
            in_ms: false,
            guard_ms: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdrm_core::rx::framesync::FrameSyncState;

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
        let v = visuals(8, false);
        let p = spectrum_points(&v);
        assert_eq!(p.len(), 8);
        assert_eq!(p[0], [-24.0, 0.0]);
        assert_eq!(p[4], [0.0, -4.0], "bin N/2 is 0 Hz");
        assert_eq!(p[7][0], 18.0);
        assert_eq!(spectrum_span_khz(&v), (-24.0, 24.0));

        let v = visuals(8, true);
        let p = spectrum_points(&v);
        assert_eq!(p.len(), 4, "real input: upper half only");
        assert_eq!(p[0][0], 0.0);
        assert_eq!(spectrum_span_khz(&v), (0.0, 24.0));
        assert!(spectrum_points(&Visuals::default()).is_empty());
    }

    #[test]
    fn level_ranges() {
        assert_eq!(
            level_range([-35.0, -80.0, -62.5].into_iter()),
            (-80.0, -30.0)
        );
        assert_eq!(level_range(std::iter::empty()), (-100.0, 0.0));
        // Very deep noise floors are cut at 120 dB below the top …
        assert_eq!(level_range([-3.0, -300.0].into_iter()), (-120.0, 0.0));
        // … and at least 40 dB are always shown (non-finite values are ignored).
        assert_eq!(
            level_range([f64::NEG_INFINITY, -20.0].into_iter()),
            (-50.0, -10.0)
        );
    }

    fn locked(dc: f64, inverted: bool) -> RxStatus {
        RxStatus {
            dc_frequency_hz: Some(dc),
            inverted,
            mode: Some(RobustnessMode::B),
            occupancy: Some(SpectrumOccupancy::SO_3),
            frame_sync: FrameSyncState::Locked,
            ..Default::default()
        }
    }

    #[test]
    fn band_edges() {
        // Mode B, SO3: carriers −103…103, spacing 46.875 Hz → ±4851.6 Hz around DC.
        let (lo, hi) = drm_band_hz(&locked(12_000.0, false)).unwrap();
        let half = 103.5 * 46.875;
        assert!((lo - (12_000.0 - half)).abs() < 1e-9 && (hi - (12_000.0 + half)).abs() < 1e-9);

        // Inverted: the status reports −DC; the band is mirrored around +DC.
        let rx = locked(-12_000.0, true);
        assert_eq!(display_dc_hz(&rx), Some(12_000.0));
        let (lo, hi) = drm_band_hz(&rx).unwrap();
        assert!((lo - (12_000.0 - half)).abs() < 1e-9 && (hi - (12_000.0 + half)).abs() < 1e-9);

        // Single-sided 5 kHz occupancy lies entirely above DC.
        let rx = RxStatus {
            occupancy: Some(SpectrumOccupancy::SO_1),
            ..locked(10_000.0, false)
        };
        let (lo, _) = drm_band_hz(&rx).unwrap();
        assert!(lo > 10_000.0);

        assert_eq!(drm_band_hz(&RxStatus::default()), None);
        let no_mode = RxStatus {
            mode: None,
            ..locked(1.0, false)
        };
        assert_eq!(drm_band_hz(&no_mode), None);
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

    #[test]
    fn held_points_bridge_partial_sets() {
        let pts = |n: usize| -> Points { (0..n).map(|i| [i as f64, 0.0]).collect() };
        let mut h = HeldPoints::new(3);
        assert_eq!(h.update(pts(5)).len(), 5);
        assert_eq!(
            h.update(pts(2)).len(),
            5,
            "partial set: keep the complete one"
        );
        assert_eq!(h.update(pts(0)).len(), 5);
        assert_eq!(h.update(pts(1)).len(), 5);
        assert_eq!(h.update(pts(1)).len(), 1, "held too long: give up");
        assert_eq!(h.update(pts(5)).len(), 5);
        assert_eq!(
            h.update(pts(5)).len(),
            5,
            "equal size replaces (newer data)"
        );
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

    #[test]
    fn pds_geometry_mode_b() {
        // Mode B / SO3: 207 carriers, pilots every 2 → 104 IR samples covering Tu/2.
        let g = PdsGeometry::new(RobustnessMode::B, SpectrumOccupancy::SO_3).unwrap();
        assert_eq!(g.num_pil, 104);
        let tu_ms = 1024.0 / 48.0;
        assert!((g.step_ms * 104.0 - tu_ms / 2.0).abs() < 1e-9);
        assert!((g.guard_ms - 256.0 / 48.0).abs() < 1e-9);
        // guard in IR samples = 207/4 = 51.75 → rotation start ceil((104−51.75)/2) = 27
        // after it: 51.75 + 27 + 1 = 79.75 → 79, minus one.
        assert_eq!(g.rotation, 78);
    }

    #[test]
    fn pds_axis_and_db() {
        let g = PdsGeometry::new(RobustnessMode::B, SpectrumOccupancy::SO_3).unwrap();
        let mut pds = vec![1e-9; g.num_pil];
        pds[0] = 4.0; // main path at zero delay
        pds[g.num_pil - 2] = 0.4; // a pre-echo two samples early
        let p = pds_plot(&pds, Some(RobustnessMode::B), Some(SpectrumOccupancy::SO_3));
        assert!(p.in_ms);
        assert_eq!(p.points.len(), g.num_pil);
        let at = |delay: f64| {
            p.points
                .iter()
                .find(|q| (q[0] - delay).abs() < 1e-9)
                .map(|q| q[1])
        };
        assert_eq!(at(0.0), Some(0.0), "peak at 0 ms, 0 dB");
        assert!(
            (at(-2.0 * g.step_ms).unwrap() + 10.0).abs() < 1e-9,
            "pre-echo at −10 dB"
        );
        assert!(
            p.points.windows(2).all(|w| w[1][0] > w[0][0]),
            "delay axis increases"
        );
        assert_eq!(p.points.iter().map(|q| q[1]).fold(0.0, f64::min), DB_FLOOR);

        // Unknown layout or a length mismatch: raw index axis.
        let raw = pds_plot(&pds, None, None);
        assert!(!raw.in_ms && raw.guard_ms.is_none());
        assert_eq!(raw.points[0], [0.0, 0.0]);
        let mismatch = pds_plot(
            &pds[..10],
            Some(RobustnessMode::B),
            Some(SpectrumOccupancy::SO_3),
        );
        assert!(!mismatch.in_ms);
        assert_eq!(pds_plot(&[0.0; 4], None, None), PdsPlot::default());
    }

    #[test]
    fn from_snapshot_combines_everything() {
        let mut snap = Snapshot {
            rx: locked(12_000.0, false),
            visuals: visuals(16, true),
            ..Default::default()
        };
        snap.visuals.chain.snr_profile = vec![(-103, 20.0), (103, 18.0)];
        let d = PlotData::from_snapshot(&snap);
        assert_eq!(d.spectrum.len(), 8);
        assert_eq!(d.dc_khz, Some(12.0));
        assert!(d.band_khz.is_some());
        assert_eq!(d.carriers, Some((-103.0, 103.0)));
        assert_eq!(d.snr, vec![[-103.0, 20.0], [103.0, 18.0]]);
        assert!(d.fac.is_empty() && d.pds.is_empty());
    }
}
