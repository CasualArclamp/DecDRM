//! MOT Broadcast Website viewer: the files received so far and a button that opens the
//! start page in the system's web browser (only when clicked, never by itself).

use super::data_info::fmt_bytes;
use super::placeholder;
use super::services::service_name;
use crate::data::{DataServices, choose_service};
use crate::website::{SiteFiles, SiteStore, is_html_page};
use decdrm_engine::ServiceView;
use eframe::egui::{self, RichText, Ui};
use std::path::Path;

/// Viewer state: the service shown and the outcome of the last "Open in browser".
#[derive(Default)]
pub struct WebsiteView {
    service: Option<u8>,
    message: Option<Result<String, String>>,
}

/// Why the start page cannot be opened yet, or its file.
pub fn start_page_file(
    site: &decdrm_data::website::Website,
    files: &SiteFiles,
    short_id: u8,
) -> Result<std::path::PathBuf, String> {
    let Some((name, _)) = site.start_page() else {
        return Err(match site.index() {
            Some(index) => format!("The start page {index} has not been received yet."),
            None => "No start page yet (neither a signalled one nor index.html).".into(),
        });
    };
    if !is_html_page(name) {
        return Err(format!(
            "The start page {name} is not an HTML file, so it is not opened."
        ));
    }
    let path = files
        .file_path(short_id, name)
        .ok_or_else(|| format!("The start page {name} has an unusable name."))?;
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("{} is not on disk (yet).", path.display()))
    }
}

impl WebsiteView {
    /// Prefer service `short_id` (shown if it has website files).
    pub fn focus(&mut self, short_id: u8) {
        self.service = Some(short_id);
    }

    pub fn show(
        &mut self,
        ui: &mut Ui,
        data: &DataServices,
        files: &SiteFiles,
        services: &[ServiceView],
    ) {
        let ids = data.website_ids();
        self.service = choose_service(self.service, &ids);
        let (Some(id), Some(service)) = (self.service, self.service.and_then(|id| data.get(id)))
        else {
            placeholder(ui, "No broadcast website received yet.");
            return;
        };
        if ids.len() > 1 {
            ui.horizontal(|ui| {
                ui.label("Service");
                for &other in &ids {
                    ui.selectable_value(&mut self.service, Some(other), other.to_string());
                }
            });
        }
        let site = &service.website;
        let name = services
            .iter()
            .find(|s| s.short_id == id)
            .map_or_else(|| format!("Service {id}"), service_name);
        ui.label(RichText::new(name).strong());
        ui.label(format!(
            "{} files, {}; start page: {}",
            site.len(),
            fmt_bytes(site.total_bytes() as u64),
            site.start_page().map_or("–", |(p, _)| p)
        ));

        // Open the start page: only on a click, and only an HTML file.
        let start = start_page_file(site, files, id);
        ui.horizontal(|ui| {
            let button = ui.add_enabled(start.is_ok(), egui::Button::new("Open in browser"));
            let button = match &start {
                Ok(path) => button.on_hover_text(format!("Open {}", path.display())),
                Err(why) => button.on_disabled_hover_text(why.as_str()),
            };
            if button.clicked()
                && let Ok(path) = &start
            {
                self.message = Some(open_in_browser(path));
            }
        });
        match &self.message {
            Some(Ok(m)) => {
                ui.label(RichText::new(m).weak().small());
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().warn_fg_color, e);
            }
            None => {}
        }
        if let Some(dir) = files.dir(id) {
            let who = match files.store() {
                SiteStore::Engine(_) => "saved by the receiver in",
                SiteStore::Own(_) => "copied for the browser to",
            };
            ui.label(
                RichText::new(format!("Files {who} {}", dir.display()))
                    .weak()
                    .small(),
            );
        }
        if files.pending() > 0 {
            ui.label(
                RichText::new(format!(
                    "{} file(s) wait for their service's id before they are saved",
                    files.pending()
                ))
                .weak()
                .small(),
            );
        }
        if files.failed > 0 {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!(
                    "{} file(s) could not be saved: {}",
                    files.failed,
                    files.last_error.as_deref().unwrap_or("?")
                ),
            );
        }

        ui.separator();
        let start_name = site.start_page().map(|(p, _)| p.to_string());
        // The path gets the width the type and size columns leave (long paths are cut,
        // in full on hover).
        let path_width = (ui.available_width() - 250.0).max(120.0);
        egui::ScrollArea::vertical()
            .id_salt("website_files")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new(("website_grid", id))
                    .num_columns(3)
                    .striped(true)
                    .spacing([12.0, 2.0])
                    .show(ui, |ui| {
                        for path in site.paths() {
                            let Some(file) = site.get(path) else { continue };
                            let mut text = RichText::new(path).monospace();
                            if start_name.as_deref() == Some(path) {
                                text = text.strong();
                            }
                            ui.scope(|ui| {
                                ui.set_width(path_width);
                                ui.add(egui::Label::new(text).truncate());
                            })
                            .response
                            .on_hover_text(path);
                            ui.label(RichText::new(&file.mime).weak());
                            ui.label(RichText::new(fmt_bytes(file.data.len() as u64)).monospace());
                            ui.end_row();
                        }
                    });
            });
    }
}

/// Hand the page to the system (it opens in the default browser). Rust note:
/// `open::that_detached` starts the browser without waiting for it, so the GUI does not
/// freeze while the browser runs.
fn open_in_browser(path: &Path) -> Result<String, String> {
    // A data directory given as a relative path is relative to this program's working
    // directory, which the browser does not share.
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    open::that_detached(&path)
        .map(|()| format!("Opened {}", path.display()))
        .map_err(|e| format!("Cannot open {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdrm_data::DataEvent;

    #[test]
    fn start_page_needs_an_html_file_on_disk() {
        let base = std::env::temp_dir().join(format!("decdrm-gui-start-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut files = SiteFiles::new(SiteStore::Own(base.clone()));
        let mut data = DataServices::default();
        let services = [ServiceView {
            short_id: 0,
            service_id: 0xABCDEF,
            ..ServiceView::default()
        }];
        // As `RxSession::poll` does: note website files, update the model, write.
        let deliver = |data: &mut DataServices, files: &mut SiteFiles, event: DataEvent| {
            if let DataEvent::WebsiteFile { path, .. } = &event {
                files.received(0, path);
            }
            data.apply(0, &event, None);
            files.flush(data, &services);
        };
        let file = |path: &str| DataEvent::WebsiteFile {
            path: path.into(),
            mime: "text/html".into(),
            data: b"<html>".to_vec(),
        };
        let index = |path: &str| DataEvent::WebsiteIndex { path: path.into() };
        let start = |data: &DataServices, files: &SiteFiles| {
            let site = &data.get(0).expect("service 0").website;
            start_page_file(site, files, 0)
        };
        deliver(&mut data, &mut files, file("news.html"));
        let why = start(&data, &files).unwrap_err();
        assert!(why.contains("No start page"), "{why}");
        // A signalled start page that is not HTML is never opened.
        deliver(&mut data, &mut files, index("run.exe"));
        deliver(&mut data, &mut files, file("run.exe"));
        let why = start(&data, &files).unwrap_err();
        assert!(why.contains("not an HTML file"), "{why}");
        deliver(&mut data, &mut files, index("index.html"));
        let why = start(&data, &files).unwrap_err();
        assert!(why.contains("not been received"), "{why}");
        deliver(&mut data, &mut files, file("index.html"));
        assert_eq!(
            start(&data, &files).unwrap(),
            base.join("ABCDEF").join("index.html")
        );
        std::fs::remove_dir_all(&base).unwrap();
    }
}
