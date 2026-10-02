//! Plot tabs: input spectrum and its waterfall, constellations, decoded audio, channel
//! and its fading over time, impulse response, delay–Doppler map, SNR, diversity
//! combining (`panels::diversity`) and the reception history.
//!
//! The plots are monitoring displays: their axes are set from the data every frame and
//! zooming/dragging is disabled; hovering shows the value under the cursor.

use super::{Palette, placeholder};
use crate::diversity::DiversityHistory;
use crate::history::History;
use crate::plots::{AUDIO_FLOOR_DB, AudioPlot, DB_FLOOR, PlotData, Points, SpectrumPlot};
use crate::settings::PlotTab;
use crate::fading::FadingMap;
use crate::ring_image::RingImage;
use crate::waterfall::Waterfall;
use decdrm_core::rx::DelayDoppler;
use decdrm_core::rx::scatter::FLOOR_DB;
use eframe::egui::{Color32, RichText, TextureHandle, TextureOptions, Ui};
use egui_plot::{
    HLine, HoverPosition, Line, LineStyle, MarkerShape, Plot, PlotBounds, PlotImage, PlotPoint,
    PlotPoints, Points as Scatter, Span, VLine,
};

/// The images of the map tabs on the GPU, updated while their tab is shown.
#[derive(Default)]
pub struct PlotTextures {
    waterfall: RingImage,
    fading: RingImage,
    delay_doppler: Option<(TextureHandle, DelayDoppler)>,
}

/// Tab bar plus the selected tab.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut Ui,
    tab: &mut PlotTab,
    data: &PlotData,
    waterfall: &Waterfall,
    fading: &FadingMap,
    textures: &mut PlotTextures,
    waterfall_fit: &mut bool,
    history: &History,
    diversity: Option<(&decdrm_engine::DiversityView, &DiversityHistory)>,
) {
    ui.horizontal_wrapped(|ui| {
        for t in PlotTab::ALL {
            // The Diversity tab only while diversity reception runs (or is selected).
            if t == PlotTab::Diversity && diversity.is_none() && *tab != t {
                continue;
            }
            ui.selectable_value(tab, t, t.label());
        }
    });
    ui.separator();
    let pal = Palette::for_ui(ui);
    let avail = ui.available_size();
    match tab {
        PlotTab::Overview => overview(ui, data, &pal),
        PlotTab::Spectrum => spectrum_plot(
            ui,
            "spectrum",
            "input spectrum",
            &data.spectrum,
            &pal,
            avail.y,
        ),
        PlotTab::Waterfall => waterfall_plot(ui, data, waterfall, &mut textures.waterfall, waterfall_fit, &pal, avail.y - 22.0),
        PlotTab::Fading => fading_plot(ui, fading, &mut textures.fading, &pal, avail.y - 22.0),
        PlotTab::DelayDoppler => delay_doppler_plot(ui, data, &mut textures.delay_doppler, &pal, avail.y - 22.0),
        PlotTab::Constellations => {
            let side = (avail.x / 3.0 - 8.0).min(avail.y - 24.0).max(80.0);
            constellation_row(ui, data, &pal, side);
        }
        PlotTab::Audio => audio_plot(ui, &data.audio, &pal, avail.y - 22.0),
        PlotTab::Channel => channel(ui, data, &pal),
        PlotTab::Impulse => impulse(ui, data, &pal, avail.y),
        PlotTab::Snr => snr(ui, data, &pal, avail.y),
        PlotTab::Diversity => super::diversity::show(ui, diversity, &data.msc, &data.msc_ideal, &pal),
        PlotTab::History => super::history::show(ui, history, &pal),
        // Not a plot: the application draws it below the tab bar (`panels::schedule`).
        PlotTab::Schedule => {}
    }
}

