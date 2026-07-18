# Determinism under host load

Besom claims byte-identical telemetry on identical ticks. That claim was measured on an idle
workstation. This asks the question CI forces: does it survive a machine that is small, busy,
or both?

Short answer: **small is fine, busy is not — and the leak is the harness's, not cFS's.**

## Method

`besomctl check N` boots cFS twice and self-compares, so one invocation is one complete
determinism trial. 15 trials per condition, 3000 ticks (30 s simulated), `BESOM_COOP=1`.

Trials run strictly serially. CPU contention is the independent variable, so running trials
concurrently would confound the thing being measured. (They are serial anyway for a duller
reason: the harness binds a fixed telemetry port, so only one cFS can run at a time.)

Two factors crossed — core count, because GitHub runners are 2–4 vCPU, and contention, because
they are shared:

| condition | placement identical | shifted | failed outright | packets |
|---|---|---|---|---|
| idle, 16 cores | **15/15** | 0 | 0 | 382 |
| idle, 2 cores (`taskset -c 0,1`) | **15/15** | 0 | 0 | 382 |
| loaded, 16 cores (`stress-ng --cpu 16`) | 14/15 | 1 | 0 | 382 |
| loaded, 2 cores (4 workers on those cores) | **11/15** | 3 | 1 | 381 |

Idle is perfect: 30 runs, 30 identical, on either core count. Loaded and small — the CI case —
passes 11 times in 15.

### Core count is not the problem

15/15 on two cores, on the same 382 packets. Besom does not need a big machine, and CI will not
fail merely for running on a small one. That was worth establishing before blaming the runner.

### Contention is the problem

Every failure has the same shape, which is the most useful thing in the table:

    load-allcores  t1   94/382 packets moved, max 10.0 ticks
    load-2core     t2   93/381 packets moved, max 10.0 ticks
    load-2core     t5   93/381 packets moved, max 10.0 ticks
    load-2core     t14  93/381 packets moved, max 10.0 ticks

Always about a quarter of the stream, always a maximum of *exactly* 10 ticks. That is not jitter
smeared across the run — jitter would give varying counts and varying magnitudes. It is one
discrete event: a single one-cycle phase slip in `besom_io` (10 Hz = 10 ticks), after which every
subsequent packet of the affected streams is stamped one cycle over. The same one-period slip
`run.rs` already documents for the un-gated-boot case, arriving by a different route.

One trial failed outright rather than merely shifting, under loaded 2 cores.

A 73% pass rate is unusable for CI, and even the 93% at 16 cores is the worst possible number:
rare enough to look like a fluke and get re-run, frequent enough to erode trust in every green
build.

This is not confined to synthetic load. A `check 3000` run immediately after a full cFS build
came out `90/382 packets shifted, max 5.0 ticks`, then passed 3 times out of 3 once the machine
went quiet — the ordinary rhythm of working on the project (build, then test) is enough to trip
it.

## The mechanism

Tick placement is not measured from when a packet reaches the harness. It is read out of the
packet's own cFE secondary header (`transcript.rs`, `pkt.time_secs()`), which `CFE_SB_TransmitMsg`
stamps with *simulated* time — frozen for the entire duration of one granted tick.

That is a strong constraint, and it rules most candidates out. However cFE's tasks interleave
*within* a tick, every packet they transmit carries the same simulated timestamp. So no amount of
intra-tick scheduling noise can move a packet between ticks. **Only a decision taken at a tick
boundary can.**

The boundary decision is `quiesce::wait`. After each granted tick it polls
`/proc/<pid>/task/*/stat` until no cFS thread is in state `R`, then lets the run proceed.

### The obvious hypothesis, and why it is wrong

`quiesce::wait` gives up after a deadline and grants the next tick **anyway**:

```rust
// quiesce::wait, before
"Timing out is not an error: it means the process is genuinely busy, and
 stalling the whole run over it would be worse than proceeding."
```

That is plainly a host-timed decision inside a harness built to have none, and it was silent —
no counter, no log line. It is the natural suspect, so the first change was to count expiries and
make the budget tunable (`$BESOM_QUIESCE_MS`), and the first experiment varied it under load.

**The measurement refuted it.** Under `stress-ng --cpu 16`, placement shifts occur with
`quiesce::timeouts() == 0`:

    quiesce_ms  trial  placement   shifted  stalls
    2000        1      identical   0        0
    2000        2      identical   0        0
    2000        3      SHIFTED     94       0
    2000        4      identical   0        0
    2000        5      identical   0        0

