//! MOT Broadcast Website (ETSI TS 101 498): a receiver-side file cache for a GUI
//! (Dream `CWebsiteCache` in `BWSViewer.cpp`) and a transmitter helper that loads a
//! directory tree into a directory-mode MOT carousel.

use crate::decoder::DataEvent;
use crate::mot::{MotEncoder, profile};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// One cached file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WebsiteFile {
    /// MIME type.
    pub mime: String,
    /// File content.
    pub data: Vec<u8>,
}

/// Files of a received broadcast website, keyed by their MOT ContentName path.
#[derive(Debug, Clone, Default)]
pub struct Website {
    files: BTreeMap<String, WebsiteFile>,
    index: Option<String>,
}

/// Normalise a ContentName into a cache key: forward slashes, no leading slash, no
/// `.` components.
fn normalise(path: &str) -> String {
    path.split(['/', '\\'])
        .filter(|c| !c.is_empty() && *c != ".")
        .collect::<Vec<_>>()
        .join("/")
}

impl Website {
    /// Empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Update from a decoder event; returns `true` if the event concerned the website.
    pub fn apply(&mut self, event: &DataEvent) -> bool {
        match event {
            DataEvent::WebsiteFile { path, mime, data } => {
                self.insert(path, mime, data.clone());
                true
            }
            DataEvent::WebsiteIndex { path } => {
                self.index = Some(normalise(path));
                true
            }
            _ => false,
        }
    }

    /// Store a file.
    pub fn insert(&mut self, path: &str, mime: &str, data: Vec<u8>) {
        self.files.insert(
            normalise(path),
            WebsiteFile {
                mime: mime.to_owned(),
                data,
            },
        );
    }

    /// Look up a file (leading slashes and backslashes are tolerated).
    pub fn get(&self, path: &str) -> Option<&WebsiteFile> {
        self.files.get(&normalise(path))
    }

    /// The start page signalled in the MOT directory (DirectoryIndex).
    pub fn index(&self) -> Option<&str> {
        self.index.as_deref()
    }

    /// The start page file, if received: the DirectoryIndex, else `index.html`.
    pub fn start_page(&self) -> Option<(&str, &WebsiteFile)> {
        let candidates = self
            .index
            .iter()
            .map(String::as_str)
            .chain(["index.html", "index.htm"]);
        for c in candidates {
            if let Some((k, v)) = self.files.get_key_value(c) {
                return Some((k.as_str(), v));
            }
        }
        None
    }

    /// All cached paths in sorted order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// Number of files.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// `true` if no file was received.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Total bytes cached.
    pub fn total_bytes(&self) -> usize {
        self.files.values().map(|f| f.data.len()).sum()
    }

    /// Forget everything (e.g. on a service change).
    pub fn clear(&mut self) {
        self.files.clear();
        self.index = None;
    }
}

/// Turn a received ContentName into a relative path that is safe to create below a
/// download directory: rejects `..`, absolute paths and drive prefixes (ContentNames
/// come off the air and must not escape the target directory).
pub fn safe_relative_path(content_name: &str) -> Option<PathBuf> {
    let norm = normalise(content_name);
    if norm.is_empty() {
        return None;
    }
    let mut out = PathBuf::new();
    for part in norm.split('/') {
        if part == ".." || part.contains(':') {
            return None;
        }
        out.push(part);
    }
    // Belt and braces: every component must be a plain name.
    out.components()
        .all(|c| matches!(c, Component::Normal(_)))
        .then_some(out)
}

/// Build a directory-mode MOT carousel with every file below `root` (ContentNames are
/// the paths relative to `root` with `/` separators). `index` becomes the
/// DirectoryIndex for the basic and unrestricted-PC profiles.
pub fn encoder_from_dir(
    root: impl AsRef<Path>,
    index: Option<&str>,
) -> std::io::Result<MotEncoder> {
    let root = root.as_ref();
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort();
    let mut enc = MotEncoder::directory_mode();
    for (name, path) in files {
        let data = std::fs::read(&path)?;
        enc.add_file(&name, data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    }
    if let Some(index) = index {
        enc.set_directory_index(profile::BASIC, index);
        enc.set_directory_index(profile::UNRESTRICTED_PC, index);
    }
    Ok(enc)
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let name = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            out.push((name, path));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_and_index() {
        let mut w = Website::new();
        assert!(w.apply(&DataEvent::WebsiteFile {
            path: "/news/a.html".into(),
            mime: "text/html".into(),
            data: b"a".to_vec()
        }));
        assert!(w.apply(&DataEvent::WebsiteFile {
            path: "index.html".into(),
            mime: "text/html".into(),
            data: b"i".to_vec()
        }));
        assert_eq!(w.get("news/a.html").unwrap().data, b"a");
        assert_eq!(w.get("\\news\\a.html").unwrap().data, b"a");
        assert_eq!(w.start_page().unwrap().0, "index.html");
        w.apply(&DataEvent::WebsiteIndex {
            path: "news/a.html".into(),
        });
        assert_eq!(w.start_page().unwrap().0, "news/a.html");
        assert_eq!(w.paths().collect::<Vec<_>>(), ["index.html", "news/a.html"]);
        assert_eq!(w.total_bytes(), 2);
    }

    #[test]
    fn safe_paths() {
        assert_eq!(
            safe_relative_path("/img/./logo.png"),
            Some(PathBuf::from("img").join("logo.png"))
        );
        assert_eq!(safe_relative_path("../../etc/passwd"), None);
        assert_eq!(safe_relative_path("C:\\Windows\\x"), None);
        assert_eq!(safe_relative_path("//"), None);
    }
}
