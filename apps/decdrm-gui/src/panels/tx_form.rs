//! Form view of the station configuration (the Transmitter tab's "Station" view).
//!
//! The form edits the TOML document in place with `toml_edit`: a value the form changes
//! keeps its trailing comment, and everything the form does not show (alternative
//! frequencies, EPG programmes, …) stays exactly as written. The "TOML" view shows the
//! same text, which is what gets saved and transmitted.

use super::meter::{MeterKind, meter_with_text};
use super::source::DeviceLists;
use decdrm_core::fac::{LANGUAGES, PROGRAMME_TYPES};
use decdrm_core::params::{ChannelLayout, RobustnessMode, SpectrumOccupancy};
use decdrm_core::tx::output::{OutputFormat, recommended_if_range_hz, suggested_if_hz};
use eframe::egui::{self, Color32, RichText, Ui};
use std::path::Path;
use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table, TableLike, Value};

/// What the form needs besides the document.
pub struct FormCtx<'a> {
    pub devices: &'a mut DeviceLists,
    /// Directory relative paths in the configuration are resolved against.
    pub base_dir: &'a Path,
    /// The multiplex of the last successful check, for the capacity bar.
    pub bar: Option<&'a PlanBar>,
    /// Input levels (RMS, peak dBFS) of the services while transmitting, by position.
    pub levels: Vec<Option<(f32, f32)>>,
}

/// The multiplex as a bar: MSC capacity and the streams that fill it.
#[derive(Debug, Clone, Default)]
pub struct PlanBar {
    /// MSC bytes per 400 ms frame.
    pub capacity: usize,
    pub segments: Vec<Segment>,
}

/// One stream of the bar.
#[derive(Debug, Clone)]
pub struct Segment {
    pub label: String,
    pub bytes: usize,
    pub audio: bool,
    /// Carried in part A, the higher protected part.
    pub part_a: bool,
}

impl PlanBar {
    /// The bar of a checked configuration.
    pub fn of(cfg: &decdrm_station::StationConfig, plan: &decdrm_station::MultiplexPlan) -> Self {
        let c = &plan.capacity;
        let capacity = (c.vspp_bits + c.hpp_bits + c.lpp_bits) / 8;
        let label_of = |service: usize| cfg.services.get(service).map_or_else(|| format!("service {service}"), |s| s.label.clone());
        let segments = plan
            .streams
            .iter()
            .map(|s| {
                let (bytes, part_a) = (s.bytes(), s.lengths.part_a > 0);
                match &s.content {
                    decdrm_station::StreamContent::Audio { service } => {
                        Segment { label: format!("{} audio", label_of(*service)), bytes, audio: true, part_a }
                    }
                    decdrm_station::StreamContent::Data { apps, .. } => {
                        let names: Vec<String> = apps
                            .iter()
                            .filter_map(|a| cfg.services.get(a.service).and_then(|sv| sv.applications().nth(a.index)))
                            .map(|app| format!("{:?}", app.kind).to_lowercase())
                            .collect();
                        Segment { label: names.join(" + "), bytes, audio: false, part_a }
                    }
                }
            })
            .collect();
        Self { capacity, segments }
    }
}

/// Draw the form; returns whether the document changed.
pub fn show(ui: &mut Ui, doc: &mut DocumentMut, ctx: &mut FormCtx) -> bool {
    let mut changed = false;
    if let Some(bar) = ctx.bar {
        capacity_bar(ui, bar);
        ui.add_space(6.0);
    }
    let mdi = doc.get("mdi").is_some_and(Item::is_table_like);
    card(ui, "Modulator", "MDI from a content server", |ui| changed |= modulator(ui, doc, ctx));
    // A modulator takes the channel, the services and the clock from the MDI.
    if !mdi {
        card(ui, "Channel", "how the signal is built", |ui| changed |= channel(ui, doc));
        changed |= services(ui, doc, ctx);
    }
    card(ui, "Output", "level and format of the signal (the toolbar picks file or sound card)", |ui| changed |= output(ui, doc, ctx));
    card(ui, "Channel simulator", "impair the signal to test receivers", |ui| changed |= simulator(ui, doc));
    if !mdi {
        card(ui, "Clock", "SDC time and date", |ui| changed |= clock(ui, doc));
    }
    ui.label(
        RichText::new("Alternative frequencies, EPG programmes and other details: TOML view.")
            .weak()
            .italics(),
    );
    changed
}

// ---------------------------------------------------------------------------------
// Layout helpers
// ---------------------------------------------------------------------------------

/// A framed section with a title (also used by the status panel).
pub fn card(ui: &mut Ui, title: &str, subtitle: &str, add: impl FnOnce(&mut Ui)) {
    egui::Frame::group(ui.style()).inner_margin(egui::Margin::same(10)).corner_radius(egui::CornerRadius::same(6)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(title).strong().size(15.0));
            ui.label(RichText::new(subtitle).weak());
        });
        ui.add_space(4.0);
        add(ui);
    });
    ui.add_space(8.0);
}

/// A two-column grid (labels left, controls right).
fn grid(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, add: impl FnOnce(&mut Ui)) {
    egui::Grid::new(id).num_columns(2).spacing([18.0, 6.0]).min_col_width(130.0).show(ui, add);
}

fn row_label(ui: &mut Ui, text: &str) -> egui::Response {
    ui.label(RichText::new(text).weak())
}

/// A combo box over `(value, label)` options; returns the new value when changed.
fn combo<'a>(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, current: &str, options: &'a [(&'a str, &'a str)], width: f32) -> Option<&'a str> {
    combo_where(ui, id, current, options, width, |_| true, "")
}

/// [`combo`] with the options `allowed` rejects greyed out, explained by `why_not`.
fn combo_where<'a>(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    current: &str,
    options: &'a [(&'a str, &'a str)],
    width: f32,
    allowed: impl Fn(&str) -> bool,
    why_not: &str,
) -> Option<&'a str> {
    let shown = options.iter().find(|(v, _)| canon(v) == canon(current)).map_or(current, |(_, l)| l);
    let mut picked = None;
    egui::ComboBox::from_id_salt(id).width(width).selected_text(shown).show_ui(ui, |ui| {
        for (v, l) in options {
            let option = egui::Button::selectable(canon(v) == canon(current), *l);
            if ui.add_enabled(allowed(v), option).on_disabled_hover_text(why_not).clicked() && canon(v) != canon(current) {
                picked = Some(*v);
            }
        }
    });
    picked
}

/// Name matching as the station configuration does it: case-insensitive, ignoring
/// "-", "_" and spaces, with "QAM64" = "64-QAM".
fn canon(s: &str) -> String {
    let t: String = s.chars().filter(|c| !matches!(c, '-' | '_' | ' ')).flat_map(char::to_lowercase).collect();
    match t.strip_prefix("qam") {
        Some(digits) if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) => format!("{digits}qam"),
        _ => t,
    }
}

// ---------------------------------------------------------------------------------
// Document helpers (work on standard and inline tables alike)
// ---------------------------------------------------------------------------------

fn get_str(t: &dyn TableLike, k: &str) -> Option<String> {
    t.get(k)?.as_str().map(str::to_string)
}

fn get_int(t: &dyn TableLike, k: &str) -> Option<i64> {
    t.get(k)?.as_integer()
}

fn get_num(t: &dyn TableLike, k: &str) -> Option<f64> {
    let item = t.get(k)?;
    item.as_float().or_else(|| item.as_integer().map(|i| i as f64))
}

fn get_bool(t: &dyn TableLike, k: &str) -> Option<bool> {
    t.get(k)?.as_bool()
}

/// Set a value, keeping the comments around an existing one.
fn set(t: &mut dyn TableLike, k: &str, v: impl Into<Value>) {
    let v = v.into();
    match t.get_mut(k) {
        Some(Item::Value(old)) => {
            let decor = old.decor().clone();
            *old = v;
            *old.decor_mut() = decor;
        }
        _ => {
            t.insert(k, Item::Value(v));
        }
    }
}

