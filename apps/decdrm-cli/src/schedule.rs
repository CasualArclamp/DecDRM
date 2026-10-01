//! `decdrm schedule` — which DRM stations are on the air now, from EiBi's or Dream's
//! broadcast schedule (crate `decdrm-schedule`), to know what to tune a web SDR to.
//!
//! The schedule is a local file in the per-user schedule directory (next to the GUI's
//! settings; `--dir` for another); `--update` downloads it first with curl or wget —
//! nothing is downloaded otherwise. `--at` evaluates another moment, `--freq` shows what
//! is scheduled near a frequency, `--all` lists every entry instead of those on the air.

use anyhow::{Context, Result};
use decdrm_schedule::{AirState, Entry, PREVIEW_MIN, UtcTime, source};
use std::path::PathBuf;

#[derive(clap::Args)]
pub struct ScheduleArgs {
    /// List every entry, not only those on the air (or starting within 15 minutes).
    #[arg(long)]
    all: bool,
    /// Download the schedule first (with curl or wget).
    #[arg(long)]
    update: bool,
    /// Look at this UTC time instead of now (ISO 8601, e.g. 2026-10-01T14:30Z).
    #[arg(long, value_name = "TIME", value_parser = parse_time)]
    at: Option<UtcTime>,
    /// Only entries near this frequency (kHz, or e.g. "6.14 MHz").
    #[arg(long, value_name = "KHZ", value_parser = parse_khz)]
    freq: Option<f64>,
    /// How far from --freq an entry may be, kHz.
    #[arg(long, value_name = "KHZ", default_value_t = decdrm_schedule::MATCH_TOLERANCE_KHZ)]
    tolerance: f64,
    /// Schedule source: "eibi" (EiBi, all shortwave broadcasts), "dream" (Dream's DRMDX
    /// list) or one from sources.toml (see --sources).
    #[arg(long, value_name = "NAME", default_value = "eibi")]
    source: String,
    /// Also list broadcasts that are not DRM (EiBi's schedule has all of them).
    #[arg(long)]
    all_broadcasts: bool,
    /// Only entries whose station, language, target, country or site contains TEXT.
    #[arg(long, value_name = "TEXT")]
    filter: Option<String>,
    /// List the sources and their local files, then exit.
    #[arg(long)]
    sources: bool,
    /// Schedule directory (default: `schedule` in DecDRM's per-user settings directory).
    #[arg(long, value_name = "DIR")]
    dir: Option<PathBuf>,
}

fn parse_time(s: &str) -> Result<UtcTime, String> {
    UtcTime::parse(s).ok_or_else(|| format!("not an ISO 8601 time: {s} (e.g. 2026-10-01T14:30Z)"))
}

fn parse_khz(s: &str) -> Result<f64, String> {
    decdrm_schedule::parse_frequency_input(s).ok_or_else(|| format!("not a frequency in 100 kHz … 30 MHz: {s}"))
}

pub fn run(a: ScheduleArgs) -> Result<()> {
    let dir = match a.dir.clone() {
        Some(d) => d,
        None => {
            source::default_dir().context("no per-user configuration directory (APPDATA / HOME unset); give --dir")?
        }
    };
    let (sources, warning) = source::load_sources(&dir);
    if let Some(w) = warning {
        eprintln!("warning: {w}");
    }
    let now = a.at.unwrap_or_else(UtcTime::now);
    let date = now.date();
    if a.sources {
        list_sources(&sources, &dir, now);
        return Ok(());
    }
    let names = || sources.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ");
    let src = source::find_source(&sources, &a.source)
        .with_context(|| format!("no schedule source \"{}\" (there are: {})", a.source, names()))?;

    if a.update {
        println!("downloading {} …", src.url_at(date));
        let u = source::update(src, &dir, date)?;
        println!(
            "saved {} ({:.0} kB, {} entries, {} DRM)",
            u.loaded.copy.path.display(),
            u.bytes as f64 / 1e3,
            u.loaded.schedule.entries.len(),
            u.loaded.schedule.drm_count()
        );
    }
    let loaded = source::load(src, &dir, date)?.with_context(|| {
        format!(
            "no {} schedule in {} yet: run `decdrm schedule --update{}` to download {}",
            src.label(),
            dir.display(),
            if a.source.eq_ignore_ascii_case("eibi") { String::new() } else { format!(" --source {}", src.name) },
            src.url_at(date)
        )
    })?;
    let schedule = &loaded.schedule;
    let updated = loaded.modified.map(|t| format!(", updated {t}")).unwrap_or_default();
    println!(
        "{}: {}{updated} — {} entries, {} DRM",
        src.label(),
        loaded.copy.path.display(),
        schedule.entries.len(),
        schedule.drm_count()
    );
    match schedule.skipped.len() {
        0 => {}
        1 => println!("(1 unreadable line skipped)"),
        n => println!("({n} unreadable lines skipped)"),
    }
    if let (false, Some(season)) = (loaded.copy.current, loaded.copy.season) {
        println!(
            "note: this file is for season {season}; at {date} it is season {} — `--update` fetches it",
            decdrm_schedule::Season::at(date)
        );
    }

    let filter = a.filter.as_deref().map(str::to_lowercase);
    let candidates: Vec<&Entry> = schedule
        .entries
        .iter()
        .filter(|e| a.all_broadcasts || e.drm)
        .filter(|e| a.freq.is_none_or(|f| e.matches_frequency(f, a.tolerance)))
        .filter(|e| filter.as_deref().is_none_or(|f| e.matches_text(f)))
        .collect();
    let rows: Vec<(&Entry, AirState)> = candidates
        .iter()
        .map(|e| (*e, e.state_at(now, PREVIEW_MIN)))
        .filter(|(_, s)| a.all || *s != AirState::Off)
        .collect();

    let what = if a.all_broadcasts { "broadcasts" } else { "DRM broadcasts" };
    let khz = decdrm_schedule::format_khz;
    let near = a.freq.map(|f| format!(" within ±{} kHz of {} kHz", khz(a.tolerance), khz(f))).unwrap_or_default();
    let when = format!("{now} ({})", now.weekday().name());
    if a.all {
        println!("\nAll {what}{near} ({}); * = on the air at {when}:", rows.len());
    } else {
        let on = rows.iter().filter(|(_, s)| s.is_on()).count();
        let what = if a.all_broadcasts { "Broadcasts" } else { what };
        println!("\n{what}{near} on the air at {when}: {on}");
    }
    print_table(&rows);
    if rows.is_empty() && a.freq.is_some() && !a.all && !candidates.is_empty() {
        println!("\nscheduled there at other times:");
        print_table(&candidates.iter().map(|e| (*e, AirState::Off)).collect::<Vec<_>>());
    }
    if !rows.is_empty() {
        println!(
            "\n* on the air   < ends within {} min   > starts within {PREVIEW_MIN} min",
            decdrm_schedule::ENDING_SOON_MIN
        );
    }
    Ok(())
}

