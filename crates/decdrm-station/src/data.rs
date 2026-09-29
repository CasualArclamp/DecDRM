//! Data applications: turns each configured application into a `decdrm_data` carousel
//! (a [`DataUnitSource`]) and the applications of one stream into its packet-mode
//! [`DataEncoder`].
//!
//! * Slideshow — every JPEG/PNG of a folder, cycled in name order
//!   ([`SlideShowFeeder`]: MOT header mode, TriggerTime "now").
//! * Website — a directory tree in a MOT directory-mode carousel.
//! * Journaline — pages from a TOML/JSON page file ([`JournalineFile`]).
//! * EPG — a schedule (TS 102 818 `epg`/`schedule`/`programme`) from inline or file
//!   programmes, binary encoded (TS 102 371) in a directory-mode MOT carousel with
//!   ScopeStart/ScopeEnd/ScopeId (the service id).

use crate::config::{AppKind, AppSettings, EpgFile, EpgProgramme, JournalineFile, JournalinePage, JournalineRow, StationConfig};
use decdrm_data::encoder::DataUnitSource;
use decdrm_data::epg::{self, EpgElement, EpgValue};
use decdrm_data::journaline::{JournalineEncoder, ListItem, MenuItem, NmlObject, ROOT_OBJECT_ID};
use decdrm_data::mot::{MotEncoder, content_type};
use decdrm_data::slideshow::SlideShowFeeder;
use decdrm_data::time::MotTime;
use decdrm_data::{DataEncoder, DataServiceConfig, website};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Largest MOT segment size (13-bit field).
const MAX_SEGMENT_SIZE: usize = decdrm_data::mot::MAX_SEGMENT_SIZE;

/// A boxed data-unit source that can move to another thread.
pub(crate) type Source = Box<dyn DataUnitSource + Send>;

fn is_image(path: &Path) -> bool {
    path.is_file()
        && path.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png"))
}

/// The resolved `path` of an application, or a problem text if it is missing.
fn required_path(cfg: &StationConfig, app: &AppSettings) -> Result<PathBuf, String> {
    app.path.as_ref().map(|p| cfg.resolve(p)).ok_or_else(|| format!("{} needs a `path`", app.kind))
}

/// Cheap checks of an application's content (files exist, page file and schedule
/// parse) for [`crate::StationConfig::validate`]; the content itself is loaded by
/// [`build_source`].
pub(crate) fn check_content(cfg: &StationConfig, app: &AppSettings) -> Result<(), String> {
    if let Some(s) = app.segment_size
        && !(1..=MAX_SEGMENT_SIZE).contains(&s)
    {
        return Err(format!("segment_size {s} is outside 1-{MAX_SEGMENT_SIZE}"));
    }
    match app.kind {
        AppKind::Slideshow => {
            let dir = required_path(cfg, app)?;
            let n = std::fs::read_dir(&dir)
                .map_err(|e| format!("slideshow folder {}: {e}", dir.display()))?
                .filter_map(|e| e.ok())
                .filter(|e| is_image(&e.path()))
                .count();
            if n == 0 {
                return Err(format!("slideshow folder {} has no JPEG or PNG images", dir.display()));
            }
        }
        AppKind::Website => {
            let dir = required_path(cfg, app)?;
            if !dir.is_dir() {
                return Err(format!("website directory {} does not exist", dir.display()));
            }
            if let Some(index) = &app.index
                && !dir.join(index).is_file()
            {
                return Err(format!("website start page {index} not found in {}", dir.display()));
            }
        }
        AppKind::Journaline => {
            let file = load_journaline(&required_path(cfg, app)?)?;
            journaline_encoder(&file, app.compress)?;
        }
        AppKind::Epg => {
            let programmes = epg_programmes(cfg, app)?;
            epg_object(&programmes, 0)?;
        }
    }
    Ok(())
}

