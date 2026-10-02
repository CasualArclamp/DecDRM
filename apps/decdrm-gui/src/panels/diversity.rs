//! The Diversity tab: how diversity reception mixes the two KiwiSDRs' signals. Both
//! receive every MSC cell; the combiner weights each copy by its SNR in that receiver
//! (|H|²/σ², maximum-ratio combining, `decdrm_core::rx::diversity`) and adds them. The
//! tab shows the weights per carrier of the last combined frame, the SNRs and the
//! weight split frame by frame, and the constellations before and after combining.

use super::plots::{base_plot, constellation, hover_label};
use super::{Palette, placeholder};
use crate::diversity::DiversityHistory;
use crate::plots::{Points, constellation_points};
use decdrm_core::rx::MixRecord;
use decdrm_engine::DiversityView;
use eframe::egui::{self, Color32, RichText, Ui};
use egui_plot::{
    FilledArea, HLine, Legend, Line, LineStyle, MarkerShape, PlotBounds, PlotPoints, Points as Scatter, uniform_grid_spacer,
};

/// A value of a record (a series of the plots over time).
type Value = fn(&MixRecord) -> Option<f64>;

/// One record per multiplex frame: 400 ms.
const FRAME_S: f64 = 0.4;
/// Seconds of the plots over time.
const SPAN_S: f64 = 120.0;

/// Colours of KiwiSDR 1, KiwiSDR 2 and their combination.
struct Colors {
    a: Color32,
    b: Color32,
    both: Color32,
    lost: Color32,
}

impl Colors {
    fn new(ui: &Ui, pal: &Palette) -> Self {
        if ui.visuals().dark_mode {
            Self {
                a: Color32::from_rgb(90, 160, 255),
                b: Color32::from_rgb(255, 165, 60),
                both: Color32::from_rgb(90, 210, 120),
                lost: pal.error,
            }
        } else {
            Self {
                a: Color32::from_rgb(20, 90, 210),
                b: Color32::from_rgb(210, 105, 0),
                both: Color32::from_rgb(20, 140, 60),
                lost: pal.error,
            }
        }
    }
}

/// `color` at `alpha` (0–255) for area fills.
fn fill(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

fn db(x: f32) -> f64 {
    10.0 * f64::from(x).max(1e-6).log10()
}

/// The tab; `combined` and `ideal` are the main MSC constellation (in diversity
/// reception the combined cells) and its ideal points.
pub fn show(ui: &mut Ui, view: Option<(&DiversityView, &DiversityHistory)>, combined: &Points, ideal: &Points, pal: &Palette) {
    let Some((d, history)) = view else {
        placeholder(ui, "Diversity reception is off: enter a second KiwiSDR in the source bar, or right-click one in Find….");
        return;
    };
    let c = Colors::new(ui, pal);
    egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
        ui.add(
            egui::Label::new(
                RichText::new(
                    "Both KiwiSDRs receive every MSC cell. Each copy is weighted by its SNR in that receiver (|H|²/σ², \
                     maximum-ratio combining) and the two are added: where one KiwiSDR's signal fades, the other carries \
                     the cell, and the SNRs add up.",
                )
                .weak()
                .small(),
            )
            .wrap(),
        );
        summary(ui, d, history, &c);
        ui.separator();
        carriers(ui, d, &c);
        ui.separator();
        over_time(ui, history, &c);
        ui.separator();
        ui.label(RichText::new("MSC constellations of the last frame").strong());
        let side = ((ui.available_width() - 24.0) / 3.0).clamp(80.0, 240.0);
        let (a, b) = (constellation_points(&d.msc[0]), constellation_points(&d.msc[1]));
        ui.horizontal(|ui| {
            constellation(ui, "KiwiSDR 1", &a, ideal, c.a, pal.ideal, side);
            constellation(ui, "KiwiSDR 2", &b, ideal, c.b, pal.ideal, side);
            constellation(ui, "Combined", combined, ideal, c.both, pal.ideal, side);
        });
    });
}

