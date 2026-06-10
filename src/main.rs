#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod pdf_engine;
mod annotations;
mod ui;

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("IKRA — Professional PDF Editor")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([800.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "IKRA PDF Editor",
        native_options,
        Box::new(|cc| Ok(Box::new(app::IkraApp::new(cc)))),
    )
}