/// Set a value given as TOML source (e.g. a hex integer, which keeps its spelling).
fn set_raw(t: &mut dyn TableLike, k: &str, raw: &str) {
    if let Ok(v) = raw.parse::<Value>() {
        set(t, k, v);
    }
}

fn remove(t: &mut dyn TableLike, k: &str) {
    t.remove(k);
}

/// A child table (standard or inline), created as a standard table if missing.
fn child<'a>(t: &'a mut dyn TableLike, k: &str) -> &'a mut dyn TableLike {
    if !t.get(k).is_some_and(Item::is_table_like) {
        t.insert(k, Item::Table(Table::new()));
    }
    t.get_mut(k).and_then(Item::as_table_like_mut).expect("just made a table")
}

/// A top-level table of the document, created if missing.
fn section<'a>(doc: &'a mut DocumentMut, k: &str) -> &'a mut dyn TableLike {
    child(doc.as_table_mut(), k)
}

// ---------------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------------

const MODES: &[(&str, &str)] = &[
    ("A", "A — ground wave, few echoes"),
    ("B", "B — sky wave (the usual choice)"),
    ("C", "C — long distance, strong echoes"),
    ("D", "D — severe Doppler and delay"),
];
const OCCUPANCIES: &[(&str, &str)] =
    &[("0", "4.5 kHz"), ("1", "5 kHz"), ("2", "9 kHz"), ("3", "10 kHz"), ("4", "18 kHz"), ("5", "20 kHz")];
const MSC_MODES: &[(&str, &str)] = &[
    ("16-QAM", "16-QAM — robust"),
    ("64-QAM", "64-QAM — more capacity"),
    ("HMsym", "64-QAM hierarchical (HMsym)"),
    ("HMmix", "64-QAM hierarchical (HMmix)"),
];
const SDC_MODES: &[(&str, &str)] = &[("4-QAM", "4-QAM — robust, half the capacity"), ("16-QAM", "16-QAM")];
const INTERLEAVING: &[(&str, &str)] = &[("long", "Long (2 s) — rides out fading"), ("short", "Short (400 ms) — less delay")];
const PROTECTION_16: &[(&str, &str)] = &[("0", "0 — code rate 0.5 (most robust)"), ("1", "1 — code rate 0.62")];
const PROTECTION_64: &[(&str, &str)] =
    &[("0", "0 — code rate 0.5 (most robust)"), ("1", "1 — code rate 0.6"), ("2", "2 — code rate 0.71"), ("3", "3 — code rate 0.78")];

fn channel(ui: &mut Ui, doc: &mut DocumentMut) -> bool {
    let mut changed = false;
    let uep = uses_part_a(doc);
    let ch = section(doc, "channel");
    let mode = get_str(ch, "mode").unwrap_or_else(|| "B".into());
    let msc = get_str(ch, "msc_mode").unwrap_or_else(|| "64-QAM".into());
    let sixteen = canon(&msc) == "16qam";
    let hierarchical = canon(&msc).starts_with("hm");
    grid(ui, "tx_form_channel", |ui| {
        row_label(ui, "Robustness mode");
        if let Some(v) = combo(ui, "tx_mode", &mode, MODES, 280.0) {
            set(ch, "mode", v);
            // Modes C and D only have 10 and 20 kHz.
            if matches!(v, "C" | "D") && !matches!(get_int(ch, "occupancy"), Some(3 | 5)) {
                set(ch, "occupancy", 3i64);
            }
            changed = true;
        }
        ui.end_row();
        row_label(ui, "Bandwidth");
        let occ = get_int(ch, "occupancy").unwrap_or(3).to_string();
        let allowed: Vec<(&str, &str)> =
            OCCUPANCIES.iter().copied().filter(|(v, _)| !matches!(mode.as_str(), "C" | "D") || matches!(*v, "3" | "5")).collect();
        if let Some(v) = combo(ui, "tx_occupancy", &occ, &allowed, 280.0) {
            set(ch, "occupancy", v.parse::<i64>().unwrap_or(3));
            changed = true;
        }
        ui.end_row();
        row_label(ui, "MSC modulation");
        if let Some(v) = combo(ui, "tx_msc", &msc, MSC_MODES, 280.0) {
            set(ch, "msc_mode", v);
            // 16-QAM has protection levels 0-1 only.
            if canon(v) == "16qam" {
                for key in ["protection_b", "protection_a"] {
                    if get_int(ch, key).unwrap_or(0) > 1 {
                        set(ch, key, 1i64);
                    }
                }
            }
            if uep {
                keep_part_a_stronger(ch);
            }
            changed = true;
        }
        ui.end_row();
        row_label(ui, "SDC modulation");
        let sdc = get_str(ch, "sdc_mode").unwrap_or_else(|| "16-QAM".into());
        if let Some(v) = combo(ui, "tx_sdc", &sdc, SDC_MODES, 280.0) {
            set(ch, "sdc_mode", v);
            changed = true;
        }
        ui.end_row();
        row_label(ui, "Interleaving");
        let il = get_str(ch, "interleaving").unwrap_or_else(|| "long".into());
        if let Some(v) = combo(ui, "tx_interleaving", &il, INTERLEAVING, 280.0) {
            set(ch, "interleaving", v);
            changed = true;
        }
        ui.end_row();
        let levels = if sixteen { PROTECTION_16 } else { PROTECTION_64 };
        row_label(ui, "Protection").on_hover_text("Error protection of the multiplex: a lower code rate survives a weaker signal, a higher one carries more");
        let pb = get_int(ch, "protection_b").unwrap_or(1).to_string();
        if let Some(v) = combo(ui, "tx_prot_b", &pb, levels, 280.0) {
            set(ch, "protection_b", v.parse::<i64>().unwrap_or(0));
            if uep {
                keep_part_a_stronger(ch);
            }
            changed = true;
        }
        ui.end_row();
        row_label(ui, "Protection, part A")
            .on_hover_text("For the streams switched to Part A (unequal error protection); it must be more robust than Protection");
        if uep {
            let pb = get_int(ch, "protection_b").unwrap_or(1);
            let pa = get_int(ch, "protection_a").unwrap_or(0).to_string();
            ui.horizontal(|ui| {
                let stronger = |v: &str| v.parse::<i64>().is_ok_and(|level| level < pb);
                let why = "Part A must be protected more strongly (a lower level) than Protection";
                if let Some(v) = combo_where(ui, "tx_prot_a", &pa, levels, 280.0, stronger, why) {
                    set(ch, "protection_a", v.parse::<i64>().unwrap_or(0));
                    changed = true;
                }
                if pb == 0 {
                    let warn = ui.visuals().warn_fg_color;
                    ui.colored_label(warn, "needs Protection above 0");
                }
            });
        } else {
            ui.add_enabled_ui(false, |ui| {
                egui::ComboBox::from_id_salt("tx_prot_a_unused")
                    .width(280.0)
                    .selected_text("not used: no stream is in part A")
                    .show_ui(ui, |_| {})
                    .response
                    .on_disabled_hover_text("Switch a service or data application to Part A to protect it more strongly than the rest");
            });
        }
        ui.end_row();
        if hierarchical {
            row_label(ui, "Hierarchical protection");
            let ph = get_int(ch, "protection_hierarchical").unwrap_or(0).to_string();
            if let Some(v) = combo(ui, "tx_prot_h", &ph, PROTECTION_64, 280.0) {
                set(ch, "protection_hierarchical", v.parse::<i64>().unwrap_or(0));
                changed = true;
            }
            ui.end_row();
        }
    });
    changed
}

/// What the part A switches need while the services are drawn.
struct Parts {
    /// Part B's protection level; at 0 nothing is stronger, so part A is unavailable.
    protection_b: i64,
    /// Switches of named shared data streams: every application of the stream follows.
    shared: Vec<(String, bool)>,
    /// A stream was switched to part A (part A's level may need lowering).
    turned_on: bool,
}

