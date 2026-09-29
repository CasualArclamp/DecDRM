//! History tab: signal quality (SNR, MER, WMER), channel (Doppler, delay spread,
//! sample-rate offset) and the error rates per 10 s over the last minutes, in three
//! plots sharing one time axis (seconds before the newest snapshot).
//!
//! Clicking a legend entry hides that curve (egui_plot's legend does this by itself).

use super::{Palette, placeholder};
use crate::history::{BIN_SECONDS, Checked, History, Metric, fmt_ago};
use crate::plots::Points;
use eframe::egui::{Color32, RichText, Ui};
use egui_plot::{
    Corner, HoverPosition, Legend, Line, MarkerShape, Plot, PlotBounds, PlotPoint, PlotPoints,
    Points as Scatter, uniform_grid_spacer,
};

/// Colour of a figure's curve.
fn metric_color(m: Metric, pal: &Palette) -> Color32 {
    match m {
        Metric::Snr => pal.snr,
        Metric::Mer => pal.spectrum,
        Metric::Wmer => pal.pds,
        Metric::Doppler => pal.marker,
        Metric::DelaySpread => pal.group_delay,
        Metric::Sro => pal.sdc,
    }
}

/// Colour and marker of a channel's error-rate dots.
fn checked_style(c: Checked, pal: &Palette) -> (Color32, MarkerShape) {
    match c {
        Checked::Fac => (pal.fac, MarkerShape::Circle),
        Checked::Sdc => (pal.sdc, MarkerShape::Square),
        Checked::Msc => (pal.msc, MarkerShape::Diamond),
        Checked::Audio => (pal.pds, MarkerShape::Up),
    }
}

/// One plot of figures.
struct Curves {
    id: &'static str,
    metrics: &'static [Metric],
    /// `Some(span)`: a steady range around all the curves, at least `span` wide (the dB
    /// figures, which should not jump with every tenth of a dB). `None`: the range
    /// follows the curves shown, so hiding one in the legend (say the sample-rate
    /// offset, far from the others) rescales the rest; 0 … 1 is always in view.
    min_span: Option<f64>,
}

const QUALITY: Curves = Curves {
    id: "history_quality",
    metrics: &Metric::QUALITY,
    min_span: Some(10.0),
};

const CHANNEL: Curves = Curves {
    id: "history_channel",
    metrics: &Metric::CHANNEL,
    min_span: None,
};

/// Display range of the curves of `metrics`: their extent with a margin, at least
/// `min_span` wide.
fn value_range(history: &History, metrics: &[Metric], min_span: f64) -> (f64, f64) {
    let (mut lo, mut hi) = metrics
        .iter()
        .flat_map(|&m| history.segments(m))
        .flatten()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
            (lo.min(p[1]), hi.max(p[1]))
        });
    if lo > hi {
        (lo, hi) = (0.0, min_span);
    }
    let margin = ((hi - lo) * 0.1).max(0.5);
    (lo, hi) = (lo - margin, hi + margin);
    if hi - lo < min_span {
        let mid = (lo + hi) / 2.0;
        (lo, hi) = (mid - min_span / 2.0, mid + min_span / 2.0);
    }
    (lo, hi)
}

/// Empty time right of "now", seconds (room for the newest error-rate dots).
const RIGHT_MARGIN_S: f64 = 3.0;

/// A plot on the shared time axis: `window` seconds up to now. Every plot gets the same
/// x range and y-axis width, so the three line up; the cursor is shared.
fn time_plot<'a>(id: &str, window: f64, height: f32, x_labels: bool) -> Plot<'a> {
    let spacing = if window > 120.0 {
        [10.0, 30.0, 60.0]
    } else {
        [5.0, 10.0, 30.0]
    };
    let plot = Plot::new(id)
        .height(height)
        .allow_zoom(false)
        .allow_drag(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .allow_double_click_reset(false)
        .show_crosshair(false)
        .link_cursor("history_time", [true, false])
        .show_axes([x_labels, true])
        .y_axis_min_width(44.0)
        .x_grid_spacer(uniform_grid_spacer(move |_| spacing))
        .x_axis_formatter(|mark, _| fmt_ago(mark.value))
        .legend(
            Legend::default()
                .position(Corner::LeftTop)
                .follow_insertion_order(true)
                .background_alpha(0.6),
        );
    if x_labels {
        plot.x_axis_label("time before the newest snapshot (m:ss of signal)")
    } else {
        plot
    }
}

fn position_of(pos: &HoverPosition<'_>) -> PlotPoint {
    match pos {
        HoverPosition::NearDataPoint { position, .. } | HoverPosition::Elsewhere { position } => {
            *position
        }
    }
}

/// Draw the curves of one plot (each figure possibly in several runs, one legend entry).
fn curves(ui: &mut Ui, history: &History, spec: &Curves, height: f32, pal: &Palette) {
    let window = history.window();
    let fixed = spec
        .min_span
        .map(|span| value_range(history, spec.metrics, span));
    let metrics = spec.metrics;
    let mut plot = time_plot(spec.id, window, height, false);
    if fixed.is_none() {
        // egui_plot fits the y axis to the items shown (hidden ones are left out).
        plot = plot.include_y(0.0).include_y(1.0);
    }
    plot.label_formatter(move |pos: &HoverPosition<'_>| {
        let p = position_of(pos);
        let HoverPosition::NearDataPoint { plot_name, .. } = pos else {
            return Some(format!("{} (m:ss)", fmt_ago(p.x)));
        };
        // The legend name is "label (unit)"; the value's unit is in there.
        let unit = metrics
            .iter()
            .find(|m| plot_name.starts_with(m.label()))
            .map_or("", |m| m.unit());
        Some(format!(
            "{plot_name}\n{:.2} {unit} at {} (m:ss)",
            p.y,
            fmt_ago(p.x)
        ))
    })
    .show(ui, |p| {
        p.set_plot_bounds_x(-window..=RIGHT_MARGIN_S);
        if let Some((lo, hi)) = fixed {
            p.set_plot_bounds_y(lo..=hi);
        }
        for &m in metrics {
            let color = metric_color(m, pal);
            let name = format!("{} ({})", m.label(), m.unit());
            for run in history.segments(m) {
                // A single point would not show as a line: draw it as a dot.
                if run.len() == 1 {
                    p.points(
                        Scatter::new(name.clone(), PlotPoints::new(run))
                            .color(color)
                            .radius(2.0),
                    );
                } else {
                    p.line(
                        Line::new(name.clone(), PlotPoints::new(run))
                            .color(color)
                            .width(1.3),
                    );
                }
            }
        }
    });
}

