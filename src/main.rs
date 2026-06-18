#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod pdf_engine;
mod annotations;
mod ui;
mod tab;
mod layered_view;
mod text_index;
mod bookmarks;

fn main() -> eframe::Result<()> {
    // ── Linux Wayland / KDE Dolphin Fix ──────────────────────────────────────
    // Force the X11 backend (XWayland) when running on Linux.
    // This allows mature, rock-solid drag-and-drop negotiation with Dolphin.
    #[cfg(target_os = "linux")]
    {
        if std::env::var("WINIT_UNIX_BACKEND").is_err() {
            unsafe { std::env::set_var("WINIT_UNIX_BACKEND", "x11"); }
        }
    }

    // Load icon for runtime window taskbars (Linux/Windows)
    let icon_data = image::load_from_memory(include_bytes!("../assets/ikra.png"))
        .ok()
        .map(|img| {
            let rgba = img.into_rgba8();
            let (width, height) = rgba.dimensions();
            egui::IconData { rgba: rgba.into_raw(), width, height }
        });

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
        .with_title("IKRA — Professional PDF Editor")
        .with_icon(icon_data.unwrap_or_default())
        .with_inner_size([1400.0, 900.0])
        .with_min_inner_size([800.0, 600.0])
        .with_transparent(false)
        .with_drag_and_drop(true),
        ..Default::default()
    };

    let initial_file = std::env::args().nth(1);

    eframe::run_native(
        "IKRA PDF Editor",
        native_options,
        Box::new(|cc| Ok(Box::new(app::IkraApp::new(cc, initial_file)))),
    )
}