fn services(ui: &mut Ui, doc: &mut DocumentMut, ctx: &mut FormCtx) -> bool {
    let mut changed = false;
    let protection_b = doc.get("channel").and_then(Item::as_table_like).and_then(|ch| get_int(ch, "protection_b")).unwrap_or(1);
    let mut parts = Parts { protection_b, shared: Vec::new(), turned_on: false };
    if !doc.as_table().get("service").is_some_and(Item::is_array_of_tables) {
        doc.as_table_mut().insert("service", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let list = doc.get_mut("service").and_then(Item::as_array_of_tables_mut).expect("made above");
    let mut remove_at = None;
    let count = list.len();
    for (i, svc) in list.iter_mut().enumerate() {
        let label = get_str(svc, "label").unwrap_or_default();
        let title = format!("Service {i}");
        egui::Frame::group(ui.style()).inner_margin(egui::Margin::same(10)).corner_radius(egui::CornerRadius::same(6)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(&title).strong().size(15.0));
                ui.label(RichText::new(&label).size(15.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Remove").on_hover_text("Remove this service").clicked() && confirm_remove(&label) {
                        remove_at = Some(i);
                    }
                });
            });
            ui.add_space(4.0);
            changed |= service(ui, svc, i, ctx, &mut parts);
        });
        ui.add_space(8.0);
    }
    if let Some(i) = remove_at {
        list.remove(i);
        changed = true;
    }
    if count < 4 {
        ui.horizontal(|ui| {
            if ui.button("+ Audio service").clicked() {
                list.push(new_service(count, true));
                changed = true;
            }
            if ui.button("+ Data service").clicked() {
                list.push(new_service(count, false));
                changed = true;
            }
            ui.label(RichText::new(format!("{count} of 4 services")).weak());
        });
        ui.add_space(8.0);
    }
    for (name, a) in &parts.shared {
        sync_shared_parts(doc, name, *a);
    }
    if parts.turned_on {
        keep_part_a_stronger(section(doc, "channel"));
    }
    changed
}

/// Ask before removing a service (its settings are gone unless the file is reloaded).
fn confirm_remove(label: &str) -> bool {
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title("Remove service")
        .set_description(format!("Remove the service \"{label}\" from the station?"))
        .set_buttons(rfd::MessageButtons::YesNo)
        .show()
        == rfd::MessageDialogResult::Yes
}

/// A new service with Short Id `n`: an audio service with a test tone, or a data
/// service with Journaline pages.
fn new_service(n: usize, audio: bool) -> Table {
    let mut t = Table::new();
    set(&mut t, "label", if audio { format!("Audio {}", n + 1) } else { format!("Data {}", n + 1) });
    set_raw(&mut t, "id", &format!("0x{:06X}", 0xD0D001 + n as u32));
    if audio {
        let mut a = Table::new();
        set(&mut a, "codec", "he-aac");
        set(&mut a, "core_rate", 12_000i64);
        let mut input = InlineTable::new();
        input.insert("tone_hz", Value::from(1000.0));
        a.insert("input", Item::Value(Value::InlineTable(input)));
        t.insert("audio", Item::Table(a));
    } else {
        let mut d = Table::new();
        set(&mut d, "type", "journaline");
        set(&mut d, "path", "journaline.toml");
        set(&mut d, "bitrate", 1200i64);
        t.insert("data", Item::Table(d));
    }
    t
}

fn service(ui: &mut Ui, svc: &mut Table, i: usize, ctx: &mut FormCtx, parts: &mut Parts) -> bool {
    let mut changed = false;
    let is_audio = svc.get("audio").is_some_and(Item::is_table_like);
    grid(ui, ("tx_form_service", i), |ui| {
        row_label(ui, "Label");
        let mut label = get_str(svc, "label").unwrap_or_default();
        ui.horizontal(|ui| {
            if ui.add(egui::TextEdit::singleline(&mut label).char_limit(16).desired_width(200.0)).changed() {
                set(svc, "label", label.as_str());
                changed = true;
            }
            ui.label(RichText::new(format!("{}/16", label.chars().count())).weak().small());
        });
        ui.end_row();
        row_label(ui, "Service ID");
        let id = get_int(svc, "id").or_else(|| get_int(svc, "service_id")).unwrap_or(0);
        let key = egui::Id::new(("tx_service_id", i));
        let mut text = ui.data_mut(|d| d.get_temp::<String>(key)).unwrap_or_else(|| format!("{id:06X}"));
        ui.horizontal(|ui| {
            ui.label(RichText::new("0x").monospace().weak());
            let edit = ui.add(egui::TextEdit::singleline(&mut text).desired_width(80.0).font(egui::TextStyle::Monospace));
            if edit.changed() {
                if let Ok(v) = u32::from_str_radix(text.trim(), 16)
                    && v > 0
                    && v < 1 << 24
                {
                    set_raw(svc, "id", &format!("0x{v:06X}"));
                    changed = true;
                }
                ui.data_mut(|d| d.insert_temp(key, text.clone()));
            }
            if edit.lost_focus() {
                ui.data_mut(|d| d.remove::<String>(key));
            }
            let valid = u32::from_str_radix(text.trim(), 16).is_ok_and(|v| v > 0 && v < 1 << 24);
            if !valid {
                let warn = ui.visuals().warn_fg_color;
                ui.colored_label(warn, "6 hex digits, not 0");
            }
        });
        ui.end_row();
        row_label(ui, "Language");
        let lang = svc.get("language").and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_integer().and_then(|n| LANGUAGES.get(n as usize).map(|s| s.to_string())))).unwrap_or_else(|| "No language specified".into());
        let langs: Vec<(&str, &str)> = LANGUAGES.iter().map(|l| (*l, *l)).collect();
        if let Some(v) = combo(ui, ("tx_lang", i), &lang, &langs, 200.0) {
            set(svc, "language", v);
            changed = true;
        }
        ui.end_row();
        row_label(ui, "Country (ISO)");
        let mut country = get_str(svc, "iso_country").unwrap_or_default();
        ui.horizontal(|ui| {
            if ui.add(egui::TextEdit::singleline(&mut country).char_limit(2).desired_width(36.0).hint_text("gb")).changed() {
                if country.is_empty() {
                    remove(svc, "iso_country");
                } else {
                    set(svc, "iso_country", country.to_lowercase().as_str());
                }
                changed = true;
            }
            ui.label(RichText::new("language (ISO 639-2)").weak());
            let mut iso_lang = get_str(svc, "iso_language").unwrap_or_default();
            if ui.add(egui::TextEdit::singleline(&mut iso_lang).char_limit(3).desired_width(44.0).hint_text("eng")).changed() {
                if iso_lang.is_empty() {
                    remove(svc, "iso_language");
                } else {
                    set(svc, "iso_language", iso_lang.to_lowercase().as_str());
                }
                changed = true;
            }
        });
        ui.end_row();
        if is_audio {
            row_label(ui, "Programme type");
            let pty = svc.get("programme_type").and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_integer().and_then(|n| PROGRAMME_TYPES.get(n as usize).map(|s| s.to_string())))).unwrap_or_else(|| "No programme type".into());
            let ptys: Vec<(&str, &str)> = PROGRAMME_TYPES.iter().map(|p| (*p, *p)).collect();
            if let Some(v) = combo(ui, ("tx_pty", i), &pty, &ptys, 200.0) {
                set(svc, "programme_type", v);
                changed = true;
            }
            ui.end_row();
        }
    });
    ui.add_space(6.0);
    if is_audio {
        let audio = child(svc, "audio");
        changed |= audio_settings(ui, audio, i, ctx, parts);
    } else if svc.get("data").is_some_and(Item::is_table_like) {
        ui.label(RichText::new("Main application").strong());
        let data = child(svc, "data");
        changed |= app_row(ui, data, ("tx_data", i), ctx, parts);
    }
    changed |= extra_apps(ui, svc, i, ctx, parts);
    changed
}

