//! The Schedule tab's state: the schedule of the chosen source, the rows to show, and
//! the frequency being received.
//!
//! Reading a schedule file (EiBi's has some 15 000 lines) and downloading one happen on
//! a background thread, never on the UI thread: [`ScheduleView::request_load`] /
//! [`ScheduleView::request_update`] start a job, and [`ScheduleView::poll`] (called
//! every frame) collects its result through a channel — the same pattern as the
//! receiver's events. One job runs at a time; a request made meanwhile waits for it.
//! Downloads happen only when the user presses *Update schedule*.

use crate::settings::{Settings, SourceKind};
use decdrm_schedule::{
    AirState, Entry, Loaded, MATCH_TOLERANCE_KHZ, PREVIEW_MIN, Source, UtcTime, source,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// Schedule options remembered between runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduleSettings {
    /// Source name: `eibi`, `dream`, or one from `sources.toml`.
    pub source: String,
    /// List every entry instead of those on the air (or starting soon).
    pub show_all: bool,
    /// Also list broadcasts that are not DRM (EiBi lists all of them).
    pub all_broadcasts: bool,
    /// Text filter (station, language, target, country, site).
    pub filter: String,
    /// Frequency typed by the user (kHz, as typed); highlights matching rows.
    pub freq: String,
}

impl Default for ScheduleSettings {
    fn default() -> Self {
        Self {
            source: "eibi".into(),
            show_all: false,
            all_broadcasts: false,
            filter: String::new(),
            freq: String::new(),
        }
    }
}

/// Default schedule directory: `schedule` next to the settings file (so a test run
/// with `--config` keeps everything in its own directory, and the default settings in
/// `%APPDATA%\decdrm` give the library's default `%APPDATA%\decdrm\schedule`), else the
/// library's per-user default, else in the temporary directory.
pub fn default_dir(settings_file: Option<&Path>) -> PathBuf {
    settings_file
        .and_then(Path::parent)
        .map(|d| d.join("schedule"))
        .or_else(source::default_dir)
        .unwrap_or_else(|| std::env::temp_dir().join("decdrm-schedule"))
}

/// Where the received frequency comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Typed into the Schedule tab.
    Typed,
    /// From the recording's file name (e.g. KiwiSDR's `…_6140.00_iq.wav`).
    FileName,
}

/// The frequency being received, for highlighting the schedule.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reception {
    pub khz: f64,
    /// When it was recorded (from the file name); `None`: now.
    pub time: Option<UtcTime>,
    pub origin: Origin,
}

/// The frequency typed into the Schedule tab, else the one in the recording's file name
/// (when the source is a recording).
pub fn reception(settings: &Settings) -> Option<Reception> {
    if let Some(khz) = decdrm_schedule::parse_frequency_input(&settings.schedule.freq) {
        return Some(Reception {
            khz,
            time: None,
            origin: Origin::Typed,
        });
    }
    let file = settings
        .file
        .as_deref()
        .filter(|_| settings.source == SourceKind::File)?;
    let info = decdrm_schedule::recording_info(&file.to_string_lossy())?;
    Some(Reception {
        khz: info.khz,
        time: info.time,
        origin: Origin::FileName,
    })
}

/// A loaded schedule with the lower-case text the filter searches.
pub struct LoadedSchedule {
    pub loaded: Loaded,
    /// Per entry: frequency, station, language, target, country, site and note.
    search: Vec<String>,
}

impl LoadedSchedule {
    fn new(loaded: Loaded) -> Self {
        let search = loaded
            .schedule
            .entries
            .iter()
            .map(|e| {
                [
                    e.khz_label().as_str(),
                    &e.station,
                    &e.language,
                    &e.target,
                    &e.country,
                    &e.site,
                    &e.note,
                ]
                .join("\u{1}")
                .to_lowercase()
            })
            .collect();
        Self { loaded, search }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.loaded.schedule.entries
    }
}

