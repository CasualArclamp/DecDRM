//! Schedule sources: where a schedule is downloaded from, in which format, and where
//! its local copy lives.
//!
//! **Directory**: the schedule files live in a per-user directory next to the GUI's
//! settings ([`default_dir`]: `%APPDATA%\decdrm\schedule` on Windows,
//! `$XDG_CONFIG_HOME/decdrm/schedule` or `~/.config/decdrm/schedule` elsewhere); the GUI
//! run with `--config FILE` uses `schedule` next to that file, `decdrm schedule --dir`
//! any directory. A file's modification time is its last update.
//!
//! **Sources** ([`default_sources`]): `eibi` — EiBi's file for the current season
//! (`http://www.eibispace.de/dx/sked-{season}.csv`, stored as `sked-a26.csv` …) — and
//! `dream` — the DRMDX schedule Dream downloads ([`dream::SCHEDULE_URL`], stored as
//! `DRMSchedule.ini`, so Dream's own file can be copied in). `sources.toml` in the
//! directory adds sources or replaces defaults of the same name:
//!
//! ```toml
//! [[source]]
//! name = "mylist"                      # for `decdrm schedule --source mylist`
//! title = "My DRM list"                # shown in the GUI (optional)
//! format = "eibi"                      # "eibi" (EiBi CSV) or "dream" (DRMSchedule.ini)
//! url = "https://example.org/drm.csv"  # {season} = the current season, e.g. a26
//! file = "drm.csv"                     # local file name (optional; {season} allowed)
//! ```
//!
//! Downloads ([`update`]) happen only on request, into a temporary file that replaces
//! the old copy only when it parses into at least one entry (an error page must not
//! wipe a working schedule). Files can also be saved into the directory by hand.

use crate::download::{DownloadError, fetch};
use crate::season::Season;
use crate::time::{Date, UtcTime};
use crate::{Format, Schedule, dream, eibi, parse};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Optional source list in the schedule directory.
pub const SOURCES_FILE: &str = "sources.toml";
/// Placeholder for the season code in URLs and file names.
pub const SEASON_PLACEHOLDER: &str = "{season}";

/// One schedule source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    /// Short name, e.g. for `decdrm schedule --source NAME`.
    pub name: String,
    /// Longer name shown in the GUI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub format: Format,
    /// Download URL; `{season}` is replaced by the current season (`a26`).
    pub url: String,
    /// Local file name (`{season}` allowed); default: the URL's file name if it has a
    /// `.csv`, `.ini` or `.txt` extension, else the source name with the format's
    /// extension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// The built-in sources: EiBi's current season file, then Dream's DRMDX schedule.
pub fn default_sources() -> Vec<Source> {
    vec![
        Source {
            name: "eibi".into(),
            title: Some("EiBi (eibispace.de)".into()),
            format: Format::Eibi,
            url: eibi::URL_TEMPLATE.into(),
            file: Some(eibi::FILE_TEMPLATE.into()),
        },
        Source {
            name: "dream".into(),
            title: Some("Dream / DRMDX (DRMSchedule.ini)".into()),
            format: Format::Dream,
            url: dream::SCHEDULE_URL.into(),
            file: Some(dream::FILE_NAME.into()),
        },
    ]
}