const CODECS: &[(&str, &str)] = &[
    ("aac", "AAC"),
    ("he-aac", "HE-AAC (AAC + SBR)"),
    ("he-aac-v2", "HE-AAC v2 (SBR + parametric stereo)"),
    ("xhe-aac", "xHE-AAC (USAC)"),
    ("opus", "Opus"),
    ("encodec", "EnCodec (experimental)"),
];
const CORE_RATES: &[(&str, &str)] = &[("12000", "12 kHz core (5 frames per 400 ms)"), ("24000", "24 kHz core (10 frames)")];
const XHE_RATES: &[(&str, &str)] = &[
    ("auto", "Automatic (from the bit rate)"),
    ("9600", "9.6 kHz"),
    ("12000", "12 kHz"),
    ("16000", "16 kHz"),
    ("19200", "19.2 kHz"),
    ("24000", "24 kHz"),
    ("32000", "32 kHz"),
    ("38400", "38.4 kHz (mono)"),
    ("48000", "48 kHz"),
];
const ENCODEC_RATES: &[(&str, &str)] =
    &[("auto", "Highest that fits"), ("1.5", "1.5 kbit/s"), ("3", "3 kbit/s"), ("6", "6 kbit/s"), ("12", "12 kbit/s"), ("24", "24 kbit/s")];

/// Which kind of audio input a table describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    Tone,
    File,
    LineIn,
    Stream,
}

impl InputKind {
    const ALL: [InputKind; 4] = [InputKind::Tone, InputKind::File, InputKind::LineIn, InputKind::Stream];

    fn of(t: &dyn TableLike) -> Self {
        if t.contains_key("file") {
            InputKind::File
        } else if t.contains_key("device") {
            InputKind::LineIn
        } else if t.contains_key("url") {
            InputKind::Stream
        } else {
            InputKind::Tone
        }
    }

    fn label(self) -> &'static str {
        match self {
            InputKind::Tone => "Test tone",
            InputKind::File => "File",
            InputKind::LineIn => "Line in",
            InputKind::Stream => "Web stream",
        }
    }
}

fn audio_settings(ui: &mut Ui, audio: &mut dyn TableLike, i: usize, ctx: &mut FormCtx, parts: &mut Parts) -> bool {
    let mut changed = false;
    let codec = get_str(audio, "codec").unwrap_or_else(|| "he-aac".into());
    let c = canon(&codec);
    let codecs: Vec<(&str, &str)> =
        CODECS.iter().copied().filter(|(v, _)| *v != "encodec" || cfg!(feature = "encodec") || canon(v) == c).collect();
    grid(ui, ("tx_form_audio", i), |ui| {
        row_label(ui, "Codec");
        if let Some(v) = combo(ui, ("tx_codec", i), &codec, &codecs, 280.0) {
            set(audio, "codec", v);
            // Keys of the old codec that the new one does not take.
            let aac = matches!(canon(v).as_str(), "aac" | "heaac" | "heaacv2");
            if !aac {
                remove(audio, "core_rate");
            } else if !audio.contains_key("core_rate") {
                set(audio, "core_rate", 12_000i64);
            }
            if canon(v) != "xheaac" {
                remove(audio, "sample_rate");
                remove(audio, "sbr_ratio");
            }
            if canon(v) != "encodec" {
                remove(audio, "bandwidth_kbps");
            }
            changed = true;
        }
        ui.end_row();
        match c.as_str() {
            "aac" | "heaac" | "heaacv2" => {
                row_label(ui, "Core rate");
                let rate = get_int(audio, "core_rate").unwrap_or(12_000).to_string();
                if let Some(v) = combo(ui, ("tx_core", i), &rate, CORE_RATES, 280.0) {
                    set(audio, "core_rate", v.parse::<i64>().unwrap_or(12_000));
                    changed = true;
                }
                ui.end_row();
            }
            "xheaac" => {
                row_label(ui, "Sampling rate");
                let rate = get_int(audio, "sample_rate").map_or_else(|| "auto".into(), |r| r.to_string());
                if let Some(v) = combo(ui, ("tx_xhe_rate", i), &rate, XHE_RATES, 280.0) {
                    if v == "auto" {
                        remove(audio, "sample_rate");
                    } else {
                        set(audio, "sample_rate", v.parse::<i64>().unwrap_or(24_000));
                    }
                    changed = true;
                }
                ui.end_row();
            }
            "encodec" => {
                row_label(ui, "Bit rate");
                let rate = get_num(audio, "bandwidth_kbps").map_or_else(|| "auto".into(), |r| format!("{r}"));
                if let Some(v) = combo(ui, ("tx_encodec_rate", i), &rate, ENCODEC_RATES, 280.0) {
                    if v == "auto" {
                        remove(audio, "bandwidth_kbps");
                    } else {
                        set(audio, "bandwidth_kbps", v.parse::<f64>().unwrap_or(6.0));
                    }
                    changed = true;
                }
                ui.end_row();
            }
            _ => {}
        }
        if !matches!(c.as_str(), "heaacv2" | "encodec") {
            row_label(ui, "Channels");
            let mut stereo = get_bool(audio, "stereo").unwrap_or(false);
            if ui.checkbox(&mut stereo, "Stereo").changed() {
                if stereo {
                    set(audio, "stereo", true);
                } else {
                    set(audio, "stereo", false);
                }
                changed = true;
            }
            ui.end_row();
        }
        row_label(ui, "Protection");
        changed |= part_switch(ui, audio, "Part A (stronger protection)", parts);
        ui.end_row();
        row_label(ui, "Text messages").on_hover_text("One message per line (up to 128 bytes each), sent one after the other");
        let mut text = audio
            .get("text")
            .and_then(Item::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        if ui.add(egui::TextEdit::multiline(&mut text).desired_rows(2).desired_width(320.0).hint_text("One message per line")).changed() {
            let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            if lines.is_empty() {
                remove(audio, "text");
            } else {
                let mut arr = toml_edit::Array::new();
                for l in lines {
                    arr.push(l);
                }
                set(audio, "text", Value::Array(arr));
            }
            changed = true;
        }
        ui.end_row();
    });
    ui.add_space(6.0);
    let input = child(audio, "input");
    changed |= input_settings(ui, input, i, ctx);
    changed
}

fn input_settings(ui: &mut Ui, input: &mut dyn TableLike, i: usize, ctx: &mut FormCtx) -> bool {
    let mut changed = false;
    let kind = InputKind::of(input);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Audio input").strong());
        // While transmitting: the level going into the encoder.
        if let Some(level) = ctx.levels.get(i).copied().flatten() {
            meter_with_text(ui, ("tx_form_level", i), Some(level), 140.0, MeterKind::Audio);
        }
        for k in InputKind::ALL {
            if ui.selectable_label(kind == k, k.label()).clicked() && kind != k {
                for key in ["file", "device", "url", "tone_hz", "loop", "stream_titles"] {
                    remove(input, key);
                }
                match k {
                    InputKind::Tone => set(input, "tone_hz", 1000.0),
                    InputKind::File => set(input, "file", "programme.flac"),
                    InputKind::LineIn => set(input, "device", "default"),
                    InputKind::Stream => set(input, "url", "http://"),
                }
                changed = true;
            }
        }
    });
    grid(ui, ("tx_form_input", i), |ui| match kind {
        InputKind::Tone => {
            row_label(ui, "Frequency");
            let mut f = get_num(input, "tone_hz").unwrap_or(1000.0);
            if ui.add(egui::DragValue::new(&mut f).range(20.0..=20_000.0).speed(10.0).suffix(" Hz")).changed() {
                set(input, "tone_hz", f);
                changed = true;
            }
            ui.end_row();
            row_label(ui, "Level");
            let mut l = get_num(input, "level_dbfs").unwrap_or(-12.0);
            if ui.add(egui::DragValue::new(&mut l).range(-60.0..=0.0).speed(0.5).suffix(" dBFS")).changed() {
                set(input, "level_dbfs", l);
                changed = true;
            }
            ui.end_row();
        }
        InputKind::File => {
            row_label(ui, "File");
            let mut path = get_str(input, "file").unwrap_or_default();
            ui.horizontal(|ui| {
                if ui.add(egui::TextEdit::singleline(&mut path).desired_width(260.0).hint_text("WAV or FLAC")).changed() {
                    set(input, "file", path.as_str());
                    changed = true;
                }
                if ui.button("Browse…").clicked()
                    && let Some(p) = pick_file(ctx.base_dir, "Audio file", &["wav", "flac"])
                {
                    set(input, "file", p.as_str());
                    changed = true;
                }
            });
            ui.end_row();
            row_label(ui, "At the end");
            let mut looped = get_bool(input, "loop").unwrap_or(true);
            if ui.checkbox(&mut looped, "Start again (loop)").changed() {
                set(input, "loop", looped);
                changed = true;
            }
            ui.end_row();
            changed |= gain_row(ui, input);
        }
        InputKind::LineIn => {
            row_label(ui, "Sound card");
            let current = get_str(input, "device").unwrap_or_else(|| "default".into());
            ui.horizontal(|ui| {
                let shown = if current == "default" { "Default input".to_string() } else { current.clone() };
                egui::ComboBox::from_id_salt(("tx_line_in", i)).width(260.0).selected_text(shown).show_ui(ui, |ui| {
                    if ui.selectable_label(current == "default", "Default input").clicked() {
                        set(input, "device", "default");
                        changed = true;
                    }
                    for name in ctx.devices.inputs().to_vec() {
                        if ui.selectable_label(current == name, &name).clicked() {
                            set(input, "device", name.as_str());
                            changed = true;
                        }
                    }
                });
                if ui.small_button("⟳").on_hover_text("Refresh the device list").clicked() {
                    ctx.devices.refresh();
                }
            });
            ui.end_row();
            changed |= gain_row(ui, input);
        }
        InputKind::Stream => {
            row_label(ui, "Stream URL");
            let mut url = get_str(input, "url").unwrap_or_default();
            if ui
                .add(egui::TextEdit::singleline(&mut url).desired_width(320.0).hint_text("http://… (Icecast/Shoutcast, MP3, AAC, Ogg; or a .m3u/.pls)"))
                .changed()
            {
                set(input, "url", url.trim());
                changed = true;
            }
            ui.end_row();
            row_label(ui, "Titles");
            let mut titles = get_bool(input, "stream_titles").unwrap_or(true);
            if ui.checkbox(&mut titles, "Send the stream's \"now playing\" as text messages").changed() {
                set(input, "stream_titles", titles);
                changed = true;
            }
            ui.end_row();
            changed |= gain_row(ui, input);
        }
    });
    changed
}

