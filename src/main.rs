//! Starter — a small launcher that can start and stop groups of programs.
//!
//! Usage: `starter [path/to/launcher.toml]`

// No console window behind the GUI in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod fonts;
mod hotkey;
mod processes;
mod theme;

fn main() -> eframe::Result<()> {
    let config_path = config::config_path_from_args();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Starter")
            .with_inner_size([880.0, 600.0])
            .with_min_inner_size([620.0, 400.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Starter",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            // Built here, on the thread that runs the event loop, because that
            // is where the operating system expects hotkeys to be reserved.
            let hotkeys = hotkey::Registry::global(&cc.egui_ctx);
            Ok(Box::new(app::LauncherApp::new(config_path, hotkeys)))
        }),
    )
}
