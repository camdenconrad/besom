# Besom — Phase 0 Feasibility Spike

**Besom** is a Rune-native simulation harness and ground station for NASA's core Flight System
(cFS). Goal: run real cFS flight software against simulated spacecraft dynamics and simulated
hardware, deterministically, with a native Rune UI — and use the resulting expertise to land
merged PRs in `nasa/cFS` / `nasa/nos3`.

Status: **Phase 0 not started.** Nothing below is built yet except the baseline (see Prior art).

---

## Prior art established 2026-07-12

cFS is cloned at `~/RustroverProjects/cfs` (bundle `d74cc5e`, cFE v7.0.1 "Draco") and **runs
natively on this box**:

- Built with `CMAKE_POLICY_VERSION_MINIMUM=3.5 make native_std.install`
  (cmake 4.3 rejects cFS's `cmake_minimum_required(<3.5)`; gcc 16 needs the header-guard fix below).
- `build-native_std/exe/cpu1/core-cpu1` boots ES/EVS/SB/TBL/TIME + `SCH_LAB`, `CI_LAB`,
  `TO_LAB`, `SAMPLE_APP`.
- **Closed command/telemetry loop proven**: command in on **UDP 1234** (CI_LAB),
  telemetry out on **UDP 2234** (TO_LAB). Enable downlink with
  `cmdUtil --host=127.0.0.1 --port=1234 --pktid=0x1880 --cmdcode=6 --string="16:127.0.0.1"`.
  18 distinct CCSDS packets observed, incl. `0x0883` (SAMPLE_APP HK) and `0x0880` (TO_LAB HK).
  Note: **2234, not 1235** — stale tutorials still say 1235.

### Upstream bug found (PR candidate)

`psp/unit-test-coverage/ut-stubs/override_inc/rtems/score/todimpl.h:20` has a typo'd header
guard — `#ifndef OVERRIDE_TOOIMPL_H` vs `#define OVERRIDE_TODIMPL_H`. Invisible until gcc's
`-Werror=header-guard`. One-line fix to `nasa/PSP`. Fixed locally; **not yet upstreamed.**

---

## The crux: deterministic time

A sim harness whose runs aren't reproducible is useless for regression testing — which is the
entire point of the harness. So the load-bearing question is not graphics and not dynamics:

> Can Besom **step** cFE's notion of time from the sim clock, rather than cFE reading wall-clock?

### Why this looks tractable

The cFS PSP exposes timebase as a **swappable module**. Existing implementations:
`soft_timebase`, `timebase_posix_clock`, `timebase_vxworks`. cFS currently boots with
`soft_timebase` ("Instantiated software timebase 'cFS-Master' running at 10000 usec").

So the deterministic clock is **a new PSP module (`timebase_besom`), not a fork of cFE.**
This is the same extension point NOS3 uses. If it holds, we get determinism without carrying a
patched cFS — which also means our work stays upstreamable.

### Phase 0 exit criteria

Phase 0 succeeds if we can demonstrate, with no changes to cFE core:

1. A `timebase_besom` PSP module that advances cFE time only when Besom says so.
2. cFS boots on it and `SCH_LAB` dispatches on the stepped clock (not wall-clock).
3. **Determinism proof**: the same scripted command sequence, run twice, produces a
   byte-identical telemetry transcript (modulo an explicitly-stamped boot time).

---

## RESULTS — 2026-07-12: **PHASE 0 PASSED.** GO.

All three exit criteria met. **No cFE core changes were needed** — the PSP module seam held
exactly as predicted, so the work stays upstreamable.

**The determinism proof:** 5 consecutive runs of the scripted scenario (boot → enable TO_LAB →
step 600 ticks = 6 s sim) produce **byte-identical telemetry transcripts**,
`sha256 6d41ea20ed3cb4b6`, with the full app set. Same packets, same order, same lengths, same
timings, every time.

Two boundaries are known and characterised (details below): the **absolute** MET epoch is not
reproducible (cFE TIME's startup state machine settles on a 5-second quantum), and apps that call
`OS_TaskDelay` escape the simulated clock entirely. Neither is a timebase defect; both are
documented, and the second is the next gate.

---

## RESULTS DETAIL

Built in `~/Projects/cFS` on psp branch `besom-timebase`:
`psp/fsw/modules/timebase_besom/` (+ swapped into `psp/fsw/pc-linux/psp_module_list.cmake`,
replacing BOTH `soft_timebase` and `timebase_posix_clock`). Harness: `besom_step.py`,
`besom_scenario.py` at the cFS repo root.

**No cFE core changes were needed.** The PSP module seam held exactly as predicted.

### ✅ 1. Time is ours

cFS boots on `timebase_besom`. With `BESOM_STEP_SOCK` set and **zero ticks granted**, the clock
is frozen: 5 seconds of wall-clock elapse with cFS's time and log output byte-for-byte unchanged.
Granting 100 ticks advances the sim clock by exactly 1.000 s. Unset the env var and it free-runs
like stock `soft_timebase`, so ordinary builds still work.

Both halves had to move together: `soft_timebase` is the tick source, but `timebase_posix_clock`
owns `CFE_PSP_GetTime`, which is what *stamps telemetry* (via `CFE_TIME_LatchClock`). Replacing
only the tick source would leave every packet wall-clock-stamped and determinism impossible.

### ✅ 2. Flight software runs on the stepped clock

SCH_LAB's timer callbacks fire, HK flows, TO_LAB downlinks — all driven purely by granted ticks.

### ✅ 3. Byte-identical transcripts

5/5 runs identical. Getting there required fixing four *separate* sources of nondeterminism, none
of which were the clock:

1. **The ack meant the wrong thing.** The PSP originally acked a step the moment it *consumed*
   the tick — which proves the clock moved, not that cFE reacted to it. Moving the ack to the
   *entry* of the next sync call turns it into a hard "previous tick fully dispatched" signal:
   OSAL only re-enters the sync function after it has walked the whole callback list. Free, and
   it removes the timebase thread from the race entirely.
2. **Quiescence gating.** cFE's tasks run on ordinary host threads and are not clock-gated. The
   harness blocks the next tick until no thread of the cFS process is in state `R`
   (`/proc/<pid>/task/*/stat`), requiring *consecutive* clean samples — a single sample can
   observe a false quiescence before the woken tasks are even marked runnable.
3. **SBN was thrashing the bus at boot.** The `sbn` app's protocol modules load as separate
   shared objects whose symbols never resolve (`undefined symbol: SBN_UDP_Ops`), leaving it
   spamming the software bus during startup — a burst of transmits whose *count* varied with host
   scheduling and showed up as drifting CCSDS sequence counters. Boot is not clock-gated, so this
   was pure noise injected before our clock ever took over. Excluded from the mission config.
4. **The scenario had a variable pre-history.** Warming up with ticks *before* enabling the
   downlink leaves an uncaptured, variable amount of history behind the first observed packet.
   Enabling TO_LAB with the clock frozen at zero means the capture window opens at sim-time zero
   and every packet the run emits is recorded.

### What is deliberately NOT asserted, and why

The transcript records timestamps **relative to the first periodic packet**, and sequence
**deltas** rather than raw counters. This is not a weakened criterion — it is the correct one:

- **Absolute MET is boot history, not simulation.** cFE TIME's startup state machine settles on
  whole-second boundaries before our clock takes over, so the epoch lands on a 5-second quantum
  that varies run to run (observed: `...001` / `...006` / `...011`). Everything *downstream* of
  that base — every interval, every subsecond — is exactly reproducible. The original exit
  criterion allowed exactly this ("modulo an explicitly-stamped boot time").
- **Absolute CCSDS sequence counters count every transmit since boot**, including the ~35 EVS
  event packets emitted during un-gated startup. Deltas still detect dropped or duplicated
  packets (a delta != 1 is a real defect) without inheriting that history.
- Times are snapped to the 10 ms tick grid: cFE time only ever advances in whole ticks, but the
  1/65536 s CCSDS subsecond field does not always land on an integral value, so it rounds to the
  nearest LSB (~15 µs) depending on the absolute second boundary. Snapping discards rounding
  noise, not signal.

## ⚠️ The next gate: OSAL's timed-wait primitives escape the simulated clock

Determinism holds at **6 s / 7 packets** (5 runs identical). At **30 s / 87 packets it breaks** —
four MIDs drift between runs: `0884` (CI_LAB HK), `088a`, `08a4` (CS HK), `08ad` (HS HK).

**Root cause: every OSAL primitive that blocks WITH A TIMEOUT pends on the HOST clock.** This is a
bigger surface than it first appears, and it is *the* architectural problem for a cFS sim harness:

- `OS_TaskDelay` → a real `clock_nanosleep` on `CLOCK_MONOTONIC`.
- `OS_QueueGet` with a timeout → `mq_timedreceive` against an absolute **host** time. This is what
  `CFE_SB_ReceiveBuffer(pipe, TIMEOUT)` becomes. cFS's **HS** app pends on exactly this
  (`hs_app.c:145`, `HS_WAKEUP_TIMEOUT`).
- The same applies to `OS_BinSemTimedWait` / `OS_CountSemTimedWait`.

Any task that sleeps *or* waits with a timeout therefore wakes on wall time and runs **outside**
the simulation, no matter how precisely ticks are gated. `CFE_SB_PEND_FOREVER` and `CFE_SB_POLL`
are unaffected (they map to `OS_PEND` / `OS_CHECK`), which is why the lab apps and cFE core stay
deterministic and only the "real" apps drift.

### SOLVED (to the limit of a threaded OSAL) — all timed waits are now sim-clock driven

**Result at 30 s of simulated time, 87 packets, full app set, over many runs:**

- **The packet STREAM is exactly reproducible, every run, without exception.** Per MID: the same
  number of packets, the same CCSDS sequence-deltas, the same lengths. Nothing is dropped,
  duplicated, reordered, or invented.
- **Tick PLACEMENT still jitters.** Typically 5–9 of the 87 packets land on a different tick, by
  up to ~19 ticks (190 ms) in the worst case observed.

So **what** the flight software does is fully deterministic. **When** it does it is not, for a
minority of packets. See "Remaining residual" below: this is intra-tick task ordering, and it
cannot be fixed with more clock control.

> An earlier draft of this document claimed every shift was exactly one tick. That was a lucky
> sample — it is not true, and the harness must not assume it. It also claimed a 5-tick shift that
> turned out to be an artifact of comparing transcripts *positionally*: entries are sorted by time,
> so one slipped packet moves its sort position and makes every later packet look different. Compare
> **per MID**, which separates "what happened" from "when".

OSAL now maintains its own simulated microsecond counter, advanced by the timebase helper thread on
every *externally-synced* tick. Every blocking-with-timeout primitive polls its event without
blocking, then waits for the next granted tick, until the **simulated** deadline passes:

- `osal/src/os/posix/src/os-impl-tasks.c` — the sim clock (mutex/cond/usec) + `OS_TaskDelay_Impl`.
- `osal/src/os/posix/src/os-impl-queues.c` — `OS_QueueGet_Impl` timed path (this is
  `CFE_SB_ReceiveBuffer(pipe, TIMEOUT)`).
- `osal/src/os/posix/src/os-impl-countsem.c` — `OS_CountSemTimedWait_Impl` (`sem_trywait` loop).
- `osal/src/os/posix/src/os-impl-binsem.c` — `OS_BinSemTimedWait_Impl` (a zeroed timespec makes
  the generic take a non-blocking check).
- `osal/src/os/shared/src/osapi-timebase.c` — advances the clock per tick; weak no-op defaults so
  OSAL's own coverage tests (which stub the OS layer) still link.
- `osal/src/os/shared/inc/os-shared-timebase.h` — `is_external_sync` flag + hook prototypes.

Two constraints shape this design:

**OSAL sits BELOW the PSP and cannot read `CFE_PSP_GetTime`.** The tick is the only information
crossing that boundary — so OSAL must track simulated time itself. It is enough.

**The tick thread is the sole advancer of simulated time**, so it is marked (thread-local) and
always takes the real-sleep path; otherwise it would deadlock waiting for a clock only it can move.

### The bug that cost the most time (a self-inflicted one)

The first attempt at the queue change **killed telemetry entirely** — cFS ran, TO_LAB enabled, sim
time advanced, no errors, zero packets. It looked like a deep SB/locking problem. It wasn't: in
restructuring the branches, the **`OS_CHECK` path lost its `mq_timedreceive` call** and just fell
through with `sizeCopied = -1`. `OS_CHECK` is `CFE_SB_POLL`, which SCH_LAB and the SB itself use
everywhere — so every poll silently failed and the bus went quiet. Restore the call and it works.

Lesson: when a timing change produces *total* silence rather than jitter, suspect a broken code
path, not a subtle race.

## Shipped: `crates/besom/` — the harness *and* the ground station

`livewall-besom` is a library with two binaries. 12 unit tests. Verified end to end against live
cFS.

**`besomctl`** — the headless harness (no eframe; the GUI is feature-gated so CI can run this
without a display):

```
besomctl run   [ticks]    boot cFS on the simulated clock, print the transcript
besomctl check [ticks]    run it twice; FAIL if the stream differs, REPORT placement jitter
```

**`besom`** — the ground station, a native egui/wgpu Rune app. Live telemetry grid (per-stream
counts, last MET, dropped-packet detection from CCSDS sequence gaps), decoded EVS event log, and a
command builder. Its transport pane is the unusual part: because Besom grants every tick, **Pause
genuinely freezes the spacecraft** rather than merely freezing the display, and **Step advances it
by an exact number of ticks**. A ground station paced to wall time cannot do either. Confirmed
flying: 281 packets, MET 89.3 s, zero drops, with the flight software's own events rendering as
`INFO TO_LAB — TO telemetry output enabled for IP 127.0.0.1`.

Modules mirror the findings: `ccsds` (codec), `clock` (the step protocol), `quiesce` (the `/proc`
gate), `evs` (event decoding), `transcript` (what is and is not assertable), `session` (a live
operator-driven run), `run` (scripted scenarios), `gui`.

### Two things worth knowing if you touch this code

**EVS event decoding is not what the struct says.** Read off `CFE_EVS_LongEventTlm_Payload_t` you
get a 12-byte header and big-endian fields, and every event decodes as plausible garbage — blank
app name, empty text, no error. On the wire the telemetry header is **16 bytes** (a 4-byte spare
after the timestamp) and the payload fields are **little-endian**; only the CCSDS primary header is
big-endian. The unit test is built from bytes captured off a real cFS, with SpacecraftID = 66 as
the giveaway. Do not "fix" it to match the struct.

**cFS is spawned with `PR_SET_PDEATHSIG`.** A Drop guard is not enough: on SIGTERM, a panic, or a
crash the process exits without unwinding, and cFS is left running as an orphan **still holding UDP
2234** — so the next launch silently receives no telemetry and looks broken. The kernel now kills it
with us, however we die.

## Remaining residual: intra-tick task ordering — the hard limit

Typically 5–9 of 87 packets land on a different tick, by up to ~19 ticks (190 ms). *Which* ones
varies run to run.

**This is not a clock problem and cannot be fixed with more clock control.** Within a single granted
tick, cFE's tasks are *simultaneous* in simulated time — and the **host** scheduler picks the order
they actually run in. TO_LAB is the clearest example: its downlink loop returns from a timed receive
the moment a telemetry message arrives, then polls its command pipe, so whether it sees `SEND_HK`
in the tick it arrived depends on whether SCH_LAB's task happened to run first. If it misses, the
packet slips to the next tick.

Consequences, handled differently:

- **Same-tick packet ORDER** is not a property of the simulation (it is Linux's scheduler choice),
  so the transcript sorts packets sharing a simulated timestamp by `(t, MID)`. Ordering *across*
  ticks is preserved and still fully asserted.
- **Same-tick task ORDER** can still push an event across a tick boundary. Eliminating this requires
  **deterministic intra-tick scheduling** — serializing task execution, i.e. a cooperative
  scheduler inside OSAL. That is a far larger change than a clock, and it is the true frontier for
  a byte-deterministic cFS.

### What to assert in a regression harness (do this)

Assert on the **stream** — per MID: packet count, sequence-deltas, lengths. It is exactly
reproducible and it catches every real defect: a dropped packet, a duplicate, a wrong size, a
reordering, a wrong value. This is `Transcript::same_stream`.

Do **not** assert on tick placement. Report it (`Transcript::differences`) but do not fail on it:
it is Linux's scheduler showing through, and no tolerance you pick will be both tight enough to be
useful and loose enough to be stable. Timing determinism needs the cooperative scheduler below.

The final drain must also settle: a packet emitted on the last tick can still be in flight, so the
harness re-drains until two consecutive passes find nothing. Draining once yields 7 or 8 packets
depending on host timing — a capture artifact, not a determinism failure.

### Prior art: NOS3 does NOT solve this

Checked directly (`nasa-itc/osal`, `nasa-itc/PSP`, both cloned):
- Their OSAL fork leaves `OS_TaskDelay_Impl` as a host `clock_nanosleep` — same as stock.
- **Their PSP has no custom timebase module at all** — just stock `soft_timebase` +
  `timebase_posix_clock`.

So NOS3 runs cFS on the **real clock, paced to wall time**. It has no deterministic clock and does
not address the timed-wait problem. `timebase_besom` is already ahead of it, and closing the
timed-wait gap would be genuinely novel work — plausibly upstreamable, and exactly the kind of
contribution that gets noticed.

## Gotchas found the hard way (do not re-learn these)

- **Startup race — cost the most time.** cFS boot does NOT need the timebase (OSAL tasks run on
  host threads), so it proceeds while our clock is frozen. If the harness starts stepping before
  SCH_LAB has called `OS_TimerSet`, the whole tick budget burns before its timer is armed, it
  never fires, and **zero telemetry is ever produced** — which looks exactly like "the timebase is
  broken." Wait for `CI_LAB listening` in the log before granting the first tick.
- **Inject commands with time frozen.** Sending a command while still stepping means CI_LAB picks
  it up at a host-scheduling-dependent moment, so the sim-time the command takes effect varies.
  Freeze, send, wait for the app's ack event, then resume.
- Telemetry is on **UDP 2234**, not 1235 (stale tutorials).
- The OSAL external-sync contract: `uint32 (*)(osal_id_t)` — blocks, returns **elapsed
  microseconds**. Returning 0 means "unknown elapsed", and OSAL re-invokes with a spin limit.

## Verdict: GO

The design is validated and the gate is passed. A PSP timebase module gives full control of cFE
time with **no cFE fork**, and a deterministic, reproducible telemetry transcript is achievable —
proven, not asserted. Besom can be built on this.

Next, in order:
1. Resolve the `OS_TaskDelay` question above (it bounds how much of a real app set can be
   deterministic — decide before the device-sim layer, since device sims will want to sleep).
2. Port the harness from Python to Rust as `crates/besom/` (CCSDS codec + clock + step protocol).
3. Ground-station UI, then device sims, then dynamics. 3D viewport last — it is the reward, not
   the risk.

### Abort criteria

- If stepping time requires **forking cFE core** (not just the PSP), abort the
  fully-deterministic design. Fall back to **soft-real-time** (free-running clock, sim paced to
  wall time) — which is what NOS3 effectively does — and accept that regression tests assert on
  tolerances rather than exact transcripts. Say so explicitly rather than pretending.
- If a Rust↔C PSP module boundary proves unworkable, the timebase module stays C and Besom
  drives it over IPC. Cost: a few hundred µs of jitter per step; acceptable.

---

## Architecture sketch (post-spike)

Besom is a new crate in the Rune monorepo (`crates/besom/`), reusing the r* stack.

| Layer | What it does | Reuse |
|---|---|---|
| **Transport** | CCSDS space-packet codec; UDP to CI_LAB/TO_LAB | new |
| **Clock** | authoritative sim clock; drives `timebase_besom` PSP module | new (the crux) |
| **Dynamics** | orbit propagator + attitude; 42 (NASA, open source) or own | 42 vs. own = open |
| **Device sims** | simulated sensors/actuators behind the OSAL/PSP `iodriver` seam | new |
| **Ground UI** | telemetry grid, command builder, event log | `uikit`, egui |
| **3D view** | orbit/attitude viewport | **Bellatrix** (`~/RustroverProjects/Bellatrix`) |
| **Test runner** | scripted `send cmd → step N → assert tlm` | new |

### Known integration friction

**DECIDED 2026-07-12: port the scene math only; do NOT take `gpu.rs`.**

Bellatrix is on wgpu 29, the Rune egui/eframe apps are on wgpu 22 — they cannot share a
device/queue across that gap. Rather than upgrade the shell stack or carry a second renderer, Besom
takes only `scene.rs`, `camera.rs`, `light.rs` (scene graph, camera rig, lighting math — all
version-agnostic) and leaves Bellatrix's 9k-line `gpu.rs` behind. Besom draws through the existing
Rune wgpu-22 stack.

Rationale: Bellatrix is a mesh/DOF renderer, not an orbit visualiser. Its pipeline solves problems
Besom doesn't have (depth-of-field, material system) and none of the ones it does (orbital scale,
coordinate frames, trajectory ribbons). The camera rig and scene graph are the actual asset.

---

## Deliberately out of scope for Phase 0

- Any 3D. The viewport is the *reward*, not the risk; it proves nothing about feasibility.
- Ground-station UI polish.
- Flight-hardware bus fidelity (I2C/SPI/CAN timing).
- Upstreaming. (Chosen sequencing: build first, contribute later — though the `todimpl.h` fix is
  free and should go up whenever convenient.)

---

## Refs

- cFS bundle: <https://github.com/nasa/cFS>
- NOS3 (the thing Besom is an alternative to): <https://github.com/nasa/nos3>
- 42 dynamics sim: <https://github.com/nasa/42>
- Local baseline: `~/RustroverProjects/cfs` (builds; loop proven 2026-07-12)