fn gain_row(ui: &mut Ui, input: &mut dyn TableLike) -> bool {
    row_label(ui, "Gain");
    let mut g = get_num(input, "gain_db").unwrap_or(0.0);
    let changed = ui.add(egui::DragValue::new(&mut g).range(-40.0..=40.0).speed(0.25).suffix(" dB")).changed();
    if changed {
        if g == 0.0 {
            remove(input, "gain_db");
        } else {
            set(input, "gain_db", g);
        }
    }
    ui.end_row();
    changed
}

const APP_KINDS: &[(&str, &str)] = &[
    ("slideshow", "MOT slideshow"),
    ("website", "Broadcast website"),
    ("journaline", "Journaline"),
    ("epg", "Programme guide (EPG)"),
    ("tpeg", "TPEG"),
    ("raw", "Raw data"),
];

/// One data application: type, file or folder, bit rate, part A.
fn app_row(ui: &mut Ui, app: &mut dyn TableLike, id: impl std::hash::Hash + std::fmt::Debug + Copy, ctx: &mut FormCtx, parts: &mut Parts) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        let kind = get_str(app, "type").unwrap_or_else(|| "journaline".into());
        if let Some(v) = combo(ui, (id, "kind"), &kind, APP_KINDS, 170.0) {
            set(app, "type", v);
            changed = true;
        }
        let mut path = get_str(app, "path").unwrap_or_default();
        let hint = match canon(&kind).as_str() {
            "slideshow" => "folder of images",
            "website" => "site folder",
            "journaline" => "page file (.toml)",
            "epg" => "schedule (.toml, optional)",
            _ => "data file",
        };
        if ui.add(egui::TextEdit::singleline(&mut path).desired_width(200.0).hint_text(hint)).changed() {
            if path.is_empty() {
                remove(app, "path");
            } else {
                set(app, "path", path.as_str());
            }
            changed = true;
        }
        if ui.small_button("…").on_hover_text("Choose").clicked() {
            let picked = if matches!(canon(&kind).as_str(), "slideshow" | "website") {
                pick_folder(ctx.base_dir)
            } else {
                pick_file(ctx.base_dir, "Data", &["toml", "json", "bin", "*"])
            };
            if let Some(p) = picked {
                set(app, "path", p.as_str());
                changed = true;
            }
        }
        let mut rate = get_int(app, "bitrate").unwrap_or(2000);
        if ui.add(egui::DragValue::new(&mut rate).range(100..=64_000).speed(50.0).suffix(" bit/s")).changed() {
            set(app, "bitrate", rate);
            changed = true;
        }
        changed |= part_switch(ui, app, "Part A", parts);
    });
    changed
}

const PART_A_HELP: &str = "Send this stream in part A, coded at the Channel card's \"Protection, part A\" instead of \
     \"Protection\" (unequal error protection). Part A bytes take more of the channel, so the other streams get less.";

/// The part A switch of one stream (an audio service or a data application).
fn part_switch(ui: &mut Ui, t: &mut dyn TableLike, text: &str, parts: &mut Parts) -> bool {
    if get_bool(t, "hierarchical").unwrap_or(false) {
        ui.label(RichText::new("hierarchical layer").weak())
            .on_hover_text("This stream is in the hierarchical layer (TOML view), which has its own protection level");
        return false;
    }
    let mut on = is_part_a(t);
    // At part B's most robust level nothing is stronger; switching off always works.
    let possible = on || parts.protection_b > 0;
    let r = ui
        .add_enabled(possible, egui::Checkbox::new(&mut on, text))
        .on_hover_text(PART_A_HELP)
        .on_disabled_hover_text("Protection is already at its most robust level (0): choose a higher Protection in the Channel card first");
    if !r.changed() {
        return false;
    }
    set_part(t, on);
    if let Some(name) = get_str(t, "stream") {
        parts.shared.push((name, on));
    }
    parts.turned_on |= on;
    true
}

/// Whether a stream table sets `part = "A"` (spelled as the station accepts it).
fn is_part_a(t: &dyn TableLike) -> bool {
    get_str(t, "part").is_some_and(|p| matches!(canon(&p).as_str(), "a" | "higher" | "high"))
}

/// Put a stream into part A, or back into part B (the default, so the key goes).
fn set_part(t: &mut dyn TableLike, a: bool) {
    if a {
        set(t, "part", "A");
    } else {
        remove(t, "part");
    }
}

/// Whether any stream is in part A: an audio service or data application with
/// `part = "A"` that is not in the hierarchical layer (which has no part).
fn uses_part_a(doc: &DocumentMut) -> bool {
    let in_a = |t: &dyn TableLike| is_part_a(t) && !get_bool(t, "hierarchical").unwrap_or(false);
    doc.get("service").and_then(Item::as_array_of_tables).is_some_and(|list| {
        list.iter().any(|svc| {
            ["audio", "data"].into_iter().any(|k| svc.get(k).and_then(Item::as_table_like).is_some_and(in_a))
                || svc.get("app").and_then(Item::as_array_of_tables).is_some_and(|apps| apps.iter().any(|app| in_a(app)))
        })
    })
}

