//! Schedule tab: which DRM stations are on the air now, from EiBi's or Dream's
//! schedule — Dream's Stations dialog as a tab — so a web SDR can be tuned to them.
//! The state lives in [`crate::schedule::ScheduleView`]; this module draws it.
//!
//! Rows are painted cell by cell inside `ScrollArea::show_rows`, which lays out only
//! the visible rows: EiBi's full list (some 15 000 broadcasts) costs no more than a
//! screenful.

use super::{led_color, placeholder};
use crate::indicators::Led;
use crate::schedule::{Origin, Reception, Row, ScheduleSettings, ScheduleView, Status, describe};
use decdrm_schedule::{
    AirState, ENDING_SOON_MIN, Entry, Format, MATCH_TOLERANCE_KHZ, PREVIEW_MIN, Season, UtcTime,
    format_khz,
};
use eframe::egui::{
    self, Color32, CornerRadius, Painter, Rect, RichText, Sense, TextStyle, TextWrapMode, Ui, pos2,
    vec2,
};
use std::time::Duration;

/// Draw the tab. `reception` is the frequency being received (typed, or from the
/// recording's file name), whose rows are highlighted.
pub fn show(
    ui: &mut Ui,
    view: &mut ScheduleView,
    settings: &mut ScheduleSettings,
    reception: Option<Reception>,
) {
    let now = UtcTime::now();
    // The clock and the on-air states change on the minute: repaint then.
    let to_next_minute = 60 - u64::from(now.second_of_day() % 60);
    ui.ctx()
        .request_repaint_after(Duration::from_secs(to_next_minute));
    controls(ui, view, settings, now);
    view.update_rows(settings, now);
    ui.horizontal_wrapped(|ui| {
        update_buttons(ui, view, settings, now);
        ui.separator();
        status(ui, view, settings);
    });
    warnings(ui, view, now);
    if let Some(r) = &reception {
        reception_line(ui, view, r, settings.all_broadcasts);
    }
    ui.separator();
    if let Some(index) = table(ui, view, reception.as_ref()) {
        // A click copies the frequency (to paste into the web SDR) and highlights it.
        if let Some(e) = view.data.as_ref().and_then(|d| d.entries().get(index)) {
            let khz = e.khz_label();
            ui.ctx().copy_text(khz.clone());
            settings.freq = khz;
        }
    }
}

/// Clock, source, view options, filter, frequency.
fn controls(ui: &mut Ui, view: &mut ScheduleView, settings: &mut ScheduleSettings, now: UtcTime) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!("{} UTC", now.hhmm()))
                .monospace()
                .strong(),
        )
        .on_hover_text(format!(
            "{} ({}). Broadcast schedules are in UTC.",
            now.date(),
            now.weekday().name()
        ));
        ui.separator();
        let current = view
            .source()
            .map(|s| s.label().to_string())
            .unwrap_or_default();
        let mut chosen = None;
        egui::ComboBox::from_id_salt("schedule_source")
            .selected_text(current)
            .show_ui(ui, |ui| {
                for s in &view.sources {
                    let selected = s.name.eq_ignore_ascii_case(&settings.source);
                    let hover = format!("{} format, {}", s.format, s.url_at(now.date()));
                    if ui
                        .selectable_label(selected, s.label())
                        .on_hover_text(hover)
                        .clicked()
                    {
                        chosen = Some(s.name.clone());
                    }
                }
            })
            .response
            .on_hover_text(
                "Where the schedule comes from. EiBi lists every shortwave broadcast (DRM ones \
                 are marked by the word DRM); Dream's list (DRMDX) only DRM. More sources: \
                 sources.toml in the schedule folder (see the user guide).",
            );
        if let Some(name) = chosen
            && !name.eq_ignore_ascii_case(&settings.source)
        {
            settings.source = name.clone();
            view.request_load(&name);
        }
        ui.selectable_value(&mut settings.show_all, false, "On air now")
            .on_hover_text(format!(
                "Broadcasts on the air now, and those starting within {PREVIEW_MIN} minutes."
            ));
        ui.selectable_value(&mut settings.show_all, true, "All")
            .on_hover_text("The whole schedule.");
        let eibi = view.source().is_some_and(|s| s.format == Format::Eibi);
        ui.add_enabled(
            eibi,
            egui::Checkbox::new(&mut settings.all_broadcasts, "Non-DRM too"),
        )
        .on_hover_text("Also list EiBi's analogue (AM) broadcasts.");
        ui.separator();
        ui.label("Filter");
        ui.add(
            egui::TextEdit::singleline(&mut settings.filter)
                .desired_width(130.0)
                .hint_text("station, language…"),
        );
        ui.label("Frequency");
        ui.add(
            egui::TextEdit::singleline(&mut settings.freq)
                .desired_width(70.0)
                .hint_text("kHz"),
        )
        .on_hover_text(
            "The frequency you receive (kHz, or e.g. 6.14 MHz): its rows are highlighted. \
             Empty: the frequency in the recording's file name (KiwiSDR: …_6140.00_iq.wav).",
        );
        if !settings.freq.is_empty() && ui.small_button("×").on_hover_text("Clear").clicked() {
            settings.freq.clear();
        }
    });
}

