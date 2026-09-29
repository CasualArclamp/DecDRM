//! Plot tabs: input spectrum, constellations, channel, impulse response and SNR.
//!
//! The plots are monitoring displays: their axes are set from the data every frame and
//! zooming/dragging is disabled; hovering shows the value under the cursor.

use super::{Palette, placeholder};
use crate::plots::{DB_FLOOR, PlotData, Points};
use crate::settings::PlotTab;
use eframe::egui::{Color32, Ui};
use egui_plot::{HLine, HoverPosition, Line, LineStyle, Plot, PlotBounds, PlotPoints, Points as Scatter, Span, VLine};

/// Tab bar plus the selected tab.
pub fn show(ui: &mut Ui, tab: &mut PlotTab, data: &PlotData) {
    ui.horizontal(|ui| {
        for t in PlotTab::ALL {
            ui.selectable_value(tab, t, t.label());
        }
    });
    ui.separator();
    let pal = Palette::for_ui(ui);
    let avail = ui.available_size();
    match tab {
        PlotTab::Overview => overview(ui, data, &pal),
        PlotTab::Spectrum => spectrum(ui, data, &pal, avail.y),
        PlotTab::Constellations => {
            let side = (avail.x / 3.0 - 8.0).min(avail.y - 24.0).max(80.0);
            constellation_row(ui, data, &pal, side);
        }
        PlotTab::Channel => channel(ui, data, &pal),
        PlotTab::Impulse => impulse(ui, data, &pal, avail.y),
        PlotTab::Snr => snr(ui, data, &pal, avail.y),
    }
}

/// Spectrum on top, the three constellations below.
fn overview(ui: &mut Ui, data: &PlotData, pal: &Palette) {
    let avail = ui.available_size();
    let side = (avail.x / 3.0 - 8.0).min(avail.y * 0.5).max(80.0);
    let spectrum_height = (avail.y - side - 30.0).max(120.0);
    spectrum(ui, data, pal, spectrum_height);
    constellation_row(ui, data, pal, side);
}

/// Common settings of all plots.
fn base_plot<'a>(id: &str) -> Plot<'a> {
    Plot::new(id)
        .allow_zoom(false)
        .allow_drag(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .allow_double_click_reset(false)
        .show_crosshair(false)
}

/// Hover label `x unit_x, y unit_y` with the given decimals.
fn hover_label(ux: &'static str, dx: usize, uy: &'static str, dy: usize) -> impl Fn(&HoverPosition<'_>) -> Option<String> {
    move |pos: &HoverPosition<'_>| {
        let p = match pos {
            HoverPosition::NearDataPoint { position, .. } | HoverPosition::Elsewhere { position } => position,
        };
        Some(format!("{:.dx$} {ux}\n{:.dy$} {uy}", p.x, p.y))
    }
}

fn line(name: &str, points: &Points, color: Color32) -> Line<'static> {
    Line::new(name.to_string(), PlotPoints::new(points.clone())).color(color).width(1.2)
}

fn spectrum(ui: &mut Ui, data: &PlotData, pal: &Palette, height: f32) {
    let (x0, x1) = data.spectrum_khz;
    let (y0, y1) = data.spectrum_db;
    base_plot("spectrum")
        .height(height)
        .x_axis_label("frequency (kHz)")
        .y_axis_label("power (dB)")
        .label_formatter(hover_label("kHz", 2, "dB", 1))
        .show(ui, |p| {
            p.set_plot_bounds(PlotBounds::from_min_max([x0, y0], [x1, y1]));
            if let Some((lo, hi)) = data.band_khz {
                p.span(Span::new("DRM signal", lo..=hi).fill(pal.band).border_width(0.0));
            }
            if let Some(dc) = data.dc_khz {
                p.vline(VLine::new("DC carrier", dc).color(pal.marker).style(LineStyle::dashed_dense()));
            }
            if !data.spectrum.is_empty() {
                p.line(line("input spectrum", &data.spectrum, pal.spectrum).width(1.0));
            }
        });
}

/// FAC, SDC and MSC constellations side by side, each `side` × `side`.
fn constellation_row(ui: &mut Ui, data: &PlotData, pal: &Palette, side: f32) {
    ui.horizontal(|ui| {
        constellation(ui, "FAC", &data.fac, pal.fac, side);
        constellation(ui, "SDC", &data.sdc, pal.sdc, side);
        constellation(ui, "MSC", &data.msc, pal.msc, side);
    });
}