/// Give every application of the shared stream `name` the same part, as the station
/// requires.
fn sync_shared_parts(doc: &mut DocumentMut, name: &str, a: bool) {
    let Some(list) = doc.get_mut("service").and_then(Item::as_array_of_tables_mut) else { return };
    for svc in list.iter_mut() {
        if let Some(data) = svc.get_mut("data").and_then(Item::as_table_like_mut)
            && get_str(data, "stream").as_deref() == Some(name)
        {
            set_part(data, a);
        }
        if let Some(apps) = svc.get_mut("app").and_then(Item::as_array_of_tables_mut) {
            for app in apps.iter_mut() {
                if get_str(app, "stream").as_deref() == Some(name) {
                    set_part(app, a);
                }
            }
        }
    }
}

/// With a stream in part A, keep part A's level below part B's (part A is the higher
/// protected part), as far as part B leaves room.
fn keep_part_a_stronger(ch: &mut dyn TableLike) {
    let pb = get_int(ch, "protection_b").unwrap_or(1);
    if get_int(ch, "protection_a").unwrap_or(0) >= pb && pb > 0 {
        set(ch, "protection_a", pb - 1);
    }
}

/// The service's extra data applications (`[[service.app]]`).
fn extra_apps(ui: &mut Ui, svc: &mut Table, i: usize, ctx: &mut FormCtx, parts: &mut Parts) -> bool {
    let mut changed = false;
    let has = svc.get("app").is_some_and(Item::is_array_of_tables);
    if has {
        ui.add_space(4.0);
        ui.label(RichText::new(if svc.contains_key("data") { "More applications" } else { "Data applications" }).strong());
        let apps = svc.get_mut("app").and_then(Item::as_array_of_tables_mut).expect("checked");
        let mut remove_at = None;
        for (k, app) in apps.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                changed |= app_row(ui, app, ("tx_app", i, k), ctx, parts);
                if ui.small_button("Remove").on_hover_text("Remove this application").clicked() {
                    remove_at = Some(k);
                }
            });
        }
        if let Some(k) = remove_at {
            apps.remove(k);
            if apps.is_empty() {
                svc.remove("app");
            }
            changed = true;
        }
    }
    if ui.small_button("+ Data application").on_hover_text("Add a slideshow, Journaline pages, … to this service").clicked() {
        if !has {
            svc.insert("app", Item::ArrayOfTables(ArrayOfTables::new()));
        }
        let apps = svc.get_mut("app").and_then(Item::as_array_of_tables_mut).expect("made above");
        let mut t = Table::new();
        set(&mut t, "type", "slideshow");
        set(&mut t, "path", "slides");
        set(&mut t, "bitrate", 2000i64);
        apps.push(t);
        changed = true;
    }
    changed
}

const FORMATS: &[(&str, &str)] = &[("real", "Real IF (1 channel)"), ("iq", "I/Q (2 channels)")];
const SAMPLE_FORMATS: &[(&str, &str)] = &[("int16", "16-bit"), ("int24", "24-bit"), ("float32", "32-bit float")];

/// The channel layout (robustness mode and bandwidth) of the document, if valid.
fn channel_layout(doc: &DocumentMut) -> Option<ChannelLayout> {
    let ch = doc.get("channel").and_then(Item::as_table_like);
    let mode = match canon(&ch.and_then(|ch| get_str(ch, "mode")).unwrap_or_else(|| "B".into())).as_str() {
        "a" => RobustnessMode::A,
        "b" => RobustnessMode::B,
        "c" => RobustnessMode::C,
        "d" => RobustnessMode::D,
        _ => return None,
    };
    let occupancy = u8::try_from(ch.and_then(|ch| get_int(ch, "occupancy")).unwrap_or(3)).ok()?;
    ChannelLayout::new(mode, SpectrumOccupancy::new(occupancy)?)
}

fn output(ui: &mut Ui, doc: &mut DocumentMut, ctx: &mut FormCtx) -> bool {
    let mut changed = false;
    let layout = channel_layout(doc);
    let out = section(doc, "output");
    grid(ui, "tx_form_output", |ui| {
        row_label(ui, "Signal");
        let format = get_str(out, "format").unwrap_or_else(|| "real".into());
        if let Some(v) = combo(ui, "tx_format", &format, FORMATS, 200.0) {
            set(out, "format", v);
            changed = true;
        }
        ui.end_row();
        if canon(&format) == "iq" {
            row_label(ui, "DC carrier offset");
            let mut off = get_num(out, "iq_offset_hz").unwrap_or(0.0);
            ui.horizontal(|ui| {
                if ui.add(egui::DragValue::new(&mut off).range(-20_000.0..=20_000.0).speed(10.0).suffix(" Hz")).changed() {
                    set(out, "iq_offset_hz", off);
                    changed = true;
                }
                let mut swap = get_bool(out, "iq_swap").unwrap_or(false);
                if ui.checkbox(&mut swap, "I on the right channel").changed() {
                    set(out, "iq_swap", swap);
                    changed = true;
                }
            });
            ui.end_row();
        } else {
            row_label(ui, "IF (DC carrier)");
            let mut auto = !out.contains_key("if_hz");
            let suggested = layout.map_or(12_000.0, suggested_if_hz);
            ui.horizontal(|ui| {
                let hint = "Centre the signal at 12 kHz: the DC carrier at 12 kHz for 9 and 10 kHz, lower for 4.5 and 5 kHz \
                     (all their carriers lie above it) and for 18 and 20 kHz";
                if ui.checkbox(&mut auto, "Automatic").on_hover_text(hint).changed() {
                    if auto {
                        remove(out, "if_hz");
                    } else {
                        set(out, "if_hz", suggested);
                    }
                    changed = true;
                }
                let mut f = if auto { suggested } else { get_num(out, "if_hz").unwrap_or(suggested) };
                if !auto {
                    // The form offers what keeps the signal clear of 0 Hz and 24 kHz; a
                    // value written in the TOML view stays as it is.
                    let (min, max) = layout.map_or((3_000.0, 21_000.0), recommended_if_range_hz);
                    let edit = egui::DragValue::new(&mut f).range(min..=max).clamp_existing_to_range(false).speed(10.0).suffix(" Hz");
                    if ui.add(edit).changed() {
                        set(out, "if_hz", f);
                        changed = true;
                    }
                }
                if let Some(l) = layout {
                    let (_, (lo, hi)) = crate::tx_config::signal_band(l, OutputFormat::Real { if_hz: f });
                    ui.label(RichText::new(format!("signal {:.1}–{:.1} kHz", lo / 1e3, hi / 1e3)).weak());
                }
            });
            ui.end_row();
        }
        row_label(ui, "Level");
        let mut level = get_num(out, "level_dbfs").unwrap_or(-15.0);
        ui.horizontal(|ui| {
            if ui.add(egui::DragValue::new(&mut level).range(-40.0..=-3.0).speed(0.25).suffix(" dBFS RMS")).changed() {
                set(out, "level_dbfs", level);
                changed = true;
            }
            ui.label(RichText::new("OFDM peaks are ~10 dB higher").weak());
        });
        ui.end_row();
        row_label(ui, "Filter");
        let mut band = get_bool(out, "band_limit").unwrap_or(true);
        if ui.checkbox(&mut band, "Band-limit the signal (against the OFDM side lobes)").changed() {
            set(out, "band_limit", band);
            changed = true;
        }
        ui.end_row();
        row_label(ui, "File (as configured)");
        let mut file = get_str(out, "file").unwrap_or_default();
        ui.horizontal(|ui| {
            if ui.add(egui::TextEdit::singleline(&mut file).desired_width(200.0).hint_text("drm_station.wav")).changed() {
                if file.is_empty() {
                    remove(out, "file");
                } else {
                    set(out, "file", file.as_str());
                }
                changed = true;
            }
            let fmt = get_str(out, "sample_format").unwrap_or_else(|| "int16".into());
            if let Some(v) = combo(ui, "tx_sample_format", &fmt, SAMPLE_FORMATS, 110.0) {
                set(out, "sample_format", v);
                changed = true;
            }
        });
        ui.end_row();
        row_label(ui, "Sound card (as configured)");
        let mut dev = get_str(out, "device").unwrap_or_default();
        ui.horizontal(|ui| {
            let shown = if dev.is_empty() { "none".to_string() } else { dev.clone() };
            egui::ComboBox::from_id_salt("tx_out_device_cfg").width(220.0).selected_text(shown).show_ui(ui, |ui| {
                if ui.selectable_label(dev.is_empty(), "none").clicked() {
                    remove(out, "device");
                    changed = true;
                }
                for name in ctx.devices.outputs().to_vec() {
                    if ui.selectable_label(dev == name, &name).clicked() {
                        dev.clone_from(&name);
                        set(out, "device", name.as_str());
                        changed = true;
                    }
                }
            });
            let mut buf = get_int(out, "device_buffer_ms").unwrap_or(400);
            if ui.add(egui::DragValue::new(&mut buf).range(100..=2_000).speed(10.0).prefix("buffer ").suffix(" ms")).changed() {
                set(out, "device_buffer_ms", buf);
                changed = true;
            }
        });
        ui.end_row();
    });
    changed
}

