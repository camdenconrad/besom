//! `besom` — drive a cFS instance on a simulated clock.
//!
//!   besom run   [ticks]   run a scenario, print the telemetry transcript
//!   besom check [ticks]   run it twice and verify the stream is reproducible

use anyhow::{bail, Result};
use besom::quiesce;
use besom::run::{self, Config};

/// Say so when the harness granted time while cFS was still reacting.
///
/// `check` compares two runs and can weigh a stall against the result, but `run` and `loop`
/// print a single authoritative-looking transcript. A stalled run is still worth printing --
/// it is just not worth *trusting* as reproducible, and only saying so makes that visible.
fn warn_if_stalled() {
    let stalls = quiesce::timeouts();
    if stalls > 0 {
        eprintln!(
            "warning: quiescence timed out {stalls} time(s) -- the harness granted ticks while \
             cFS was still reacting, so this run is not reproducible. Re-run on an idle host, \
             or raise $BESOM_QUIESCE_MS."
        );
    }
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| "run".into());
    let ticks: u32 = args.next().map_or(Ok(600), |t| t.parse())?;

    let cfg = Config {
        cfs_dir: run::default_cfs_dir(),
        step_sock: "/tmp/besom.sock".into(),
        ticks,
    };

    if !cfg.cfs_dir.join("core-cpu1").exists() {
        bail!(
            "no cFS build at {} (set $BESOM_CFS_DIR)",
            cfg.cfs_dir.display()
        );
    }

    match cmd.as_str() {
        "run" => {
            quiesce::reset();
            let t = run::run(&cfg)?;
            print!("{}", t.render());
            eprintln!(
                "\n{} packets over {:.1}s of simulated time",
                t.len(),
                f64::from(ticks) * besom::TICK_USEC as f64 / 1e6
            );
            warn_if_stalled();
        }

        "check" => {
            // Coop scheduling is what makes tick PLACEMENT reproducible; without it only the
            // stream is, and asserting placement would be asserting about Linux's scheduler.
            //
            // This predicate MUST match OSAL's exactly. OSAL enables the scheduler with
            // `OS_Coop.enabled = (env != NULL && env[0] == '1')` (patches/osal-simulated-time
            // .patch), so anything not starting with '1' -- "true", "yes", "on", "2" -- leaves
            // cFS on HOST scheduling. A looser test here (`!= "0"`) would have the harness
            // assert tick placement as a hard guarantee for a run whose scheduler was never
            // switched on, and then blame the flight software for Linux's decisions.
            let coop = std::env::var("BESOM_COOP")
                .unwrap_or_else(|_| "1".into())
                .starts_with('1');

            eprintln!("run 1/2...");
            quiesce::reset();
            let a = run::run(&cfg)?;
            let stalls_a = quiesce::timeouts();

            eprintln!("run 2/2...");
            quiesce::reset();
            let b = run::run(&cfg)?;
            let stalls_b = quiesce::timeouts();
            let stalls = stalls_a + stalls_b;

            // Two empty transcripts compare equal, so every assertion below would pass while
            // measuring nothing -- the same green build you would get with the downlink dead.
            // run.rs rejects a budget inside the guard band; this catches an empty sky.
            if a.is_empty() || b.is_empty() {
                bail!(
                    "recorded no telemetry ({} and {} packets) -- nothing was verified. \
                     Is the downlink up?",
                    a.len(),
                    b.len()
                );
            }

            if !a.same_stream(&b) {
                bail!(
                    "NOT REPRODUCIBLE: the packet streams differ ({} vs {} packets)",
                    a.len(),
                    b.len()
                );
            }

            let shifted = a.differences(&b);
            let max = a.max_shift_ticks(&b);
            println!("stream reproducible: {} packets, identical", a.len());

            // A quiescence timeout means the harness granted a tick while cFS was still
            // reacting to the previous one -- a host-timed decision. Report it whatever the
            // outcome: a CLEAN run that stalled was lucky, not proven, and saying so is the
            // difference between a measurement and a coin flip that landed the right way.
            if stalls > 0 {
                println!("quiescence: {stalls} timeout(s) ({stalls_a} + {stalls_b}) -- host too busy");
            }

            if shifted.is_empty() {
                println!("tick placement: identical");
                return Ok(());
            }

            println!(
                "tick placement: {}/{} packets shifted, max {max:.1} tick(s)",
                shifted.len(),
                a.len()
            );
            for (mid, x, y) in &shifted {
                let ticks = match (x, y) {
                    (Some(x), Some(y)) => (x - y).abs() / (besom::TICK_USEC as f64 / 1e6),
                    _ => 0.0,
                };
                println!("  {mid:04x}  {ticks:.0} tick(s)");
            }

            // Placement is a GUARANTEE under coop scheduling, so a difference is a failure and
            // must exit non-zero -- printing it and returning 0 (which is what this did) makes
            // any CI job wired to `check` vacuous for the strongest property Besom offers.
            if !coop {
                println!(
                    "(cooperative scheduling off -- $BESOM_COOP does not start with '1', so cFS \
                     ran host-scheduled. Placement is not guaranteed; stream checked only.)"
                );
            } else if stalls > 0 {
                bail!(
                    "INCONCLUSIVE: placement differs, but quiescence timed out {stalls} time(s). \
                     The host was too loaded to grant ticks cleanly, so this says nothing about \
                     the flight software. Re-run on an idle machine before believing it."
                );
            } else {
                bail!(
                    "NOT REPRODUCIBLE: {}/{} packets moved between runs (max {max:.1} tick(s)) \
                     with quiescence clean throughout -- the flight software's timing changed.",
                    shifted.len(),
                    a.len()
                );
            }
        }

        "loop" => {
            // Prove the sensor feed keeps up: run cFS, push vehicle state in, and
            // measure how far the flight software's reported position lags the
            // truth we sent it.
            quiesce::reset();
            let (t, l) = run::run_with_loop(&cfg)?;
            warn_if_stalled();
            println!("{} packets, {} state reports from the flight software", t.len(), l.samples);

            if l.samples == 0 {
                bail!("cFS never reported vehicle state -- is besom_io in the build?");
            }

            let Some(f) = l.last else { bail!("no state") };
            println!(
                "cFS accepted {} state updates ({} malformed)",
                f.rx_count, f.rx_err_count
            );
            println!(
                "worst disagreement: lat {:.6}deg  lon {:.6}deg",
                l.max_lat_deg, l.max_lon_deg
            );

            // Both sides are the same f64 over a lossless local link. A real
            // divergence means cFS is reading a backlogged socket and reporting
            // state from the past.
            if l.max_lat_deg > 0.01 || l.max_lon_deg > 0.01 {
                bail!("the flight software is reporting STALE state -- the sensor feed is falling behind");
            }
            println!("closed loop verified: cFS reports exactly the state it was given");
        }

        other => bail!("unknown command {other:?} (expected: run, check, loop)"),
    }

    Ok(())
}
