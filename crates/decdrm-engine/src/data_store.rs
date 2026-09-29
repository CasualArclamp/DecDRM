//! Saving received data objects (slideshow images, website files, EPG) to disk.

use decdrm_data::DataEvent;
use decdrm_data::website::safe_relative_path;
use std::path::{Path, PathBuf};

/// Writes data-service objects below a base directory:
/// `slides/`, `website/service<N>/`, `epg/`.
pub struct DataStore {
    base: PathBuf,
    slide_count: u64,
}

impl DataStore {
    pub fn new(base: PathBuf) -> Self {
        Self { base, slide_count: 0 }
    }

    /// Save the object carried by `event` (if it is one worth saving) and return a
    /// log line.
    pub fn save(&mut self, short_id: u8, event: &DataEvent) -> Option<String> {
        match event {
            DataEvent::SlideShowImage { name, mime, data, .. } => {
                self.slide_count += 1;
                let ext = extension(name, mime);
                let stem = safe_relative_path(name)
                    .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| format!("slide{}", self.slide_count));
                let path = self.base.join("slides").join(format!("s{short_id}_{:05}_{stem}.{ext}", self.slide_count));
                write(&path, data).map(|()| format!("saved slide {} ({} bytes)", path.display(), data.len()))
            }
            DataEvent::WebsiteFile { path, data, .. } => {
                let rel = safe_relative_path(path)?;
                let full = self.base.join("website").join(format!("service{short_id}")).join(rel);
                write(&full, data).map(|()| format!("saved website file {}", full.display()))
            }
            DataEvent::Epg { name, xml } => {
                let rel = safe_relative_path(name).unwrap_or_else(|| PathBuf::from("epg"));
                let mut full = self.base.join("epg").join(rel);
                full.set_extension("xml");
                write(&full, xml.as_bytes()).map(|()| format!("saved EPG {}", full.display()))
            }
            _ => None,
        }
    }
}

fn extension(name: &str, mime: &str) -> String {
    if let Some(e) = Path::new(name).extension() {
        return e.to_string_lossy().to_ascii_lowercase();
    }
    match mime {
        m if m.contains("png") => "png".into(),
        m if m.contains("gif") => "gif".into(),
        m if m.contains("jpeg") || m.contains("jpg") => "jpg".into(),
        _ => "bin".into(),
    }
}

fn write(path: &Path, data: &[u8]) -> Option<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    std::fs::write(path, data).ok()
}