const CHANNEL_MODELS: &[(&str, &str)] = &[
    ("1", "1 — AWGN only"),
    ("2", "2 — Rice with delay"),
    ("3", "3 — US Consortium"),
    ("4", "4 — CCIR poor"),
    ("5", "5 — severe Doppler"),
    ("6", "6 — severe Doppler and delay"),
];

fn simulator(ui: &mut Ui, doc: &mut DocumentMut) -> bool {
    let mut changed = false;
    let mut on = doc.as_table().get("simulate").is_some_and(Item::is_table_like);
    if ui.checkbox(&mut on, "Impair the transmitted signal").changed() {
        if on {
            let mut t = Table::new();
            set(&mut t, "channel", 3i64);
            set(&mut t, "snr_db", 20.0);
            doc.as_table_mut().insert("simulate", Item::Table(t));
        } else {
            doc.as_table_mut().remove("simulate");
        }
        changed = true;
    }
    if !on {
        return changed;
    }
    let sim = section(doc, "simulate");
    grid(ui, "tx_form_sim", |ui| {
        row_label(ui, "Channel model");
        let model = get_int(sim, "channel").unwrap_or(1).to_string();
        if let Some(v) = combo(ui, "tx_sim_model", &model, CHANNEL_MODELS, 240.0) {
            set(sim, "channel", v.parse::<i64>().unwrap_or(1));
            changed = true;
        }
        ui.end_row();
        row_label(ui, "Noise");
        let mut noisy = sim.contains_key("snr_db");
        ui.horizontal(|ui| {
            if ui.checkbox(&mut noisy, "SNR").changed() {
                if noisy {
                    set(sim, "snr_db", 20.0);
                } else {
                    remove(sim, "snr_db");
                }
                changed = true;
            }
            if noisy {
                let mut snr = get_num(sim, "snr_db").unwrap_or(20.0);
                if ui.add(egui::DragValue::new(&mut snr).range(-5.0..=60.0).speed(0.1).suffix(" dB")).changed() {
                    set(sim, "snr_db", snr);
                    changed = true;
                }
            }
        });
        ui.end_row();
        row_label(ui, "Frequency offset");
        let mut fo = get_num(sim, "frequency_offset_hz").unwrap_or(0.0);
        if ui.add(egui::DragValue::new(&mut fo).range(-500.0..=500.0).speed(0.5).suffix(" Hz")).changed() {
            set(sim, "frequency_offset_hz", fo);
            changed = true;
        }
        ui.end_row();
        row_label(ui, "Clock error");
        let mut sro = get_num(sim, "sample_rate_offset_ppm").unwrap_or(0.0);
        if ui.add(egui::DragValue::new(&mut sro).range(-500.0..=500.0).speed(0.5).suffix(" ppm")).changed() {
            set(sim, "sample_rate_offset_ppm", sro);
            changed = true;
        }
        ui.end_row();
    });
    changed
}

/// `[mdi]`: transmit MDI from a content server instead of the services.
fn modulator(ui: &mut Ui, doc: &mut DocumentMut, ctx: &mut FormCtx) -> bool {
    let mut changed = false;
    let mut on = doc.get("mdi").is_some_and(Item::is_table_like);
    if ui
        .checkbox(&mut on, "Transmit MDI from a content server instead of the services")
        .on_hover_text(
            "DecDRM as a modulator: the multiplex comes over UDP (MDI, ETSI TS 102 820) from a content server \
             (Dream, a commercial one) or from a recording, and is transmitted as it is.",
        )
        .changed()
    {
        if on {
            let t = section(doc, "mdi");
            if get_str(t, "input").is_none() {
                set(t, "input", "8000");
            }
        } else {
            doc.remove("mdi");
        }
        changed = true;
    }
    if !on {
        return changed;
    }
    let t = section(doc, "mdi");
    grid(ui, "tx_form_mdi", |ui| {
        row_label(ui, "Input");
        ui.horizontal(|ui| {
            let mut input = get_str(t, "input").unwrap_or_default();
            if ui
                .add(egui::TextEdit::singleline(&mut input).desired_width(220.0).hint_text("UDP port, group:port or a recording"))
                .on_hover_text(
                    "A UDP port (8000), a multicast group (239.1.2.3:8000), interface and group \
                     (192.168.1.5:239.1.2.3:8000), a sender too (10.0.0.9:192.168.1.5:239.1.2.3:8000), or a recording \
                     (.pcap, .rsM, …)",
                )
                .changed()
            {
                set(t, "input", input.as_str());
                changed = true;
            }
            if ui.small_button("…").on_hover_text("Choose an MDI recording").clicked()
                && let Some(p) = pick_file(ctx.base_dir, "MDI recordings", super::source::MDI_EXTENSIONS)
            {
                set(t, "input", p.as_str());
                changed = true;
            }
        });
        ui.end_row();
        row_label(ui, "Reserve");
        let mut n = get_int(t, "buffer_frames").unwrap_or(3);
        if ui
            .add(egui::DragValue::new(&mut n).range(1..=50).suffix(" frames"))
            .on_hover_text("With a sound card: frames (400 ms each) held before transmitting, a reserve against network jitter")
            .changed()
        {
            set(t, "buffer_frames", n);
            changed = true;
        }
        ui.end_row();
    });
    ui.label(
        RichText::new("The channel, the services, the clock and the alternative frequencies come from the MDI.")
            .weak()
            .small(),
    );
    changed
}

fn clock(ui: &mut Ui, doc: &mut DocumentMut) -> bool {
    let mut changed = false;
    let t = section(doc, "time");
    grid(ui, "tx_form_time", |ui| {
        row_label(ui, "Time and date");
        let mut on = get_bool(t, "enabled").unwrap_or(true);
        if ui.checkbox(&mut on, "Send (SDC type 8, once per minute)").changed() {
            set(t, "enabled", on);
            changed = true;
        }
        ui.end_row();
        row_label(ui, "Local time offset");
        let mut has = t.contains_key("local_offset_minutes");
        ui.horizontal(|ui| {
            if ui.checkbox(&mut has, "Signal").changed() {
                if has {
                    set(t, "local_offset_minutes", 0i64);
                } else {
                    remove(t, "local_offset_minutes");
                }
                changed = true;
            }
            if has {
                let mut m = get_int(t, "local_offset_minutes").unwrap_or(0);
                if ui.add(egui::DragValue::new(&mut m).range(-720..=840).speed(30.0).suffix(" min")).changed() {
                    set(t, "local_offset_minutes", (m / 30) * 30);
                    changed = true;
                }
            }
        });
        ui.end_row();
    });
    changed
}

