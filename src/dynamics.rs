//! Spacecraft dynamics: where the vehicle is and which way it is pointing.
//!
//! This is the piece that turns telemetry into a spacecraft. cFS's lab apps have
//! no vehicle model — they emit housekeeping into the void — so nothing on the
//! ground can *show* you anything. Besom propagates a real orbit and attitude on
//! the **same simulated clock** it grants to the flight software, so the picture
//! and the telemetry are the same run. Pause, and the spacecraft stops in the
//! sky exactly where the flight software stopped.
//!
//! The model is deliberately honest about what it is:
//!
//! * **Two-body gravity only.** No J2, no drag, no third bodies. For a LEO
//!   visualisation over minutes-to-hours this is right to within a few km, and
//!   every term you add is a term you have to validate. J2 is the first thing to
//!   add when that matters (it dominates real LEO perturbations).
//! * **Fixed-step RK4.** The step is the simulated tick, so the integration is
//!   reproducible: same ticks in, same trajectory out. A variable-step
//!   integrator would silently make the picture depend on host timing, which is
//!   the exact property this whole project exists to eliminate.

/// Earth's gravitational parameter, km³/s².
pub const MU_EARTH: f64 = 398_600.4418;
/// Equatorial radius, km.
pub const R_EARTH: f64 = 6378.137;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Vec3 {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0, z: 0.0 };

    pub fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    pub fn norm(self) -> f64 {
        (self.x * self.x + self.y * self.y + self.z * self.z).sqrt()
    }

    pub fn scale(self, k: f64) -> Self {
        Self::new(self.x * k, self.y * k, self.z * k)
    }

    pub fn add(self, o: Self) -> Self {
        Self::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }

    pub fn cross(self, o: Self) -> Self {
        Self::new(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }
}

/// Position and velocity in an Earth-centred inertial frame (km, km/s).
#[derive(Debug, Clone, Copy)]
pub struct Orbit {
    pub pos: Vec3,
    pub vel: Vec3,
}

impl Orbit {
    /// A circular orbit at `altitude_km`, inclined by `inclination_deg`.
    pub fn circular(altitude_km: f64, inclination_deg: f64) -> Self {
        let r = R_EARTH + altitude_km;
        let v = (MU_EARTH / r).sqrt(); // vis-viva, circular case
        let i = inclination_deg.to_radians();

        Self {
            pos: Vec3::new(r, 0.0, 0.0),
            // Velocity perpendicular to the radius, tilted out of the equator by
            // the inclination. At this epoch the ascending node is along +x.
            vel: Vec3::new(0.0, v * i.cos(), v * i.sin()),
        }
    }

    pub fn altitude_km(&self) -> f64 {
        self.pos.norm() - R_EARTH
    }

    pub fn speed_kms(&self) -> f64 {
        self.vel.norm()
    }

    /// Orbital period in seconds, from the current state's semi-major axis.
    pub fn period_secs(&self) -> f64 {
        let r = self.pos.norm();
        let energy = self.vel.norm().powi(2) / 2.0 - MU_EARTH / r;
        if energy >= 0.0 {
            return f64::INFINITY; // escape trajectory
        }
        let a = -MU_EARTH / (2.0 * energy);
        std::f64::consts::TAU * (a.powi(3) / MU_EARTH).sqrt()
    }

    /// Sub-satellite point (latitude, longitude in degrees) in an inertial frame.
    ///
    /// Note this ignores Earth's rotation, so the longitude is inertial rather
    /// than a true ground track. Honest simplification: rendering a rotating
    /// Earth needs a sidereal time model, which this does not yet have.
    pub fn subpoint_deg(&self) -> (f64, f64) {
        let r = self.pos.norm();
        let lat = (self.pos.z / r).asin().to_degrees();
        let lon = self.pos.y.atan2(self.pos.x).to_degrees();
        (lat, lon)
    }

    /// Advance by `dt` seconds of SIMULATED time using fixed-step RK4.
    pub fn step(&mut self, dt: f64) {
        let (k1p, k1v) = derivative(self.pos, self.vel);
        let (k2p, k2v) = derivative(
            self.pos.add(k1p.scale(dt / 2.0)),
            self.vel.add(k1v.scale(dt / 2.0)),
        );
        let (k3p, k3v) = derivative(
            self.pos.add(k2p.scale(dt / 2.0)),
            self.vel.add(k2v.scale(dt / 2.0)),
        );
        let (k4p, k4v) = derivative(self.pos.add(k3p.scale(dt)), self.vel.add(k3v.scale(dt)));

        let w = dt / 6.0;
        self.pos = self.pos.add(
            k1p.add(k2p.scale(2.0)).add(k3p.scale(2.0)).add(k4p).scale(w),
        );
        self.vel = self.vel.add(
            k1v.add(k2v.scale(2.0)).add(k3v.scale(2.0)).add(k4v).scale(w),
        );
    }
}