/// The carousel of one application. `service_id` is the owning service's id (EPG
/// ScopeId).
pub(crate) fn build_source(cfg: &StationConfig, app: &AppSettings, service_id: u32) -> Result<Source, String> {
    let segment = |mot: &mut MotEncoder| -> Result<(), String> {
        if let Some(s) = app.segment_size {
            mot.set_segment_size(s).map_err(|e| e.to_string())?;
        }
        Ok(())
    };
    Ok(match app.kind {
        AppKind::Slideshow => {
            let dir = required_path(cfg, app)?;
            let mut feeder =
                SlideShowFeeder::from_dir(&dir).map_err(|e| format!("slideshow folder {}: {e}", dir.display()))?;
            if feeder.is_empty() {
                return Err(format!("slideshow folder {} has no JPEG or PNG images", dir.display()));
            }
            segment(feeder.encoder_mut())?;
            Box::new(feeder)
        }
        AppKind::Website => {
            let dir = required_path(cfg, app)?;
            let index = app.index.clone().or_else(|| dir.join("index.html").is_file().then(|| "index.html".to_string()));
            let mut mot = website::encoder_from_dir(&dir, index.as_deref())
                .map_err(|e| format!("website directory {}: {e}", dir.display()))?;
            if mot.objects().is_empty() {
                return Err(format!("website directory {} is empty", dir.display()));
            }
            mot.set_compress_directory(app.compress);
            segment(&mut mot)?;
            Box::new(mot)
        }
        AppKind::Journaline => {
            let file = load_journaline(&required_path(cfg, app)?)?;
            Box::new(journaline_encoder(&file, app.compress)?)
        }
        AppKind::Epg => {
            let programmes = epg_programmes(cfg, app)?;
            let (header, body) = epg_object(&programmes, service_id)?;
            let mut mot = MotEncoder::directory_mode();
            segment(&mut mot)?;
            mot.add_object(header, body).map_err(|e| e.to_string())?;
            Box::new(mot)
        }
    })
}

/// The packet-mode encoder of a data stream carrying `apps` (application settings,
/// packet id, source) with packets of `packet_len` bytes in total.
pub(crate) fn stream_encoder(packet_len: usize, apps: Vec<(&AppSettings, u8, Source)>) -> Result<DataEncoder, String> {
    let mut iter = apps.into_iter();
    let (first, id, source) = iter.next().ok_or("a data stream without applications")?;
    let mut cfg = DataServiceConfig::packet(first.kind.user_application(), id, 0);
    cfg.packet_len = packet_len;
    let mut enc = DataEncoder::new(&cfg, source).map_err(|e| e.to_string())?;
    for (_, id, source) in iter {
        enc.add_packet_channel(id, true, source).map_err(|e| e.to_string())?;
    }
    Ok(enc)
}

// ---------------------------------------------------------------------------------
// Journaline
// ---------------------------------------------------------------------------------

/// Read a Journaline page file: JSON if the extension is `.json`, TOML otherwise.
pub fn load_journaline(path: &Path) -> Result<JournalineFile, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("Journaline page file {}: {e}", path.display()))?;
    let json = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("json"));
    if json {
        serde_json::from_str(&text).map_err(|e| format!("Journaline page file {}: {e}", path.display()))
    } else {
        toml::from_str(&text).map_err(|e| format!("Journaline page file {}: {e}", path.display()))
    }
}

/// One page as an NML object.
fn nml_page(p: &JournalinePage) -> Result<NmlObject, String> {
    Ok(match (&p.menu, &p.text, &p.list) {
        (Some(items), None, None) => {
            NmlObject::menu(p.id, &p.title, items.iter().map(|l| MenuItem::new(l.link, &l.text)).collect())
        }
        (None, Some(text), None) => NmlObject::plain_text(p.id, &p.title, text),
        (None, None, Some(rows)) => {
            let mut items = Vec::new();
            for row in rows {
                match row {
                    JournalineRow::Single(s) => items.push(ListItem::row(s)),
                    JournalineRow::Cells(cells) => {
                        for (i, c) in cells.iter().enumerate() {
                            items.push(if i == 0 { ListItem::row(c) } else { ListItem::cell(c) });
                        }
                    }
                }
            }
            NmlObject::list(p.id, &p.title, items)
        }
        (None, None, None) => NmlObject::title_only(p.id, &p.title),
        _ => return Err(format!("Journaline page {} has more than one of `menu`, `text` and `list`", p.id)),
    })
}

