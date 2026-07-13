//! The ground station window.
//!
//! Three panes: the transport (which owns simulated time), the telemetry
//! streams, and the event log. The transport is the unusual one — because Besom
//! grants every tick, PAUSE genuinely stops the spacecraft rather than merely
//! stopping the display, and STEP advances it by an exact number of ticks.

use crate::evs::Severity;
use crate::theme;
use crate::view3d::{self, Camera};
use crate::session::{Cmd, Session, State};
use eframe::egui;

/// Well-known message ids, so the operator sees names rather than hex.
fn stream_name(msg_id: u16) -> &'static str {
    if msg_id == crate::fsw::STATE_TLM_MID {
        return "BESOM_IO state";
    }
    match msg_id {
        0x0800 => "cFE ES",
        0x0801 => "cFE EVS",
        0x0803 => "cFE SB",
        0x0804 => "cFE TBL",
        0x0805 => "cFE TIME",
        0x0808 => "EVS events",
        0x0880 => "TO_LAB",
        0x0883 => "SAMPLE_APP",
        0x0884 => "CI_LAB",
        0x08a4 => "CS",
        0x08ad => "HS",
        _ => "",
    }
}

pub struct Besom {
    session: Session,
    state: State,
    /// Command builder: MID / function code, in hex as the operator thinks of them.
    cmd_mid: String,
    cmd_fn: String,
    step_ticks: u32,
    warp: u32,
    camera: Camera,
}

impl Besom {
    pub fn new(cc: &eframe::CreationContext<'_>, session: Session) -> Self {
        theme::apply(&cc.egui_ctx);
        let me = Self {
            session,
            state: State::default(),
            cmd_mid: "1882".into(), // SAMPLE_APP
            cmd_fn: "0".into(),     // NOOP
            step_ticks: 100,
            warp: 50,
            camera: Camera::default(),
        };
        // Apply the default warp so a fresh window shows an orbit progressing,
        // not a dot that has barely moved.
        me.session.send(Cmd::Warp(me.warp));
        me
    }
}

impl eframe::App for Besom {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.state = self.session.snapshot();

        // Simulated time only moves when we grant it, but the *display* is still
        // a real-time view of a live process, so keep repainting.
        ctx.request_repaint_after(std::time::Duration::from_millis(100));

        #[cfg(feature = "rune")]
        crate::chrome::title_bar(ctx, "besom — cFS ground station");

        egui::TopBottomPanel::top("transport").show(ctx, |ui| self.transport(ui));

        // Telemetry and events sit to the side. The orbit gets the middle of the
        // window, because the whole point is to SEE the spacecraft.
        egui::SidePanel::right("data").resizable(true).default_width(430.0).show(ctx, |ui| {
            self.telemetry(ui);
            ui.separator();
            ui.heading("Events");
            self.event_log(ui);
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::none())
            .show(ctx, |ui| {
                view3d::show(ui, &mut self.camera, &self.state.vehicle, &self.state.trail);
            });
    }

    /// Kill cFS on the way out. Without this the process exits before the worker
    /// thread unwinds, and the spacecraft is orphaned still holding UDP 2234 —
    /// so the *next* launch gets no telemetry and looks broken.
    fn on_exit(&mut self) {
        self.session.shutdown();
    }
}