/// The error rates as dots per 10 s bin (offset a little per channel so they do not
/// hide each other); hovering shows the counts.
fn error_rates(ui: &mut Ui, history: &History, height: f32, pal: &Palette) {
    let window = history.window();
    time_plot("history_errors", window, height, true)
        .label_formatter(|pos: &HoverPosition<'_>| {
            let HoverPosition::NearDataPoint {
                plot_name,
                position,
                ..
            } = pos
            else {
                return Some(format!("{} (m:ss)", fmt_ago(position_of(pos).x)));
            };
            let channel = Checked::ALL.iter().find(|c| c.label() == *plot_name)?;
            let bin = history.bin_at(position.x)?;
            let (ok, bad) = bin.count(*channel);
            let latest = history.latest().unwrap_or(0.0);
            Some(format!(
                "{plot_name}: {bad} of {} bad ({:.1} %)\n{} … {} (m:ss)",
                ok + bad,
                position.y,
                fmt_ago(bin.start - latest),
                fmt_ago(bin.start + BIN_SECONDS - latest),
            ))
        })
        .show(ui, |p| {
            p.set_plot_bounds(PlotBounds::from_min_max(
                [-window, -5.0],
                [RIGHT_MARGIN_S, 105.0],
            ));
            for (i, &c) in Checked::ALL.iter().enumerate() {
                let (color, shape) = checked_style(c, pal);
                let offset = (i as f64 - 1.5) * BIN_SECONDS * 0.15;
                let points: Points = history
                    .error_points(c)
                    .into_iter()
                    .map(|[x, y]| [x + offset, y])
                    .collect();
                if !points.is_empty() {
                    p.points(
                        Scatter::new(c.label(), PlotPoints::new(points))
                            .shape(shape)
                            .color(color)
                            .filled(true)
                            .radius(3.5),
                    );
                }
            }
        });
}

pub fn show(ui: &mut Ui, history: &History, pal: &Palette) {
    if history.is_empty() {
        placeholder(
            ui,
            "No history yet: it fills while the receiver runs (the last five minutes of signal).",
        );
        return;
    }
    // Three plots and their titles share the height: 36 %, 36 % and 28 %.
    let title_height = 20.0;
    let plots_height = (ui.available_height() - 3.0 * title_height - 26.0).max(240.0);
    let heading = |ui: &mut Ui, text: &str| {
        ui.label(RichText::new(text).strong());
    };
    heading(ui, "Signal quality");
    curves(ui, history, &QUALITY, plots_height * 0.36, pal);
    heading(ui, "Channel");
    curves(ui, history, &CHANNEL, plots_height * 0.36, pal);
    heading(ui, "Errors per 10 s (%)");
    error_rates(ui, history, plots_height * 0.28, pal);
    ui.label(
        RichText::new(format!(
            "{} samples covering {} (m:ss) of signal; time relative to the newest snapshot; click a legend entry to hide a curve",
            history.samples(),
            fmt_ago(-history.span()).trim_start_matches('−'),
        ))
        .weak()
        .small(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdrm_core::rx::RxState;
    use decdrm_engine::{MetricsSample, Snapshot};

    #[test]
    fn ranges_cover_the_data() {
        let mut h = History::default();
        assert_eq!(
            value_range(&h, &Metric::QUALITY, 10.0),
            (-1.0, 11.0),
            "no data"
        );
        let mut s = Snapshot::default();
        s.push_metrics(MetricsSample {
            t: 1.0,
            state: RxState::Locked,
            snr_db: Some(20.0),
            mer_db: Some(18.0),
            sro_hz: -3.0,
            ..MetricsSample::default()
        });
        h.push(&s);
        let (lo, hi) = value_range(&h, &Metric::QUALITY, 10.0);
        assert!(lo < 18.0 && hi > 20.0 && hi - lo >= 10.0, "{lo} {hi}");
        let (lo, hi) = value_range(&h, &Metric::CHANNEL, 2.0);
        assert!(lo < -3.0 && hi > 0.0, "{lo} {hi}");
        assert!(QUALITY.min_span.is_some() && CHANNEL.min_span.is_none());
    }
}
