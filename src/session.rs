//! A live session: cFS running under our clock, driven by an operator.
//!
//! The session thread owns the spacecraft. Because Besom grants every tick, the
//! operator can *pause simulated time* — the flight software freezes mid-flight,
//! exactly, and resumes with no discontinuity. A ground station paced to wall
//! time cannot do that, and it is the whole point of owning the clock.
//!
//! The UI never touches cFS directly: it sends [`Cmd`]s in and reads a
//! [`State`] snapshot out, so a stalled or slow UI can never perturb the
//! simulation's timing.

use crate::ccsds::{build_command, TlmPacket};
use crate::clock::{Clock, TICK_USEC};
use crate::dynamics::{Vec3, Vehicle};
use crate::evs::{self, Event};
use crate::quiesce;
use crate::run::{Cfs, Config, CI_PORT, TLM_PORT};
use anyhow::Result;
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::net::UdpSocket;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// What the operator can ask of the spacecraft.
pub enum Cmd {
    Play,
    Pause,
    /// Ticks to grant per iteration. Simulated time can outrun the wall clock --
    /// there is no physical rate to respect, only how fast cFS can actually be
    /// driven, so a 92-minute orbit is watchable in a couple of minutes.
    Warp(u32),
    /// Advance exactly n ticks, then pause. The reason to own a clock.
    StepTicks(u32),
    /// Send a raw command to CI_LAB.
    Send { msg_id: u16, fn_code: u8, payload: Vec<u8> },
    Shutdown,
}

/// One telemetry stream, as the operator sees it.
#[derive(Debug, Clone, Default)]
pub struct Stream {
    pub count: u64,
    pub last_seq: u16,
    /// Gaps in the CCSDS sequence counter: packets the spacecraft sent that we
    /// never saw. On a real link this is the number that matters.
    pub gaps: u64,
    pub last_time: f64,
    pub len: usize,
}

#[derive(Debug, Clone, Default)]
pub struct State {
    pub running: bool,
    pub alive: bool,
    /// Simulated mission time, in seconds. Advances only when we grant it.
    pub sim_secs: f64,
    pub streams: BTreeMap<u16, Stream>,
    pub events: Vec<Event>,
    pub packets: u64,
    pub error: Option<String>,
    /// Where the spacecraft is, propagated on the SAME granted ticks as the
    /// flight software. Pause, and it stops in the sky exactly where cFS stopped.
    pub vehicle: Vehicle,
    /// Recent positions, for the orbit trail.
    pub trail: Vec<Vec3>,
    /// cFE's absolute MET at the first packet. Its epoch is a large constant
    /// that also varies run to run (cFE TIME settles on a 5-second quantum at
    /// boot), so showing it raw is both ugly and misleading. Packet times are
    /// displayed relative to it.
    epoch: Option<f64>,
}

pub struct Session {
    tx: Sender<Cmd>,
    state: Arc<Mutex<State>>,
}

impl Session {
    /// Boot cFS and take ownership of its clock. Starts paused: the spacecraft
    /// is alive but time is not moving.
    pub fn start(cfg: Config) -> Self {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(State::default()));

        let worker_state = Arc::clone(&state);
        thread::spawn(move || {
            if let Err(e) = drive(cfg, rx, &worker_state) {
                let mut s = worker_state.lock().unwrap();
                s.error = Some(format!("{e:#}"));
                s.alive = false;
                s.running = false;
            }
        });