/// Spectrum of the decoded audio, 0 Hz … half its sample rate, in dB relative to full
/// scale.
fn audio_plot(ui: &mut Ui, a: &AudioPlot, pal: &Palette, height: f32) {
    if a.points.is_empty() {
        placeholder(
            ui,
            "No decoded audio: the spectrum appears while an audio service is decoded.",
        );
        return;
    }
    base_plot("audio_spectrum")
        .height(height.max(120.0))
        .x_axis_label("frequency (kHz)")
        .y_axis_label("level (dBFS)")
        .label_formatter(hover_label("kHz", 2, "dBFS", 1))
        .show(ui, |p| {
            p.set_plot_bounds(PlotBounds::from_min_max(
                [0.0, AUDIO_FLOOR_DB],
                [a.top_khz.max(1.0), 0.0],
            ));
            p.line(
                line("decoded audio", &a.points, pal.msc)
                    .width(1.0)
                    .fill(AUDIO_FLOOR_DB as f32),
            );
        });
    let channels = match a.channels {
        1 => "1 channel".to_string(),
        n => format!("mean of {n} channels"),
    };
    let codec = if a.codec.is_empty() {
        String::new()
    } else {
        format!("{} — ", a.codec)
    };
    ui.label(
        RichText::new(format!(
            "{codec}decoded audio at {:.1} kHz ({channels}); {}-point FFT ({:.1} Hz bins), averaged over ~{:.1} s; a full-scale sine reads 0 dB",
            f64::from(a.sample_rate) / 1e3,
            decdrm_engine::audio_out::AUDIO_FFT_LEN,
            a.bin_hz,
            decdrm_engine::audio_out::AUDIO_AVERAGE_S,
        ))
        .weak()
        .small(),
    );
}

/// Spectrum on top, the three constellations below.
fn overview(ui: &mut Ui, data: &PlotData, pal: &Palette) {
    let avail = ui.available_size();
    let side = (avail.x / 3.0 - 8.0).min(avail.y * 0.5).max(80.0);
    let spectrum_height = (avail.y - side - 30.0).max(120.0);
    spectrum_plot(
        ui,
        "spectrum",
        "input spectrum",
        &data.spectrum,
        pal,
        spectrum_height,
    );
    constellation_row(ui, data, pal, side);
}

/// Common settings of all plots.
pub(super) fn base_plot<'a>(id: &str) -> Plot<'a> {
    Plot::new(id)
        .allow_zoom(false)
        .allow_drag(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .allow_double_click_reset(false)
        .show_crosshair(false)
}

/// Hover label `x unit_x, y unit_y` with the given decimals.
pub(super) fn hover_label(
    ux: &'static str,
    dx: usize,
    uy: &'static str,
    dy: usize,
) -> impl Fn(&HoverPosition<'_>) -> Option<String> {
    move |pos: &HoverPosition<'_>| {
        let p = match pos {
            HoverPosition::NearDataPoint { position, .. }
            | HoverPosition::Elsewhere { position } => position,
        };
        Some(format!("{:.dx$} {ux}\n{:.dy$} {uy}", p.x, p.y))
    }
}

fn line(name: &str, points: &Points, color: Color32) -> Line<'static> {
    Line::new(name.to_string(), PlotPoints::new(points.clone()))
        .color(color)
        .width(1.2)
}

/// A spectrum with the DRM band shaded and the DC carrier marked; `id` keeps the
/// receiver's and the transmitter's plots apart.
pub fn spectrum_plot(
    ui: &mut Ui,
    id: &str,
    name: &str,
    s: &SpectrumPlot,
    pal: &Palette,
    height: f32,
) {
    let (x0, x1) = s.span_khz;
    let (y0, y1) = s.db_range;
    base_plot(id)
        .height(height)
        .x_axis_label("frequency (kHz)")
        .y_axis_label("power (dB)")
        .label_formatter(hover_label("kHz", 2, "dB", 1))
        .show(ui, |p| {
            p.set_plot_bounds(PlotBounds::from_min_max([x0, y0], [x1, y1]));
            if let Some((lo, hi)) = s.band_khz {
                p.span(
                    Span::new("DRM signal", lo..=hi)
                        .fill(pal.band)
                        .border_width(0.0),
                );
            }
            if let Some(dc) = s.dc_khz {
                p.vline(
                    VLine::new("DC carrier", dc)
                        .color(pal.marker)
                        .style(LineStyle::dashed_dense()),
                );
            }
            if !s.points.is_empty() {
                p.line(line(name, &s.points, pal.spectrum).width(1.0));
            }
        });
}

