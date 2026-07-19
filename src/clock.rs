//! The simulated clock: Besom's half of the `timebase_besom` protocol.
//!
//! cFS runs on a PSP timebase module whose OSAL sync function blocks on a UNIX
//! datagram socket. cFE time advances only when we send a step, so this type is
//! the sole source of time for the flight software.
//!
//! Wire protocol (v1), little-endian, over `$BESOM_STEP_SOCK`:
//! ```text
//!   Besom -> PSP : u32  step_usec    (microseconds to advance; must be nonzero)
//!                  u32  sensor_len   (optional; omitted or 0 = no sensor block)
//!                  u8[] sensor_block (opaque to the PSP)
//!   PSP  -> Besom: u64  sim_usec     (simulated clock, AFTER the step is dispatched)
//! ```
//!
//! A bare 4-byte step is still valid v0, so an unpatched PSP and the free-running path both
//! keep working.
//!
//! The sensor block travels *inside* the step because that is what makes the flight software's
//! view of the world a function of simulated time. The PSP installs it before advancing the
//! clock, so there is no instant at which cFS is at time T without the state belonging to T,
//! and no queue whose depth the kernel could decide. See [`crate::fsw`].
//!
//! The reply is sent from the *entry* of the PSP's next sync call. OSAL only
//! re-enters that function once it has finished walking the timebase's callback
//! list, so receiving it is a hard guarantee that the previous tick was fully
//! dispatched. (Acking on consumption instead — the obvious placement — reports
//! "done" before any of the tick's work has happened, which makes a run
//! irreproducible by construction.)
//!
//! It does NOT prove the *tasks* woken by those callbacks have finished; they
//! run on their own threads. That is what [`crate::quiesce`] is for.

use anyhow::{Context, Result};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The tick cFS is configured for (`CFE_PSP_SOFT_TIMEBASE_PERIOD`).
pub const TICK_USEC: u32 = 10_000;

pub struct Clock {
    sock: UnixDatagram,
    psp_path: PathBuf,
    our_path: PathBuf,
    sim_usec: u64,
}

impl Clock {
    /// Connect to a cFS instance whose PSP has bound `psp_path`.
    ///
    /// Our own socket must be bound: the PSP replies with `sendto` to the peer
    /// address it saw, so an unbound datagram socket would never get an ack.
    pub fn connect(psp_path: impl AsRef<Path>) -> Result<Self> {
        let psp_path = psp_path.as_ref().to_path_buf();
        let our_path = PathBuf::from(format!("/tmp/besom-ctl-{}.sock", std::process::id()));

        let _ = std::fs::remove_file(&our_path);
        let sock = UnixDatagram::bind(&our_path)
            .with_context(|| format!("binding besom control socket {}", our_path.display()))?;
        sock.set_read_timeout(Some(Duration::from_secs(5)))?;

        Ok(Self { sock, psp_path, our_path, sim_usec: 0 })
    }

    /// Simulated microseconds granted so far.
    pub fn sim_usec(&self) -> u64 {
        self.sim_usec
    }

    /// Grant one tick and block until cFE has dispatched it.
    pub fn step(&mut self, usec: u32) -> Result<u64> {
        self.step_with_sensor(usec, &[])
    }

    /// Grant one tick, delivering `sensor` to the flight software as part of it.
    ///
    /// The sensor block rides the step rather than travelling on a socket of its own, and that
    /// is the whole point: the PSP installs it *before* advancing simulated time, so cFS cannot
    /// observe time T without also observing the state belonging to T. Sending state separately
    /// meant the flight software saw whatever the kernel had delivered by the time it looked,
    /// which is a host decision -- it published a sample a full cycle stale in some runs and not
    /// others, identically and invisibly in each.
    ///
    /// An empty `sensor` sends a bare v0 step, which the PSP still accepts.
    pub fn step_with_sensor(&mut self, usec: u32, sensor: &[u8]) -> Result<u64> {
        assert!(usec > 0, "a zero step would tell the PSP no time has passed");

        let mut msg = Vec::with_capacity(8 + sensor.len());
        msg.extend_from_slice(&usec.to_le_bytes());
        if !sensor.is_empty() {
            msg.extend_from_slice(&(sensor.len() as u32).to_le_bytes());
            msg.extend_from_slice(sensor);
        }

        self.sock
            .send_to(&msg, &self.psp_path)
            .with_context(|| format!("sending step to {}", self.psp_path.display()))?;

        let mut reply = [0u8; 8];
        let (n, _) = self.sock.recv_from(&mut reply).with_context(|| {
            // A missing ack has two very different causes and they need different answers.
            //
            // The ack for THIS step is sent at the entry of the PSP's NEXT sync call -- that is
            // what proves the previous tick was fully dispatched. Re-entering that call needs the
            // cooperative token. So under BESOM_COOP=1 a task that busy-waits on the mission clock
            // while holding the token deadlocks the run outright: exactly one tick can be granted,
            // the ack never comes, and time can never reach whatever the task is waiting for.
            //
            // That is structural, not a bug to be fixed here. Any wait that needs MANY ticks to
            // pass while holding exclusive execution cannot make progress cooperatively. cFS's own
            // cfe_testcase suite contains one (sb_performance_test calibrates CPU speed by
            // spinning until CFE_PSP_GetTime advances 100 ms). Such code runs fine under
            // BESOM_COOP=0, where the timebase thread is not gated by the token.
            if std::env::var("BESOM_COOP").unwrap_or_else(|_| "1".into()).starts_with('1') {
                "cFS did not acknowledge the step.\n\
                 With BESOM_COOP=1 this usually means a task is waiting on the mission clock while \
                 holding the cooperative token: only one tick can be in flight, so the clock cannot \
                 reach what the task is waiting for and the run deadlocks. Look for COOP-STALL \
                 lines in cFS's log -- they name the task holding the token.\n\
                 Code that spins or sleeps across many ticks cannot run cooperatively; re-run with \
                 BESOM_COOP=0 to get the stream guarantee without tick-placement determinism."
                    .to_string()
            } else {
                "cFS did not acknowledge the step (is it running on timebase_besom?)".to_string()
            }
        })?;
        anyhow::ensure!(n == 8, "malformed step ack: {n} bytes");

        self.sim_usec = u64::from_le_bytes(reply);
        Ok(self.sim_usec)
    }

    /// Grant `n` ticks of the default period.
    pub fn step_ticks(&mut self, n: u32) -> Result<u64> {
        for _ in 0..n {
            self.step(TICK_USEC)?;
        }
        Ok(self.sim_usec)
    }
}

impl Drop for Clock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.our_path);
    }
}
