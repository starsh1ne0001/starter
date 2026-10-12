//! Starter — a small launcher that can start and stop groups of programs.
//!
//! Usage: `starter [path/to/launcher.toml]`

// No console window behind the GUI in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod console;
mod fonts;
mod hotkey;
mod processes;
mod theme;
mod tray;

fn main() -> eframe::Result<()> {
    // First thing, before a window exists: the console Windows may have opened
    // for this program is given back, so that a launcher started from a file
    // manager does not leave a black window standing behind its own.
    console::release_ours();

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
            // is where the operating system expects hotkeys to be reserved — and
            // where the icon of the notification area has to be put up for its
            // clicks to reach this program.
            let hotkeys = hotkey::Registry::global(&cc.egui_ctx);
            let tray = tray::Tray::install();
            Ok(Box::new(app::LauncherApp::new(config_path, hotkeys, tray)))
        }),
    )
}
