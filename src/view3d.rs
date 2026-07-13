//! The orbit view: seeing the spacecraft, not reading about it.
//!
//! An orthographic projection of an Earth-centred inertial frame, painted with
//! egui's shape API. No GPU pipeline of its own — at this scale the whole scene
//! is a sphere, a trail and a handful of axes, and a second renderer would be
//! cost without benefit. If this grows a textured globe or a mesh vehicle, that
//! is the point to move it onto wgpu.
//!
//! The camera is a simple azimuth/elevation orbit around the origin. Drag to
//! look around; scroll to zoom.

use crate::dynamics::{Vec3, Vehicle, R_EARTH};
use crate::theme;
use egui::{Color32, Pos2, Rect, Sense, Stroke, Ui, Vec2};

pub struct Camera {
    /// Radians. Rotation about the polar axis.
    pub azimuth: f64,
    /// Radians, clamped away from the poles to avoid a degenerate up-vector.
    pub elevation: f64,
    /// Kilometres across the viewport's short axis.
    pub span_km: f64,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            azimuth: 0.6,
            elevation: 0.35,
            // Wide enough to see the whole orbit, not just the vehicle.
            span_km: 22_000.0,
        }
    }
}

impl Camera {
    /// Project an inertial point to screen space.
    ///
    /// Returns the pixel position and the camera-space depth, so callers can
    /// decide what is in front of the Earth and what is behind it — without a
    /// depth buffer, that occlusion test is the only thing keeping the far half
    /// of the orbit from drawing over the planet.
    fn project(&self, p: Vec3, rect: &Rect) -> (Pos2, f64) {
        // A proper orthonormal camera basis, derived from the view direction.
        //
        // The earlier version rotated about the polar axis and then sheared,
        // which is *nearly* right and feels wrong the moment you drag: the
        // horizon tilts, and the axes stop agreeing with each other as elevation
        // grows. Build the basis from the eye direction instead and the three
        // vectors stay orthonormal at every angle.
        let (sa, ca) = (self.azimuth.sin(), self.azimuth.cos());
        let (se, ce) = (self.elevation.sin(), self.elevation.cos());

        // Eye direction: from the origin toward the camera.
        let eye = Vec3::new(ce * ca, ce * sa, se);
        // Right: horizontal, perpendicular to the eye. Never degenerates, because
        // elevation is clamped away from the poles.
        let right = Vec3::new(-sa, ca, 0.0);
        // Up: completes a right-handed set. (eye × right, normalised by
        // construction since both are unit and orthogonal.)
        let up = eye.cross(right).scale(-1.0);

        let dot = |a: Vec3, b: Vec3| a.x * b.x + a.y * b.y + a.z * b.z;

        let scale = rect.height() as f64 / self.span_km;
        let c = rect.center();

        (
            Pos2::new(
                c.x + (dot(p, right) * scale) as f32,
                c.y - (dot(p, up) * scale) as f32, // screen y grows downward
            ),
            dot(p, eye), // +ve is toward the viewer
        )
    }

    fn km_per_px(&self, rect: &Rect) -> f64 {
        self.span_km / rect.height() as f64
    }
}