/// What the tab can show about the chosen source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Being read.
    Loading,
    /// Not downloaded yet.
    Missing { url: String, path: PathBuf },
    /// Shown from [`ScheduleView::data`].
    Ready,
    /// The file could not be read.
    Failed(String),
}

/// One row of the table: an entry and its state now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub index: usize,
    pub state: AirState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobKind {
    Load,
    Update,
}

/// What a background job hands back.
struct JobOutcome {
    kind: JobKind,
    /// The source asked for.
    name: String,
    sources: Vec<Source>,
    sources_warning: Option<String>,
    /// The source used (`None`: no such source).
    source: Option<Source>,
    /// The schedule (`Ok(None)`: no local copy), or why there is none.
    result: Result<Option<LoadedSchedule>, String>,
    /// For an update: URL and size of the download.
    downloaded: Option<(String, u64)>,
}

/// The tab's state (see the module docs).
pub struct ScheduleView {
    dir: PathBuf,
    /// The sources (the defaults until the first job has read `sources.toml`).
    pub sources: Vec<Source>,
    pub sources_warning: Option<String>,
    /// The schedule shown (kept while a newer one loads or downloads).
    pub data: Option<LoadedSchedule>,
    pub status: Status,
    /// The last download's error, shown until the next one.
    pub update_error: Option<String>,
    /// The source the user wants (results for another one are not shown).
    wanted: String,
    job: Option<(JobKind, mpsc::Receiver<JobOutcome>)>,
    /// A request made while a job ran.
    pending: Option<(JobKind, String)>,
    log: Vec<String>,
    /// A reception to name in the log once the schedule is read (see
    /// [`ScheduleView::note_reception`]), with the "all broadcasts" option.
    pending_reception: Option<(Reception, bool)>,
    /// Counts loaded schedules, so the rows know when to recompute.
    generation: u64,
    rows: Vec<Row>,
    /// Entries on the air among those of the current DRM/all and text filters.
    on_air: usize,
    rows_key: Option<RowsKey>,
}

/// What the cached rows depend on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RowsKey {
    generation: u64,
    minute: i64,
    show_all: bool,
    all_broadcasts: bool,
    filter: String,
}

