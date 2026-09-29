//! Broadcast Website files on disk, so the system browser can show them.
//!
//! A browser needs the pages as files. With a data directory (`Settings::data_dir`) the
//! engine saves every website file already, below `<data dir>/website/service<N>/`
//! (`decdrm_engine::data_store`). Without one the GUI writes them itself, below its own
//! directory next to the settings file: `<settings dir>/websites/<service id>/`, one
//! directory per station's data service (by its 24-bit service id, so two stations
//! never mix), where newer versions of a file overwrite older ones. Nothing is ever
//! deleted there. Paths come off the air, so they go through
//! [`safe_relative_path`], which keeps every file inside the site's directory.
//!
//! The service id comes with the snapshots, and a recording decoded faster than real
//! time can deliver website files before the GUI has seen a snapshot listing their
//! service; such files wait in a list until the id is known (their content stays in
//! the data model, `DataService::website`, meanwhile).

use crate::data::DataServices;
use decdrm_data::website::safe_relative_path;
use decdrm_engine::ServiceView;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Who writes the files, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiteStore {
    /// The engine saves them below this data directory.
    Engine(PathBuf),
    /// The GUI writes them below this directory.
    Own(PathBuf),
}

/// Default directory of the GUI's own copies: `websites` next to the settings file
/// (so a test run with `--config` keeps everything in its own directory), else in the
/// temporary directory.
pub fn default_sites_dir(settings_file: Option<&Path>) -> PathBuf {
    settings_file
        .and_then(Path::parent)
        .map(|d| d.join("websites"))
        .unwrap_or_else(|| std::env::temp_dir().join("decdrm-websites"))
}

/// `true` for file names Windows reserves for devices (`CON`, `NUL`, `COM1`, …, with
/// any extension), which cannot be written as files there.
pub fn is_reserved_name(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit())
}

/// Whether a start page may be handed to the system: only HTML files, because opening
/// a file makes the system run the program registered for its type, and a
/// broadcaster's "start page" could be anything (e.g. an executable).
pub fn is_html_page(path: &str) -> bool {
    let ext = Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    matches!(ext.as_deref(), Some("html" | "htm" | "xhtml" | "xht"))
}

/// The website directories of one receiver run and the files written to them.
#[derive(Debug, Clone)]
pub struct SiteFiles {
    store: SiteStore,
    /// Directory of each service's site in this run.
    dirs: BTreeMap<u8, PathBuf>,
    /// Files received but not written yet: (short id, content name).
    pending: Vec<(u8, String)>,
    /// Files written by the GUI (own store only).
    pub written: u64,
    /// Files that could not be written, and the latest reason.
    pub failed: u64,
    pub last_error: Option<String>,
}

impl SiteFiles {
    pub fn new(store: SiteStore) -> Self {
        Self {
            store,
            dirs: BTreeMap::new(),
            pending: Vec::new(),
            written: 0,
            failed: 0,
            last_error: None,
        }
    }

    pub fn store(&self) -> &SiteStore {
        &self.store
    }

    /// The directory of service `short_id`'s site, once decided.
    pub fn dir(&self, short_id: u8) -> Option<&Path> {
        self.dirs.get(&short_id).map(PathBuf::as_path)
    }

    /// Where a file of the site is (or will be) on disk; `None` before the site's
    /// directory is decided, or for a path that would leave it.
    pub fn file_path(&self, short_id: u8, content_name: &str) -> Option<PathBuf> {
        Some(self.dir(short_id)?.join(safe_relative_path(content_name)?))
    }

    /// Files waiting for their service id.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// A website file of service `short_id` arrived; it is written by the next
    /// [`Self::flush`] that can decide the site's directory.
    pub fn received(&mut self, short_id: u8, content_name: &str) {
        if !self
            .pending
            .iter()
            .any(|(id, name)| *id == short_id && name == content_name)
        {
            self.pending.push((short_id, content_name.to_string()));
        }
    }

    /// The directory for service `short_id`, deciding it if possible: the engine's
    /// layout, or the service id below the GUI's directory (not known yet: `None`).
    fn dir_for(&mut self, short_id: u8, services: &[ServiceView]) -> Option<PathBuf> {
        if let Some(dir) = self.dirs.get(&short_id) {
            return Some(dir.clone());
        }
        let dir = match &self.store {
            SiteStore::Engine(base) => base.join("website").join(format!("service{short_id}")),
            SiteStore::Own(base) => {
                let id = services.iter().find(|s| s.short_id == short_id)?.service_id;
                base.join(format!("{:06X}", id & 0xFF_FFFF))
            }
        };
        self.dirs.insert(short_id, dir.clone());
        Some(dir)
    }

