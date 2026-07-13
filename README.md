# besom

**A deterministic simulation harness and ground station for NASA's [core Flight System](https://github.com/nasa/cFS).**

Besom owns cFS's clock. Real flight software — unmodified cFE — runs against it, but simulated time
advances only when Besom grants a tick. Run the same scenario twice and you get the same telemetry,
which is what makes it usable for regression testing, and what a harness paced to wall time cannot
give you.

It also propagates a real orbit on that same clock, so you can *watch* the spacecraft fly. Pause,
and the vehicle stops in the sky at the exact instant the flight software stopped.

```
besom          # ground station: orbit view, live telemetry, event log, commanding
besomctl run   # boot cFS on the simulated clock, print a telemetry transcript
besomctl check # run a scenario twice; fail if the packet stream differs
besomctl loop  # feed vehicle state into cFS; fail if it reports anything stale
```

## Why

cFS is what flies. But you cannot test flight software properly against a clock you do not control:
the scheduler ticks on wall time, tasks wake on wall time, and two runs of the same scenario differ
in ways that have nothing to do with the code under test. NASA's own small-sat simulator, NOS3, runs
cFS on the **real** clock — I checked its OSAL and PSP forks; there is no simulated timebase in
either.

Besom gives cFS a clock that a test can drive.

## How it works

cFS's Platform Support Package (PSP) already exposes the timebase as a **swappable module**, and
OSAL's `OS_TimeBaseCreate` accepts an *external sync function* that blocks until the next tick and
reports elapsed time. That is the seam. `timebase_besom` is a PSP module whose sync function blocks
on a UNIX socket, so cFE time advances only when the harness sends a step.

**No changes to cFE core are required.**

Two things turned out to be load-bearing and are not obvious:

**You must replace both stock timebase modules.** `soft_timebase` is the tick source, but
`timebase_posix_clock` owns `CFE_PSP_GetTime`, which is what *stamps telemetry*. Swap only the tick
source and every packet still carries a wall-clock timestamp.

**Every OSAL primitive that blocks with a timeout must be driven by the simulated clock**, not just
`OS_TaskDelay`. `OS_QueueGet` with a timeout is what `CFE_SB_ReceiveBuffer(pipe, TIMEOUT)` becomes,
and it pends on the host clock via `mq_timedreceive` — so any app that waits with a timeout (cFS's
own HS app does) wakes on wall time and escapes the simulation entirely, no matter how precisely you
gate ticks. Same for the timed semaphore waits. See `patches/osal-simulated-time.patch`.

## What is reproducible — and what is not

Measured over 30 s of simulated time, 87 packets, the full cFS app set:

- **The packet stream is exactly reproducible, every run.** Per message: the same count, the same
  CCSDS sequence deltas, the same lengths. Nothing dropped, duplicated, reordered, or invented.
- **Tick placement still jitters.** A minority of packets land a tick or two early or late.

Within a single granted tick, cFE's tasks are *simultaneous* in simulated time — and the **host**
scheduler decides which of them actually runs first. So *what* the flight software does is
deterministic; *when* it does it is not, for a minority of packets. Closing that gap needs a
cooperative scheduler inside OSAL, not a better clock.

The run's *edge* is handled at source rather than tolerated: the harness stops **recording** a guard
band of ticks before it stops **granting time**. Otherwise a packet emitted on the final tick may or
may not have reached the socket, and a periodic app whose timer was armed during un-gated boot fires
N or N+1 times over a fixed budget — the transcript's last packet then appears and disappears between
runs, which is the run's edge moving, not the software behaving differently.

**So assert on the stream, never on tick placement.** That catches every real defect — a dropped
packet, a duplicate, a wrong size, a reordering, a wrong value — without asserting on Linux's
scheduler. `Transcript::same_stream` does exactly this; `besomctl check` fails on a stream difference
and merely *reports* placement jitter.

## Closing the loop

