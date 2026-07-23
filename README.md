# Besom

A deterministic simulation harness for NASA's cFS (core Flight System) — it owns the clock, so an identical scenario run twice produces byte-identical telemetry.

cFS normally advances time off the wall clock and the host scheduler, which makes bit-for-bit reproducibility across runs essentially impossible. Besom replaces cFS's timebase with one it drives itself over a private socket, then closes off the other leaks (timed-wait primitives, thread scheduling order, boot timing, reset state) that would otherwise reintroduce nondeterminism. The result: you can run the same scenario twice and diff the telemetry streams byte-for-byte, which makes cFS behavior something you can actually test and regress against.

## What it does

- **Owns the clock.** A PSP module (`timebase_besom`) replaces cFS's stock timebase and posix-clock modules; Besom's `Clock` grants ticks one at a time over a Unix datagram socket and only advances once the tick is fully dispatched.
- **Closes the other nondeterminism leaks**, layered on top of the clock:
  - OSAL timed-wait primitives (`OS_TaskDelay`, `OS_QueueGet`, `OS_BinSemTimedWait`, `OS_CountSemTimedWait`) patched to poll simulated time instead of the real clock.
  - A cooperative scheduler (`BESOM_COOP`) that hands off a single token per OSAL task, set synchronously by the waker rather than polled by the waiter.
  - A quiescence gate that waits for all threads to go non-runnable (via `/proc/<pid>/task/*/stat`) before granting the next tick — the documented residual source of nondeterminism under host load.
  - Fixed boot budgets and phase-aligned transcript windows, instead of "wait until the log says OPERATIONAL."
  - Forced power-on reset between comparison runs, since PSP reserved memory otherwise persists across runs.
- **Closes the loop.** A companion cFS app, `besom_io`, reads simulated vehicle state through a PSP accessor and publishes it as telemetry, so cFS observes the same simulated time and the vehicle state that belongs to it together, in the same tick.
- **Compares runs byte-for-byte.** `besomctl check` runs a scenario twice and diffs the resulting transcripts, reporting tick-placement differences and distinguishing a genuine reproducibility failure from an inconclusive run (a quiescence stall).
- **Ships a ground station GUI** (`besom`, egui/eframe-based) with a telemetry grid, event log, and a 3D orbit view, for watching a scenario live.

## Architecture

- `src/clock.rs` — Besom's half of the timebase wire protocol: sends `step_usec` (+ optional sensor bytes), blocks for a `sim_usec` ack.
- `src/quiesce.rs` — polls thread state to detect quiescence before granting the next tick.
- `src/run.rs` — scripted scenario runner: boot budget, guard bands, phase alignment.
- `src/transcript.rs` — records and diffs telemetry streams (`same_stream`, `max_shift_ticks`, `payload_differences`).
- `src/ccsds.rs` / `src/evs.rs` / `src/fsw.rs` — CCSDS packet codec, event telemetry decoding, and the vehicle-state wire format shared with `besom_io`.
- `src/dynamics.rs` — fixed-step RK4 two-body orbit propagation with simple nadir-pointing attitude.
- `src/session.rs` — a live, operator-driven run used by the GUI; explicitly not reproducible the way `run.rs` scenarios are.
- `cfs/besom_io/` — the in-flight-executive C app that bridges simulated sensor state into cFS telemetry over a lock-free ring buffer.
- `patches/` — the four patches applied to a stock `nasa/cFS` checkout (timebase module, OSAL simulated-time primitives, mission config, an unrelated upstream build fix).

## Building

Requires a patched build of cFS. `ci/build-cfs.sh` is the canonical, executable version of these steps:

```sh
cp -r cfs/besom_io  cFS/apps/besom_io
git -C cFS/psp  apply patches/psp-timebase-besom.patch
git -C cFS/osal apply patches/osal-simulated-time.patch
git -C cFS      apply patches/cfs-mission-config.patch
git -C cFS/psp  apply patches/psp-header-guard.patch
cd cFS && CMAKE_POLICY_VERSION_MINIMUM=3.5 make native_std.install

export BESOM_CFS_DIR=~/cFS/build-native_std/exe/cpu1
cargo run --release --bin besomctl -- check 3000   # run a scenario twice, diff the telemetry
cargo run --release --bin besom                    # ground station GUI
```

`besomctl` builds headless with `cargo build --no-default-features` (no GUI deps); the `besom` GUI binary requires the default `app` feature.

Useful env vars: `BESOM_STEP_SOCK`, `BESOM_STEP_TIMEOUT_S`, `BESOM_COOP`, `BESOM_QUIESCE_MS`, `BESOM_QUIESCE_SAMPLES`, `BESOM_BOOT_TIMEOUT_S`.

## Status

Early (`0.1.0`) but working: the clock hand-off, cooperative-scheduler tick-placement determinism, and the closed sensor loop are in place and exercised in CI against a real, pinned cFS build. Known gaps, stated plainly:

- Payload/value comparison exists (`payload_differences`) but `check` doesn't yet assert on it — only tick placement is currently enforced.
- Only one simulated device (`besom_io`'s vehicle state) proves the path; no star tracker, IMU, or GPS simulation yet.
- Dynamics model is two-body + nadir-pointing only — no J2, no quaternion attitude.
- The quiescence gate doesn't distinguish a false-quiescent `D`-state (uninterruptible sleep) thread from a genuinely idle one.
- `session.rs` (the live GUI mode) is not reproducible the same way scripted `run.rs` scenarios are.
- `cargo fmt`/`clippy` are intentionally not yet CI-gated.

## License

Apache-2.0, matching cFS's own license.