impl Source {
    /// The title, else the name.
    pub fn label(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.name)
    }

    /// The local file name with its placeholder (see [`Source::file`]).
    fn file_template(&self) -> String {
        if let Some(f) = self.file.as_deref().filter(|f| !f.trim().is_empty()) {
            return f.trim().to_string();
        }
        let path = self.url.split(['?', '#']).next().unwrap_or("");
        let last = path.rsplit('/').next().unwrap_or("");
        let has_ext = [".csv", ".ini", ".txt"]
            .iter()
            .any(|e| last.to_ascii_lowercase().ends_with(e));
        if has_ext && last.len() > 4 {
            sanitize(last)
        } else {
            let ext = match self.format {
                Format::Eibi => "csv",
                Format::Dream => "ini",
            };
            format!("{}.{ext}", sanitize(&self.name))
        }
    }

    /// Whether URL or file name depend on the season.
    pub fn is_seasonal(&self) -> bool {
        self.url.contains(SEASON_PLACEHOLDER) || self.file_template().contains(SEASON_PLACEHOLDER)
    }

    /// The download URL for `date`.
    pub fn url_at(&self, date: Date) -> String {
        self.url
            .replace(SEASON_PLACEHOLDER, &Season::at(date).code())
    }

    /// The local file name for `date`.
    pub fn file_at(&self, date: Date) -> String {
        self.file_template()
            .replace(SEASON_PLACEHOLDER, &Season::at(date).code())
    }

    /// The local copy to use on `date`: the current file; for a seasonal source without
    /// one, the file of the latest other season present (older or, downloaded early,
    /// newer), marked as not current.
    pub fn find_local(&self, dir: &Path, date: Date) -> Option<LocalCopy> {
        let season = self.is_seasonal().then(|| Season::at(date));
        let current = dir.join(self.file_at(date));
        if current.is_file() {
            return Some(LocalCopy {
                path: current,
                season,
                current: true,
            });
        }
        let template = self.file_template();
        let (prefix, suffix) = template.split_once(SEASON_PLACEHOLDER)?;
        // Rust note: `filter_map` with `?` inside the closure skips every entry for which
        // any step gives `None` (unreadable entry, non-UTF-8 name, no match).
        let (season, name) = std::fs::read_dir(dir)
            .ok()?
            .filter_map(|entry| {
                let name = entry.ok()?.file_name().into_string().ok()?;
                let code = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
                Some((Season::parse(code)?, name))
            })
            .max_by_key(|(season, _)| *season)?;
        Some(LocalCopy {
            path: dir.join(name),
            season: Some(season),
            current: false,
        })
    }
}

/// Keep letters, digits, `.`, `-`, `_` and `{season}`'s braces; anything else becomes `_`.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-{}".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A local schedule file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalCopy {
    pub path: PathBuf,
    /// The season the file is for (seasonal sources).
    pub season: Option<Season>,
    /// The file for the current season (always, for sources without seasons).
    pub current: bool,
}

/// The source called `name` (case-insensitive).
pub fn find_source<'a>(sources: &'a [Source], name: &str) -> Option<&'a Source> {
    sources
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(name.trim()))
}

/// `sources.toml`: a list of `[[source]]` tables.
#[derive(Debug, Deserialize, Serialize)]
struct SourcesFile {
    #[serde(default, rename = "source")]
    sources: Vec<Source>,
}

/// The sources: the defaults, with those of `dir/sources.toml` (if present) replacing
/// defaults of the same name or added after them. A file that cannot be read leaves the
/// defaults, with a warning to show.
pub fn load_sources(dir: &Path) -> (Vec<Source>, Option<String>) {
    let path = dir.join(SOURCES_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (default_sources(), None),
        Err(e) => {
            return (
                default_sources(),
                Some(format!("cannot read {}: {e}", path.display())),
            );
        }
    };
    match merge_sources(&text) {
        Ok(sources) => (sources, None),
        Err(e) => (
            default_sources(),
            Some(format!("ignoring {}: {e}", path.display())),
        ),
    }
}

/// The defaults merged with the sources of a `sources.toml` text.
fn merge_sources(text: &str) -> Result<Vec<Source>, String> {
    let file: SourcesFile = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut sources = default_sources();
    for s in file.sources {
        if s.name.trim().is_empty() || s.url.trim().is_empty() {
            return Err("every [[source]] needs a name and a url".into());
        }
        match sources
            .iter_mut()
            .find(|d| d.name.eq_ignore_ascii_case(&s.name))
        {
            Some(existing) => *existing = s,
            None => sources.push(s),
        }
    }
    Ok(sources)
}

/// The default schedule directory (see the module docs); `None` when the environment
/// names no home or configuration directory.
pub fn default_dir() -> Option<PathBuf> {
    dir_for(cfg!(windows), |key| std::env::var_os(key))
}

/// [`default_dir`] for a platform, with the environment passed in as a lookup function
/// (so tests do not depend on the real environment), as the GUI's settings path.
fn dir_for(windows: bool, env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let non_empty = |key: &str| env(key).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if windows {
        non_empty("APPDATA")?
    } else {
        non_empty("XDG_CONFIG_HOME").or_else(|| non_empty("HOME").map(|h| h.join(".config")))?
    };
    Some(base.join("decdrm").join("schedule"))
}

/// Why a local schedule could not be read.
#[derive(Debug, thiserror::Error)]
#[error("{path}: {source}")]
pub struct LoadError {
    pub path: PathBuf,
    #[source]
    pub source: std::io::Error,
}

/// A schedule read from its local copy, sorted by frequency.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub schedule: Schedule,
    pub copy: LocalCopy,
    /// When the file was last written, i.e. downloaded.
    pub modified: Option<UtcTime>,
}

