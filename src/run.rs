//! Driving a cFS instance: boot it, own its clock, capture its telemetry.

use crate::ccsds::{build_command, TlmPacket};
use crate::dynamics::Vehicle;
use crate::fsw::{self, FswState};
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
    /// Launch cFS with its clock under our control, and return as soon as the PSP has bound
    /// the step socket.
    ///
    /// **This does NOT wait for the apps.** It returns while cFS is still coming up, and the
    /// caller must grant ticks during that window (see the note at the end of this function).
    /// In particular CI_LAB binds UDP 1234 some time AFTER this returns, so anything uplinked
    /// straight after `boot` is a datagram to a closed port — dropped in silence, leaving an
    /// empty sky with no error anywhere. Wait for `"CI_LAB listening on UDP port"` first.
    ///
    /// The counterpart hazard, which is why the step socket is the thing waited on: cFS boot
    /// does NOT need the timebase — OSAL tasks run on host threads — so it proceeds happily
    /// while our clock is frozen. If we start stepping before SCH_LAB has called
    /// `OS_TimerSet`, the whole tick budget burns before its timer is even armed, it never
    /// fires, and NO telemetry is ever produced. That failure looks exactly like a broken
    /// timebase and is not one.
    pub fn boot(cfg: &Config) -> Result<Self> {
        let _ = std::fs::remove_file(&cfg.step_sock);
        let log = std::env::temp_dir().join(format!("besom-cfs-{}.log", std::process::id()));

        let log_file = std::fs::File::create(&log)?;

        let mut cmd = Command::new("./core-cpu1");
        cmd.current_dir(&cfg.cfs_dir)
            // FORCE A POWER-ON RESET. Without this, a run inherits the previous run's state.
            //
            // cFE's PSP keeps its reserved memory alive between processes, so the SECOND run of
            // `check` finds it valid and comes up as a PROCESSOR reset while the first came up
            // power-on. Measured across two runs of the same scenario: ResetType 2 vs 1,
            // ProcessorResets 0 vs 1, ERLogEntries 1 vs 2, SysLogEntries 39 vs 76.
            //
            // The packet stream and tick placement do not notice, which is why this hid until
            // payloads were compared -- but "run the same scenario twice" was quietly running
            // two different scenarios, and any app that behaves differently after a processor
            // reset (checking its CDS, restoring state) would have been tested asymmetrically.
            .arg("-R")
            .arg("PO")
            .env("BESOM_STEP_SOCK", &cfg.step_sock)
            // Cooperative deterministic scheduling is ON by default: it is what
            // makes tick placement reproducible, not just the packet stream. Set
            // BESOM_COOP=0 to fall back to host scheduling (faster, but only the
            // stream is then guaranteed).
            .env(
                "BESOM_COOP",
                std::env::var("BESOM_COOP").unwrap_or_else(|_| "1".into()),
            )
            // ONE file description, shared. Calling File::create twice on the same path gives
            // two independent descriptions, each with its own offset and each truncating: cFS's
            // stdout and stderr then overwrite each other instead of interleaving, and whichever
            // wrote last wins. Every guard in this file reads that log -- "CI_LAB listening on
            // UDP port", "TO_LAB 3", "entering OPERATIONAL state" -- so a clobbered line is a
            // guard that waits for something already written and then times out, or worse,
            // misses the race it exists to catch. try_clone shares the offset.
            .stdout(Stdio::from(log_file.try_clone()?))
            .stderr(Stdio::from(log_file));

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

        // NOTE: we do NOT wait for the apps here.
        //
        // The caller must GRANT TICKS WHILE cFS BOOTS. An app that sleeps in its
        // loop (cFS's HS does) takes its cycle phase from whatever clock is
        // running: if simulated time is not yet active, those sleeps run on the
        // HOST clock, and where the app's cycle lands when ticks finally begin
        // depends on how fast the machine happened to boot. That showed up as HS's
        // entire telemetry stream shifting by exactly one of its own periods
        // between runs.
        //
        // Stepping from the first instant makes every app's timing simulated, so
        // its phase is ours.

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

/// The worst disagreement seen between the flight software's reported state and
/// the harness's own, over a run.
///
/// Both sides are the same f64 travelling over a lossless local link, so a
/// non-zero worst-case is not numerical noise -- it means cFS was reporting
/// STALE state, i.e. the sensor feed was falling behind the simulation.
#[derive(Debug, Clone, Copy, Default)]
pub struct LoopError {
    pub max_lat_deg: f64,
    pub max_lon_deg: f64,
    pub samples: u32,
    pub last: Option<FswState>,
}

/// Ticks granted past the end of the recorded window, whose packets are thrown away.
///
/// At least one full period of the SLOWEST periodic stream. cFE TIME's 1 Hz tone is the
/// slowest thing driving telemetry, so anything less than 100 ticks cannot stabilise the
/// edge -- a shorter guard leaves the last 1 Hz cycle half in and half out, and the packet
/// count wobbles.
pub const GUARD: u32 = 120;

/// Boot cFS, enable its downlink, step the clock, and record the telemetry.
pub fn run(cfg: &Config) -> Result<Transcript> {
    Ok(run_with_loop(cfg)?.0)
}

/// As [`run`], but also feeds simulated vehicle state to the `besom_io` app and
/// measures how faithfully the flight software reports it back.
pub fn run_with_loop(cfg: &Config) -> Result<(Transcript, LoopError)> {
    // A budget inside the guard band records NOTHING, and an empty transcript is not a
    // reproducible one -- it is an unasked question. Two empty transcripts compare equal, so
    // `check` would print "0 packets, identical" / "tick placement: identical" and exit 0,
    // passing just as happily with the downlink dead. Refuse the run instead of answering
    // vacuously.
    if cfg.ticks <= GUARD {
        bail!(
            "tick budget {} is inside the {GUARD}-tick guard band, so nothing would be \
             recorded -- use more than {GUARD} ticks",
            cfg.ticks
        );
    }

    let cfs = Cfs::boot(cfg)?;
    let mut clock = Clock::connect(&cfg.step_sock)?;

    let tlm = UdpSocket::bind(("0.0.0.0", TLM_PORT))
        .with_context(|| format!("binding telemetry port {TLM_PORT}"))?;
    tlm.set_nonblocking(true)?;

    let mut vehicle = Vehicle::default();
    let mut loop_err = LoopError::default();

    // STEP THROUGH BOOT.
    //
    // Grant ticks while cFS starts up, so that simulated time is running from its
    // first instant and every app's cycle phase is set by OUR clock rather than by
    // how fast the host happened to boot.
    // The boot must consume a FIXED number of ticks, not "however many it takes".
    //
    // Stepping until the log says OPERATIONAL is a HOST-timed condition: a slower
    // boot consumes more ticks, and every app's cycle phase moves with it. That
    // leaves a residual sub-phase error -- one run in nine came out shifted by 10
    // ticks, which is exactly besom_io's 10 Hz period.
    //
    // So step a fixed budget: enough for any boot, and identical every run.
    const BOOT_TICKS: u32 = 4000;
    {
        // A WALL-CLOCK backstop, not a simulated one: it exists so a wedged cFS fails instead
        // of hanging forever. It bounds nothing about the run's content.
        //
        // It is tunable because it is the one place where buying determinism with wall-clock
        // collides with a fixed wall-clock budget. Widening the quiescence confirmation window
        // (`$BESOM_QUIESCE_SAMPLES`) multiplies through all 4000 boot ticks -- 20 samples at
        // 400us is ~32s of extra polling before cFS has even finished booting -- so a value
        // that was generous at the default becomes a spurious "boot timed out" on a loaded
        // host. Raise both together, or the harness fails for the wrong reason.
        let boot_timeout = std::env::var("BESOM_BOOT_TIMEOUT_S")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);
        let deadline = Instant::now() + Duration::from_secs(boot_timeout);
        let mut booted = false;

        for _i in 0..BOOT_TICKS {
            // Feed from the very first tick. The sensor rides the step, so this costs no
            // extra syscall -- and it guarantees the PSP holds a block before besom_io's timer
            // can ever fire, so there is no startup window in which the app has no sample.
            clock.step_with_sensor(TICK_USEC, &fsw::encode_state(&vehicle, clock.sim_usec()))?;
            quiesce::wait(cfs.pid());

            if !booted && cfs.log_contains("entering OPERATIONAL state") {
                booted = true;
                if std::env::var("BESOM_DEBUG").is_ok() {
                    eprintln!("  (boot: OPERATIONAL after {_i} ticks)");
                }
            }
            if Instant::now() > deadline {
                bail!(
                    "cFS boot timed out after {boot_timeout}s of wall clock \
                     (raise $BESOM_BOOT_TIMEOUT_S; a wide $BESOM_QUIESCE_SAMPLES costs \
                     ~{}s of polling across {BOOT_TICKS} boot ticks)",
                    quiesce::confirm_window().as_millis() * u128::from(BOOT_TICKS) / 1000
                );
            }
        }

        if !booted {
            bail!("cFS never reached OPERATIONAL within {BOOT_TICKS} ticks");
        }
        if std::env::var("BESOM_DEBUG").is_ok() {
            eprintln!("  (boot: OPERATIONAL reached, padded to {BOOT_TICKS} ticks)");
        }
    }

    // BRING UP THE DOWNLINK. Two hazards, and they pull in opposite directions.
    //
    // DO NOT UPLINK BEFORE CI_LAB HAS BOUND ITS PORT. `Cfs::boot` returns as soon as the PSP
    // binds the step socket -- it does NOT wait for the apps. CI_LAB binds UDP 1234 some time
    // later, and a command sent into that gap is a datagram to a closed port: dropped, in
    // silence. TO_LAB then sits at "Awaiting enable command" forever, nothing is downlinked,
    // and cFS, the timebase and the tick stream all look healthy -- an empty sky, no error.
    // The bind itself happens on a host thread during app init, so waiting for it costs no
    // simulated time.
    //
    // DO NOT WAIT FOR THE ACK WITH THE CLOCK FROZEN. By this point the boot loop has granted
    // ticks, so OS_SimTime is ACTIVE. CI_LAB's loop is
    //     CFE_SB_ReceiveBuffer(&buf, CommandPipe, 500ms)   // 500ms = 50 ticks, simulated
    //     ... CI_LAB_ReadUpLink()                          // only reached after that returns
    // and under the OSAL patch a timed receive with simulated time active parks until
    // SIMULATED time reaches its deadline. With the clock frozen CI_LAB can never wake, never
    // polls its socket, and TO_LAB never emits EID 3 -- so waiting for the ack without
    // granting time is unsatisfiable by construction, and hangs every run until it bails.
    //
    // So grant time, but a FIXED amount: enough for several CI_LAB wakeups, and identical on a
    // fast host and a slow one, so the handshake costs the same simulated time every run.
    //
    // Note what this corrects about the older comment here. It claimed the enable took effect
    // "at an exact simulated instant" because the clock was frozen. It did not: the guard
    // waited on "TO_LAB 19", a BOOT event already in the log, so it passed vacuously and the
    // datagram simply sat in the kernel socket buffer until the phase-align loop below started
    // granting ticks. The enable was always applied whenever ticks resumed -- that is now
    // explicit, bounded, and actually verified.
    cfs.await_log("CI_LAB listening on UDP port", Duration::from_secs(10))
        .context("CI_LAB never bound its command port")?;

    let mut ip = [0u8; 16];
    ip[..9].copy_from_slice(b"127.0.0.1");
    let enable = build_command(TO_LAB_CMD_MID, TO_LAB_OUTPUT_ENABLE_CC, &ip);
    let uplink = UdpSocket::bind(("0.0.0.0", 0))?;
    uplink.send_to(&enable, ("127.0.0.1", CI_PORT))?;

    // 4x CI_LAB's 50-tick receive timeout: several chances to wake, still a fixed cost.
    //
    // DRAIN AS WE GO. TO_LAB starts downlinking the moment the enable lands, so these ticks
    // produce telemetry -- and leaving it to accumulate undrained puts it in the kernel's
    // receive buffer, where overflow is decided by socket memory rather than by the flight
    // software. Granting 200 ticks without draining lost packets nondeterministically: `check`
    // went from 15/15 identical to 89/382 packets moved ON AN IDLE HOST. This is boot history,
    // so it is discarded -- but it must be discarded by US, deterministically, not by the
    // kernel dropping whatever did not fit.
    const ENABLE_TICKS: u32 = 200;
    let mut boot_history = Transcript::new();
    for _ in 0..ENABLE_TICKS {
        clock.step_with_sensor(TICK_USEC, &fsw::encode_state(&vehicle, clock.sim_usec()))?;
        quiesce::wait(cfs.pid());
        drain(&tlm, &mut boot_history);
    }

    // A REAL guard. TO_LAB_TLMOUTENA_INF_EID (3) is the enable itself, not a boot event, so
    // this fails loudly on a lost command instead of flying blind.
    if !cfs.log_contains("TO_LAB 3") {
        bail!("TO_LAB never acknowledged the enable-output command; there would be no downlink");
    }

    // Subscribe besom_io's telemetry: TO_LAB's table is baked in at build time.
    let up = UdpSocket::bind(("0.0.0.0", 0))?;
    up.send_to(&fsw::add_packet_command(fsw::STATE_TLM_MID), ("127.0.0.1", CI_PORT))?;
    quiesce::wait(cfs.pid());

    // Anything emitted while enabling is boot history.
    drain(&tlm, &mut Transcript::new());

    // PHASE-ALIGN THE START.
    //
    // The end of the window was guarded, but not the beginning -- and the
    // beginning is the half that actually moves. cFE TIME's 1 Hz tone is armed
    // during un-gated boot, so the system's phase at tick 1 is host-dependent: a
    // fixed tick budget therefore catches N or N+1 housekeeping cycles, and the
    // packet count wobbles by a packet or two between runs. That is what made 30 s
    // checks fail about one run in three.
    //
    // So do not start counting at tick 1. Step until the system reaches a KNOWN
    // point in its cycle -- the first cFE ES housekeeping packet, which is emitted
    // once per 1 Hz cycle -- and start the window there. Both ends of the window
    // are then pinned to the same phase, and the number of cycles inside it is
    // fixed.
    const SYNC_MID: u16 = 0x0800; // cFE ES housekeeping: one per 1 Hz cycle
    // Generous: the first housekeeping cycle can take a few hundred ticks to
    // appear, because SCH_LAB will not run its table until it has seen a 1 Hz
    // packet from CFE_TIME.
    const SYNC_LIMIT: u32 = 1500;

    let mut synced = false;
    for _ in 0..SYNC_LIMIT {
        clock.step_with_sensor(TICK_USEC, &fsw::encode_state(&vehicle, clock.sim_usec()))?;
        quiesce::wait(cfs.pid());
        vehicle.step(f64::from(TICK_USEC) / 1e6);

        let mut probe = Transcript::new();
        drain(&tlm, &mut probe);

        if probe.entries().iter().any(|e| e.msg_id == SYNC_MID) {
            synced = true;
            if std::env::var("BESOM_DEBUG_SAMPLES").is_ok() {
                eprintln!("SYNCTICK {}", clock.sim_usec());
            }
            break;
        }
    }

    if !synced {
        bail!("cFS never emitted a housekeeping cycle -- cannot phase-align the run");
    }

    // GUARD BAND: stop RECORDING before we stop GRANTING TIME.
    //
    // The run's edge is otherwise not at a deterministic simulated instant. A
    // packet emitted on the final tick may or may not have reached the socket
    // before we stopped, and a periodic app whose timer was armed during un-gated
    // boot fires N or N+1 times over a fixed budget. Either way the transcript's
    // last packet appears and disappears between runs -- which is the run's edge
    // moving, not the flight software behaving differently, and it is not worth
    // weakening the stream comparison to tolerate.
    //
    // So keep stepping for a further GUARD ticks and throw those packets away.
    // The recorded window then ends at a simulated time we chose, and the packet
    // counts are stable.
    // See GUARD.
    let record_until = cfg.ticks.saturating_sub(GUARD);

    let mut transcript = Transcript::new();
    let mut discard = Transcript::new();

    for tick in 0..cfg.ticks {
        // The sensor block IS the step. "Deliver state before granting the tick that lets the
        // flight software look for it" used to be a convention this loop had to remember; now
        // there is one datagram and the state is in it, so the ordering cannot be got wrong.
        clock.step_with_sensor(TICK_USEC, &fsw::encode_state(&vehicle, clock.sim_usec()))?;
        quiesce::wait(cfs.pid());

        let sink = if tick < record_until { &mut transcript } else { &mut discard };
        drain_with_loop(&tlm, sink, &vehicle, &mut loop_err);

        vehicle.step(f64::from(TICK_USEC) / 1e6);
    }

    // A packet emitted on the very last tick can still be in flight -- TO_LAB's
    // downlink runs on its own thread and the datagram has to reach the socket
    // buffer. Draining once races that, and the run captures 7 or 8 packets
    // depending on host timing. Re-drain until two passes come up empty.
    // Anything still in flight belongs to the guard band, not the record.
    let mut idle = 0;
    while idle < 2 {
        quiesce::wait(cfs.pid());
        sleep(Duration::from_millis(50));
        idle = if drain(&tlm, &mut discard) > 0 { 0 } else { idle + 1 };
    }

    Ok((transcript.finish(), loop_err))
}

/// Drain, and where a besom_io state packet appears, compare what the flight
/// software reported against what we actually sent it.
fn drain_with_loop(
    sock: &UdpSocket,
    transcript: &mut Transcript,
    vehicle: &Vehicle,
    err: &mut LoopError,
) {
    let mut buf = [0u8; 65535];

    loop {
        let n = match sock.recv_from(&mut buf) {
            Ok((n, _)) => n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(_) => break,
        };

        let Ok(pkt) = TlmPacket::parse(&buf[..n]) else { continue };

        if let Some(f) = FswState::parse(pkt.msg_id, &buf[..n]) {
            if std::env::var("BESOM_DEBUG_SAMPLES").is_ok() {
                eprintln!("SAMPLE {} {:.9} {:.9}", f.sample_usec, f.lat_deg, f.lon_deg);
            }
            let (lat, lon) = vehicle.orbit.subpoint_deg();
            err.max_lat_deg = err.max_lat_deg.max((f.lat_deg - lat).abs());
            err.max_lon_deg = err.max_lon_deg.max((f.lon_deg - lon).abs());
            err.samples += 1;
            err.last = Some(f);
        }

        transcript.record(&pkt);
    }
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