impl ScheduleView {
    /// The view for schedule directory `dir`, starting to read `source`'s schedule.
    pub fn new(dir: PathBuf, source: &str) -> Self {
        let mut view = Self {
            dir,
            sources: source::default_sources(),
            sources_warning: None,
            data: None,
            status: Status::Loading,
            update_error: None,
            wanted: source.to_string(),
            job: None,
            pending: None,
            log: Vec::new(),
            pending_reception: None,
            generation: 0,
            rows: Vec::new(),
            on_air: 0,
            rows_key: None,
        };
        view.request_load(source);
        view
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A job is running (the GUI then repaints to poll it).
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// A download is running or waiting.
    pub fn updating(&self) -> bool {
        matches!(self.job, Some((JobKind::Update, _)))
            || matches!(self.pending, Some((JobKind::Update, _)))
    }

    /// The chosen source (the first one when the name is unknown).
    pub fn source(&self) -> Option<&Source> {
        source::find_source(&self.sources, &self.wanted).or(self.sources.first())
    }

    /// Show `name`'s schedule: read its local copy in the background.
    pub fn request_load(&mut self, name: &str) {
        if self.wanted != name {
            self.wanted = name.to_string();
            self.data = None;
            self.generation += 1;
            self.update_error = None;
        }
        self.status = Status::Loading;
        self.start(JobKind::Load, name);
    }

    /// Download `name`'s schedule in the background (only on the user's request).
    pub fn request_update(&mut self, name: &str) {
        if let Some(src) = source::find_source(&self.sources, name) {
            let date = UtcTime::now().date();
            self.log.push(format!(
                "schedule: downloading {} into {}",
                src.url_at(date),
                self.dir.display()
            ));
        }
        self.wanted = name.to_string();
        self.update_error = None;
        self.start(JobKind::Update, name);
    }

    fn start(&mut self, kind: JobKind, name: &str) {
        if self.job.is_some() {
            // An update request is not replaced by a later load request.
            if !matches!(
                (&self.pending, kind),
                (Some((JobKind::Update, _)), JobKind::Load)
            ) {
                self.pending = Some((kind, name.to_string()));
            }
            return;
        }
        let (tx, rx) = mpsc::channel();
        let (dir, name_owned) = (self.dir.clone(), name.to_string());
        // Rust note: `move` hands `dir`, `name_owned` and the sending end of the channel to
        // the new thread; the receiving end stays here. `send` fails only if the GUI has
        // dropped the receiver (closed), which is fine to ignore.
        let spawned = std::thread::Builder::new()
            .name("schedule".into())
            .spawn(move || {
                let _ = tx.send(run_job(kind, &dir, &name_owned, UtcTime::now()));
            });
        match spawned {
            Ok(_) => self.job = Some((kind, rx)),
            Err(e) => {
                self.status = Status::Failed(format!("cannot start a thread: {e}"));
            }
        }
    }

    /// Collect a finished job. Returns `true` if anything changed (repaint).
    pub fn poll(&mut self) -> bool {
        let Some((_, rx)) = &self.job else {
            return false;
        };
        let outcome = match rx.try_recv() {
            Ok(o) => o,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.job = None;
                self.status = Status::Failed("the schedule thread stopped".into());
                return true;
            }
        };
        self.job = None;
        self.apply(outcome);
        if let Some((kind, name)) = self.pending.take() {
            self.start(kind, &name);
        }
        true
    }

    fn apply(&mut self, o: JobOutcome) {
        if o.sources_warning.is_some() && o.sources_warning != self.sources_warning {
            self.log.push(format!(
                "schedule: {}",
                o.sources_warning.as_deref().unwrap_or("")
            ));
        }
        self.sources = o.sources;
        self.sources_warning = o.sources_warning;
        let Some(src) = o.source else {
            self.status = Status::Failed(format!("no schedule source \"{}\"", o.name));
            return;
        };
        let current = o.name.eq_ignore_ascii_case(&self.wanted);
        match (o.kind, o.result) {
            (JobKind::Update, Err(e)) => {
                self.log.push(format!("schedule: update failed: {e}"));
                if current {
                    self.update_error = Some(e);
                    if self.data.is_some() {
                        self.status = Status::Ready;
                    } else {
                        // Back to what the directory holds.
                        self.request_load(&o.name);
                    }
                }
            }
            (kind, Ok(Some(data))) => {
                let s = &data.loaded.schedule;
                let skipped = match s.skipped.len() {
                    0 => String::new(),
                    n => format!(", {n} unreadable lines skipped"),
                };
                let file = data.loaded.copy.path.display();
                let line = match (kind, &o.downloaded) {
                    (JobKind::Update, Some((_, bytes))) => format!(
                        "schedule: updated {file} ({:.0} kB, {} entries, {} DRM{skipped})",
                        *bytes as f64 / 1e3,
                        s.entries.len(),
                        s.drm_count()
                    ),
                    _ => format!(
                        "schedule: {} — {file}, {} entries, {} DRM{skipped}",
                        src.label(),
                        s.entries.len(),
                        s.drm_count()
                    ),
                };
                self.log.push(line);
                if current {
                    self.data = Some(data);
                    self.generation += 1;
                    self.status = Status::Ready;
                }
            }
            (_, Ok(None)) => {
                if current {
                    let date = UtcTime::now().date();
                    self.data = None;
                    self.generation += 1;
                    self.status = Status::Missing {
                        url: src.url_at(date),
                        path: self.dir.join(src.file_at(date)),
                    };
                }
            }
            (JobKind::Load, Err(e)) => {
                self.log.push(format!("schedule: {e}"));
                if current {
                    self.status = Status::Failed(e);
                }
            }
        }
        // A reception noted while the schedule was being read: name it now (or drop it
        // when there is no schedule after all).
        if !self.busy()
            && self.pending.is_none()
            && let Some((r, all)) = self.pending_reception.take()
            && let Some(line) = self.describe_reception(&r, all)
        {
            self.log.push(line);
        }
    }

