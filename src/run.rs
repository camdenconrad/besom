//! Driving a cFS instance: boot it, own its clock, capture its telemetry.

use crate::ccsds::{build_command, TlmPacket};
use crate::clock::{Clock, TICK_USEC};
use crate::quiesce;
use crate::transcript::Transcript;
use anyhow::{bail, Context, Result};
use std::io::ErrorKind;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// CI_LAB's command ingest port.
pub const CI_PORT: u16 = 1234;
/// TO_LAB's telemetry downlink port. Note: **2234**, not the 1235 that older
/// cFS tutorials still print.
pub const TLM_PORT: u16 = 2234;

const TO_LAB_CMD_MID: u16 = 0x1880;
const TO_LAB_OUTPUT_ENABLE_CC: u8 = 6;

pub struct Config {
    /// Directory holding `core-cpu1` (the cFS build's `exe/cpu1`).
    pub cfs_dir: PathBuf,
    pub step_sock: PathBuf,
    pub ticks: u32,
}

pub struct Cfs {
    child: Child,
    log: PathBuf,
}

impl Cfs {
    /// Boot cFS with its clock under our control, and wait until its apps have
    /// come up.
    ///
    /// The wait is not optional. cFS boot does NOT need the timebase — OSAL
    /// tasks run on host threads — so it proceeds happily while our clock is
    /// frozen. If we start stepping before SCH_LAB has called `OS_TimerSet`, the
    /// whole tick budget burns before its timer is even armed, it never fires,
    /// and NO telemetry is ever produced. That failure looks exactly like a
    /// broken timebase and is not one.
    pub fn boot(cfg: &Config) -> Result<Self> {
        let _ = std::fs::remove_file(&cfg.step_sock);
        let log = std::env::temp_dir().join(format!("besom-cfs-{}.log", std::process::id()));

        let mut cmd = Command::new("./core-cpu1");
        cmd.current_dir(&cfg.cfs_dir)
            .env("BESOM_STEP_SOCK", &cfg.step_sock)
            .stdout(Stdio::from(std::fs::File::create(&log)?))
            .stderr(Stdio::from(std::fs::File::create(&log)?));

        // Have the kernel kill cFS if we die, however we die.
        //
        // A Drop guard is not enough: on SIGTERM (or a panic, or a crash) the
        // process exits without unwinding, and cFS is left running as an orphan
        // still holding UDP 2234 — so the NEXT launch silently receives no
        // telemetry and looks broken. PDEATHSIG makes the guarantee
        // unconditional rather than best-effort.
        //
        // SAFETY: prctl is async-signal-safe and this runs in the forked child
        // between fork and exec, where only such calls are permitted.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = cmd
            .spawn()
            .with_context(|| format!("launching core-cpu1 in {}", cfg.cfs_dir.display()))?;

        let this = Self { child, log };

        // The PSP binds the step socket during module init.
        this.await_deadline(Duration::from_secs(10), || cfg.step_sock.exists())
            .context("PSP never bound the step socket (is timebase_besom in the build?)")?;

        // "CI_LAB listening" is the last thing logged during app startup.
        this.await_log("CI_LAB listening", Duration::from_secs(15))
            .context("cFS apps never came up")?;
        sleep(Duration::from_millis(500)); // let the last inits settle

        Ok(this)
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn log_contains(&self, needle: &str) -> bool {
        std::fs::read_to_string(&self.log).is_ok_and(|s| s.contains(needle))
    }

    fn await_log(&self, needle: &str, timeout: Duration) -> Result<()> {
        self.await_deadline(timeout, || self.log_contains(needle))
    }

    /// Block until `needle` appears in cFS's log. Used to pin a command's effect
    /// to an exact simulated instant: send it with the clock frozen, wait for
    /// the app to acknowledge, then resume.
    pub fn await_log_public(&self, needle: &str, timeout: Duration) -> Result<()> {
        self.await_log(needle, timeout)
    }

    fn await_deadline(&self, timeout: Duration, mut ready: impl FnMut() -> bool) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if ready() {
                return Ok(());
            }
            sleep(Duration::from_millis(20));
        }
        bail!("timed out")
    }
}

impl Drop for Cfs {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.log);
    }
}

/// Boot cFS, enable its downlink, step the clock, and record the telemetry.
pub fn run(cfg: &Config) -> Result<Transcript> {
    let cfs = Cfs::boot(cfg)?;
    let mut clock = Clock::connect(&cfg.step_sock)?;

    let tlm = UdpSocket::bind(("0.0.0.0", TLM_PORT))
        .with_context(|| format!("binding telemetry port {TLM_PORT}"))?;
    tlm.set_nonblocking(true)?;

    // Enable the downlink BEFORE granting any time at all.
    //
    // Two things depend on this ordering:
    //
    //  - The clock is frozen at zero, so the command takes effect at an exact
    //    simulated instant. Sending it while stepping means CI_LAB picks it up
    //    at a host-scheduling-dependent moment and the run stops being
    //    reproducible.
    //
    //  - The capture window opens at sim-time zero, so every packet the run ever
    //    emits is recorded. Warming up with ticks first leaves a variable amount
    //    of un-captured history behind the first observed packet, which surfaces
    //    as drifting sequence counters and looks like a clock bug.
    //
    // The command path itself needs no ticks: CI_LAB reads its socket on an
    // ordinary host task.
    let mut ip = [0u8; 16];
    ip[..9].copy_from_slice(b"127.0.0.1");
    let enable = build_command(TO_LAB_CMD_MID, TO_LAB_OUTPUT_ENABLE_CC, &ip);
    UdpSocket::bind(("0.0.0.0", 0))?.send_to(&enable, ("127.0.0.1", CI_PORT))?;

    // TO_LAB event 19 = "TO Lab subscribed to N messages from the table".
    cfs.await_log("TO_LAB 19", Duration::from_secs(10))
        .context("TO_LAB never acknowledged the enable command")?;
    quiesce::wait(cfs.pid());

    // Anything emitted while enabling is boot history; the run starts at tick 1.
    drain(&tlm, &mut Transcript::new());

    let mut transcript = Transcript::new();
    for _ in 0..cfg.ticks {
        clock.step(TICK_USEC)?;
        quiesce::wait(cfs.pid());
        drain(&tlm, &mut transcript);
    }

    // A packet emitted on the very last tick can still be in flight -- TO_LAB's
    // downlink runs on its own thread and the datagram has to reach the socket
    // buffer. Draining once races that, and the run captures 7 or 8 packets
    // depending on host timing. Re-drain until two passes come up empty.
    let mut idle = 0;
    while idle < 2 {
        quiesce::wait(cfs.pid());
        sleep(Duration::from_millis(50));
        idle = if drain(&tlm, &mut transcript) > 0 { 0 } else { idle + 1 };
    }

    Ok(transcript.finish())
}

fn drain(sock: &UdpSocket, transcript: &mut Transcript) -> usize {
    let mut buf = [0u8; 65535];
    let mut got = 0;

    loop {
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                if let Ok(pkt) = TlmPacket::parse(&buf[..n]) {
                    transcript.record(&pkt);
                    got += 1;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }

    got
}

/// Locate a cFS build. Honours `$BESOM_CFS_DIR`, else the canonical clone.
pub fn default_cfs_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("BESOM_CFS_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    Path::new(&home).join("Projects/cFS/build-native_std/exe/cpu1")
}