/// The weight split, the SNRs, the frame counts and the pairing.
fn summary(ui: &mut Ui, d: &DiversityView, history: &DiversityHistory, c: &Colors) {
    let s = &d.stats;
    if let Some(share) = s.share {
        share_bar(ui, share, c);
    }
    let last = history.records().iter().rev().find(|r| r.from == [true, true]);
    ui.horizontal_wrapped(|ui| {
        let text = |x: Option<f64>| x.map_or_else(|| "–".to_string(), |v| format!("{v:.1} dB"));
        let (a, b) = (last.and_then(|r| r.snr_db[0]), last.and_then(|r| r.snr_db[1]));
        let sum = last.and_then(|r| r.combined_db);
        ui.label(RichText::new("SNR of the last combined frame").weak()).on_hover_text(
            "Each KiwiSDR's SNR as the combiner weights it: the mean of |H|²/σ² over the frame's MSC cells,              with σ² measured against the combined decisions. Maximum-ratio combining adds them.",
        );
        ui.label(RichText::new(format!("KiwiSDR 1 {}", text(a))).color(c.a).monospace());
        ui.label(RichText::new(format!("KiwiSDR 2 {}", text(b))).color(c.b).monospace());
        ui.label("→");
        ui.label(RichText::new(format!("combined {}", text(sum))).color(c.both).monospace().strong());
        if let (Some(a), Some(b), Some(sum)) = (a, b, sum) {
            ui.label(RichText::new(format!("({:+.1} dB over the better one)", sum - a.max(b))).weak());
        }
    });
    let total = s.combined + s.single[0] + s.single[1] + s.lost;
    let pct = |n: u64| if total > 0 { format!(" ({:.0} %)", 100.0 * n as f64 / total as f64) } else { String::new() };
    ui.label(format!(
        "Multiplex frames: {} combined{}, {} from KiwiSDR 1 alone, {} from KiwiSDR 2 alone, {} lost, {} late",
        s.combined,
        pct(s.combined),
        s.single[0],
        s.single[1],
        s.lost,
        s.late
    ))
    .on_hover_text(
        "A frame is decoded from one KiwiSDR alone when the other did not deliver it in time (its stream \
         stalled or lags too far); late frames came after their turn and were dropped.",
    );
    let pairing = match s.lead_frames {
        Some(l) if l > 0 => format!("KiwiSDR 2's signal arrives {:.1} s after KiwiSDR 1's ({l} frames)", l as f64 * FRAME_S),
        Some(l) if l < 0 => format!("KiwiSDR 1's signal arrives {:.1} s after KiwiSDR 2's ({} frames)", -l as f64 * FRAME_S, -l),
        Some(_) => "Both KiwiSDRs' signals arrive together".to_string(),
        None => "Not paired yet: frames are matched by their content first".to_string(),
    };
    let agreement = s.agreement.map(|a| format!("; paired frames agree in {:.0} % of their cells", 100.0 * a)).unwrap_or_default();
    ui.label(RichText::new(pairing + &agreement).weak()).on_hover_text(
        "The KiwiSDRs' network delays differ, so frames are paired by their content: a frame's hard decisions \
         are compared with the other KiwiSDR's recent frames.",
    );
}

/// A bar split by the combining weight of the last combined frame.
fn share_bar(ui: &mut Ui, share: f64, c: &Colors) {
    let share = share.clamp(0.0, 1.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 20.0), egui::Sense::hover());
    let split = rect.left() + rect.width() * share as f32;
    let left = egui::Rect::from_min_max(rect.min, egui::pos2(split, rect.max.y));
    let right = egui::Rect::from_min_max(egui::pos2(split, rect.min.y), rect.max);
    let painter = ui.painter();
    painter.rect_filled(left, egui::CornerRadius::same(3), c.a);
    painter.rect_filled(right, egui::CornerRadius::same(3), c.b);
    let font = egui::FontId::proportional(12.5);
    let ink = Color32::from_gray(15);
    if left.width() > 110.0 {
        let text = format!("KiwiSDR 1  {:.0} %", 100.0 * share);
        painter.text(left.left_center() + egui::vec2(6.0, 0.0), egui::Align2::LEFT_CENTER, text, font.clone(), ink);
    }
    if right.width() > 110.0 {
        let text = format!("{:.0} %  KiwiSDR 2", 100.0 * (1.0 - share));
        painter.text(right.right_center() - egui::vec2(6.0, 0.0), egui::Align2::RIGHT_CENTER, text, font, ink);
    }
    response.on_hover_text(format!(
        "Share of the combining weight in the last combined frame: KiwiSDR 1 {:.0} %, KiwiSDR 2 {:.0} %",
        100.0 * share,
        100.0 * (1.0 - share)
    ));
}