/// The spectrum's history, newest at the top, on the Spectrum plot's frequency axis
/// with the DRM band edges and the DC carrier of the latest snapshot.
fn waterfall_plot(
    ui: &mut Ui,
    data: &PlotData,
    waterfall: &Waterfall,
    texture: &mut RingImage,
    fit: &mut bool,
    pal: &Palette,
    height: f32,
) {
    let Some((texture_id, uv)) = texture.update(ui.ctx(), "waterfall", waterfall) else {
        placeholder(
            ui,
            "No spectrum yet: the waterfall fills while the receiver runs.",
        );
        return;
    };
    // The same span as `SpectrumPlot`: 0 … 24 kHz for a real signal, ±24 kHz for I/Q.
    let (x0, x1) = if waterfall.real() {
        (0.0, 24.0)
    } else {
        (-24.0, 24.0)
    };
    let seconds = waterfall.span_s();
    // Shown: the whole band, or the DRM signal with a margin once one is found.
    let (v0, v1) = match data.spectrum.band_khz.filter(|_| *fit) {
        Some(band) => fit_span(band, (x0, x1)),
        None => (x0, x1),
    };
    base_plot("waterfall")
        .height(height.max(120.0))
        .x_axis_label("frequency (kHz)")
        .y_axis_label("time (s)")
        .label_formatter(hover_label("kHz", 2, "s", 1))
        .show(ui, |p| {
            p.set_plot_bounds(PlotBounds::from_min_max([v0, -seconds], [v1, 0.0]));
            p.image(
                PlotImage::new(
                    "waterfall",
                    texture_id,
                    PlotPoint::new((x0 + x1) / 2.0, -seconds / 2.0),
                    [(x1 - x0) as f32, seconds as f32],
                )
                .uv(uv),
            );
            if let Some((lo, hi)) = data.spectrum.band_khz {
                for x in [lo, hi] {
                    p.vline(VLine::new("DRM signal", x).color(pal.band_edge).width(1.0));
                }
            }
            if let Some(dc) = data.spectrum.dc_khz {
                p.vline(
                    VLine::new("DC carrier", dc)
                        .color(pal.marker)
                        .style(LineStyle::dashed_dense()),
                );
            }
        });
    let (lo, hi) = waterfall.levels();
    ui.horizontal(|ui| {
        ui.checkbox(fit, RichText::new("Fit to the DRM signal").small())
            .on_hover_text("Show the DRM signal with a margin around it (once one is found), instead of the whole input band");
        ui.label(
            RichText::new(format!(
                "{} rows (~{:.0} s), newest at the top; colours {lo:.0} … {hi:.0} dB, following the noise floor and the strongest signals",
                waterfall.rows(),
                waterfall.filled_s()
            ))
            .weak()
            .small(),
        );
    });
}

/// The channel gain per carrier over the last minute (see `fading`).
fn fading_plot(ui: &mut Ui, fading: &FadingMap, texture: &mut RingImage, pal: &Palette, height: f32) {
    let Some((texture_id, uv)) = texture.update(ui.ctx(), "fading", fading) else {
        placeholder(ui, "No channel estimate yet: the fading map fills once the receiver tracks a signal.");
        return;
    };
    let (x0, x1) = fading.span_khz();
    let seconds = fading.span_s();
    base_plot("fading")
        .height(height.max(120.0))
        .x_axis_label("frequency (kHz, from the DC carrier)")
        .y_axis_label("time (s)")
        .label_formatter(hover_label("kHz", 2, "s", 1))
        .show(ui, |p| {
            p.set_plot_bounds(PlotBounds::from_min_max([x0, -seconds], [x1, 0.0]));
            p.image(
                PlotImage::new(
                    "fading",
                    texture_id,
                    PlotPoint::new((x0 + x1) / 2.0, -seconds / 2.0),
                    [(x1 - x0) as f32, seconds as f32],
                )
                .uv(uv),
            );
            p.vline(VLine::new("DC carrier", 0.0).color(pal.marker).style(LineStyle::dashed_dense()));
        });
    let (lo, hi) = fading.levels();
    let m = fading.median_db();
    ui.label(
        RichText::new(format!(
            "Channel gain per carrier, a row per OFDM symbol, {} rows (~{:.0} s), newest at the top; colours {:.0} … {:+.0} dB around the median gain. \
             Dark bands are fades: two paths cancel at frequencies 1/delay apart, and a Doppler difference makes the notches move.",
            fading.rows(),
            fading.filled_s(),
            lo - m,
            hi - m
        ))
        .weak()
        .small(),
    );
}

