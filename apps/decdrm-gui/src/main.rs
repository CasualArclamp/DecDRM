fn main() {
    let _ = eframe::egui::Context::default();
    let _ = egui_plot::Plot::new("x");
    let _ = rfd::FileDialog::new();
    let _ = image::ImageFormat::Png;
    let _ = toml::to_string(&1);
}