impl Besom {
    fn transport(&mut self, ui: &mut egui::Ui) {
        if let Some(err) = &self.state.error {
            ui.colored_label(egui::Color32::from_rgb(220, 90, 90), format!("cFS: {err}"));
            return;
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let alive = self.state.alive;
            ui.add_enabled_ui(alive, |ui| {
                if self.state.running {
                    if ui.button("⏸ Pause").clicked() {
                        self.session.send(Cmd::Pause);
                    }
                } else if ui.button("▶ Play").clicked() {
                    self.session.send(Cmd::Play);
                }

                // The move a wall-clock ground station cannot make.
                if ui.button(format!("⏭ Step {}", self.step_ticks)).clicked() {
                    self.session.send(Cmd::StepTicks(self.step_ticks));
                }
                ui.add(egui::DragValue::new(&mut self.step_ticks).range(1..=10_000).suffix(" ticks"));

                ui.label("warp");
                if ui
                    .add(egui::DragValue::new(&mut self.warp).range(1..=500).suffix("×"))
                    .changed()
                {
                    self.session.send(Cmd::Warp(self.warp));
                }
            });

            ui.separator();

            // Mission time is simulated: it is frozen while paused, because the
            // spacecraft is frozen, not merely un-rendered.
            ui.monospace(format!("MET {:9.2}s", self.state.sim_secs));
            ui.label(if !alive {
                "booting…"
            } else if self.state.running {
                "running"
            } else {
                "time frozen"
            });

            ui.separator();

            // Where the spacecraft actually is. These numbers move because the
            // vehicle is being propagated on the ticks we grant cFS.
            let v = &self.state.vehicle;
            let (lat, lon) = v.orbit.subpoint_deg();
            ui.monospace(format!("ALT {:6.1} km", v.orbit.altitude_km()));
            ui.monospace(format!("VEL {:5.2} km/s", v.orbit.speed_kms()));
            ui.monospace(format!("{lat:+05.1}° {lon:+06.1}°"));

            ui.separator();
            ui.monospace(format!("{} packets", self.state.packets));

            let gaps: u64 = self.state.streams.values().map(|s| s.gaps).sum();
            if gaps > 0 {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 90, 90),
                    format!("{gaps} dropped"),
                );
            }
        });
        ui.add_space(4.0);
    }

    fn telemetry(&mut self, ui: &mut egui::Ui) {
        ui.heading("Telemetry");
        ui.add_space(4.0);

        if self.state.streams.is_empty() {
            ui.weak("No downlink yet. Press Play to grant the spacecraft some time.");
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("tlm").num_columns(6).striped(true).show(ui, |ui| {
                for h in ["MsgId", "Stream", "Count", "Last MET", "Bytes", "Dropped"] {
                    ui.strong(h);
                }
                ui.end_row();

                for (mid, s) in &self.state.streams {
                    ui.monospace(format!("{mid:04x}"));
                    ui.label(stream_name(*mid));
                    ui.monospace(s.count.to_string());
                    ui.monospace(format!("{:.2}s", s.last_time));
                    ui.monospace(s.len.to_string());
                    if s.gaps > 0 {
                        ui.colored_label(egui::Color32::from_rgb(220, 90, 90), s.gaps.to_string());
                    } else {
                        ui.weak("—");
                    }
                    ui.end_row();
                }
            });
        });

        ui.add_space(8.0);
        ui.separator();
        self.closed_loop(ui);
        ui.separator();
        ui.add_space(4.0);
        self.command_builder(ui);
    }

    /// What the FLIGHT SOFTWARE thinks, beside what we sent it.
    ///
    /// The state on the left has travelled through cFS: Besom -> besom_io ->
    /// software bus -> TO_LAB -> downlink -> here. If the two columns ever
    /// disagree, the loop is broken -- and being able to SEE that is worth more
    /// than either number alone.
    fn closed_loop(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.strong("Closed loop");
            ui.weak("state as reported BY the flight software");
        });
        ui.add_space(2.0);

        let Some(f) = self.state.fsw else {
            ui.weak("waiting for BESOM_IO telemetry…");
            return;
        };

        let truth = &self.state.vehicle;
        let (lat, lon) = truth.orbit.subpoint_deg();

        egui::Grid::new("loop").num_columns(4).striped(true).show(ui, |ui| {
            for h in ["", "flight software", "besom", "Δ"] {
                ui.strong(h);
            }
            ui.end_row();

            for (name, fsw_v, truth_v, unit) in [
                ("Altitude", f.alt_km, truth.orbit.altitude_km(), "km"),
                ("Latitude", f.lat_deg, lat, "°"),
                ("Longitude", f.lon_deg, lon, "°"),
            ] {
                let delta = fsw_v - truth_v;
                ui.label(name);
                ui.monospace(format!("{fsw_v:9.3} {unit}"));
                ui.monospace(format!("{truth_v:9.3} {unit}"));

                // A non-zero delta is not noise -- both sides are the same f64.
                // It means cFS is reporting stale state, i.e. it missed a tick.
                if delta.abs() < 1e-9 {
                    ui.colored_label(theme::GOOD, "0");
                } else {
                    ui.colored_label(theme::ACCENT, format!("{delta:+.3}"));
                }
                ui.end_row();
            }
        });

        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.weak(format!("cFS accepted {} state updates", f.rx_count));
            if f.rx_err_count > 0 {
                ui.colored_label(theme::BAD, format!("{} malformed", f.rx_err_count));
            }
        });
    }

    fn command_builder(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Command");
            ui.label("MsgId 0x");
            ui.add(egui::TextEdit::singleline(&mut self.cmd_mid).desired_width(48.0));
            ui.label("fn");
            ui.add(egui::TextEdit::singleline(&mut self.cmd_fn).desired_width(32.0));

            let parsed = u16::from_str_radix(self.cmd_mid.trim(), 16)
                .ok()
                .zip(self.cmd_fn.trim().parse::<u8>().ok());

            ui.add_enabled_ui(parsed.is_some() && self.state.alive, |ui| {
                if ui.button("Send").clicked() {
                    if let Some((msg_id, fn_code)) = parsed {
                        self.session.send(Cmd::Send { msg_id, fn_code, payload: Vec::new() });
                    }
                }
            });

            if parsed.is_none() {
                ui.weak("MsgId is hex, fn is decimal");
            } else {
                ui.weak("e.g. 1882 / 0 = SAMPLE_APP NOOP");
            }
        });
    }

    fn event_log(&mut self, ui: &mut egui::Ui) {
        if self.state.events.is_empty() {
            ui.weak("The flight software has not said anything yet.");
            return;
        }

        egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
            for ev in &self.state.events {
                let colour = match ev.severity {
                    Severity::Critical | Severity::Error => egui::Color32::from_rgb(220, 90, 90),
                    Severity::Debug => egui::Color32::GRAY,
                    Severity::Info => ui.visuals().text_color(),
                };
                ui.horizontal_wrapped(|ui| {
                    ui.monospace(format!("{:<5}", ev.severity.label()));
                    ui.monospace(format!("{:<12}", ev.app));
                    ui.colored_label(colour, &ev.text);
                });
            }
        });
    }
}