/// The delay–Doppler map (see `decdrm_core::rx::scatter`).
fn delay_doppler_plot(ui: &mut Ui, data: &PlotData, texture: &mut Option<(TextureHandle, DelayDoppler)>, pal: &Palette, height: f32) {
    let Some(map) = &data.delay_doppler else {
        placeholder(ui, "No map yet: it appears a few seconds after the receiver starts tracking a signal.");
        return;
    };
    if texture.as_ref().is_none_or(|(_, shown)| shown != map) {
        let image = delay_doppler_image(map);
        match texture {
            Some((h, shown)) => {
                h.set(image, TextureOptions::LINEAR);
                shown.clone_from(map);
            }
            None => *texture = Some((ui.ctx().load_texture("delay_doppler", image, TextureOptions::LINEAR), map.clone())),
        }
    }
    let Some((handle, _)) = texture else { return };
    let (x0, x1) = (map.delay_ms(0) - map.delay_step_ms / 2.0, map.delay_ms(map.delays) - map.delay_step_ms / 2.0);
    let (y0, y1) = (map.doppler_hz(0) - map.doppler_step_hz / 2.0, map.doppler_hz(map.dopplers) - map.doppler_step_hz / 2.0);
    base_plot("delay_doppler")
        .height(height.max(120.0))
        .x_axis_label("delay (ms)")
        .y_axis_label("Doppler shift (Hz)")
        .label_formatter(hover_label("ms", 2, "Hz", 2))
        .show(ui, |p| {
            p.set_plot_bounds(PlotBounds::from_min_max([x0, y0], [x1, y1]));
            p.image(PlotImage::new(
                "delay–Doppler",
                handle.id(),
                PlotPoint::new((x0 + x1) / 2.0, (y0 + y1) / 2.0),
                [(x1 - x0) as f32, (y1 - y0) as f32],
            ));
            for x in [0.0, map.guard_ms] {
                p.vline(VLine::new("guard interval", x).color(pal.band_edge).width(1.0));
            }
            p.hline(HLine::new("no Doppler shift", 0.0).color(pal.marker).style(LineStyle::dashed_dense()));
        });
    ui.label(
        RichText::new(format!(
            "The last {:.0} s of channel estimates: each spot is a propagation path, at its delay (from the receiver's timing) and \
             Doppler shift (from the frequency the receiver tracks); spread along the Doppler axis is the path's fading rate. \
             Colours {FLOOR_DB:.0} … 0 dB below the strongest path; lines: the guard interval (later echoes interfere).",
            map.window_s
        ))
        .weak()
        .small(),
    );
}

/// The map as an image: delays across, the highest Doppler shift at the top.
fn delay_doppler_image(map: &DelayDoppler) -> eframe::egui::ColorImage {
    let lut = crate::waterfall::palette();
    let scale = (lut.len() - 1) as f32 / -FLOOR_DB;
    let mut pixels = Vec::with_capacity(map.delays * map.dopplers);
    for r in (0..map.dopplers).rev() {
        for c in 0..map.delays {
            let i = ((map.at(r, c) - FLOOR_DB) * scale).clamp(0.0, (lut.len() - 1) as f32);
            pixels.push(lut[i as usize]);
        }
    }
    eframe::egui::ColorImage::new([map.delays, map.dopplers], pixels)
}

/// The frequency span (kHz) showing `band` with a margin of a tenth of its width on
/// each side (at least 0.5 kHz), within the whole span `full`.
fn fit_span(band: (f64, f64), full: (f64, f64)) -> (f64, f64) {
    let (lo, hi) = band;
    let margin = (0.1 * (hi - lo)).max(0.5);
    let (v0, v1) = ((lo - margin).max(full.0), (hi + margin).min(full.1));
    if v1 > v0 { (v0, v1) } else { full }
}