/// Two-body: acceleration is -mu * r / |r|^3.
fn derivative(pos: Vec3, vel: Vec3) -> (Vec3, Vec3) {
    let r = pos.norm();
    let a = pos.scale(-MU_EARTH / (r * r * r));
    (vel, a)
}

/// Attitude: a simple nadir-pointing model with a body spin.
///
/// Real attitude needs a quaternion, an inertia tensor and torques. This is the
/// honest minimum for *seeing* orientation: the vehicle points at the Earth and
/// rolls about that axis, which is what most LEO spacecraft actually do.
#[derive(Debug, Clone, Copy)]
pub struct Attitude {
    /// Roll about the nadir vector, radians.
    pub roll: f64,
    /// Roll rate, rad/s.
    pub roll_rate: f64,
}

impl Default for Attitude {
    fn default() -> Self {
        Self { roll: 0.0, roll_rate: 0.02 }
    }
}

impl Attitude {
    pub fn step(&mut self, dt: f64) {
        self.roll = (self.roll + self.roll_rate * dt) % std::f64::consts::TAU;
    }

    /// Body axes in the inertial frame: (nadir, along-track, cross-track).
    pub fn axes(&self, orbit: &Orbit) -> (Vec3, Vec3, Vec3) {
        let nadir = orbit.pos.scale(-1.0 / orbit.pos.norm());
        let h = orbit.pos.cross(orbit.vel); // orbit normal
        let cross = h.scale(1.0 / h.norm().max(1e-9));
        let along = cross.cross(nadir);
        (nadir, along, cross)
    }
}

/// The vehicle: orbit + attitude, stepped together on the simulated clock.
#[derive(Debug, Clone, Copy)]
pub struct Vehicle {
    pub orbit: Orbit,
    pub attitude: Attitude,
    /// Simulated seconds propagated so far. Should track the flight software's
    /// clock exactly -- if it does not, the picture is lying.
    pub elapsed: f64,
}

impl Default for Vehicle {
    fn default() -> Self {
        // ISS-like: 420 km, 51.6 deg. A recognisable orbit.
        Self {
            orbit: Orbit::circular(420.0, 51.6),
            attitude: Attitude::default(),
            elapsed: 0.0,
        }
    }
}

impl Vehicle {
    pub fn step(&mut self, dt: f64) {
        self.orbit.step(dt);
        self.attitude.step(dt);
        self.elapsed += dt;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_circular_orbit_stays_circular() {
        // The integrator's honesty check: after a full period the altitude must
        // come back. A drifting radius means the propagator is leaking energy.
        let mut o = Orbit::circular(420.0, 51.6);
        let alt0 = o.altitude_km();
        let period = o.period_secs();

        let dt = 0.01; // the simulated tick
        let steps = (period / dt) as usize;
        for _ in 0..steps {
            o.step(dt);
        }

        assert!(
            (o.altitude_km() - alt0).abs() < 0.1,
            "altitude drifted {:.3} km over one orbit",
            o.altitude_km() - alt0
        );
    }

    #[test]
    fn returns_to_its_starting_point_after_one_period() {
        let mut o = Orbit::circular(420.0, 51.6);
        let start = o.pos;
        let period = o.period_secs();

        let dt = 0.01;
        for _ in 0..(period / dt) as usize {
            o.step(dt);
        }

        let err = o.pos.add(start.scale(-1.0)).norm();
        assert!(err < 5.0, "closed the orbit to within {err:.2} km");
    }

    #[test]
    fn leo_period_is_about_ninety_minutes() {
        // Sanity against a number every orbital mechanic knows by heart.
        let mins = Orbit::circular(420.0, 51.6).period_secs() / 60.0;
        assert!((mins - 92.8).abs() < 1.0, "period was {mins:.1} min");
    }

    #[test]
    fn propagation_is_reproducible() {
        // The whole point: same ticks in, same trajectory out.
        let run = || {
            let mut v = Vehicle::default();
            for _ in 0..10_000 {
                v.step(0.01);
            }
            v.orbit.pos
        };

        let (a, b) = (run(), run());
        assert_eq!(a, b, "identical tick sequences must give an identical orbit");
    }

    #[test]
    fn body_axes_are_orthonormal() {
        let v = Vehicle::default();
        let (nadir, along, cross) = v.attitude.axes(&v.orbit);

        for (name, ax) in [("nadir", nadir), ("along", along), ("cross", cross)] {
            assert!((ax.norm() - 1.0).abs() < 1e-9, "{name} is not unit length");
        }
        let dot = |a: Vec3, b: Vec3| a.x * b.x + a.y * b.y + a.z * b.z;
        assert!(dot(nadir, along).abs() < 1e-9);
        assert!(dot(nadir, cross).abs() < 1e-9);
    }
}