/// *Update schedule* (the only way anything is downloaded) and *Folder*.
fn update_buttons(ui: &mut Ui, view: &mut ScheduleView, settings: &ScheduleSettings, now: UtcTime) {
    let updating = view.updating();
    let url = view
        .source()
        .map(|s| s.url_at(now.date()))
        .unwrap_or_default();
    let update = ui
        .add_enabled(!updating, egui::Button::new("⟳ Update schedule"))
        .on_hover_text(format!(
            "Download {url} (with curl or wget) into {}",
            view.dir().display()
        ));
    if update.clicked() {
        let name = settings.source.clone();
        view.request_update(&name);
    }
    if updating {
        ui.spinner();
    }
    if ui
        .button("Folder")
        .on_hover_text(format!(
            "Open the schedule folder {} (schedule files, sources.toml)",
            view.dir().display()
        ))
        .clicked()
    {
        let dir = view.dir().to_path_buf();
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|()| open::that(&dir)) {
            view.note(format!("schedule: cannot open {}: {e}", dir.display()));
        }
    }
}

/// File, update time and counts; or why there is nothing to show.
fn status(ui: &mut Ui, view: &ScheduleView, settings: &ScheduleSettings) {
    match &view.status {
        Status::Loading => {
            ui.spinner();
            ui.label("Reading the schedule…");
        }
        Status::Missing { url, path } => {
            let label = view.source().map(|s| s.label()).unwrap_or("");
            placeholder(
                ui,
                &format!(
                    "No {label} schedule yet. Press “Update schedule” to download it from {url}, \
                     or save that file as {}.",
                    path.display()
                ),
            );
        }
        Status::Failed(e) => {
            ui.colored_label(ui.visuals().warn_fg_color, e);
        }
        Status::Ready => {
            if let Some(data) = &view.data {
                let l = &data.loaded;
                let file = l
                    .copy
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let updated = l
                    .modified
                    .map(|t| format!(" · updated {t}"))
                    .unwrap_or_default();
                let (count, what) = if settings.all_broadcasts {
                    (l.schedule.entries.len(), "broadcasts")
                } else {
                    (l.schedule.drm_count(), "DRM broadcasts")
                };
                let filtered = if settings.filter.trim().is_empty() {
                    ""
                } else {
                    " (filtered)"
                };
                ui.label(
                    RichText::new(format!(
                        "{file}{updated} · {count} {what} · {} on the air{filtered}",
                        view.on_air()
                    ))
                    .weak(),
                )
                .on_hover_text(l.copy.path.display().to_string());
            }
        }
    }
}

/// Problems worth a line of their own: an unreadable `sources.toml`, last season's
/// file, a failed download.
fn warnings(ui: &mut Ui, view: &ScheduleView, now: UtcTime) {
    let warn = ui.visuals().warn_fg_color;
    if let Some(w) = &view.sources_warning {
        ui.colored_label(warn, w);
    }
    if let Some(copy) = view.data.as_ref().map(|d| &d.loaded.copy)
        && let (false, Some(season)) = (copy.current, copy.season)
    {
        ui.colored_label(
            warn,
            format!(
                "This is the schedule of season {season}; it is season {} now: press “Update \
                 schedule”.",
                Season::at(now.date())
            ),
        );
    }
    if let Some(e) = &view.update_error {
        ui.colored_label(warn, format!("Update failed: {e}"));
    }
}