/// Each KiwiSDR's SNR per carrier in the last combined frame, their sum, and the weight
/// split per carrier.
fn carriers(ui: &mut Ui, d: &DiversityView, c: &Colors) {
    ui.label(RichText::new("Per carrier, in the last combined frame").strong());
    let Some(m) = &d.carriers else {
        placeholder(ui, "No frame combined yet.");
        return;
    };
    let in_khz = d.spacing_hz > 0.0;
    let x = |i: usize| {
        let k = f64::from(m.kmin) + i as f64;
        if in_khz { k * d.spacing_hz / 1000.0 } else { k }
    };
    let (mut a, mut b, mut sum) = (Vec::new(), Vec::new(), Vec::new());
    let (mut xs, mut share) = (Vec::new(), Vec::new());
    for (i, (&wa, &wb)) in m.snr[0].iter().zip(&m.snr[1]).enumerate() {
        // Carriers without MSC cells in the frame (the DC carrier) are left out.
        if !(wa.is_finite() && wb.is_finite()) {
            continue;
        }
        let f = x(i);
        a.push([f, db(wa)]);
        b.push([f, db(wb)]);
        sum.push([f, db(wa + wb)]);
        xs.push(f);
        share.push(if wa + wb > 0.0 { 100.0 * f64::from(wa / (wa + wb)) } else { 50.0 });
    }
    let (Some(&x0), Some(&x1)) = (xs.first(), xs.last()) else { return };
    let lo = a.iter().chain(&b).map(|p| p[1]).fold(f64::INFINITY, f64::min);
    let hi = sum.iter().map(|p| p[1]).fold(f64::NEG_INFINITY, f64::max);
    let (lo, hi) = (((lo - 2.0) / 5.0).floor() * 5.0, ((hi + 2.0) / 5.0).ceil() * 5.0);
    let axis = if in_khz { "frequency (kHz, DC carrier at 0)" } else { "carrier" };
    let unit = if in_khz { "kHz" } else { "" };
    base_plot("diversity_carriers_snr")
        .height(170.0)
        .legend(Legend::default())
        .y_axis_label("SNR (dB)")
        .label_formatter(hover_label(unit, 2, "dB", 1))
        .show(ui, |p| {
            p.set_plot_bounds(PlotBounds::from_min_max([x0, lo], [x1, hi.max(lo + 10.0)]));
            p.line(Line::new("KiwiSDR 1", PlotPoints::new(a)).color(c.a).width(1.2));
            p.line(Line::new("KiwiSDR 2", PlotPoints::new(b)).color(c.b).width(1.2));
            p.line(Line::new("combined", PlotPoints::new(sum)).color(c.both).width(2.0));
        });
    ui.label(RichText::new("Each KiwiSDR's share of the weight per carrier").weak().small());
    let zeros = vec![0.0; xs.len()];
    let full = vec![100.0; xs.len()];
    base_plot("diversity_carriers_share")
        .height(90.0)
        .y_grid_spacer(uniform_grid_spacer(|_| [25.0, 50.0, 100.0]))
        .x_axis_label(axis)
        .y_axis_label("share (%)")
        .label_formatter(hover_label(unit, 2, "% KiwiSDR 1", 0))
        .show(ui, |p| {
            p.set_plot_bounds(PlotBounds::from_min_max([x0, 0.0], [x1, 100.0]));
            p.add(FilledArea::new("KiwiSDR 1", &xs, &zeros, &share).fill_color(fill(c.a, 170)));
            p.add(FilledArea::new("KiwiSDR 2", &xs, &share, &full).fill_color(fill(c.b, 170)));
            p.hline(HLine::new("", 50.0).color(Color32::from_gray(128)).style(LineStyle::dashed_dense()).width(0.8));
        });
}

/// Runs of consecutive records with a value: the line segments of a series (a gap where
/// a KiwiSDR gave no frame).
fn segments(records: &[&MixRecord], t: impl Fn(&MixRecord) -> f64, value: impl Fn(&MixRecord) -> Option<f64>) -> Vec<Vec<[f64; 2]>> {
    let mut out: Vec<Vec<[f64; 2]>> = Vec::new();
    let mut prev: Option<u64> = None;
    for r in records {
        match value(r) {
            Some(v) => {
                if prev.is_none_or(|p| r.seq != p + 1) {
                    out.push(Vec::new());
                }
                if let Some(run) = out.last_mut() {
                    run.push([t(r), v]);
                }
                prev = Some(r.seq);
            }
            None => prev = None,
        }
    }
    out
}