    /// Name in the log what the schedule has on a received frequency (when a recording
    /// or sound card starts) — now, or once the schedule being read is there.
    pub fn note_reception(&mut self, r: Reception, all_broadcasts: bool) {
        match self.describe_reception(&r, all_broadcasts) {
            Some(line) => self.log.push(line),
            None if self.busy() => self.pending_reception = Some((r, all_broadcasts)),
            None => {}
        }
    }

    /// Log lines for the application's log.
    pub fn take_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.log)
    }

    /// Add a line for the application's log.
    pub fn note(&mut self, line: String) {
        self.log.push(line);
    }

    /// Bring the rows up to date for `now` and the options `s`. They are recomputed only
    /// when the minute, the options or the schedule changed.
    ///
    /// Rust note: updating (`&mut self`) and reading ([`ScheduleView::rows`], `&self`)
    /// are separate so the drawing code can hold the rows and [`ScheduleView::data`] at
    /// the same time; a `&mut self` method returning the rows would keep the whole view
    /// borrowed mutably while they are in use.
    pub fn update_rows(&mut self, s: &ScheduleSettings, now: UtcTime) {
        let key = RowsKey {
            generation: self.generation,
            minute: now.unix().div_euclid(60),
            show_all: s.show_all,
            all_broadcasts: s.all_broadcasts,
            filter: s.filter.trim().to_lowercase(),
        };
        if self.rows_key.as_ref() != Some(&key) {
            (self.rows, self.on_air) = match &self.data {
                Some(data) => compute_rows(data, &key, now),
                None => (Vec::new(), 0),
            };
            self.rows_key = Some(key);
        }
    }

    /// The rows as of the last [`ScheduleView::update_rows`].
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// How many entries of the DRM/all and text filters are on the air (as of the last
    /// [`ScheduleView::update_rows`]).
    pub fn on_air(&self) -> usize {
        self.on_air
    }

    /// The entries matching a received frequency (nearest first; DRM ones only unless
    /// `all_broadcasts`), with those on the air at the reception time first.
    pub fn reception_matches(&self, r: &Reception, all_broadcasts: bool) -> (Vec<&Entry>, usize) {
        let Some(data) = &self.data else {
            return (Vec::new(), 0);
        };
        let t = r.time.unwrap_or_else(UtcTime::now);
        let matches: Vec<&Entry> =
            decdrm_schedule::match_frequency(data.entries(), r.khz, MATCH_TOLERANCE_KHZ)
                .into_iter()
                .filter(|e| all_broadcasts || e.drm)
                .collect();
        let total = matches.len();
        (
            matches.into_iter().filter(|e| e.is_on_air(t)).collect(),
            total,
        )
    }

    /// A log line naming what is scheduled on a received frequency (for the log when a
    /// recording starts); `None` without a loaded schedule.
    pub fn describe_reception(&self, r: &Reception, all_broadcasts: bool) -> Option<String> {
        self.data.as_ref()?;
        let (on, total) = self.reception_matches(r, all_broadcasts);
        let khz = decdrm_schedule::format_khz(r.khz);
        let at = r
            .time
            .map(|t| format!(" at {t}"))
            .unwrap_or_else(|| " now".into());
        Some(match (on.first(), total) {
            (Some(_), _) => format!(
                "schedule: {khz} kHz{at}: {}",
                on.iter()
                    .map(|e| describe(e))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            (None, 0) => format!(
                "schedule: nothing scheduled within ±{} kHz of {khz} kHz",
                decdrm_schedule::format_khz(MATCH_TOLERANCE_KHZ)
            ),
            (None, n) => format!("schedule: {khz} kHz: {n} entries, none on the air{at}"),
        })
    }
}