/// A Journaline carousel with every page of `file`, after checking the page structure:
/// unique ids, a root menu (page 0) and menu links to existing pages.
pub(crate) fn journaline_encoder(file: &JournalineFile, compress: bool) -> Result<JournalineEncoder, String> {
    if file.pages.is_empty() {
        return Err("the Journaline page file has no pages".into());
    }
    let mut ids = BTreeSet::new();
    for p in &file.pages {
        if !ids.insert(p.id) {
            return Err(format!("Journaline page id {} is used twice", p.id));
        }
    }
    match file.pages.iter().find(|p| p.id == ROOT_OBJECT_ID) {
        Some(root) if root.menu.is_some() => {}
        Some(_) => return Err("Journaline page 0 (the root) must be a menu".into()),
        None => return Err("the Journaline page file needs a root menu with id 0".into()),
    }
    let mut enc = JournalineEncoder::new();
    enc.set_compression(compress).map_err(|e| e.to_string())?;
    for p in &file.pages {
        if let Some(menu) = &p.menu
            && let Some(bad) = menu.iter().find(|l| !ids.contains(&l.link))
        {
            return Err(format!("Journaline page {} links to page {}, which does not exist", p.id, bad.link));
        }
        enc.insert(nml_page(p)?).map_err(|e| format!("Journaline page {}: {e} (at most 4092 bytes)", p.id))?;
    }
    Ok(enc)
}

// ---------------------------------------------------------------------------------
// EPG
// ---------------------------------------------------------------------------------