The deadline is never reached, so raising it cannot help. The harness is not giving up.

### It returns too early

`wait` returns after `CLEAN_SAMPLES` (3) consecutive samples showing no runnable thread, polled
every 400 µs — a **1.2 ms confirmation window**. The module's own doc comment already names the
hazard:

> "immediately after an ack the woken tasks may not be marked runnable yet, so a single clean
> sample can report a false quiescence... Requiring consecutive clean samples is what makes it
> hold."

Three samples make it hold on an idle machine. Under contention, the lag between *granting a
tick* and *the woken cFS thread actually being marked `R`* grows — the thread is waiting for a
CPU that 16 stress workers are fighting over. Once that lag exceeds 1.2 ms, all three samples are
clean and `wait` returns **before cFS has begun reacting at all**. The next tick is granted
mid-reaction.

This is a *false quiescence*, and it is worse than a timeout in one specific way: a timeout is now
counted, but an early return is indistinguishable from a genuine one. The very mechanism built to
make host-timed decisions observable cannot see it. `stalls == 0` above is not evidence the
harness behaved — it is evidence it did not notice.

The same blind spot has a second entrance: `any_runnable` treats every non-`R` state as blocked,
but a thread in `D` (uninterruptible sleep) is executing a kernel operation that completes on host
time with no tick from us.

`$BESOM_QUIESCE_SAMPLES` now exposes the window so this is testable rather than argued.

### The false-quiescence hypothesis is NOT yet confirmed

It was tested — 12 trials per arm at `SAMPLES=3` and `SAMPLES=20`, load held constant — and the
test failed to produce an answer. Both arms, honestly:

* `SAMPLES=3` (the arm that should show the defect): **0 shifts in 12**. At the ~7% per-run rate
  measured on a loaded 16-core host, seeing zero in twelve is roughly a coin flip. The baseline
  produced no events to explain, so there was nothing for the other arm to remove.
* `SAMPLES=20`: **12/12 failed outright** — `cFS boot timed out`, before recording a packet.

So the mechanism above is the best-supported explanation of the evidence, not a demonstrated one.
It is consistent with everything measured (shifts occur, timeouts do not) and no competing
mechanism survived the audit, but "the deadline is not what is being hit" is a much stronger claim
than "the confirmation window is why", and only the first has been shown.

A decisive test needs the loaded-2-core condition, where the event rate is ~25% rather than ~7%,
and enough trials to separate the arms — several hours at ~175 s per trial, more with a widened
window. Worth running before anyone relies on the fix.

### The wall-clock backstop the wide-window arm hit

`SAMPLES=20` is 8 ms of polling per granted tick, and the boot loop grants 4000 of them before
recording starts: ~32 s of extra wall clock, straight past the 60 s boot deadline that had been
hardcoded. Every run died on it.

That deadline is a legitimate backstop — it exists so a wedged cFS fails instead of hanging — but
it is also the point where "buy determinism with wall-clock" collides with a fixed wall-clock
budget, and it was not adjustable. It is now `$BESOM_BOOT_TIMEOUT_S`, and the error message says
what the current confirmation window costs across the boot budget, because the failure otherwise
looks like a broken timebase and is not one.

**The two settings must move together.** Anything that widens the quiescence window must raise the
boot timeout, or the harness fails for the wrong reason. CI sets both.

### What was ruled out

* **The cooperative scheduler.** `OS_Coop_TakeToken` also has a wall-clock deadline
  (`CLOCK_REALTIME` + 3 s), but it sits inside `while (!OS_Coop_MayRun(me))`, so on timeout it
  prints `COOP-STALL` diagnostics and re-checks. It never proceeds without the token, and run
  order stays keyed by OSAL task id. Correctly labelled a diagnostic.
* **OSAL's host fallbacks.** `sem_timedwait` / `mq_timedreceive` are gated on whether simulated
  time is active, not on load, so contention does not change which path a task takes.
* **The `OS_CHECK` ready-set race.** A non-blocking socket poll really does leave the cooperative
  ready set (`OS_COOP_EXTERNAL` is applied unconditionally), so which task re-enters first is
  Linux's choice. But per the timestamp argument above, that reorders work *within* a tick and
  cannot change which tick a packet is stamped with.

