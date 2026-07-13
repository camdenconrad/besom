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

**Runs are byte-identical.** Same packets, same order, same lengths, same sequence deltas — and
every packet on the *same tick*. Verified 11/11 across 6 s, 30 s and 60 s scenarios.

Four things are load-bearing, and each was a separate bug:

**Cooperative scheduling** (`$BESOM_COOP`, on by default). Only one OSAL task runs at a time, and the
token passes to the lowest-numbered *ready* task, keyed by OSAL task id — creation order, fixed by the
startup script. Crucially, **readiness is set by the waker**: semaphores and queues never pend in the
kernel, they poll and park on a coop channel, and the task performing the give/put marks the receiver
ready *at that instant, while holding the token*. Timer callbacks give semaphores, so this is the path
by which a tick makes tasks runnable. Anything that pends on the *host* (sockets, real sleeps, the
harness itself) leaves the ready set entirely.

**Step through boot.** An app that sleeps in its loop (cFS's HS does) takes its cycle phase from
whichever clock is running. Wait idle for cFS to boot and those sleeps run on the *host* clock, so
where the app's cycle lands when ticks begin depends on how fast the machine booted — HS's entire
stream shifted by exactly one of its own periods. Grant ticks from the first instant instead.

**A fixed boot budget.** Stepping *until the log says OPERATIONAL* is a host-timed condition: a
slower boot consumes more ticks and every app's sub-phase moves with it. The harness grants a fixed
number of boot ticks (padding past OPERATIONAL), so the phase is identical every run.

**Phase-align the recorded window.** Don't start counting at tick 1 — step until the system reaches
a known point in its cycle (the first cFE ES housekeeping packet), open the window there, and stop
recording a guard band before you stop granting time. Both ends pinned to the same phase.

Set `BESOM_COOP=0` to fall back to host scheduling: faster, but then only the *stream* is guaranteed,
not tick placement.

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

### Bringing up the downlink

TO_LAB boots *awaiting an enable command*: it downlinks nothing until the ground station tells it
where to send. That command goes out over UDP, and one more bug is worth knowing.

**Do not uplink before CI_LAB has bound its port.** `Cfs::boot` returns as soon as the PSP binds the
step socket — deliberately, because the caller must grant ticks *while* cFS boots. CI_LAB binds UDP
1234 some time after that. A command sent into the gap is a datagram to a closed port: dropped, in
silence. TO_LAB then sits at "Awaiting enable command" forever, no telemetry is ever downlinked, and
cFS, the timebase and the tick stream all look perfectly healthy — the ground station simply shows an
empty sky. Wait for the bind, resend until TO_LAB acknowledges with `TO_LAB_TLMOUTENA_INF_EID`, and
fail loudly rather than fly blind.

Waiting on the wrong event hides this rather than catching it: `TO_LAB 19` (*subscribed to the table*)
is a **boot** event and fires whether or not the enable ever arrived.

## Intra-tick determinism: how it was actually solved

Two dead ends first, recorded because both are the obvious ideas:

**Pinning cFS to a single CPU does not help.** If the tasks cannot run simultaneously, surely the
kernel must order them? Measured: placement jitter got *worse*. The quiescence poller ends up
contending with cFS for the same core.

**A cooperative token scheduler alone is not sufficient.** Ordering only the tasks *already waiting*
for the token leaves the race intact: when a tick wakes several tasks, each becomes a waiter whenever
its thread happens to get scheduled off its semaphore, so a fast-waking high-numbered task can take
the token before a low-numbered one has even arrived to ask for it. Readiness must be established by
the **waker**, synchronously — which is what the shipped scheduler does.

Two bugs found while getting there, both worth knowing:

*cFE's main thread is not an OSAL task*, so it never joins the scheduler. It was entering the
cooperative paths, where `Block` is a no-op, and spinning forever instead of pending. The coop path
must be gated on "this thread is in the schedule", not "the scheduler exists".

*Any wait that blocks on the host must leave the ready set.* The timed-wait fallbacks were gated on
simulated time being active — but during boot no ticks have been granted yet, so they fell through to
the host path **while holding the token**. cFS's ES background task waits there for 999 ms at a time,
which deadlocked boot outright: it held the token while every other task sat in `TakeToken`. gdb said
so in one stack.

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

Usable. The stream guarantee holds run after run, the closed loop verifies, and you can write a cFS
app and regression-test it today.

Known gaps, in the order they matter:

1. **Deterministic tick PLACEMENT.** The stream is reproducible; *which tick* a packet lands on is
   not, for a minority of packets. This does not affect regression testing (assert the stream), but
   it is what stands between this and a byte-deterministic cFS. Two approaches have been tried and
   neither works; see below. Don't repeat them.
2. **More device simulation.** `besom_io` proves the path; real sensors (star tracker, IMU, GPS)
   belong behind the OSAL/PSP `iodriver` seam so flight apps talk to them over the buses they would
   really use.
3. **J2 and a real attitude model.**

Licensed Apache-2.0, matching cFS.