        Self { tx, state }
    }

    pub fn snapshot(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    /// Shut cFS down and wait for it to actually die.
    ///
    /// This must be called explicitly on window close. Dropping the `Session` is
    /// NOT enough: the process exits before the worker thread unwinds, so the
    /// `Cfs` guard never runs its destructor and the spacecraft is left running
    /// as an orphan — holding UDP 2234, so the next launch silently gets no
    /// telemetry.
    pub fn shutdown(&self) {
        self.send(Cmd::Shutdown);

        // The worker drops its Cfs on the way out, which kills the child.
        for _ in 0..50 {
            if !self.state.lock().unwrap().alive {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn drive(cfg: Config, rx: Receiver<Cmd>, state: &Arc<Mutex<State>>) -> Result<()> {
    let cfs = Cfs::boot(&cfg)?;
    let mut clock = Clock::connect(&cfg.step_sock)?;

    let tlm = UdpSocket::bind(("0.0.0.0", TLM_PORT))?;
    tlm.set_nonblocking(true)?;
    let uplink = UdpSocket::bind(("0.0.0.0", 0))?;

    // Enable the downlink with the clock still frozen, so it takes effect at an
    // exact simulated instant rather than whenever the host got round to it.
    let mut ip = [0u8; 16];
    ip[..9].copy_from_slice(b"127.0.0.1");
    uplink.send_to(&build_command(0x1880, 6, &ip), ("127.0.0.1", CI_PORT))?;
    let _ = cfs.await_log_public("TO_LAB 19", Duration::from_secs(10));
    quiesce::wait(cfs.pid());

    state.lock().unwrap().alive = true;

    // The spacecraft flies on arrival; the operator pauses it. Starting frozen
    // would present an empty ground station and look like a failure.
    let mut running = true;
    let mut budget: u32 = 0;
    let mut warp: u32 = 1;
    let mut last_sample = 0.0f64;

    {
        let mut s = state.lock().unwrap();
        s.running = true;
    }

    loop {
        // Drain operator commands first: a pause must take effect before the
        // next tick is granted, not after.
        loop {
            match rx.try_recv() {
                Ok(Cmd::Play) => running = true,
                Ok(Cmd::Warp(n)) => warp = n.max(1),
                Ok(Cmd::Pause) => {
                    running = false;
                    budget = 0;
                }
                Ok(Cmd::StepTicks(n)) => {
                    running = false;
                    budget = budget.saturating_add(n);
                }
                Ok(Cmd::Send { msg_id, fn_code, payload }) => {
                    let pkt = build_command(msg_id, fn_code, &payload);
                    let _ = uplink.send_to(&pkt, ("127.0.0.1", CI_PORT));
                }
                Ok(Cmd::Shutdown) | Err(TryRecvError::Disconnected) => {
                    // Drop Cfs (killing the child) before signalling the UI, so
                    // that `shutdown()` returning means the spacecraft is gone.
                    drop(cfs);
                    state.lock().unwrap().alive = false;
                    return Ok(());
                }
                Err(TryRecvError::Empty) => break,
            }
        }

        if running || budget > 0 {
            // Grant a burst, then let the vehicle catch up by exactly the same
            // amount. Quiescence still gates every individual tick, so warping
            // changes how fast we drive cFS -- never whether it keeps up.
            let n = if budget > 0 { 1 } else { warp };
            for _ in 0..n {
                clock.step(TICK_USEC)?;
                quiesce::wait(cfs.pid());
            }
            budget = budget.saturating_sub(1);

            // Propagate the vehicle by exactly the time we granted cFS. The
            // picture and the telemetry are then the same run -- if these two
            // ever diverge, the display is lying about the flight software.
            let mut s = state.lock().unwrap();
            s.vehicle.step(f64::from(TICK_USEC) / 1e6 * f64::from(n));

            // Sample the trail sparsely: a point per second of simulated time is
            // plenty for a smooth track, and keeps a 90-minute orbit cheap.
            // One trail point per ~10 s of simulated time: smooth enough for a
            // 90-minute orbit, and bounded so a long run cannot grow without end.
            let sample = 10.0;
            let due = (s.vehicle.elapsed / sample).floor() > (last_sample / sample).floor();
            if s.trail.is_empty() || due {
                last_sample = s.vehicle.elapsed;
                let p = s.vehicle.orbit.pos;
                s.trail.push(p);

                let cap = (s.vehicle.orbit.period_secs() / sample * 1.05) as usize;
                if s.trail.len() > cap {
                    s.trail.remove(0);
                }
            }
        } else {
            // Time is frozen. Idle without burning a core; the spacecraft is
            // genuinely stopped, not merely un-rendered.
            thread::sleep(Duration::from_millis(10));
        }

        let mut s = state.lock().unwrap();
        s.running = running || budget > 0;
        s.sim_secs = clock.sim_usec() as f64 / 1e6;
        drain(&tlm, &mut s);
    }
}

fn drain(sock: &UdpSocket, state: &mut State) {
    let mut buf = [0u8; 65535];

    loop {
        let n = match sock.recv_from(&mut buf) {
            Ok((n, _)) => n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(_) => break,
        };

        let Ok(pkt) = TlmPacket::parse(&buf[..n]) else { continue };

        if let Some(ev) = evs::parse(pkt.msg_id, &buf[..n]) {
            state.events.push(ev);
            // The event log is a tail, not an archive.
            if state.events.len() > 500 {
                state.events.drain(..100);
            }
        }

        let epoch = *state.epoch.get_or_insert(pkt.time_secs());

        let entry = state.streams.entry(pkt.msg_id).or_default();
        if entry.count > 0 {
            // A delta of anything but 1 means the spacecraft transmitted packets
            // we never received.
            let delta = pkt.seq.wrapping_sub(entry.last_seq) & 0x3FFF;
            if delta != 1 {
                entry.gaps += u64::from(delta.saturating_sub(1));
            }
        }
        entry.count += 1;
        entry.last_seq = pkt.seq;
        entry.last_time = pkt.time_secs() - epoch;
        entry.len = pkt.len;

        state.packets += 1;
    }
}
