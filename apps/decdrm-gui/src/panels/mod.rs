//! Drawing code, one module per screen area. Panels read the GUI state and return
//! what the user asked for (or mutate settings directly); they hold no receiver
//! logic of their own.

pub mod broadcast;
pub mod data_info;
pub mod epg;
pub mod history;
pub mod journaline;
pub mod kiwi_list;
pub mod log;
pub mod meter;
pub mod plots;
pub mod schedule;
pub mod services;
pub mod slideshow;
pub mod source;
pub mod status_strip;
pub mod tx_form;
pub mod tx_page;
pub mod website;

use crate::indicators::Led;
use eframe::egui::{self, Color32, RichText, Sense, Stroke, Ui, vec2};

/// Colours chosen to read well on both the dark and the light theme.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub spectrum: Color32,
    pub band: Color32,
    /// Edges of the DRM band where a fill would hide the data (waterfall).
    pub band_edge: Color32,
    pub marker: Color32,
    pub fac: Color32,
    pub sdc: Color32,
    pub msc: Color32,
    /// Ideal constellation points.
    pub ideal: Color32,
    pub channel: Color32,
    pub group_delay: Color32,
    pub pds: Color32,
    pub guard: Color32,
    pub snr: Color32,
    pub error: Color32,
}

impl Palette {
    pub fn for_ui(ui: &Ui) -> Self {
        if ui.visuals().dark_mode {
            Self {
                spectrum: Color32::from_rgb(110, 170, 255),
                band: Color32::from_rgba_unmultiplied(70, 200, 110, 36),
                band_edge: Color32::from_rgb(90, 220, 130),
                marker: Color32::from_rgb(240, 150, 50),
                fac: Color32::from_rgb(240, 180, 60),
                sdc: Color32::from_rgb(90, 210, 120),
                msc: Color32::from_rgb(110, 170, 255),
                ideal: Color32::from_gray(235),
                channel: Color32::from_rgb(110, 170, 255),
                group_delay: Color32::from_rgb(240, 120, 110),
                pds: Color32::from_rgb(190, 150, 255),
                guard: Color32::from_rgba_unmultiplied(190, 150, 255, 30),
                snr: Color32::from_rgb(60, 200, 200),
                error: Color32::from_rgb(255, 110, 100),
            }
        } else {
            Self {
                spectrum: Color32::from_rgb(20, 90, 200),
                band: Color32::from_rgba_unmultiplied(30, 160, 70, 40),
                band_edge: Color32::from_rgb(20, 150, 60),
                marker: Color32::from_rgb(200, 100, 0),
                fac: Color32::from_rgb(190, 120, 0),
                sdc: Color32::from_rgb(20, 140, 60),
                msc: Color32::from_rgb(20, 90, 200),
                ideal: Color32::from_gray(20),
                channel: Color32::from_rgb(20, 90, 200),
                group_delay: Color32::from_rgb(200, 50, 40),
                pds: Color32::from_rgb(110, 60, 190),
                guard: Color32::from_rgba_unmultiplied(110, 60, 190, 30),
                snr: Color32::from_rgb(0, 130, 130),
                error: Color32::from_rgb(200, 30, 20),
            }
        }
    }
}

/// Fill colour of an indicator.
pub fn led_color(led: Led, dark_mode: bool) -> Color32 {
    match led {
        Led::Green => Color32::from_rgb(40, 200, 70),
        Led::Yellow => Color32::from_rgb(250, 190, 20),
        Led::Red => Color32::from_rgb(230, 55, 50),
        Led::Off if dark_mode => Color32::from_gray(75),
        Led::Off => Color32::from_gray(185),
    }
}

/// An indicator: a coloured dot followed by its name, with an explanation on hover.
pub fn led(ui: &mut Ui, state: Led, name: &str, help: &str) {
    let dark = ui.visuals().dark_mode;
    let response = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let (rect, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
            let painter = ui.painter();
            painter.circle_filled(rect.center(), 5.5, led_color(state, dark));
            let rim = if dark {
                Color32::from_gray(20)
            } else {
                Color32::from_gray(110)
            };
            painter.circle_stroke(rect.center(), 5.5, Stroke::new(1.0, rim));
            ui.label(name);
        })
        .response;
    response.on_hover_text(format!("{name}: {} — {help}", state.word()));
}

/// A label/value pair of the status strip; the value is monospaced so numbers do not
/// make the layout jitter. Returns the value label's response (for a tooltip).
pub fn value(ui: &mut Ui, name: &str, value: impl Into<String>) -> egui::Response {
    ui.label(RichText::new(name).weak());
    ui.label(RichText::new(value.into()).monospace())
}

/// Bold section heading.
pub fn heading(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).strong());
}

/// A dimmed placeholder line for empty views.
pub fn placeholder(ui: &mut Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).weak().italics()).wrap());
}
