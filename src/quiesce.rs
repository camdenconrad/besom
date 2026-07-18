//! Quiescence detection: knowing when the flight software has finished reacting.
//!
//! A step ack proves the clock moved and that the timebase dispatched its
//! callbacks. It does NOT prove the tasks those callbacks woke — SCH_LAB, the
//! apps, CFE_TIME's tone processing — have finished. They run on ordinary host
//! threads. Granting the next tick while they are still working makes how much
//! they get done depend on host scheduling, and the run stops being reproducible.
//!
//! Linux exposes exactly what is needed: a thread in state `R` is
//! runnable/running; anything else is blocked. When every thread of the cFS
//! process is blocked, it has finished reacting to the tick and cannot make
//! further progress until we grant more time.
//!
//! Note this is an *approximation* from outside the process: immediately after
//! an ack the woken tasks may not be marked runnable yet, so a single clean
//! sample can report a false quiescence and we would sail straight past the
//! work. Requiring consecutive clean samples is what makes it hold.
//!
//! It cannot fix intra-tick *ordering* — within one granted tick cFE's tasks are
//! simultaneous in simulated time and the host scheduler picks who runs first.
//! That is the residual ±1-tick jitter documented in docs/besom-phase0.md.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::thread::sleep;
use std::time::{Duration, Instant};

const DEFAULT_CLEAN_SAMPLES: u32 = 3;
const POLL: Duration = Duration::from_micros(400);
const DEFAULT_TIMEOUT_MS: u64 = 2_000;

/// Consecutive all-blocked samples required before declaring quiescence.
///
/// Tunable via `$BESOM_QUIESCE_SAMPLES` because this, not the timeout, is what decides whether
/// quiescence is real. The default of 3 at `POLL` is a 1.2 ms window: if the tick's woken tasks
/// have not been marked `R` by then, every sample is clean and `wait` returns before cFS has
/// begun reacting. That is a FALSE quiescence, and unlike an expiry it increments nothing --
/// the next tick is granted mid-reaction and the counter still reads zero.
///
/// Measured: under CPU contention, placement shifts occur with `timeouts() == 0`, so the
/// deadline is not what is being hit. See docs/determinism-under-load.md.
fn clean_samples() -> u32 {
    static N: OnceLock<u32> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("BESOM_QUIESCE_SAMPLES")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_CLEAN_SAMPLES)
    })
}

/// How long [`wait`] will hold out for quiescence before giving up and granting time anyway.
///
/// Tunable via `$BESOM_QUIESCE_MS` because the right value is a property of the HOST, not of
/// the flight software. The default suits an idle machine. On a loaded or heavily
/// oversubscribed one -- CI runners especially -- cFS takes longer in wall-clock terms to
/// finish reacting to a tick, and a budget that was generous when idle starts expiring; every
/// expiry is the harness moving on mid-reaction, which is how host load leaks into a run that
/// is supposed to be immune to it. Raising this trades wall-clock time for determinism.
fn timeout() -> Duration {
    static T: OnceLock<Duration> = OnceLock::new();
    *T.get_or_init(|| {
        let ms = std::env::var("BESOM_QUIESCE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        Duration::from_millis(ms)
    })
}

/// How many times [`wait`] gave up before cFS actually went quiet.
///
/// This is the harness's own determinism budget, and it must be observable.
/// Proceeding on timeout grants the next tick while flight software is still
/// working, which is precisely a host-timed decision -- the thing the whole
/// design exists to avoid. A run with a non-zero count did not measure what it
/// claims to measure, and a placement difference between two such runs says
/// nothing about the flight software.
///
/// **Zero is not proof of health.** This counts only the LATE failure -- giving
/// up. It cannot see the early one: returning after [`clean_samples`] samples
/// that were all clean because the tick's woken tasks had not been marked `R`
/// yet. Measured under load, placement shifts happen with this counter at zero,
/// so an early return is the failure that actually bites. See
/// docs/determinism-under-load.md.
static TIMEOUTS: AtomicU64 = AtomicU64::new(0);

/// How long a clean run of samples takes to confirm quiescence.
///
/// The cost paid after every granted tick, so callers with a fixed wall-clock budget (the boot
/// loop) can say what widening the window will cost them.
pub fn confirm_window() -> Duration {
    POLL * clean_samples()
}

/// Timeouts recorded since the last [`reset`].
pub fn timeouts() -> u64 {
    TIMEOUTS.load(Ordering::Relaxed)
}

/// Zero the counter. Call before a run whose quiescence you intend to report.
pub fn reset() {
    TIMEOUTS.store(0, Ordering::Relaxed);
}

/// Block until no thread of `pid` is runnable, or the timeout elapses.
///
/// Timing out is not an error in the sense that it aborts nothing -- stalling
/// the whole run over a genuinely busy process would be worse. But it is not
/// free either: it is the harness choosing to move on while cFS is mid-flight,
/// so it is COUNTED (see [`timeouts`]) rather than passed over in silence.
pub fn wait(pid: u32) {
    let deadline = Instant::now() + timeout();
    let mut clean = 0;

    while Instant::now() < deadline {
        match any_runnable(pid) {
            None => return, // process gone; the caller will notice
            Some(true) => clean = 0,
            Some(false) => {
                clean += 1;
                if clean >= clean_samples() {
                    return;
                }
            }
        }
        sleep(POLL);
    }

    TIMEOUTS.fetch_add(1, Ordering::Relaxed);
}

/// `None` if the process is gone. Otherwise whether any thread is runnable.
fn any_runnable(pid: u32) -> Option<bool> {
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?;

    let mut saw_any = false;
    for task in tasks.flatten() {
        let Ok(stat) = std::fs::read_to_string(task.path().join("stat")) else {
            continue; // thread exited mid-scan
        };

        // The state field follows the parenthesised comm, which may itself
        // contain spaces or parens -- so split on the LAST ')'.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let Some(state) = rest.split_whitespace().next() else {
            continue;
        };

        saw_any = true;
        if state == "R" {
            return Some(true);
        }
    }

    saw_any.then_some(false)
}