/// "Receiving 6140 kHz: KCBS Pyongyang …" for the received frequency.
fn reception_line(ui: &mut Ui, view: &ScheduleView, r: &Reception, all_broadcasts: bool) {
    let khz = format_khz(r.khz);
    let origin = match (r.origin, r.time) {
        (Origin::Typed, _) => String::new(),
        (Origin::FileName, None) => " (from the file name)".into(),
        (Origin::FileName, Some(t)) => format!(" (from the file name, recorded {t})"),
    };
    let head = format!("▶ Receiving {khz} kHz{origin}");
    let text = if view.data.is_none() {
        head
    } else {
        let (on, total) = view.reception_matches(r, all_broadcasts);
        let when = if r.time.is_some() { "then" } else { "now" };
        match (on.as_slice(), total) {
            ([], 0) => format!(
                "{head}: nothing scheduled within ±{} kHz",
                format_khz(MATCH_TOLERANCE_KHZ)
            ),
            ([], n) => format!("{head}: {n} entries, none on the air {when} (highlighted)"),
            (on, _) => {
                let mut list: Vec<String> = on.iter().take(3).map(|e| describe(e)).collect();
                if on.len() > 3 {
                    list.push(format!("{} more", on.len() - 3));
                }
                format!("{head}: {}", list.join(" · "))
            }
        }
    };
    let color = ui.visuals().hyperlink_color;
    ui.add(egui::Label::new(RichText::new(text).strong().color(color)).wrap());
}

/// Column layout: left edges and widths, from the available width.
struct Columns {
    /// (title, x offset, width, right-aligned).
    cols: Vec<(&'static str, f32, f32, bool)>,
}

/// Index of each column in [`Columns::cols`].
const C_DOT: usize = 0;
const C_KHZ: usize = 1;
const C_UTC: usize = 2;
const C_DAYS: usize = 3;
const C_STATION: usize = 4;
const C_LANGUAGE: usize = 5;
const C_TARGET: usize = 6;
const C_SITE: usize = 7;
const C_KW: usize = 8;
const C_NOTE: usize = 9;

impl Columns {
    fn new(width: f32, power: bool) -> Self {
        let fixed = [
            ("", 18.0, false),
            ("kHz", 62.0, true),
            ("UTC", 84.0, false),
            ("Days", 100.0, false),
        ];
        let kw = if power { 48.0 } else { 0.0 };
        let fixed_total: f32 = fixed.iter().map(|c| c.1).sum::<f32>() + 96.0 + kw;
        let flex = (width - fixed_total).max(320.0);
        let widths = [
            ("Station", flex * 0.33, false),
            ("Language", 96.0, false),
            ("Target", flex * 0.2, false),
            ("Site", flex * 0.29, false),
            ("kW", kw, true),
            ("Valid / note", flex * 0.18, false),
        ];
        let mut x = 0.0;
        let cols = fixed
            .into_iter()
            .chain(widths)
            .map(|(title, w, right)| {
                let c = (title, x, w, right);
                x += w;
                c
            })
            .collect();
        Self { cols }
    }

    fn rect(&self, row: Rect, col: usize) -> Rect {
        let (_, x, w, _) = self.cols[col];
        Rect::from_min_size(pos2(row.left() + x, row.top()), vec2(w, row.height()))
    }
}

/// Paint `text` into `cell`, cut with "…" when too wide. Returns whether it was cut.
fn cell(ui: &Ui, painter: &Painter, cell: Rect, text: &str, color: Color32, right: bool) -> bool {
    if text.is_empty() || cell.width() < 8.0 {
        return false;
    }
    let galley = egui::WidgetText::from(RichText::new(text).color(color)).into_galley(
        ui,
        Some(TextWrapMode::Truncate),
        cell.width() - 8.0,
        TextStyle::Body,
    );
    let y = cell.center().y - galley.size().y / 2.0;
    let x = if right {
        cell.right() - 8.0 - galley.size().x
    } else {
        cell.left() + 2.0
    };
    let cut = galley.elided;
    painter.galley(pos2(x, y), galley, color);
    cut
}

fn state_color(state: AirState, dark: bool) -> Color32 {
    match state {
        AirState::OnAir => led_color(Led::Green, dark),
        AirState::EndingSoon => Color32::from_rgb(240, 120, 40),
        AirState::StartingSoon => led_color(Led::Yellow, dark),
        AirState::Off => led_color(Led::Off, dark),
    }
}

/// The table (header and rows). Returns the entry index of a clicked row.
fn table(ui: &mut Ui, view: &ScheduleView, reception: Option<&Reception>) -> Option<usize> {
    let data = view.data.as_ref()?;
    let entries = data.entries();
    let rows: &[Row] = view.rows();
    if rows.is_empty() {
        placeholder(
            ui,
            "Nothing to list: no broadcast on the air matches. “All” shows the whole schedule.",
        );
        return None;
    }
    let power = rows.iter().any(|r| entries[r.index].power_kw.is_some());
    let cols = Columns::new(ui.available_width(), power);
    let row_h = ui.text_style_height(&TextStyle::Body) + 6.0;
    let dark = ui.visuals().dark_mode;

    // Header.
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::hover());
    let strong = ui.visuals().strong_text_color();
    for (i, (title, _, _, right)) in cols.cols.iter().enumerate() {
        cell(ui, ui.painter(), cols.rect(rect, i), title, strong, *right);
    }
    response.on_hover_text(format!(
        "Dot: green on the air, orange ends within {ENDING_SOON_MIN} min, yellow starts within \
         {PREVIEW_MIN} min, grey off. Highlighted: the frequency you receive (±{} kHz). Click a \
         row to copy its frequency.",
        format_khz(MATCH_TOLERANCE_KHZ)
    ));