/// All programmes of an EPG application (inline ones, then those of `path`), sorted
/// by start time.
fn epg_programmes(cfg: &StationConfig, app: &AppSettings) -> Result<Vec<(i64, EpgProgramme)>, String> {
    let mut all: Vec<EpgProgramme> = app.programmes.clone();
    if let Some(p) = &app.path {
        let path = cfg.resolve(p);
        let text = std::fs::read_to_string(&path).map_err(|e| format!("EPG file {}: {e}", path.display()))?;
        let file: EpgFile = toml::from_str(&text).map_err(|e| format!("EPG file {}: {e}", path.display()))?;
        all.extend(file.programmes);
    }
    if all.is_empty() {
        return Err("the EPG has no programmes (add [[...programme]] entries or a `path`)".into());
    }
    let mut out = Vec::with_capacity(all.len());
    for p in all {
        let start = crate::time::parse_iso8601(&p.start)
            .ok_or_else(|| format!("EPG programme \"{}\": start time \"{}\" is not ISO 8601", p.title, p.start))?;
        if p.title.trim().is_empty() {
            return Err("an EPG programme has an empty title".into());
        }
        if !(1..=1092).contains(&p.duration_min) {
            return Err(format!("EPG programme \"{}\": duration {} min is outside 1-1092", p.title, p.duration_min));
        }
        out.push((start, p));
    }
    out.sort_by_key(|(t, _)| *t);
    Ok(out)
}

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// The programme information object (MOT header and TS 102 371 binary body) of a
/// schedule.
fn epg_object(programmes: &[(i64, EpgProgramme)], service_id: u32) -> Result<(decdrm_data::mot::MotHeader, Vec<u8>), String> {
    let first = programmes.first().map(|(t, _)| *t).unwrap_or(0);
    let last = programmes.iter().map(|(t, p)| t + 60 * i64::from(p.duration_min)).max().unwrap_or(first);
    let mut schedule = EpgElement::new("schedule").attr("version", EpgValue::U16(1)).child(
        EpgElement::new("scope")
            .attr("startTime", EpgValue::Time(MotTime::from_unix(first)))
            .attr("stopTime", EpgValue::Time(MotTime::from_unix(last))),
    );
    for (i, (start, p)) in programmes.iter().enumerate() {
        let mut prog = EpgElement::new("programme")
            .attr("shortId", EpgValue::U24(i as u32 + 1))
            .attr("version", EpgValue::U16(1))
            .child(EpgElement::new("mediumName").text(truncate_chars(&p.title, 16)));
        if p.title.chars().count() > 16 {
            prog = prog.child(EpgElement::new("longName").text(truncate_chars(&p.title, 128)));
        }
        prog = prog.child(
            EpgElement::new("location").child(
                EpgElement::new("time")
                    .attr("time", EpgValue::Time(MotTime::from_unix(*start)))
                    .attr("duration", EpgValue::Duration((p.duration_min * 60) as u16)),
            ),
        );
        if let Some(d) = p.description.as_deref().filter(|d| !d.is_empty()) {
            prog = prog.child(
                EpgElement::new("mediaDescription").child(EpgElement::new("shortDescription").text(truncate_chars(d, 180))),
            );
        }
        schedule = schedule.child(prog);
    }
    let root = EpgElement::new("epg").attr("system", EpgValue::Enum("DRM".into())).child(schedule);
    let body = epg::encode(&root).map_err(|e| format!("EPG: {e}"))?;
    let header = epg::mot_header(
        content_type::EPG_PROGRAMME_INFORMATION,
        "",
        Some(MotTime::from_unix(first)),
        Some(MotTime::from_unix(last)),
        Some(service_id & 0xFF_FFFF),
    );
    Ok((header, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::JournalineLink;
    use decdrm_data::{DataDecoder, DataEvent, UserApplication};

    fn page(id: u16, title: &str) -> JournalinePage {
        JournalinePage { id, title: title.into(), menu: None, text: None, list: None }
    }

    #[test]
    fn journaline_structure_checks() {
        let mut root = page(0, "Root");
        root.menu = Some(vec![JournalineLink { link: 1, text: "One".into() }]);
        let mut one = page(1, "One");
        one.text = Some("Body".into());
        let ok = JournalineFile { pages: vec![root.clone(), one.clone()] };
        assert_eq!(journaline_encoder(&ok, false).unwrap().len(), 2);
        let missing_link = JournalineFile { pages: vec![root.clone()] };
        assert!(journaline_encoder(&missing_link, false).unwrap_err().contains("page 1"));
        let no_root = JournalineFile { pages: vec![one.clone()] };
        assert!(journaline_encoder(&no_root, false).unwrap_err().contains("root"));
        let mut both = one.clone();
        both.list = Some(vec![JournalineRow::Single("x".into())]);
        assert!(nml_page(&both).is_err());
        let dup = JournalineFile { pages: vec![root, one.clone(), one] };
        assert!(journaline_encoder(&dup, false).unwrap_err().contains("twice"));
    }

    /// Page files may be TOML or JSON (by extension); both describe the same pages.
    #[test]
    fn journaline_page_file_formats() {
        let dir = tempfile::tempdir().unwrap();
        let json = dir.path().join("pages.json");
        std::fs::write(
            &json,
            r#"{"page": [
                {"id": 0, "title": "Root", "menu": [{"link": 5, "text": "Table"}]},
                {"id": 5, "title": "Table", "list": [["a", "b"], "single row"]}
            ]}"#,
        )
        .unwrap();
        let toml_path = dir.path().join("pages.toml");
        std::fs::write(
            &toml_path,
            "[[page]]\nid = 0\ntitle = \"Root\"\nmenu = [{ link = 5, text = \"Table\" }]\n\
             [[page]]\nid = 5\ntitle = \"Table\"\nlist = [[\"a\", \"b\"], \"single row\"]\n",
        )
        .unwrap();
        let (a, b) = (load_journaline(&json).unwrap(), load_journaline(&toml_path).unwrap());
        assert_eq!(a, b);
        let enc = journaline_encoder(&a, true).unwrap();
        let table = enc.get(5).unwrap();
        assert_eq!(
            table.body,
            decdrm_data::journaline::NmlBody::List(vec![ListItem::row("a"), ListItem::cell("b"), ListItem::row("single row")])
        );
        std::fs::write(&json, "{\"page\": [{\"id\": 0}]}").unwrap();
        assert!(load_journaline(&json).unwrap_err().contains("title"));
    }

    /// A website directory becomes a directory-mode carousel that the receiver's data
    /// decoder turns back into files and a start page.
    #[test]
    fn website_carousel_decodes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("img")).unwrap();
        std::fs::write(dir.path().join("index.html"), "<html>DecDRM</html>").unwrap();
        std::fs::write(dir.path().join("img/logo.png"), [0x89, b'P', b'N', b'G', 1, 2, 3]).unwrap();
        let cfg = StationConfig { base_dir: Some(dir.path().to_path_buf()), ..Default::default() };
        let mut app = AppSettings::new(AppKind::Website);
        app.path = Some(".".into());
        app.compress = true;
        check_content(&cfg, &app).unwrap();
        let src = build_source(&cfg, &app, 1).unwrap();
        let mut enc = stream_encoder(48, vec![(&app, 0, src)]).unwrap();
        let mut dec = DataDecoder::new(DataServiceConfig::packet(UserApplication::BroadcastWebsite, 0, 45));
        let events: Vec<DataEvent> = (0..10).flat_map(|_| dec.push_frame(&enc.next_frame(480))).collect();
        let files: BTreeSet<String> = events
            .iter()
            .filter_map(|e| match e {
                DataEvent::WebsiteFile { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(files, ["img/logo.png".to_string(), "index.html".to_string()].into());
        assert!(events.iter().any(|e| matches!(e, DataEvent::WebsiteIndex { path } if path == "index.html")));
    }

    #[test]
    fn epg_schedule_decodes() {
        let progs = vec![
            (
                crate::time::parse_iso8601("2026-09-29T19:00:00Z").unwrap(),
                EpgProgramme {
                    title: "Evening Music With A Long Name".into(),
                    start: String::new(),
                    duration_min: 60,
                    description: Some("Songs".into()),
                },
            ),
            (
                crate::time::parse_iso8601("2026-09-29T18:00:00Z").unwrap(),
                EpgProgramme { title: "News".into(), start: String::new(), duration_min: 30, description: None },
            ),
        ];
        let (header, body) = epg_object(&progs, 0xD0D001).unwrap();
        let xml = epg::decode_to_xml(&body).unwrap();
        assert!(xml.contains("<mediumName>News</mediumName>"), "{xml}");
        assert!(xml.contains("<longName>Evening Music With A Long Name</longName>"), "{xml}");
        assert!(xml.contains(r#"duration="PT1H""#), "{xml}");
        assert_eq!(header.epg_scope_id(), Some(0xD0D001));
        // Through a packet stream and the receiver's data decoder.
        let mut mot = MotEncoder::directory_mode();
        mot.add_object(header, body).unwrap();
        let src: Source = Box::new(mot);
        let app = AppSettings::new(AppKind::Epg);
        let mut enc = stream_encoder(48, vec![(&app, 2, src)]).unwrap();
        let mut dec = DataDecoder::new(DataServiceConfig::packet(UserApplication::Epg, 2, 45));
        let found = (0..20).flat_map(|_| dec.push_frame(&enc.next_frame(480))).any(|e| matches!(e, DataEvent::Epg { .. }));
        assert!(found);
    }
}
