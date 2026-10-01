//! The public KiwiSDR list behind the "Find a KiwiSDR" window: read from DecDRM's copy
//! on a background thread, and downloaded (curl or wget, like the schedules) only when
//! the user asks.

use decdrm_engine::decdrm_kiwi::{DIRECTORY_URL, KiwiEntry, parse_directory};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::SystemTime;

/// Name of DecDRM's copy of the list.
pub const FILE_NAME: &str = "kiwisdr_com.js";

/// `kiwi` next to the settings file (a `--config` run keeps its own), else in the
/// temporary directory.
pub fn default_dir(settings_file: Option<&Path>) -> PathBuf {
    settings_file
        .and_then(Path::parent)
        .map(|d| d.join("kiwi"))
        .unwrap_or_else(|| std::env::temp_dir().join("decdrm-kiwi"))
}

/// A finished background job.
struct Loaded {
    entries: Vec<KiwiEntry>,
    /// When the copy was written; `None`: there is no copy yet.
    modified: Option<SystemTime>,
}

/// The list and the window's filter.
pub struct KiwiList {
    /// The window is shown.
    pub open: bool,
    dir: PathBuf,
    pub entries: Vec<KiwiEntry>,
    /// When the copy was downloaded (`None`: no copy yet).
    pub modified: Option<SystemTime>,
    pub error: Option<String>,
    job: Option<Receiver<Result<Loaded, String>>>,
    loaded_once: bool,
    /// Text that location, name or antenna must contain.
    pub filter: String,
    /// Only Kiwis that are online, allow apps and have a free channel.
    pub usable_only: bool,
}

impl KiwiList {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            open: false,
            dir,
            entries: Vec::new(),
            modified: None,
            error: None,
            job: None,
            loaded_once: false,
            filter: String::new(),
            usable_only: true,
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(FILE_NAME)
    }

    /// Show the window; the first time, read the local copy (never downloads).
    pub fn open_window(&mut self) {
        self.open = true;
        if !self.loaded_once && self.job.is_none() {
            self.loaded_once = true;
            self.start(false);
        }
    }

    /// Download a fresh list (the old copy stays if the download fails).
    pub fn update_list(&mut self) {
        if self.job.is_none() {
            self.loaded_once = true;
            self.start(true);
        }
    }

    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    fn start(&mut self, download: bool) {
        let (tx, rx) = mpsc::channel();
        let dir = self.dir.clone();
        std::thread::Builder::new()
            .name("kiwi-list".into())
            .spawn(move || {
                let _ = tx.send(job(&dir, download));
            })
            .expect("spawn thread");
        self.job = Some(rx);
        self.error = None;
    }

    /// Collect a finished job; `true` if something changed.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.job else { return false };
        match rx.try_recv() {
            Ok(result) => {
                self.job = None;
                match result {
                    Ok(l) => {
                        self.entries = l.entries;
                        self.modified = l.modified;
                    }
                    Err(e) => self.error = Some(e),
                }
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.job = None;
                self.error = Some("reading the KiwiSDR list failed".into());
                true
            }
        }
    }

    /// The Kiwis to show for `freq_khz` (0: any), by location (owners write it freely,
    /// so no attempt is made to find the country in it); those without one last.
    pub fn rows(&self, freq_khz: f64) -> Vec<&KiwiEntry> {
        let needle = self.filter.trim().to_lowercase();
        let mut rows: Vec<&KiwiEntry> = self
            .entries
            .iter()
            .filter(|e| !self.usable_only || usable(e))
            .filter(|e| freq_khz <= 0.0 || e.covers(freq_khz))
            .filter(|e| {
                needle.is_empty()
                    || [&e.location, &e.name, &e.antenna].iter().any(|t| t.to_lowercase().contains(&needle))
            })
            .collect();
        rows.sort_by_cached_key(|e| (e.location.trim().is_empty(), e.location.trim().to_lowercase(), e.name.to_lowercase()));
        rows
    }

    /// (all, online and allowing apps, of those with a free channel).
    pub fn counts(&self) -> (usize, usize, usize) {
        let apps = self.entries.iter().filter(|e| e.online && e.allows_apps()).count();
        let usable = self.entries.iter().filter(|e| usable(e)).count();
        (self.entries.len(), apps, usable)
    }
}

/// Online, its owner allows apps, and a channel is free.
pub fn usable(e: &KiwiEntry) -> bool {
    e.online && e.allows_apps() && e.free() > 0
}

fn job(dir: &Path, download: bool) -> Result<Loaded, String> {
    let path = dir.join(FILE_NAME);
    if download {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let part = dir.join(format!("{FILE_NAME}.part"));
        decdrm_schedule::fetch(DIRECTORY_URL, &part).map_err(|e| e.to_string())?;
        let text = std::fs::read_to_string(&part).map_err(|e| format!("{}: {e}", part.display()))?;
        let entries = match parse_directory(&text) {
            Ok(v) if !v.is_empty() => v,
            Ok(_) => {
                let _ = std::fs::remove_file(&part);
                return Err(format!("{DIRECTORY_URL} gave an empty list; the previous copy was kept"));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                return Err(format!("{DIRECTORY_URL}: {e}; the previous copy was kept"));
            }
        };
        std::fs::rename(&part, &path).map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok(Loaded { entries, modified: Some(SystemTime::now()) });
    }
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Loaded { entries: Vec::new(), modified: None }),
        Err(e) => Err(format!("{}: {e}", path.display())),
        Ok(text) => {
            let entries = parse_directory(&text).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(Loaded { entries, modified: std::fs::metadata(&path).and_then(|m| m.modified()).ok() })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"var kiwisdr_com = [
        {"name":"A","url":"http://a.example:8073","status":"active","offline":"no","users":"1","users_max":"8","ext_api":"4","loc":"Mishima, Japan","bands":"0-30000000"},
        {"name":"B","url":"http://b.example","status":"active","offline":"no","users":"8","users_max":"8","ext_api":"4","loc":"Tokyo, Japan"},
        {"name":"C","url":"http://c.example","status":"active","offline":"no","users":"0","users_max":"8","ext_api":"0","loc":"Adelaide, Australia"},
        {"name":"D","url":"http://d.example","status":"active","offline":"no","users":"0","users_max":"4","ext_api":"2","loc":"Bern, Switzerland","antenna":"Mini-Whip","bands":"0-5000000"},
    ];"#;

    #[test]
    fn reads_filters_and_groups_the_list() {
        let dir = std::env::temp_dir().join(format!("decdrm-kiwi-list-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE_NAME), LIST).unwrap();
        let mut list = KiwiList::new(dir.clone());
        list.open_window();
        let t = std::time::Instant::now();
        while list.busy() && t.elapsed().as_secs() < 5 {
            list.poll();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(list.error, None);
        assert_eq!(list.counts(), (4, 3, 2));
        // Usable only: A (Mishima) and D (Bern), by location.
        let names = |rows: Vec<&KiwiEntry>| rows.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(list.rows(0.0)), ["D", "A"]);
        // D only reaches 5 MHz.
        assert_eq!(names(list.rows(6140.0)), ["A"]);
        list.filter = "whip".into();
        assert_eq!(names(list.rows(0.0)), ["D"]);
        list.filter.clear();
        list.usable_only = false;
        assert_eq!(names(list.rows(0.0)), ["C", "D", "A", "B"], "Adelaide, Bern, Mishima, Tokyo");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_copy_yet_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("decdrm-kiwi-none-{}", std::process::id()));
        let loaded = job(&dir, false).unwrap();
        assert!(loaded.entries.is_empty() && loaded.modified.is_none());
    }
}