/// Read the local copy of `source` for `date`; `Ok(None)` when there is none yet.
pub fn load(source: &Source, dir: &Path, date: Date) -> Result<Option<Loaded>, LoadError> {
    let Some(copy) = source.find_local(dir, date) else {
        return Ok(None);
    };
    let io = |source| LoadError {
        path: copy.path.clone(),
        source,
    };
    let bytes = std::fs::read(&copy.path).map_err(io)?;
    let modified = std::fs::metadata(&copy.path)
        .and_then(|m| m.modified())
        .ok()
        .map(UtcTime::from_system);
    let mut schedule = parse(source.format, &bytes);
    schedule.sort_by_frequency();
    Ok(Some(Loaded {
        schedule,
        copy,
        modified,
    }))
}

/// A finished update.
#[derive(Debug, Clone)]
pub struct Updated {
    /// What was downloaded.
    pub url: String,
    /// Size of the file, bytes.
    pub bytes: u64,
    /// The new schedule.
    pub loaded: Loaded,
}

/// Download the current file of `source` into `dir` (only call this when the user asked
/// for it). The download goes to `<file>.part`, is parsed, and replaces the local copy
/// only if it holds at least one entry.
pub fn update(source: &Source, dir: &Path, date: Date) -> Result<Updated, DownloadError> {
    std::fs::create_dir_all(dir).map_err(io_error(dir))?;
    let url = source.url_at(date);
    let path = dir.join(source.file_at(date));
    let part = dir.join(format!("{}.part", source.file_at(date)));
    let _ = std::fs::remove_file(&part);
    // The temporary file is removed on every way out but success.
    let bytes = fetch_checked(&url, &part, &path, source.format).inspect_err(|_| {
        let _ = std::fs::remove_file(&part);
    })?;
    let loaded = load(source, dir, date)
        .map_err(|e| DownloadError::Io {
            path: e.path,
            source: e.source,
        })?
        .ok_or_else(|| DownloadError::Io {
            path: path.clone(),
            source: std::io::ErrorKind::NotFound.into(),
        })?;
    Ok(Updated { url, bytes, loaded })
}

/// Download `url` to `part`, check that it parses as `format` with at least one entry,
/// and rename it to `path`. Returns the size.
fn fetch_checked(
    url: &str,
    part: &Path,
    path: &Path,
    format: Format,
) -> Result<u64, DownloadError> {
    fetch(url, part)?;
    let bytes = std::fs::read(part).map_err(io_error(part))?;
    if parse(format, &bytes).entries.is_empty() {
        return Err(DownloadError::NotASchedule {
            url: url.to_string(),
            format,
        });
    }
    std::fs::rename(part, path).map_err(io_error(path))?;
    Ok(bytes.len() as u64)
}