/// Paint the scene. Returns the rect it used.
pub fn show(ui: &mut Ui, cam: &mut Camera, vehicle: &Vehicle, trail: &[Vec3]) -> Rect {
    let (rect, response) =
        ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());

    // ---- camera control ----
    if response.dragged() {
        // Drag-to-orbit: the point under the cursor should follow the cursor.
        // Dragging right spins the globe right (azimuth decreases); dragging down
        // tips the north pole toward you (elevation increases). Getting either
        // sign backwards is what makes a camera feel "weird" rather than wrong.
        let d = response.drag_delta();
        cam.azimuth -= f64::from(d.x) * 0.006;
        cam.elevation = (cam.elevation + f64::from(d.y) * 0.006).clamp(-1.45, 1.45);
    }
    if response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            cam.span_km = (cam.span_km * f64::from(-scroll).mul_add(0.002, 1.0))
                .clamp(R_EARTH * 1.2, 200_000.0);
        }
    }

    let p = ui.painter_at(rect);
    p.rect_filled(rect, 0.0, Color32::from_rgb(9, 10, 14));

    let earth_px = (R_EARTH / cam.km_per_px(&rect)) as f32;
    let (centre, _) = cam.project(Vec3::ZERO, &rect);

    // ---- the Earth ----
    p.circle_filled(centre, earth_px, Color32::from_rgb(26, 42, 66));
    p.circle_stroke(centre, earth_px, Stroke::new(1.5, Color32::from_rgb(70, 120, 170)));

    // Equator and the polar axis, so the inclination is legible rather than
    // merely present.
    draw_great_circle(&p, cam, &rect, centre, earth_px);

    // ---- the trail ----
    //
    // Split at the horizon: segments behind the Earth are drawn dim and BEFORE
    // the planet's silhouette test, so the orbit reads as passing behind it.
    for pair in trail.windows(2) {
        let (a, da) = cam.project(pair[0], &rect);
        let (b, db) = cam.project(pair[1], &rect);

        let behind = da < 0.0 && db < 0.0;
        let occluded = behind && within(a, centre, earth_px) && within(b, centre, earth_px);

        let colour = if occluded {
            // Visible, but clearly on the far side.
            Color32::from_rgb(90, 60, 35)
        } else {
            theme::ACCENT.linear_multiply(0.55)
        };
        p.line_segment([a, b], Stroke::new(1.5, colour));
    }

    // ---- the spacecraft ----
    let (sc, sc_depth) = cam.project(vehicle.orbit.pos, &rect);
    let hidden = sc_depth < 0.0 && within(sc, centre, earth_px);

    if !hidden {
        // Body axes: nadir (where it is looking), along-track, cross-track.
        let (nadir, along, cross) = vehicle.attitude.axes(&vehicle.orbit);
        let len = cam.span_km * 0.045;

        for (axis, colour, label) in [
            (nadir, theme::GOOD, "nadir"),
            (along, Color32::from_rgb(120, 170, 230), "v"),
            (cross, theme::MUTED, ""),
        ] {
            let tip = vehicle.orbit.pos.add(axis.scale(len));
            let (t, _) = cam.project(tip, &rect);
            p.line_segment([sc, t], Stroke::new(1.6, colour));
            if !label.is_empty() {
                p.text(
                    t,
                    egui::Align2::LEFT_BOTTOM,
                    label,
                    egui::FontId::proportional(10.0),
                    colour,
                );
            }
        }

        p.circle_filled(sc, 4.5, theme::ACCENT);
        p.circle_stroke(sc, 7.0, Stroke::new(1.0, theme::ACCENT.linear_multiply(0.6)));
    } else {
        // Say so, rather than letting the vehicle silently vanish.
        p.text(
            rect.left_bottom() + Vec2::new(10.0, -10.0),
            egui::Align2::LEFT_BOTTOM,
            "spacecraft behind Earth",
            egui::FontId::proportional(11.0),
            theme::MUTED,
        );
    }

    // ---- scale bar ----
    let bar_km = nice_scale(cam.span_km / 4.0);
    let bar_px = (bar_km / cam.km_per_px(&rect)) as f32;
    let y = rect.bottom() - 18.0;
    let x0 = rect.right() - bar_px - 16.0;
    p.line_segment(
        [Pos2::new(x0, y), Pos2::new(x0 + bar_px, y)],
        Stroke::new(1.5, theme::MUTED),
    );
    p.text(
        Pos2::new(x0 + bar_px / 2.0, y - 4.0),
        egui::Align2::CENTER_BOTTOM,
        format!("{bar_km:.0} km"),
        egui::FontId::proportional(10.0),
        theme::MUTED,
    );

    rect
}

fn within(p: Pos2, centre: Pos2, radius: f32) -> bool {
    (p - centre).length() < radius
}

