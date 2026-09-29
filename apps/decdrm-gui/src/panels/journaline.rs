//! Journaline browser: menus, text pages and lists, with back / home navigation.

use super::placeholder;
use crate::data::{DataServices, choose_service, list_rows};
use decdrm_data::journaline::{JournalineBrowser, NmlBody};
use eframe::egui::{self, RichText, Ui};

/// Viewer state: which Journaline service is shown.
#[derive(Default)]
pub struct JournalineView {
    service: Option<u8>,
}

/// Navigation the user asked for in this frame.
enum Nav {
    Follow(usize),
    Back,
    Home,
    Open(u16),
}

/// Title of page `id`, or its object id while it has not been received.
fn page_title(browser: &JournalineBrowser, id: u16) -> String {
    browser
        .get(id)
        .map(|p| p.title.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| format!("0x{id:04X}"))
}

impl JournalineView {
    /// Prefer service `short_id` (shown if it has Journaline pages).
    pub fn focus(&mut self, short_id: u8) {
        self.service = Some(short_id);
    }

    pub fn show(&mut self, ui: &mut Ui, data: &mut DataServices) {
        let ids = data.journaline_ids();
        self.service = choose_service(self.service, &ids);
        let Some(id) = self.service else {
            placeholder(ui, "No Journaline pages received yet.");
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
        let Some(service) = data.get_mut(id) else {
            return;
        };
        let browser = &mut service.journaline;

        // Rust note: drawing borrows the browser immutably (page texts, menu entries),
        // so a click is only recorded as a `Nav` value and applied after drawing.
        let mut nav = None;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(browser.path().len() > 1, egui::Button::new("◀ Back"))
                .clicked()
            {
                nav = Some(Nav::Back);
            }
            if ui.button("Home").clicked() {
                nav = Some(Nav::Home);
            }
            ui.label(RichText::new(format!("{} pages", browser.len())).weak());
            ui.separator();
            // Breadcrumbs: every level of the path, clickable except the last.
            let path = browser.path().to_vec();
            for (depth, &page) in path.iter().enumerate() {
                if depth > 0 {
                    ui.label(RichText::new("›").weak());
                }
                let title = page_title(browser, page);
                if depth + 1 == path.len() {
                    ui.label(RichText::new(title).strong());
                } else if ui.link(title).clicked() {
                    nav = Some(Nav::Open(page));
                }
            }
        });
        ui.separator();

        egui::ScrollArea::vertical()
            .id_salt("journaline_page")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let current = browser.current_id();
                match browser.current() {
                    None => placeholder(ui, &format!("Waiting for page 0x{current:04X}…")),
                    Some(page) => {
                        ui.label(RichText::new(page.title.trim()).heading());
                        if browser.was_updated(current) {
                            ui.label(RichText::new("updated").weak().italics());
                        }
                        ui.add_space(4.0);
                        match &page.body {
                            NmlBody::Menu(_) => {
                                for (i, entry) in browser.menu_entries(current).iter().enumerate() {
                                    let text = if entry.available {
                                        RichText::new(&entry.text)
                                    } else {
                                        RichText::new(format!("{} (not received yet)", entry.text))
                                            .weak()
                                    };
                                    if ui
                                        .add_enabled(
                                            entry.available,
                                            egui::Button::new(text).wrap(),
                                        )
                                        .clicked()
                                    {
                                        nav = Some(Nav::Follow(i));
                                    }
                                }
                            }
                            NmlBody::PlainText(text) => {
                                ui.add(egui::Label::new(text).wrap());
                            }
                            NmlBody::List(items) => {
                                egui::Grid::new("journaline_list")
                                    .striped(true)
                                    .show(ui, |ui| {
                                        for row in list_rows(items) {
                                            for cell in row {
                                                ui.add(egui::Label::new(cell).wrap());
                                            }
                                            ui.end_row();
                                        }
                                    });
                            }
                            NmlBody::TitleOnly => {}
                        }
                    }
                }
            });

        match nav {
            Some(Nav::Follow(i)) => {
                browser.follow(i);
            }
            Some(Nav::Back) => {
                browser.back();
            }
            Some(Nav::Home) => browser.home(),
            Some(Nav::Open(page)) => {
                // Going to an ancestor: pop back to it.
                while browser.current_id() != page && browser.back() {}
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdrm_data::journaline::{MenuItem, NmlObject};

    #[test]
    fn titles_fall_back_to_the_object_id() {
        let mut b = JournalineBrowser::new();
        assert_eq!(page_title(&b, 0x12), "0x0012");
        b.insert(NmlObject::menu(
            0,
            " News ",
            vec![MenuItem::new(0x12, "Sport")],
        ));
        assert_eq!(page_title(&b, 0), "News");
        b.insert(NmlObject::title_only(0x12, "  "));
        assert_eq!(page_title(&b, 0x12), "0x0012", "blank titles use the id");
    }
}