`besom_io` (in `cfs/`) is a cFS app that runs *inside* the flight executive. It receives simulated
vehicle state from the harness and publishes it on the software bus, so flight apps consume
spacecraft state over the interfaces they would really use — and the ground station reads state back
that has travelled **through** cFS, not Besom's own copy of it.

Two details keep it deterministic, and both are the difference between a sim and a toy:

* It wakes on an OSAL timer bound to `cFS-Master` — the timebase Besom steps — so it runs on
  simulated time.
* It reads its socket **non-blocking**, and **drains to the newest sample**. Reading one datagram
  per cycle while the harness sends one per tick consumes the queue slower than it fills: the
  backlog grows without bound and the published state falls steadily further into the past. It
  looks like a small plausible lag and is in fact unbounded drift. A sensor reports what is true
  *now*.

`besomctl loop` proves it: it feeds state in and fails if the flight software reports anything stale.

```
$ besomctl loop 600
cFS accepted 590 state updates (0 malformed)
worst disagreement: lat 0.000506deg  lon 0.000401deg
closed loop verified: cFS reports exactly the state it was given
```

## Running it

You need a cFS build carrying the Besom PSP module, the OSAL simulated-time changes, and the
`besom_io` app. From a cFS bundle checkout:

```sh
cp -r /path/to/besom/cfs/besom_io cFS/apps/besom_io

git -C cFS/psp  apply /path/to/besom/patches/psp-timebase-besom.patch
git -C cFS/osal apply /path/to/besom/patches/osal-simulated-time.patch
git -C cFS      apply /path/to/besom/patches/cfs-mission-config.patch

cd cFS
CMAKE_POLICY_VERSION_MINIMUM=3.5 make native_std.install   # cmake ≥4 needs the policy shim
```

Then point Besom at the build:

```sh
export BESOM_CFS_DIR=~/cFS/build-native_std/exe/cpu1
cargo run --release --bin besomctl -- check 3000
cargo run --release --bin besom            # the ground station
```

Without `$BESOM_STEP_SOCK` set, `timebase_besom` free-runs exactly like stock `soft_timebase`, so a
patched PSP stays usable for ordinary (non-simulated) runs.

## The dynamics

Two-body gravity, fixed-step RK4, integrated on the simulated tick — so the trajectory is
reproducible for the same reason the telemetry is. It is honest about what it is not: no J2, no
drag, no third bodies, and the attitude model is nadir-pointing with a roll rate rather than a real
quaternion/inertia propagation. J2 is the first thing to add when LEO accuracy starts to matter.

The orbit view is drawn with egui's shape API rather than a bespoke GPU pipeline: at this scale the
scene is a sphere, a trail and a few axes, and a second renderer would be cost without benefit.

## Layout

| | |
|---|---|
| `clock` | the step protocol — Besom's half of `timebase_besom` |
| `quiesce` | waits until cFS has finished reacting to a tick |
| `ccsds` / `evs` | packet codec, event-message decoding |
| `transcript` | what is and is not assertable across runs |
| `session` | a live, operator-driven run |
| `run` | scripted scenarios |
| `dynamics` | orbit + attitude propagation |
| `fsw` | the link to `besom_io`, the sensor bridge inside cFS |
| `view3d` / `gui` | the ground station |

## Status

A working harness and ground station, not a finished product. Known gaps, in the order they matter:

1. **Deterministic intra-tick scheduling** — the last thing between this and a byte-deterministic
   cFS. (Pinning cFS to a single CPU does *not* achieve it — measured, and it made placement worse.
   It needs cooperative scheduling inside OSAL, so that only one task runs at a time in a defined
   order.)
2. **More device simulation.** `besom_io` proves the path; real sensors (star tracker, IMU, GPS)
   belong behind the OSAL/PSP `iodriver` seam so flight apps talk to them over the buses they would
   really use.
3. **J2 and a real attitude model.**

Licensed Apache-2.0, matching cFS.