/// A `map_err` function turning an I/O error into a [`DownloadError`] about `path`.
fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> DownloadError {
    let path = path.to_path_buf();
    // Rust note: `move` makes the returned closure own `path`.
    move |source| DownloadError::Io { path, source }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> Date {
        Date::new(y, m, day).unwrap()
    }

    /// A fresh scratch directory per test (tests run in parallel threads).
    fn scratch(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("decdrm-schedule-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn default_sources_resolve_by_season() {
        let s = default_sources();
        let eibi = find_source(&s, "EiBi").unwrap();
        assert!(eibi.is_seasonal());
        assert_eq!(
            eibi.url_at(d(2026, 10, 1)),
            "http://www.eibispace.de/dx/sked-a26.csv"
        );
        assert_eq!(eibi.file_at(d(2026, 11, 1)), "sked-b26.csv");
        let dream = find_source(&s, "dream").unwrap();
        assert!(!dream.is_seasonal());
        assert_eq!(dream.file_at(d(2026, 10, 1)), "DRMSchedule.ini");
        assert_eq!(dream.url_at(d(2026, 10, 1)), dream::SCHEDULE_URL);
        assert!(find_source(&s, "nope").is_none());
    }

    #[test]
    fn file_names_from_urls() {
        let src = |url: &str, format| Source {
            name: "my list".into(),
            title: None,
            format,
            url: url.into(),
            file: None,
        };
        assert_eq!(
            src("https://x.org/a/drm.csv?x=1", Format::Eibi).file_at(d(2026, 1, 1)),
            "drm.csv"
        );
        assert_eq!(
            src("https://x.org/cgi?get=list", Format::Dream).file_at(d(2026, 1, 1)),
            "my_list.ini"
        );
        assert_eq!(
            src("https://x.org/sked-{season}.csv", Format::Eibi).file_at(d(2026, 6, 1)),
            "sked-a26.csv"
        );
        assert_eq!(
            src("https://x.org/", Format::Eibi).file_at(d(2026, 6, 1)),
            "my_list.csv"
        );
    }

    #[test]
    fn sources_file_merges_with_the_defaults() {
        let text = r#"
[[source]]
name = "eibi"
format = "eibi"
url = "https://mirror.example/sked-{season}.csv"

[[source]]
name = "mine"
title = "My list"
format = "dream"
url = "https://example.org/DRM.ini"
"#;
        let s = merge_sources(text).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].url, "https://mirror.example/sked-{season}.csv");
        assert_eq!(
            s[0].file_at(d(2026, 10, 1)),
            "sked-a26.csv",
            "file name from the URL"
        );
        assert_eq!(s[1].name, "dream");
        assert_eq!((s[2].label(), s[2].format), ("My list", Format::Dream));
        assert!(
            merge_sources("[[source]]\nname = \"x\"\nformat = \"eibi\"\nurl = \"\"\n").is_err()
        );
        assert!(
            merge_sources("[[source]]\nname = \"x\"\nformat = \"html\"\nurl = \"u\"\n").is_err()
        );
        assert_eq!(merge_sources("").unwrap(), default_sources());

        let dir = scratch("sources");
        assert_eq!(load_sources(&dir), (default_sources(), None));
        std::fs::write(dir.join(SOURCES_FILE), "this is = not toml [").unwrap();
        let (s, warn) = load_sources(&dir);
        assert_eq!(s, default_sources());
        assert!(warn.is_some_and(|w| w.contains(SOURCES_FILE)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_copies_and_stale_seasons() {
        let dir = scratch("local");
        let eibi = default_sources().remove(0);
        let today = d(2026, 11, 15); // season B26
        assert_eq!(eibi.find_local(&dir, today), None);
        assert!(load(&eibi, &dir, today).unwrap().is_none());

        // Only last season's file: used, but not current.
        std::fs::write(
            dir.join("sked-a26.csv"),
            "6140;0000-2400;;KRE;KCBS;K;EAs;DRM;1;;\n",
        )
        .unwrap();
        std::fs::write(dir.join("sked-a25.csv"), "x").unwrap();
        std::fs::write(dir.join("unrelated.csv"), "x").unwrap();
        let copy = eibi.find_local(&dir, today).unwrap();
        assert_eq!(copy.path, dir.join("sked-a26.csv"));
        assert_eq!(
            (copy.season, copy.current),
            (Some(Season::new(2026, false)), false)
        );
        let loaded = load(&eibi, &dir, today).unwrap().unwrap();
        assert_eq!(loaded.schedule.entries.len(), 1);
        assert!(loaded.modified.is_some());

        // The current season's file wins.
        std::fs::write(dir.join("sked-b26.csv"), "").unwrap();
        let copy = eibi.find_local(&dir, today).unwrap();
        assert_eq!(
            (copy.path.file_name().unwrap().to_str(), copy.current),
            (Some("sked-b26.csv"), true)
        );

        // A source without seasons has exactly one file.
        let dream = default_sources().remove(1);
        assert_eq!(dream.find_local(&dir, today), None);
        std::fs::write(dir.join("DRMSchedule.ini"), "").unwrap();
        assert!(
            dream
                .find_local(&dir, today)
                .is_some_and(|c| c.current && c.season.is_none())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn directory_like_the_gui_settings() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == k)
                    .map(|(_, v)| OsString::from(*v))
            }
        };
        assert_eq!(
            dir_for(true, env(&[("APPDATA", r"C:\Users\me\AppData\Roaming")])),
            Some(
                PathBuf::from(r"C:\Users\me\AppData\Roaming")
                    .join("decdrm")
                    .join("schedule")
            )
        );
        assert_eq!(dir_for(true, env(&[])), None);
        assert_eq!(
            dir_for(false, env(&[("XDG_CONFIG_HOME", "/cfg")])),
            Some(PathBuf::from("/cfg/decdrm/schedule"))
        );
        assert_eq!(
            dir_for(false, env(&[("XDG_CONFIG_HOME", ""), ("HOME", "/home/me")])),
            Some(PathBuf::from("/home/me/.config/decdrm/schedule"))
        );
    }
}