/// The multiplex as a bar: one segment per stream, sized by its bytes.
pub fn capacity_bar(ui: &mut Ui, bar: &PlanBar) {
    let used: usize = bar.segments.iter().map(|s| s.bytes).sum();
    let kbps = |bytes: usize| bytes as f64 * 8.0 / 400.0;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Multiplex").strong());
        ui.label(RichText::new(format!("{:.2} kbit/s MSC capacity, {:.0} % used", kbps(bar.capacity), 100.0 * used as f64 / bar.capacity.max(1) as f64)).weak());
    });
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 26.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let (background, audio_fill) = (ui.visuals().extreme_bg_color, ui.visuals().selection.bg_fill);
    painter.rect_filled(rect, egui::CornerRadius::same(4), background);
    let data_fill = Color32::from_rgb(0x1D, 0x9E, 0x75);
    let mut x = rect.left();
    for s in &bar.segments {
        let w = rect.width() * s.bytes as f32 / bar.capacity.max(1) as f32;
        let seg = egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(w.max(1.0), rect.height()));
        painter.rect_filled(seg.shrink2(egui::vec2(0.5, 0.0)), egui::CornerRadius::same(3), if s.audio { audio_fill } else { data_fill });
        if s.part_a {
            painter.rect_stroke(seg.shrink(1.0), egui::CornerRadius::same(3), egui::Stroke::new(2.0, PART_A_COLOR), egui::StrokeKind::Inside);
        }
        let text = format!("{}{} · {:.1} kbit/s", s.label, part_a_note(s), kbps(s.bytes));
        let galley = painter.layout_no_wrap(text, egui::FontId::proportional(12.0), Color32::WHITE);
        if galley.size().x + 8.0 < w {
            painter.galley(egui::pos2(seg.left() + 4.0, seg.center().y - galley.size().y / 2.0), galley, Color32::WHITE);
        }
        x += w;
    }
    let mut legend = String::new();
    for s in &bar.segments {
        legend.push_str(&format!("{}{}: {:.2} kbit/s   ", s.label, part_a_note(s), kbps(s.bytes)));
    }
    ui.label(RichText::new(legend.trim_end()).weak().small());
}

/// Outline of the part A streams in the bar.
const PART_A_COLOR: Color32 = Color32::from_rgb(0xEF, 0x9F, 0x27);

fn part_a_note(s: &Segment) -> &'static str {
    if s.part_a { " (part A)" } else { "" }
}

/// Ask for a file; the path relative to `base` when it lies below it.
fn pick_file(base: &Path, what: &str, extensions: &[&str]) -> Option<String> {
    let mut dialog = rfd::FileDialog::new().set_directory(base);
    if !extensions.contains(&"*") {
        dialog = dialog.add_filter(what, extensions);
    }
    dialog.add_filter("All files", &["*"]).pick_file().map(|p| relative(base, &p))
}

fn pick_folder(base: &Path) -> Option<String> {
    rfd::FileDialog::new().set_directory(base).pick_folder().map(|p| relative(base, &p))
}

fn relative(base: &Path, p: &Path) -> String {
    p.strip_prefix(base).map_or_else(|_| p.display().to_string(), |r| r.display().to_string().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_like_the_station_does() {
        assert_eq!(canon("64-QAM"), canon("QAM64"));
        assert_eq!(canon("64qam"), "64qam");
        assert_eq!(canon("HE-AAC v2"), canon("he_aac_v2"));
        assert_ne!(canon("16-QAM"), canon("64-QAM"));
    }

    #[test]
    fn edits_keep_comments_and_other_content() {
        let mut doc: DocumentMut = "[channel]\nmode = \"B\"   # robustness mode\n\n[[afs.multiplex]]\nkhz = [5990]\n".parse().unwrap();
        set(section(&mut doc, "channel"), "mode", "A");
        let text = doc.to_string();
        assert!(text.contains("mode = \"A\"   # robustness mode"), "{text}");
        assert!(text.contains("[[afs.multiplex]]"), "{text}");
    }

    #[test]
    fn hex_ids_and_inline_inputs() {
        let mut doc: DocumentMut = "[[service]]\nlabel = \"X\"\nid = 0xD0D001\n[service.audio]\ncodec = \"aac\"\ninput = { tone_hz = 600.0 }\n".parse().unwrap();
        let svc = doc.get_mut("service").and_then(Item::as_array_of_tables_mut).unwrap().get_mut(0).unwrap();
        set_raw(svc, "id", "0xABCDEF");
        let input = child(child(svc, "audio"), "input");
        assert_eq!(InputKind::of(input), InputKind::Tone);
        set(input, "tone_hz", 1000.0);
        let text = doc.to_string();
        assert!(text.contains("id = 0xABCDEF"), "{text}");
        assert!(text.contains("input = { tone_hz = 1000.0 }"), "{text}");
        // The edited document is still a valid station configuration.
        let cfg: decdrm_station::StationConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg.services[0].id, 0xABCDEF);
    }

    #[test]
    fn part_a_switches() {
        let mut doc: DocumentMut = r#"
[channel]
msc_mode = "16-QAM"
protection_b = 1   # part B
protection_a = 1
[[service]]
label = "R"
id = 0xD0D001
[service.audio]
codec = "aac"
input = { tone_hz = 1000.0 }
[[service.app]]
type = "journaline"
stream = "data"
[[service.app]]
type = "epg"
stream = "data"
[[service.app]]
type = "slideshow"
part = "A"
hierarchical = true
"#
        .parse()
        .unwrap();
        // A hierarchical stream has no part.
        assert!(!uses_part_a(&doc));
        let svc = doc.get_mut("service").and_then(Item::as_array_of_tables_mut).unwrap().get_mut(0).unwrap();
        set_part(child(svc, "audio"), true);
        assert!(uses_part_a(&doc));
        // Part A's level drops below part B's; part B's comment stays.
        keep_part_a_stronger(section(&mut doc, "channel"));
        assert_eq!(get_int(section(&mut doc, "channel"), "protection_a"), Some(0));
        assert!(doc.to_string().contains("protection_b = 1   # part B"));
        // A shared stream switches as a whole, and back.
        sync_shared_parts(&mut doc, "data", true);
        assert_eq!(doc.to_string().matches("part = \"A\"").count(), 4, "{doc}");
        sync_shared_parts(&mut doc, "data", false);
        assert_eq!(doc.to_string().matches("part = \"A\"").count(), 2, "{doc}");
        let cfg: decdrm_station::StationConfig = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(cfg.services[0].audio.as_ref().unwrap().part, decdrm_station::Part::A);
        assert_eq!(cfg.services[0].apps[0].part, decdrm_station::Part::B);
        // Nothing is stronger than part B's level 0: part A's level stays (the check
        // then explains); a level that is already stronger stays too.
        let ch = section(&mut doc, "channel");
        set(ch, "protection_b", 0i64);
        keep_part_a_stronger(ch);
        assert_eq!(get_int(ch, "protection_a"), Some(0));
        set(ch, "msc_mode", "64-QAM");
        set(ch, "protection_b", 3i64);
        set(ch, "protection_a", 1i64);
        keep_part_a_stronger(ch);
        assert_eq!(get_int(ch, "protection_a"), Some(1));
    }

    #[test]
    fn new_services_are_valid() {
        let mut doc: DocumentMut = "[channel]\nmode = \"B\"\noccupancy = 3\nmsc_mode = \"16-QAM\"\n".parse().unwrap();
        doc.as_table_mut().insert("service", Item::ArrayOfTables(ArrayOfTables::new()));
        let list = doc.get_mut("service").and_then(Item::as_array_of_tables_mut).unwrap();
        list.push(new_service(0, true));
        list.push(new_service(1, false));
        let cfg: decdrm_station::StationConfig = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(cfg.services.len(), 2);
        assert_eq!(cfg.services[1].id, 0xD0D002);
    }
}
