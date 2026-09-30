//! Saving received data objects (slideshow images, website files, EPG) to disk, and
//! capturing the data of applications DecDRM does not interpret (TPEG, unknown ones).

use decdrm_data::DataEvent;
use decdrm_data::datagroup::DataGroup;
use decdrm_data::website::safe_relative_path;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Writes data-service objects below a base directory: `slides/`,
/// `website/service<N>/`, `epg/`, and `raw/` for the captures:
///
/// * `raw/service<N>_tpeg.bin` (user application 0x004) or `raw/service<N>_app<XXX>.bin`
///   (any other uninterpreted application, id in hex): the data fields of the MSC data
///   groups received with a valid CRC, one after the other — for TPEG the stream of
///   TPEG transport frames. Damaged data groups are left out.
/// * `raw/service<N>_stream_app<XXX>.bin`: the bytes of a synchronous stream mode
///   service, frame after frame.
///
/// A capture file is started afresh by the first data of each run and then appended to.
pub struct DataStore {
    base: PathBuf,
    slide_count: u64,
    /// Open capture files.
    captures: BTreeMap<PathBuf, File>,
}

impl DataStore {
    pub fn new(base: PathBuf) -> Self {
        Self { base, slide_count: 0, captures: BTreeMap::new() }
    }

    /// Append `data` to the capture file `name` below `raw/`; a log line when the file
    /// is created.
    fn capture(&mut self, name: String, what: &str, data: &[u8]) -> Option<String> {
        let path = self.base.join("raw").join(name);
        let mut line = None;
        if !self.captures.contains_key(&path) {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).ok()?;
            }
            let file = File::create(&path).ok()?;
            self.captures.insert(path.clone(), file);
            line = Some(format!("capturing {what} to {}", path.display()));
        }
        self.captures.get_mut(&path)?.write_all(data).ok()?;
        line
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
            DataEvent::Raw { user_app_id, data_group } => {
                let dg = DataGroup::parse(data_group).ok()?;
                let (name, what) = if *user_app_id == 0x004 {
                    (format!("service{short_id}_tpeg.bin"), format!("the TPEG data of service {short_id}"))
                } else {
                    (
                        format!("service{short_id}_app{user_app_id:03X}.bin"),
                        format!("the data of application {user_app_id:#05X} of service {short_id}"),
                    )
                };
                self.capture(name, &what, &dg.data)
            }
            DataEvent::StreamData { user_app_id, data } => self.capture(
                format!("service{short_id}_stream_app{user_app_id:03X}.bin"),
                &format!("the stream data of service {short_id}"),
                data,
            ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use decdrm_data::datagroup::group_type;

    #[test]
    fn raw_data_is_captured() {
        let dir = std::env::temp_dir().join(format!("decdrm-engine-raw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = DataStore::new(dir.clone());
        let unit = |data: &[u8]| DataGroup::new(group_type::GENERAL_DATA, data.to_vec()).to_bytes();
        let tpeg = |data: &[u8]| DataEvent::Raw { user_app_id: 0x004, data_group: unit(data) };
        let first = store.save(1, &tpeg(b"TPEG1")).expect("a log line for the new file");
        assert!(first.contains("TPEG"), "{first}");
        assert_eq!(store.save(1, &tpeg(b"TPEG2")), None, "no line for more data");
        let mut damaged = unit(b"XXXXX");
        damaged[3] ^= 0xFF;
        store.save(1, &DataEvent::Raw { user_app_id: 0x004, data_group: damaged });
        store.save(2, &DataEvent::Raw { user_app_id: 0x123, data_group: unit(b"other") });
        store.save(0, &DataEvent::StreamData { user_app_id: 0x0AB, data: b"stream".to_vec() });
        drop(store);
        let read = |name: &str| std::fs::read(dir.join("raw").join(name)).unwrap();
        assert_eq!(read("service1_tpeg.bin"), b"TPEG1TPEG2", "the damaged group is left out");
        assert_eq!(read("service2_app123.bin"), b"other");
        assert_eq!(read("service0_stream_app0AB.bin"), b"stream");
        // A new run starts the capture afresh.
        let mut store = DataStore::new(dir.clone());
        store.save(1, &tpeg(b"new"));
        drop(store);
        assert_eq!(read("service1_tpeg.bin"), b"new");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