/// One constellation: fixed ±1.5 axes on a square plot without axis labels, so the
/// data area itself is square.
fn constellation(ui: &mut Ui, name: &str, points: &Points, color: Color32, side: f32) {
    ui.vertical(|ui| {
        ui.label(format!("{name} ({} cells)", points.len()));
        let radius = if points.len() > 3000 { 1.0 } else { 1.6 };
        base_plot(&format!("constellation_{name}"))
            .width(side)
            .height(side)
            .data_aspect(1.0)
            .show_axes(false)
            .label_formatter(hover_label("I", 2, "Q", 2))
            .show(ui, |p| {
                p.set_plot_bounds(PlotBounds::from_min_max([-1.5, -1.5], [1.5, 1.5]));
                p.hline(HLine::new("", 0.0).color(Color32::from_gray(128)).width(0.5));
                p.vline(VLine::new("", 0.0).color(Color32::from_gray(128)).width(0.5));
                if !points.is_empty() {
                    p.points(Scatter::new(name.to_string(), PlotPoints::new(points.clone())).color(color).radius(radius));
                }
            });
    });
}

fn carrier_bounds(data: &PlotData, points: &Points) -> (f64, f64) {
    data.carriers.or_else(|| Some((points.first()?[0], points.last()?[0]))).unwrap_or((-100.0, 100.0))
}

fn channel(ui: &mut Ui, data: &PlotData, pal: &Palette) {
    if data.chan_db.is_empty() {
        placeholder(ui, "No channel estimate yet.");
        return;
    }
    let h = (ui.available_height() - 40.0) / 2.0;
    let (k0, k1) = carrier_bounds(data, &data.chan_db);
    let (m0, m1) = crate::plots::level_range(data.chan_db.iter().map(|p| p[1]));
    ui.label("Channel magnitude |H|²");
    base_plot("channel_magnitude")
        .height(h)
        .x_axis_label("carrier")
        .y_axis_label("dB")
        .link_axis("carrier_axis", [true, false])
        .label_formatter(hover_label("", 0, "dB", 1))
        .show(ui, |p| {
            p.set_plot_bounds(PlotBounds::from_min_max([k0, m0.max(m1 - 60.0)], [k1, m1]));
            p.line(line("|H|²", &data.chan_db, pal.channel));
        });
    ui.label("Group delay");
    let (g0, g1) = data.group_delay_range;
    base_plot("group_delay")
        .height(h)
        .x_axis_label("carrier")
        .y_axis_label("ms")
        .link_axis("carrier_axis", [true, false])
        .label_formatter(hover_label("", 1, "ms", 3))
        .show(ui, |p| {
            p.set_plot_bounds(PlotBounds::from_min_max([k0, g0], [k1, g1]));
            p.line(line("group delay", &data.group_delay_ms, pal.group_delay));
        });
}

fn impulse(ui: &mut Ui, data: &PlotData, pal: &Palette, height: f32) {
    if data.pds.is_empty() {
        placeholder(ui, "No impulse response yet.");
        return;
    }
    let (x0, x1) = (data.pds.first().map_or(0.0, |p| p[0]), data.pds.last().map_or(1.0, |p| p[0]));
    let unit = if data.pds_in_ms { "ms" } else { "samples" };
    base_plot("impulse")
        .height(height)
        .x_axis_label(format!("delay ({unit})"))
        .y_axis_label("dB (rel. peak)")
        .label_formatter(hover_label(unit, 3, "dB", 1))
        .show(ui, |p| {
            p.set_plot_bounds(PlotBounds::from_min_max([x0, DB_FLOOR - 2.0], [x1.max(x0 + 1e-6), 3.0]));
            if let Some(g) = data.guard_ms {
                p.span(Span::new("guard interval", 0.0..=g).fill(pal.guard).border_width(0.0));
            }
            p.line(line("power delay profile", &data.pds, pal.pds));
        });
}

fn snr(ui: &mut Ui, data: &PlotData, pal: &Palette, height: f32) {
    if data.snr.is_empty() {
        placeholder(ui, "No per-carrier SNR yet (needs MSC decoding).");
        return;
    }
    let (k0, k1) = carrier_bounds(data, &data.snr);
    let top = data.snr.iter().map(|p| p[1]).fold(0.0_f64, f64::max);
    let top = ((top + 3.0) / 5.0).ceil() * 5.0;
    base_plot("snr")
        .height(height)
        .x_axis_label("carrier")
        .y_axis_label("SNR (dB)")
        .label_formatter(hover_label("", 0, "dB", 1))
        .show(ui, |p| {
            p.set_plot_bounds(PlotBounds::from_min_max([k0, 0.0], [k1, top.max(10.0)]));
            p.line(line("MSC SNR", &data.snr, pal.snr).fill(0.0));
        });
}