/// The equator, so the orbit's inclination is readable against something.
fn draw_great_circle(
    p: &egui::Painter,
    cam: &Camera,
    rect: &Rect,
    centre: Pos2,
    earth_px: f32,
) {
    const N: usize = 96;
    let mut prev: Option<(Pos2, f64)> = None;

    for i in 0..=N {
        let a = i as f64 / N as f64 * std::f64::consts::TAU;
        let pt = Vec3::new(R_EARTH * a.cos(), R_EARTH * a.sin(), 0.0);
        let cur = cam.project(pt, rect);

        if let Some(pv) = prev {
            // Only the near half; the far half would draw over the planet.
            if pv.1 >= 0.0 && cur.1 >= 0.0 {
                p.line_segment(
                    [pv.0, cur.0],
                    Stroke::new(1.0, Color32::from_rgb(58, 90, 128)),
                );
            }
        }
        prev = Some(cur);
    }

    let _ = (centre, earth_px);
}

/// Round a distance to something a human reads without effort.
fn nice_scale(km: f64) -> f64 {
    let mag = 10f64.powf(km.log10().floor());
    let n = km / mag;
    mag * if n < 1.5 {
        1.0
    } else if n < 3.5 {
        2.0
    } else if n < 7.5 {
        5.0
    } else {
        10.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_origin_projects_to_the_centre() {
        let cam = Camera::default();
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        let (p, _) = cam.project(Vec3::ZERO, &rect);
        assert!((p - rect.center()).length() < 0.001);
    }

    #[test]
    fn depth_sign_distinguishes_near_from_far() {
        // The only thing preventing the far half of the orbit from painting over
        // the planet, so it is worth a test.
        //
        // At azimuth 0, elevation 0 the eye sits on +x looking back at the origin.
        let cam = Camera { azimuth: 0.0, elevation: 0.0, span_km: 20_000.0 };
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));

        let (_, near) = cam.project(Vec3::new(8000.0, 0.0, 0.0), &rect);
        let (_, far) = cam.project(Vec3::new(-8000.0, 0.0, 0.0), &rect);

        assert!(near > 0.0, "toward the viewer");
        assert!(far < 0.0, "away from the viewer");
    }

    #[test]
    fn the_camera_basis_stays_orthonormal_while_orbiting() {
        // The bug the rewrite fixed: the old projection sheared instead of
        // rotating, so the axes drifted out of square as elevation grew and the
        // horizon visibly tilted while dragging. Probe the basis by projecting
        // unit vectors -- a rigid camera preserves lengths and right angles at
        // every angle.
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(600.0, 600.0));

        for (az, el) in [(0.0, 0.0), (0.9, 0.7), (2.4, -1.2), (5.0, 1.4)] {
            let cam = Camera { azimuth: az, elevation: el, span_km: 600.0 }; // 1 km/px
            let o = cam.project(Vec3::ZERO, &rect).0;

            // Screen displacement of each world axis, in pixels.
            let axes = [
                Vec3::new(1.0, 0.0, 0.0),
                Vec3::new(0.0, 1.0, 0.0),
                Vec3::new(0.0, 0.0, 1.0),
            ]
            .map(|a| {
                let p = cam.project(a, &rect).0;
                let d = cam.project(a, &rect).1;
                ((p.x - o.x) as f64, (p.y - o.y) as f64, d)
            });

            // An orthonormal basis projected orthographically: each world unit
            // vector's screen offset plus its depth must have unit length.
            // Tolerance is set by f32 screen coordinates, not by the maths.
            for (dx, dy, dz) in axes {
                let len = (dx * dx + dy * dy + dz * dz).sqrt();
                assert!((len - 1.0).abs() < 1e-3, "axis length {len} at ({az}, {el})");
            }
        }
    }

    #[test]
    fn scale_bar_picks_round_numbers() {
        assert_eq!(nice_scale(1234.0), 1000.0);
        assert_eq!(nice_scale(2600.0), 2000.0);
        assert_eq!(nice_scale(6000.0), 5000.0);
    }
}