    /// Write the pending files whose site directory can be decided now (with the
    /// engine saving them, only note the directory). Their content is taken from
    /// `data`, so a file updated twice meanwhile is written once, in its newest version.
    pub fn flush(&mut self, data: &DataServices, services: &[ServiceView]) {
        for (short_id, name) in std::mem::take(&mut self.pending) {
            let Some(dir) = self.dir_for(short_id, services) else {
                self.pending.push((short_id, name));
                continue;
            };
            if matches!(self.store, SiteStore::Engine(_)) {
                continue;
            }
            let Some(file) = data.get(short_id).and_then(|s| s.website.get(&name)) else {
                continue;
            };
            let result = match safe_relative_path(&name) {
                Some(rel) if rel.iter().any(|c| is_reserved_name(&c.to_string_lossy())) => {
                    Err(format!("{name}: reserved file name"))
                }
                Some(rel) => write(&dir.join(rel), &file.data),
                None => Err(format!("{name}: unsafe path, not saved")),
            };
            match result {
                Ok(()) => self.written += 1,
                Err(e) => {
                    self.failed += 1;
                    self.last_error = Some(e);
                }
            }
        }
    }
}

fn write(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, data).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdrm_data::DataEvent;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("decdrm-gui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A website file of service `short_id` into the data model and the file list.
    fn arrive(
        data: &mut DataServices,
        sites: &mut SiteFiles,
        short_id: u8,
        path: &str,
        body: &[u8],
    ) {
        let event = DataEvent::WebsiteFile {
            path: path.into(),
            mime: "text/html".into(),
            data: body.to_vec(),
        };
        data.apply(short_id, &event, None);
        sites.received(short_id, path);
    }

    fn service(short_id: u8, service_id: u32) -> ServiceView {
        ServiceView {
            short_id,
            service_id,
            ..ServiceView::default()
        }
    }

    #[test]
    fn own_store_waits_for_the_service_id() {
        let base = temp_dir("sites");
        let mut sites = SiteFiles::new(SiteStore::Own(base.clone()));
        let mut data = DataServices::default();
        arrive(&mut data, &mut sites, 1, "/index.html", b"old");
        arrive(&mut data, &mut sites, 1, "img/logo.png", b"png");
        arrive(&mut data, &mut sites, 1, "/index.html", b"<html>");
        assert_eq!(sites.pending(), 2, "a repeated file is listed once");
        // No snapshot has listed the service yet: nothing can be written.
        sites.flush(&data, &[]);
        assert_eq!((sites.pending(), sites.written), (2, 0));
        assert_eq!(sites.dir(1), None);
        // Now it has.
        sites.flush(&data, &[service(1, 0x00E2_1234)]);
        let dir = base.join("E21234");
        assert_eq!(sites.dir(1), Some(dir.as_path()));
        assert_eq!(sites.pending(), 0);
        assert_eq!(sites.written, 2);
        assert_eq!(
            std::fs::read(dir.join("index.html")).unwrap(),
            b"<html>",
            "newest version"
        );
        assert_eq!(
            std::fs::read(dir.join("img").join("logo.png")).unwrap(),
            b"png"
        );
        assert_eq!(
            sites.file_path(1, "index.html"),
            Some(dir.join("index.html"))
        );
        // Paths that would leave the directory are refused.
        arrive(&mut data, &mut sites, 1, "../../escape.html", b"x");
        sites.flush(&data, &[]);
        assert_eq!(sites.failed, 1);
        assert!(!base.join("escape.html").exists());
        assert_eq!(sites.file_path(1, "../x"), None);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn engine_store_only_names_the_directory() {
        let base = temp_dir("engine-sites");
        let mut sites = SiteFiles::new(SiteStore::Engine(base.clone()));
        let mut data = DataServices::default();
        arrive(&mut data, &mut sites, 3, "index.html", b"<html>");
        sites.flush(&data, &[]);
        let dir = base.join("website").join("service3");
        assert_eq!(
            sites.dir(3),
            Some(dir.as_path()),
            "known without a service id"
        );
        assert!(!dir.exists(), "the engine writes these files");
        assert_eq!((sites.written, sites.pending()), (0, 0));
    }

    #[test]
    fn names_and_pages() {
        for name in ["CON", "nul.txt", "Com1.html", "LPT9"] {
            assert!(is_reserved_name(name), "{name}");
        }
        for name in ["CONSOLE.html", "index.html", "COM10", "com"] {
            assert!(!is_reserved_name(name), "{name}");
        }
        assert!(is_html_page("index.html") && is_html_page("news/Page.HTM"));
        assert!(!is_html_page("setup.exe") && !is_html_page("index") && !is_html_page("a.js"));
        assert_eq!(
            default_sites_dir(Some(Path::new("/cfg/decdrm/gui.toml"))),
            PathBuf::from("/cfg/decdrm/websites")
        );
    }
}