    let mut clicked = None;
    ui.scope(|ui| {
        // `show_rows` places row i at i × (height + item spacing): no gaps between rows.
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical()
            .id_salt("schedule_rows")
            .auto_shrink([false, false])
            .show_rows(ui, row_h, rows.len(), |ui, range| {
                for row in &rows[range] {
                    let e = &entries[row.index];
                    let matched =
                        reception.is_some_and(|r| e.matches_frequency(r.khz, MATCH_TOLERANCE_KHZ));
                    if draw_row(ui, &cols, row_h, e, row.state, matched, dark) {
                        clicked = Some(row.index);
                    }
                }
            });
    });
    clicked
}

/// One row; returns whether it was clicked.
fn draw_row(
    ui: &mut Ui,
    cols: &Columns,
    row_h: f32,
    e: &Entry,
    state: AirState,
    matched: bool,
    dark: bool,
) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
    let v = ui.visuals();
    let painter = ui.painter();
    if matched {
        painter.rect_filled(
            rect,
            CornerRadius::same(2),
            v.selection.bg_fill.gamma_multiply(0.6),
        );
    } else if response.hovered() {
        painter.rect_filled(rect, CornerRadius::same(2), v.widgets.hovered.weak_bg_fill);
    }
    let dot = cols.rect(rect, C_DOT);
    painter.circle_filled(dot.center(), 4.5, state_color(state, dark));
    let color = match (matched, state) {
        (true, _) => v.strong_text_color(),
        (false, AirState::Off) => v.weak_text_color(),
        _ => v.text_color(),
    };
    let note = [e.validity_label(), e.note.clone()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    let texts = [
        (C_KHZ, e.khz_label()),
        (C_UTC, e.times()),
        (C_DAYS, e.days_label()),
        (C_STATION, e.station.clone()),
        (C_LANGUAGE, e.language.clone()),
        (C_TARGET, e.target.clone()),
        (C_SITE, e.site.clone()),
        (C_KW, e.power_kw.map(format_khz).unwrap_or_default()),
        (C_NOTE, note),
    ];
    for (col, text) in &texts {
        let right = cols.cols[*col].3;
        cell(ui, painter, cols.rect(rect, *col), text, color, right);
    }
    let clicked = response.clicked();
    // The details only when the tooltip shows (the closure runs then).
    response.on_hover_ui(|ui| {
        ui.label(details(e));
    });
    clicked
}

/// Everything about an entry, for the row's tooltip.
fn details(e: &Entry) -> String {
    let mut lines = vec![
        format!(
            "{} kHz · {} UTC · {}{}",
            e.khz_label(),
            e.times(),
            e.days_label(),
            if e.drm { " · DRM" } else { "" }
        ),
        e.station.clone(),
    ];
    let pairs = [
        ("Language", e.language.clone()),
        ("Target", e.target.clone()),
        ("Country", e.country.clone()),
        ("Site", e.site.clone()),
        (
            "Power",
            e.power_kw
                .map(|p| format!("{} kW", format_khz(p)))
                .unwrap_or_default(),
        ),
        ("Valid", e.validity_label()),
        ("Note", e.note.clone()),
    ];
    lines.extend(
        pairs
            .into_iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| format!("{k}: {v}")),
    );
    lines.push(format!(
        "Line {} of the schedule file. Click: copy {} and highlight it.",
        e.line,
        e.khz_label()
    ));
    lines.join("\n")
}