/// The sources with their URLs and local files.
fn list_sources(sources: &[decdrm_schedule::Source], dir: &std::path::Path, now: UtcTime) {
    let date = now.date();
    for s in sources {
        println!("{:<8} {} ({} format)", s.name, s.label(), s.format);
        println!("         url:  {}", s.url_at(date));
        match s.find_local(dir, date) {
            Some(copy) => {
                let modified = std::fs::metadata(&copy.path)
                    .and_then(|m| m.modified())
                    .map(|t| format!(", updated {}", UtcTime::from_system(t)))
                    .unwrap_or_default();
                let stale = match (copy.current, copy.season) {
                    (false, Some(season)) => format!(" — season {season}, not the current one"),
                    _ => String::new(),
                };
                println!("         file: {}{modified}{stale}", copy.path.display());
            }
            None => println!("         file: {} (not downloaded yet)", dir.join(s.file_at(date)).display()),
        }
    }
    println!("directory: {} (more sources: {})", dir.display(), source::SOURCES_FILE);
}

/// Cut `s` to at most `max` characters, ending in "…" when cut.
fn fit(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// Print entries as aligned columns (widths from the content, long texts cut).
fn print_table(rows: &[(&Entry, AirState)]) {
    if rows.is_empty() {
        return;
    }
    let mark = |s: AirState| match s {
        AirState::OnAir => '*',
        AirState::EndingSoon => '<',
        AirState::StartingSoon => '>',
        AirState::Off => ' ',
    };
    let power = rows.iter().any(|(e, _)| e.power_kw.is_some());
    let validity = rows.iter().any(|(e, _)| e.valid_from.is_some() || e.valid_to.is_some());
    // (header, cap on the width, cell text) per column.
    type Cell = fn(&Entry) -> String;
    let mut cols: Vec<(&str, usize, Cell)> = vec![
        ("kHz", 9, |e| e.khz_label()),
        ("UTC", 9, |e| e.times()),
        ("Days", 18, |e| e.days_label()),
        ("Station", 32, |e| e.station.clone()),
        ("Language", 16, |e| e.language.clone()),
        ("Target", 22, |e| e.target.clone()),
        ("Site", 36, |e| e.site.clone()),
    ];
    if power {
        cols.push(("kW", 6, |e| e.power_kw.map(|p| format!("{p}")).unwrap_or_default()));
    }
    if validity {
        cols.push(("Valid", 26, |e| e.validity_label()));
    }
    if rows.iter().any(|(e, _)| !e.note.is_empty()) {
        cols.push(("Note", 30, |e| e.note.clone()));
    }
    let cells: Vec<Vec<String>> =
        rows.iter().map(|(e, _)| cols.iter().map(|(_, cap, f)| fit(&f(e), *cap)).collect()).collect();
    let widths: Vec<usize> = cols
        .iter()
        .enumerate()
        .map(|(i, (h, _, _))| cells.iter().map(|r| r[i].chars().count()).chain([h.chars().count()]).max().unwrap_or(0))
        .collect();
    let line = |mark: char, texts: &[String]| {
        let mut out = format!("{mark} {:>w$}", texts[0], w = widths[0]);
        for (t, w) in texts.iter().zip(&widths).skip(1) {
            out.push_str("  ");
            out.push_str(t);
            out.extend(std::iter::repeat_n(' ', w.saturating_sub(t.chars().count())));
        }
        out.trim_end().to_string()
    };
    let header: Vec<String> = cols.iter().map(|(h, _, _)| h.to_string()).collect();
    println!("{}", line(' ', &header));
    for ((_, state), texts) in rows.iter().zip(&cells) {
        println!("{}", line(mark(*state), texts));
    }
}