/// `KCBS Pyongyang (Korean, East Asia, 0000-2400 daily)`.
pub fn describe(e: &Entry) -> String {
    let details: Vec<String> = [
        e.language.clone(),
        e.target.clone(),
        format!("{} {}", e.times(), e.days_label()),
    ]
    .into_iter()
    .filter(|s| !s.trim().is_empty())
    .collect();
    format!("{} ({})", e.station, details.join(", "))
}

/// Filter and evaluate the entries (see [`ScheduleView::rows`]).
fn compute_rows(data: &LoadedSchedule, key: &RowsKey, now: UtcTime) -> (Vec<Row>, usize) {
    let mut on_air = 0;
    let rows = data
        .entries()
        .iter()
        .enumerate()
        .filter(|(_, e)| key.all_broadcasts || e.drm)
        .filter(|(i, _)| key.filter.is_empty() || data.search[*i].contains(&key.filter))
        .filter_map(|(index, e)| {
            let state = e.state_at(now, PREVIEW_MIN);
            on_air += usize::from(state.is_on());
            (key.show_all || state != AirState::Off).then_some(Row { index, state })
        })
        .collect();
    (rows, on_air)
}

/// The job body, on the background thread: read the sources, then load or update.
fn run_job(kind: JobKind, dir: &Path, name: &str, now: UtcTime) -> JobOutcome {
    let (sources, sources_warning) = source::load_sources(dir);
    let src = source::find_source(&sources, name).cloned();
    let date = now.date();
    let (result, downloaded) = match (&src, kind) {
        (None, _) => (Ok(None), None),
        (Some(src), JobKind::Load) => (
            source::load(src, dir, date)
                .map(|o| o.map(LoadedSchedule::new))
                .map_err(|e| e.to_string()),
            None,
        ),
        (Some(src), JobKind::Update) => match source::update(src, dir, date) {
            Ok(u) => (
                Ok(Some(LoadedSchedule::new(u.loaded))),
                Some((u.url, u.bytes)),
            ),
            Err(e) => (Err(e.to_string()), None),
        },
    };
    JobOutcome {
        kind,
        name: name.to_string(),
        sources,
        sources_warning,
        source: src,
        result,
        downloaded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const FIXTURE: &str = "\
kHz:75;Time(UTC):93;Days:59;ITU:49;Station:201;Lng:49;Target:62;Remarks:135;P:35;Start:60;Stop:60;
3965;0000-2400;;F;Radio France Int. DRM;F;Eu;i;1;;
5995;0600-0700;Mo-Fr;D;Deutsche Welle;E;WAf;;1;;
6140;0000-2400;;KRE;KCBS Pyongyang DRM;K;EAs;k;1;;
6145;1000-1100;;D;Other DRM;D;Eu;;1;;
";

    /// A schedule directory with an EiBi file for every season (tests run on any date).
    fn scratch(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("decdrm-gui-schedule-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let season = decdrm_schedule::Season::at(UtcTime::now().date()).code();
        std::fs::write(dir.join(format!("sked-{season}.csv")), FIXTURE).unwrap();
        dir
    }

    fn wait(view: &mut ScheduleView) {
        let t0 = Instant::now();
        while view.busy() && t0.elapsed() < Duration::from_secs(10) {
            view.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!view.busy(), "job did not finish");
    }

    #[test]
    fn loads_in_the_background_and_filters_rows() {
        let dir = scratch("load");
        let mut view = ScheduleView::new(dir.clone(), "eibi");
        assert_eq!(view.status, Status::Loading);
        wait(&mut view);
        assert_eq!(view.status, Status::Ready);
        assert_eq!(view.data.as_ref().unwrap().entries().len(), 4);
        assert!(
            view.take_log()
                .iter()
                .any(|l| l.contains("4 entries, 3 DRM"))
        );

        let mut s = ScheduleSettings::default();
        let t = UtcTime::parse("2026-10-01T10:30Z").unwrap();
        let stations = |view: &ScheduleView| -> Vec<String> {
            let entries = view.data.as_ref().unwrap().entries();
            view.rows()
                .iter()
                .map(|r| entries[r.index].station.clone())
                .collect()
        };
        view.update_rows(&s, t);
        assert_eq!(view.on_air(), 3);
        assert_eq!(
            stations(&view),
            ["Radio France Int. DRM", "KCBS Pyongyang DRM", "Other DRM"]
        );
        s.filter = "korean".into();
        view.update_rows(&s, t);
        assert_eq!(stations(&view), ["KCBS Pyongyang DRM"]);
        s.filter.clear();
        s.all_broadcasts = true;
        s.show_all = true;
        view.update_rows(&s, t);
        assert_eq!(view.rows().len(), 4);

        // The received frequency: typed, else from the recording's name.
        let r = Reception {
            khz: 6142.0,
            time: Some(t),
            origin: Origin::Typed,
        };
        let (on, total) = view.reception_matches(&r, false);
        assert_eq!(total, 2);
        assert_eq!(on.len(), 2);
        assert_eq!(on[0].station, "KCBS Pyongyang DRM", "nearest first");
        let line = view.describe_reception(&r, false).unwrap();
        assert!(
            line.contains("6142 kHz at 2026-10-01 10:30 UTC: KCBS"),
            "{line}"
        );

        // Another source without a local copy.
        view.request_load("dream");
        wait(&mut view);
        assert!(matches!(view.status, Status::Missing { .. }));
        view.update_rows(&s, t);
        assert!(view.data.is_none() && view.rows().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reception_noted_while_loading_is_logged_when_read() {
        let dir = scratch("note");
        let mut view = ScheduleView::new(dir.clone(), "eibi");
        let r = Reception {
            khz: 6140.0,
            time: UtcTime::parse("2026-10-01T10:30Z"),
            origin: Origin::FileName,
        };
        // The job's result is only taken by `poll`, so the schedule is not there yet.
        view.note_reception(r, false);
        assert!(view.take_log().is_empty());
        wait(&mut view);
        let log = view.take_log();
        assert!(
            log.iter()
                .any(|l| l.contains("6140 kHz at 2026-10-01 10:30 UTC: KCBS Pyongyang DRM")),
            "{log:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reception_from_settings() {
        let mut s = Settings {
            file: Some("x/SND.kiwisdr.areg.org.au_2026-09-30T12_46_51Z_6140.00_iq.wav".into()),
            ..Settings::default()
        };
        let r = reception(&s).unwrap();
        assert_eq!((r.khz, r.origin), (6140.0, Origin::FileName));
        assert_eq!(r.time.unwrap().to_string(), "2026-09-30 12:46 UTC");
        s.schedule.freq = "7.325 MHz".into();
        assert_eq!(reception(&s).unwrap().khz, 7325.0, "typed wins");
        s.schedule.freq = "not a number".into();
        assert_eq!(reception(&s).unwrap().origin, Origin::FileName);
        s.source = SourceKind::Device;
        assert_eq!(reception(&s), None, "a sound card has no file name");
        s.file = Some("x/DW_ModeB_10kHz.flac".into());
        s.source = SourceKind::File;
        assert_eq!(reception(&s), None);
    }

    #[test]
    fn directory_next_to_the_settings() {
        assert_eq!(
            default_dir(Some(Path::new("cfg/gui.toml"))),
            Path::new("cfg").join("schedule")
        );
        assert!(
            default_dir(None).ends_with("schedule")
                || default_dir(None).ends_with("decdrm-schedule")
        );
    }
}
