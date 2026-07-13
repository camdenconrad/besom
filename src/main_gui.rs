//! besom — a ground station for cFS, on a clock we own.

#![windows_subsystem = "windows"]

use besom::gui::Besom;
use besom::run::{self, Config};
use besom::session::Session;

fn main() -> eframe::Result {
    let cfg = Config {
        cfs_dir: run::default_cfs_dir(),
        step_sock: "/tmp/besom.sock".into(),
        ticks: 0, // the operator grants time; the session does not run a script
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("besom")
            .with_app_id("besom")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "besom",
        options,
        Box::new(|cc| Ok(Box::new(Besom::new(cc, Session::start(cfg))))),
    )
}