/// FAC, SDC and MSC constellations side by side, each `side` × `side`.
fn constellation_row(ui: &mut Ui, data: &PlotData, pal: &Palette, side: f32) {
    ui.horizontal(|ui| {
        let plots = [
            ("FAC", &data.fac, &data.fac_ideal, pal.fac),
            ("SDC", &data.sdc, &data.sdc_ideal, pal.sdc),
            ("MSC", &data.msc, &data.msc_ideal, pal.msc),
        ];
        for (name, cells, ideal, color) in plots {
            constellation(ui, name, cells, ideal, color, pal.ideal, side);
        }
    });
}

/// One constellation: fixed ±1.5 axes on a square plot without axis labels, so the
/// data area itself is square. The ideal points of the signalled modulation are drawn
/// as small crosses on top of the received cells.
pub(super) fn constellation(
    ui: &mut Ui,
    name: &str,
    points: &Points,
    ideal: &Points,
    color: Color32,
    ideal_color: Color32,
    side: f32,
) {
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
                p.hline(
                    HLine::new("", 0.0)
                        .color(Color32::from_gray(128))
                        .width(0.5),
                );
                p.vline(
                    VLine::new("", 0.0)
                        .color(Color32::from_gray(128))
                        .width(0.5),
                );
                if !points.is_empty() {
                    p.points(
                        Scatter::new(name.to_string(), PlotPoints::new(points.clone()))
                            .color(color)
                            .radius(radius),
                    );
                }
                if !ideal.is_empty() {
                    p.points(
                        Scatter::new("ideal", PlotPoints::new(ideal.clone()))
                            .shape(MarkerShape::Plus)
                            .color(ideal_color)
                            .radius(4.0),
                    );
                }
            });
    });
}

fn carrier_bounds(data: &PlotData, points: &Points) -> (f64, f64) {
    data.carriers
        .or_else(|| Some((points.first()?[0], points.last()?[0])))
        .unwrap_or((-100.0, 100.0))
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
    let (x0, x1) = (
        data.pds.first().map_or(0.0, |p| p[0]),
        data.pds.last().map_or(1.0, |p| p[0]),
    );
    let unit = if data.pds_in_ms { "ms" } else { "samples" };
    base_plot("impulse")
        .height(height)
        .x_axis_label(format!("delay ({unit})"))
        .y_axis_label("dB (rel. peak)")
        .label_formatter(hover_label(unit, 3, "dB", 1))
        .show(ui, |p| {
            p.set_plot_bounds(PlotBounds::from_min_max(
                [x0, DB_FLOOR - 2.0],
                [x1.max(x0 + 1e-6), 3.0],
            ));
            if let Some((g0, g1)) = data.guard_ms {
                p.span(
                    Span::new("guard interval", g0..=g1)
                        .fill(pal.guard)
                        .border_width(0.0),
                );
            }
            // The estimated extent of the impulse response (the delay spread the
            // timing tracking works with).
            if let Some((b, e)) = data.spread_ms {
                for x in [b, e] {
                    p.vline(
                        VLine::new("delay spread", x)
                            .color(pal.marker)
                            .style(LineStyle::dashed_dense()),
                    );
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waterfall_fits_the_signal() {
        // A 10 kHz signal around 0 Hz in a ±24 kHz I/Q band: 1 kHz either side.
        let (a, b) = fit_span((-5.2, 5.2), (-24.0, 24.0));
        assert!((a + 6.24).abs() < 1e-9 && (b - 6.24).abs() < 1e-9, "{a} {b}");
        // A 4.5 kHz signal: at least 0.5 kHz of margin.
        assert_eq!(fit_span((12.0, 16.3), (0.0, 24.0)), (11.5, 16.8));
        // Near the edge of a real band: clipped to it.
        assert_eq!(fit_span((0.2, 9.0), (0.0, 24.0)).0, 0.0);
        // A degenerate band shows everything.
        assert_eq!(fit_span((30.0, 31.0), (0.0, 24.0)), (0.0, 24.0));
    }
}
