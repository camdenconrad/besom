//! `besom` — drive a cFS instance on a simulated clock.
//!
//!   besom run   [ticks]   run a scenario, print the telemetry transcript
//!   besom check [ticks]   run it twice and verify the stream is reproducible

use anyhow::{bail, Result};
use besom::run::{self, Config};

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
            let t = run::run(&cfg)?;
            print!("{}", t.render());
            eprintln!(
                "\n{} packets over {:.1}s of simulated time",
                t.len(),
                f64::from(ticks) * besom::TICK_USEC as f64 / 1e6
            );
        }

        "check" => {
            eprintln!("run 1/2...");
            let a = run::run(&cfg)?;
            eprintln!("run 2/2...");
            let b = run::run(&cfg)?;

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

            if shifted.is_empty() {
                println!("tick placement: identical");
            } else {
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
            }
        }

        other => bail!("unknown command {other:?} (expected: run, check)"),
    }

    Ok(())
}