## What changed

`quiesce::wait` now counts every expiry, exposes the count, and takes its budget from
`$BESOM_QUIESCE_MS`. The silence was the real defect: a determinism harness may trade wall-clock
for determinism, but it must not degrade quietly.

`besomctl check` now distinguishes the two ways a placement difference can arise:

* **NOT REPRODUCIBLE** — placement moved with quiescence clean throughout. The flight software's
  timing changed. This is the real failure.
* **INCONCLUSIVE** — placement moved *and* the harness stalled. The host was too loaded to grant
  ticks cleanly, so the run says nothing about the flight software either way.

Conflating those is how a determinism harness earns a reputation for flakiness and then gets
ignored. `run` and `loop` warn on stalls too, rather than printing an authoritative-looking
transcript with host timing silently folded into it.

CI therefore raises `BESOM_QUIESCE_MS` rather than weakening the assertion: buy determinism with
wall-clock, do not give up the guarantee.

## One packet still moves with the host

At 3000 ticks an idle run records 382 packets and a loaded 2-core run records 381. Both runs
*within* an invocation agree, so `check` passes — the transcript is self-consistent but is not yet
a pure function of the scenario. That is fine for run-twice-and-compare and fatal for the obvious
next feature: record a golden transcript once and assert against it later.

It is not a lost packet. Diffing per-MID counts between an idle run and a loaded 2-core run:

| MID | idle | loaded, 2 cores |
|---|---|---|
| 0803 | 7 | 6 |
| 0880 | 5 | **6** |
| 0883 | 6 | 5 |
| 0884 | 6 | 5 |
| 088a | 5 | 4 |
| 08aa | 4 | **5** |
| 08b8 | 4 | **5** |

Seven separate periodic streams each gain or lose exactly one cycle — four down, three up —
netting the single packet. Nothing is dropped; the window's *phase* relative to each app has
moved, so each stream catches N or N+1 of its cycles inside it.

The guard band pins the trailing edge and the housekeeping-packet sync pins the leading edge, but
only to the nearest 1 Hz cycle. Sub-phase within that quantum still rides on the host, which is
the same residual error the README already describes for un-gated boot. It is invisible to
`check` because both runs of one invocation share it.

## Two fixes that were wrong before they were right

Both were caught by running them, not by reading them, which is the argument for the CI job.

**Waiting for the downlink ack with the clock frozen.** `session.rs` verifies the enable by
waiting for `TO_LAB 3`; porting that to `run.rs` looked like straightforward de-duplication. It
fails every run. By that point the boot loop has granted ticks, so `OS_SimTime` is active and
CI_LAB's `CFE_SB_ReceiveBuffer(pipe, 500 ms)` parks in *simulated* time — it cannot poll its
socket, cannot see the enable, and cannot ack, so the wait is unsatisfiable by construction.
`session.rs` is safe only because it arrives there having granted zero ticks. The two files sit in
different OSAL time regimes at the same line of code.

    OLD binary: exit=0  packets=10
    NEW binary: exit=1  packets=0   "TO_LAB never acknowledged the enable-output command"

The fix is a fixed `ENABLE_TICKS` budget: grant time so CI_LAB can actually read, but a fixed
amount so the handshake costs the same simulated time on any host.

**Granting those ticks without draining.** The first version of that fix stepped 200 ticks and
drained nothing. TO_LAB begins downlinking the moment the enable lands, so that telemetry
accumulated in the kernel receive buffer, where what survives is decided by socket memory rather
than by the flight software. `check 3000` went from 15/15 identical to:

    NOT REPRODUCIBLE: 89/382 packets moved between runs (max 3.0 tick(s))
    with quiescence clean throughout

— on an **idle** host. The packets are boot history and are thrown away either way; the point is
that they must be thrown away by the harness, deterministically, not by the kernel dropping
whatever did not fit. Draining inside the loop restored 382/382 identical, 3 runs out of 3.

Note the shape: 3-tick shifts, not the 10-tick `besom_io` slip that host load produces. A
determinism harness that reports *how* a run differs, not just that it does, is what made these
two distinguishable at a glance.

## The second run was not the same scenario as the first

Found while measuring what payload comparison would cost, and worth stating separately because
it is not about load at all.

