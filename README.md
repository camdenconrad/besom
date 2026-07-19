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
in ways that have nothing to do with the code under test.

NASA's own small-sat simulator, NOS3, does distribute a simulated clock — its OSAL fork carries a
whole `src/os/nos/` layer, and the timebase reads it
([`os-impl-timebase.c:365`](https://github.com/nasa-itc/osal/blob/master/src/os/nos/src/os-impl-timebase.c)).
So the interesting difference is not whether cFS's clock is simulated. It is **who decides when the
clock moves**.

In NOS3 the tick source paces itself against the wall clock and publishes fire-and-forget
([`nos_time_driver/src/time_driver.cpp`](https://github.com/nasa-itc/nos_time_driver)):

```cpp
do {
    gettimeofday(&_now, NULL);
    _last_time_diff = time_diff();
} while (_last_time_diff < _real_microseconds_per_tick);   // spin until REAL time passes
...
_time_bus_info[i].time_bus->set_time(_time_counter);       // publish; do not wait for anyone
```

Nothing acknowledges the tick and nothing waits for the flight software to finish reacting to it.
The `+`/`-` keys change `_real_microseconds_per_tick` — it is a *speed-up* knob (capped at "no
faster than 200x real time"), which makes simulated time run faster or slower than the wall clock
uniformly, not independent of it. How much work cFS completes per tick therefore still depends on
how fast the host is and what else it is doing. NOS3 does not claim run-to-run reproducibility, and
does not test for it.

Besom's clock does not move until the harness says so, and the harness does not say so until cFS
has stopped reacting to the previous tick. That is the whole difference, and it is why two runs can
be compared byte-for-byte instead of within a tolerance.

### TrickCFS got there first

[TrickCFS](https://github.com/nasa/trickcfs) synchronises cFS with NASA's Trick simulation
executive, and it is closer to this design than anything else I found. Read before claiming
novelty:

* Its PSP timebase reads Trick's clock, not the host's — `exec_get_time_tics()` in
  `psp/fsw/Trick-pc-linux/src/cfe_psp_timebase_posix_clock.c`, printing *"Using Trick executive
  clock as CFE timebase"* on init. `os-impl-posix-gettime.c` does the same.
* It takes over the timed-wait primitives too, which is the half most attempts miss:
  `OS_TaskDelay_Impl` is `SCH_TRICK_schedule_delay(taskId, millisecond)` — handed to Trick's
  scheduler, not `clock_nanosleep`. The queue layer is replaced outright (`TrickCFSQueue_*`).
* It has a completion-synchronisation mechanism, `SCH_TRICK_mark_pipe_as_complete`, so the
  executive knows when a pipe is done rather than assuming.

So "drive cFS from a simulated executive clock, and fix the OSAL primitives that would otherwise
escape it" is **not** a novel idea, and this project did not invent it.

What is different is the purpose, and therefore the contract. TrickCFS exists to couple cFS to an
integrated vehicle simulation; I found no claim of run-to-run reproducibility anywhere in the
repository and no test comparing two runs. Besom exists to make the transcript diffable, which is a
narrower goal that buys a stronger guarantee: identical packets on identical ticks, asserted by
`besomctl check`, and CI that fails when it stops being true. The cost side differs too — TrickCFS
needs Trick, a large simulation framework in its own right, where Besom is a standalone binary and
four patches.

If you are already a Trick shop, use TrickCFS. Besom is for the case where you want reproducible
cFS regression tests on a laptop without adopting a simulation environment to get them.

Credit where due: NOS3 is far ahead of Besom on nearly everything else — roughly twenty simulated
components speaking real UART, I2C, SPI and CAN, [42](https://github.com/ericstoneking/42) for
dynamics, four ground-system options, and a packaged container. It is a fuller simulator. It is not
a deterministic one.

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

**What cannot run cooperatively.** Code that waits for *many* ticks of mission time while holding
exclusive execution deadlocks under `BESOM_COOP=1`, and this is structural rather than a bug. The
acknowledgement for a tick is sent when the PSP re-enters its sync call — that is what proves the
previous tick was fully dispatched — and re-entering needs the cooperative token. So exactly one
tick can be in flight. A task that spins until the clock advances is therefore waiting for
something that cannot happen until it yields.

cFS's own `cfe_testcase` suite contains one: `sb_performance_test` calibrates CPU speed by spinning
until `CFE_PSP_GetTime` advances 100 ms. Perfectly reasonable against a real clock, unsatisfiable
against a granted one. `besomctl` detects the missing acknowledgement and says so, naming the cause
rather than reporting a bare socket timeout, and cFS's log carries `COOP-STALL` lines identifying
the task holding the token.

Such workloads run fine under `BESOM_COOP=0`, where the timebase thread is not gated by the token —
you keep the stream guarantee and lose tick-placement determinism. The alternative would be to
acknowledge a tick on consumption rather than on completion, which would make every run
irreproducible; that trade is not worth making for a benchmark.

Within a single granted tick, cFE's tasks are *simultaneous* in simulated time. Left to the host
scheduler, *which* of them runs first is Linux's choice, and placement jitters for a minority of
packets: *what* the flight software does would be deterministic, *when* it does it would not. Closing
that gap needed a cooperative scheduler inside OSAL, not a better clock — which is what `BESOM_COOP`
now is, and why placement is reproducible above rather than merely tolerated. How it was actually
solved, and the two obvious ideas that do not work, are below.

The run's *edge* is handled at source rather than tolerated: the harness stops **recording** a guard
band of ticks before it stops **granting time**. Otherwise a packet emitted on the final tick may or
may not have reached the socket, and a periodic app whose timer was armed during un-gated boot fires
N or N+1 times over a fixed budget — the transcript's last packet then appears and disappears between
runs, which is the run's edge moving, not the software behaving differently.

**Under `BESOM_COOP=0`, assert on the stream and never on tick placement.** Host scheduling gives you
the stream guarantee only, and a placement assertion there is an assertion about Linux's scheduler.
`Transcript::same_stream` is that weaker check: it catches a dropped packet, a duplicate, a wrong
size, a reordering. With coop scheduling on (the default), `besomctl check` holds both, and fails on
a difference in either.

**It does not catch a wrong value.** `Entry::stream_key` is `(msg_id, seq_delta, len)`, and
`TlmPacket` parses only the 12-byte CCSDS header — no payload byte is ever retained, so a packet
whose contents are wrong but whose length is unchanged is invisible to the comparison. An app that
publishes a stale latitude or a wrong counter passes `check` today. Payload comparison is the single
most valuable thing to add next, and it is not a small change: it needs a rule for fields that may
legitimately differ before it can be asserted on.

Two narrower gaps in the same comparison, both real:

* A packet dropped at the **leading** edge of a MID's stream is forgiven. The first packet of each
  MID is recorded with `seq_delta: None`, so losing it promotes the next one into that slot and the
  prefix realigns; the length difference of one is then absorbed by the trailing-boundary tolerance.
  The guard band pins the trailing edge only.
* `besomctl check N` for `N <= 120` used to record nothing and report it as reproducible. Two empty
  transcripts compare equal, so it printed `0 packets, identical` and exited 0 — the same green
  result a dead downlink would give. Budgets inside the guard band are now refused outright.

### Host load is the one thing that still gets in

Reproducibility was measured against the two ways a CI runner differs from a workstation — how many
cores it has, and how busy they are. 15 trials per condition, 3000 ticks, `BESOM_COOP=1`:

| condition | placement identical |
|---|---|
| idle, 16 cores | 15/15 |
| idle, 2 cores | 15/15 |
| loaded, 16 cores | 14/15 |
| loaded, 2 cores | 11/15 (one failed outright) |

**Core count is not the problem.** Pinned to two cores — a runner's shape — placement held 15/15 on
the same 382 packets. Besom does not need a big machine.

**Contention is.** And every failure has the same shape: about a quarter of the stream moved, by a
maximum of *exactly* 10 ticks. Not jitter — one discrete one-cycle phase slip in `besom_io` (10 Hz
= 10 ticks), after which the affected streams are stamped one cycle over.

The leak is the harness's own, and it is not the obvious one. Tick placement is stamped from the
packet's own cFE header — simulated time, frozen for a whole granted tick — so intra-tick
scheduling noise *cannot* move a packet between ticks. Only a decision at a tick boundary can, and
that decision is `quiesce::wait`.

The natural suspect is its deadline: it gives up after `$BESOM_QUIESCE_MS` and grants the next
tick anyway. **Measurement refuted that** — under load, shifts happen with `timeouts() == 0`. The
deadline is never reached, so raising it does nothing.

`wait` returns after 3 consecutive samples showing no runnable cFS thread, polled every 400 µs — a
1.2 ms confirmation window. Under contention the lag between granting a tick and the woken thread
actually being marked `R` exceeds that window, so all three samples come up clean and the harness
declares quiescence *before cFS has begun reacting*. A false quiescence, and unlike an expiry it
increments nothing: `stalls == 0` is not evidence the harness behaved, only that it did not
notice. `$BESOM_QUIESCE_SAMPLES` exposes the window.

`check` now separates the two ways placement can differ: **NOT REPRODUCIBLE** (placement moved,
quiescence clean) from **INCONCLUSIVE** (placement moved and the harness stalled — the host was too
busy for the run to mean anything). Conflating those is how a determinism harness earns a
reputation for flakiness and then gets ignored.

Full method and numbers: [docs/determinism-under-load.md](docs/determinism-under-load.md).

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

# Not a Besom change: nasa/PSP has a typo'd header guard (OVERRIDE_TOOIMPL_H vs
# OVERRIDE_TODIMPL_H) in an RTEMS coverage stub. cFS builds the coverage targets during
# `make install`, so -Werror=header-guard fails the build on any recent GCC. Carried here
# only so these instructions complete; it belongs upstream.
git -C cFS/psp  apply /path/to/besom/patches/psp-header-guard.patch

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

Usable. cFS is byte-deterministic under the harness — same packets, same ticks, run after run — the
closed loop verifies, and you can write a cFS app and regression-test it today.

Deterministic tick **placement** was the last gap and is now closed by the cooperative scheduler; it
is what `BESOM_COOP` buys you. Two approaches were tried before it and neither works — they are
written up below, so nobody repeats them.

Known gaps, in the order they matter:

1. **More device simulation.** `besom_io` proves the path; real sensors (star tracker, IMU, GPS)
   belong behind the OSAL/PSP `iodriver` seam so flight apps talk to them over the buses they would
   really use.
2. **J2 and a real attitude model.**

Licensed Apache-2.0, matching cFS.
