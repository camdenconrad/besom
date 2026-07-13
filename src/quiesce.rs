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

use std::thread::sleep;
use std::time::{Duration, Instant};

/// Consecutive all-blocked samples required before declaring quiescence.
const CLEAN_SAMPLES: u32 = 3;
const POLL: Duration = Duration::from_micros(400);
const TIMEOUT: Duration = Duration::from_secs(2);

/// Block until no thread of `pid` is runnable, or the timeout elapses.
///
/// Timing out is not an error: it means the process is genuinely busy, and
/// stalling the whole run over it would be worse than proceeding.
pub fn wait(pid: u32) {
    let deadline = Instant::now() + TIMEOUT;
    let mut clean = 0;

    while Instant::now() < deadline {
        match any_runnable(pid) {
            None => return, // process gone; the caller will notice
            Some(true) => clean = 0,
            Some(false) => {
                clean += 1;
                if clean >= CLEAN_SAMPLES {
                    return;
                }
            }
        }
        sleep(POLL);
    }
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
