//! besom — a ground station for cFS, on a clock we own.

#![windows_subsystem = "windows"]

use besom::gui::Besom;
use besom::run::{self, Config};
use besom::session::Session;

/// The window/dock icon, rasterised from the SVG at build time.
fn icon() -> egui::IconData {
    let png = include_bytes!("../assets/besom-256.png");
    let img = image::load_from_memory(png).expect("besom icon").into_rgba8();
    let (width, height) = img.dimensions();
    egui::IconData { rgba: img.into_raw(), width, height }
}

fn main() -> eframe::Result {
    let cfg = Config {
        cfs_dir: run::default_cfs_dir(),
        step_sock: "/tmp/besom.sock".into(),
        ticks: 0, // the operator grants time; the session does not run a script
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("besom")
            .with_app_id("besom") // Wayland app_id -> dock icon match
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([900.0, 600.0])
            .with_icon(icon())
            // Rune advertises no server-side decorations, so under the `rune`
            // feature we draw our own title bar. Elsewhere, keep the normal ones.
            .with_decorations(cfg!(not(feature = "rune"))),
        ..Default::default()
    };

    eframe::run_native(
        "besom",
        options,
        Box::new(|cc| Ok(Box::new(Besom::new(cc, Session::start(cfg))))),
    )
}