cFE's PSP keeps its reserved memory alive between processes. `besomctl check` runs cFS twice, and
the second run found that memory still valid and came up as a **processor reset** while the first
came up **power-on**:

| field | run 1 | run 2 |
|---|---|---|
| `ResetType` | 2 (POWERON) | 1 (PROCESSOR) |
| `ProcessorResets` | 0 | 1 |
| `ERLogEntries` | 1 | 2 |
| `SysLogEntries` | 39 | 76 |
| `SysLogBytesUsed` | 3029 | 3072 |

So the tool whose entire job is "run the same scenario twice and compare" was running two
different scenarios. Neither the packet stream nor tick placement notices — the shape of the run
is unchanged — which is why this survived every check until payload contents were compared.

It is not cosmetic. Any app that behaves differently after a processor reset — checking its CDS,
restoring state, replaying its exception log — was being exercised along two different paths, and
the difference would have been read as a regression in whichever run happened to go second.

Fixed by passing `-R PO`. `PO` is documented as the *default*, which is why this was not obvious:
the PSP honours existing reserved memory over the default. Measured effect on cross-run payload
differences at 3000 ticks:

    before:  318/382 packets differ, MIDs 0800 0801 0804 0805 08b8 08f0
    after:   288/382 packets differ, MID  08f0

Five of the six unstable streams were one root cause rather than five counters, and some run pairs
now come out byte-identical *including payloads*.

## Gaps this exposed in the comparison itself

Measuring the above meant reading `Transcript` closely. Three things it does not catch:

* **A wrong value.** `Entry::stream_key` is `(msg_id, seq_delta, len)` and `TlmPacket` parses only
  the 12-byte CCSDS header — no payload byte is ever retained. An app publishing a stale latitude
  or a wrong counter passes `check` today. Both the README and `docs/phase0.md` claimed otherwise;
  they no longer do. Payload comparison is the most valuable thing to add next, and it is not
  small: it needs a rule for fields that may legitimately differ before it can be asserted on.
* **A packet dropped at the leading edge of a MID's stream.** The first packet of each MID is
  recorded with `seq_delta: None`, so losing it promotes the next into that slot and the prefix
  realigns; the length difference of one is absorbed by the trailing-boundary tolerance. The guard
  band pins the trailing edge only.
* **Nothing at all, when the budget is inside the guard band.** `check N` for `N <= 120` recorded
  zero packets, and two empty transcripts compare equal — so it printed `0 packets, identical`,
  `tick placement: identical`, and exited 0. Exactly the green build a dead downlink would give.
  Now refused outright.

## What CI actually does on a real runner

The first run of the workflow, on a stock GitHub-hosted runner with
`BESOM_QUIESCE_SAMPLES=20` and `BESOM_BOOT_TIMEOUT_S=300`:

    stream reproducible: 382 packets, identical
    tick placement: identical
    cFS accepted 643 state updates (0 malformed)
    worst disagreement: lat 0.000000deg  lon 0.000000deg
    closed loop verified: cFS reports exactly the state it was given

Green, with no `quiescence:` line — zero stalls — and the same 382 packets an idle 16-core
workstation records. `ci/build-cfs.sh` also built cFS from a clean nasa/cFS clone at the pinned
ref, so the README's instructions do complete on a machine that has never seen this project.

**This is one run, and one run is not a pass rate.** It says the configuration is viable, not that
the flakiness measured above is solved: a runner that happened to be quiet proves less than the
73% measured under deliberate contention. What it does establish is that the strict assertion is
worth keeping in CI rather than pre-emptively weakened to stream-only — the failure mode to watch
for is an occasional INCONCLUSIVE, which is diagnosable, rather than silent drift.

## Still open

* `quiesce` treats every non-`R` thread state as blocked, but `D` (uninterruptible sleep) makes
  progress on host time with no tick granted. That is a *false quiescence* — and unlike a timeout
  it is not counted, so the mechanism built to make host-timed decisions observable cannot see it.
  Deliberately not changed here: it would alter the very behaviour these measurements describe.
* Payload comparison, per above.
* `session.rs` grants no ticks during boot and has no fixed boot budget or phase alignment, so GUI
  sessions are not reproducible the way `run.rs` scenarios are. Defensible — it is an operator
  tool, not a test harness — but it is why the two paths sit in different OSAL time regimes, which
  is a live trap for anyone porting a fix between them.