/// The SNRs and the weight split frame by frame, the last two minutes.
fn over_time(ui: &mut Ui, history: &DiversityHistory, c: &Colors) {
    ui.label(RichText::new("Frame by frame, the last two minutes").strong());
    let records = history.records();
    let Some(last) = records.back() else {
        placeholder(ui, "No frames yet.");
        return;
    };
    let t = |r: &MixRecord| (r.seq as f64 - last.seq as f64) * FRAME_S;
    let shown: Vec<&MixRecord> = records.iter().filter(|r| t(r) >= -SPAN_S).collect();
    let x0 = shown.first().map_or(-SPAN_S, |r| t(r)).min(-10.0);
    let values = shown.iter().flat_map(|r| r.snr_db.iter().flatten().chain(r.combined_db.iter()));
    let (lo, hi) = values.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    let (lo, hi) = if lo.is_finite() { (((lo - 2.0) / 5.0).floor() * 5.0, ((hi + 2.0) / 5.0).ceil() * 5.0) } else { (0.0, 30.0) };
    // Frames from one KiwiSDR alone, and lost frames, as marks along the bottom.
    let mark = lo + 0.04 * (hi - lo).max(10.0);
    let marks = |pred: fn(&MixRecord) -> bool| -> Vec<[f64; 2]> { shown.iter().filter(|r| pred(r)).map(|r| [t(r), mark]).collect() };
    let alone = [marks(|r| r.from == [true, false]), marks(|r| r.from == [false, true])];
    let lost = marks(|r| r.from == [false, false]);
    base_plot("diversity_time_snr")
        .height(160.0)
        .legend(Legend::default())
        .y_axis_label("SNR (dB)")
        .label_formatter(hover_label("s", 1, "dB", 1))
        .show(ui, |p| {
            p.set_plot_bounds(PlotBounds::from_min_max([x0, lo], [0.0, hi.max(lo + 10.0)]));
            let series: [(&str, Color32, f32, Value); 3] = [
                ("KiwiSDR 1", c.a, 1.2, |r| r.snr_db[0]),
                ("KiwiSDR 2", c.b, 1.2, |r| r.snr_db[1]),
                ("combined", c.both, 2.0, |r| r.combined_db),
            ];
            for (name, color, width, value) in series {
                for run in segments(&shown, t, value) {
                    p.line(Line::new(name, PlotPoints::new(run)).color(color).width(width));
                }
            }
            for (name, color, pts) in [("KiwiSDR 1 alone", c.a, &alone[0]), ("KiwiSDR 2 alone", c.b, &alone[1])] {
                if !pts.is_empty() {
                    p.points(Scatter::new(name, PlotPoints::new(pts.clone())).color(color).radius(2.5));
                }
            }
            if !lost.is_empty() {
                p.points(Scatter::new("lost", PlotPoints::new(lost)).color(c.lost).shape(MarkerShape::Cross).radius(3.5));
            }
        });
    ui.label(RichText::new("KiwiSDR 1's share of the weight in each combined frame (the rest is KiwiSDR 2's)").weak().small());
    let share = segments(&shown, t, |r| r.share.map(|s| 100.0 * s));
    base_plot("diversity_time_share")
        .height(90.0)
        .y_grid_spacer(uniform_grid_spacer(|_| [25.0, 50.0, 100.0]))
        .x_axis_label("time (s)")
        .y_axis_label("share (%)")
        .label_formatter(hover_label("s", 1, "% KiwiSDR 1", 0))
        .show(ui, |p| {
            p.set_plot_bounds(PlotBounds::from_min_max([x0, 0.0], [0.0, 100.0]));
            p.hline(HLine::new("", 50.0).color(Color32::from_gray(128)).style(LineStyle::dashed_dense()).width(0.8));
            for run in share {
                p.line(Line::new("KiwiSDR 1", PlotPoints::new(run)).color(c.a).width(1.2).fill(0.0).fill_alpha(0.35));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_break_at_gaps() {
        let rec = |seq, a: Option<f64>| MixRecord { seq, snr_db: [a, None], ..MixRecord::default() };
        let records = [rec(0, Some(1.0)), rec(1, Some(2.0)), rec(2, None), rec(3, Some(4.0)), rec(5, Some(6.0))];
        let refs: Vec<&MixRecord> = records.iter().collect();
        let runs = segments(&refs, |r| r.seq as f64, |r| r.snr_db[0]);
        assert_eq!(runs, vec![vec![[0.0, 1.0], [1.0, 2.0]], vec![[3.0, 4.0]], vec![[5.0, 6.0]]]);
    }
}
