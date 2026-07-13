//! A telemetry transcript: the reproducible record of what a run did.
//!
//! What is and is not asserted here is the whole lesson of the Phase 0 spike
//! (docs/besom-phase0.md). Getting this wrong produces a harness that either
//! fails constantly on noise or passes while missing real defects.
//!
//! **Asserted** — the packet stream. Which MIDs, in what order, with what
//! lengths, with no gaps or duplicates. This is exactly reproducible.
//!
//! **Not asserted:**
//! - *Absolute* mission time. cFE TIME's startup state machine settles on
//!   whole-second boundaries before our clock takes over, so the epoch lands on
//!   a 5-second quantum that varies run to run. Times are therefore recorded
//!   relative to the first periodic packet.
//! - *Absolute* CCSDS sequence counters. They count every transmit since boot,
//!   including the ~35 EVS events emitted during un-gated startup. Deltas still
//!   catch a dropped or duplicated packet (a delta != 1 is a real defect)
//!   without inheriting that history.
//! - *Order within a single tick*. Those packets are simultaneous in simulated
//!   time; their order is Linux's scheduler choice, not a property of the
//!   simulation. Entries sharing a timestamp are sorted by MID so they compare
//!   stably. Ordering ACROSS ticks is preserved and fully asserted.
//! - *Exact tick placement*, within one tick. Intra-tick task ordering can push
//!   an event across a tick boundary (see [`Transcript::differences`]).

use crate::ccsds::{MsgId, TlmPacket, EVS_EVENT_MID};
use crate::clock::TICK_USEC;
use std::collections::HashMap;
use std::fmt::Write as _;

const TICK_SECS: f64 = TICK_USEC as f64 / 1e6;

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub msg_id: MsgId,
    /// Sequence delta from this MID's previous packet. `None` on first sight.
    pub seq_delta: Option<u16>,
    /// Seconds since the first periodic packet, snapped to the tick grid.
    /// `None` for packets that arrived before the anchor.
    pub rel_time: Option<f64>,
    pub len: usize,
}

impl Entry {
    /// The part of an entry that is exactly reproducible: what the packet was,
    /// not which tick it happened to land on.
    fn stream_key(&self) -> (MsgId, Option<u16>, usize) {
        (self.msg_id, self.seq_delta, self.len)
    }
}

#[derive(Debug, Default)]
pub struct Transcript {
    entries: Vec<Entry>,
    prev_seq: HashMap<MsgId, u16>,
    epoch: Option<f64>,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, pkt: &TlmPacket) {
        let t = pkt.time_secs();

        // Anchor on the first PERIODIC packet. The EVS event stream is
        // asynchronous and is emitted around the enable command while the clock
        // is still frozen, so anchoring on it would import boot timing.
        if self.epoch.is_none() && pkt.msg_id != EVS_EVENT_MID {
            self.epoch = Some(t);
        }

        let rel_time = self.epoch.map(|e| {
            // Snap to the tick grid: cFE time only ever advances in whole ticks,
            // but the 1/65536 s CCSDS field cannot always represent one exactly,
            // so it rounds to the nearest LSB (~15 us). Snapping drops that
            // rounding noise, not signal.
            ((t - e) / TICK_SECS).round() * TICK_SECS
        });

        let seq_delta = self
            .prev_seq
            .insert(pkt.msg_id, pkt.seq)
            .map(|prev| pkt.seq.wrapping_sub(prev) & 0x3FFF);

        self.entries.push(Entry { msg_id: pkt.msg_id, seq_delta, rel_time, len: pkt.len });
    }

    /// Finish the run: sort same-instant packets so they compare stably.
    pub fn finish(mut self) -> Self {
        self.entries.sort_by(|a, b| {
            let ka = (a.rel_time.is_none(), a.rel_time.map(f64::to_bits), a.msg_id);
            let kb = (b.rel_time.is_none(), b.rel_time.map(f64::to_bits), b.msg_id);
            ka.partial_cmp(&kb).expect("total order")
        });
        self
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Each MID's own ordered sequence of packets.
    ///
    /// This is the placement-independent view. Comparing whole transcripts
    /// positionally does NOT work: entries are sorted by time, so a single
    /// packet slipping one tick changes its sort position and makes the entire
    /// sequence appear to differ. Grouping by MID first separates *what the
    /// flight software did* from *which tick it landed on*, which are two
    /// genuinely different questions.
    fn by_msg_id(&self) -> HashMap<MsgId, Vec<&Entry>> {
        let mut map: HashMap<MsgId, Vec<&Entry>> = HashMap::new();
        for e in &self.entries {
            map.entry(e.msg_id).or_default().push(e);
        }
        map
    }

    /// True when two runs produced the same packet stream — the property that
    /// actually holds, and the one a regression test should assert.
    ///
    /// Per MID: the same number of packets, with the same sequence deltas (so a
    /// drop or duplicate fails) and the same lengths. Deliberately ignores which
    /// tick each packet landed on — intra-tick task ordering decides that, and
    /// asserting on it is asserting on Linux's scheduler.
    pub fn same_stream(&self, other: &Self) -> bool {
        let (a, b) = (self.by_msg_id(), other.by_msg_id());

        a.len() == b.len()
            && a.iter().all(|(mid, ea)| {
                b.get(mid).is_some_and(|eb| {
                    ea.len() == eb.len()
                        && ea.iter().zip(eb).all(|(x, y)| x.stream_key() == y.stream_key())
                })
            })
    }

    /// Packets whose tick placement moved between two runs, as
    /// `(msg_id, self_time, other_time)`. Compared per MID, so a shift in one
    /// message does not cascade into apparent shifts in every other.
    pub fn differences(&self, other: &Self) -> Vec<(MsgId, Option<f64>, Option<f64>)> {
        let (a, b) = (self.by_msg_id(), other.by_msg_id());
        let mut out = Vec::new();

        for (mid, ea) in &a {
            let Some(eb) = b.get(mid) else { continue };
            for (x, y) in ea.iter().zip(eb.iter()) {
                if x.rel_time != y.rel_time {
                    out.push((*mid, x.rel_time, y.rel_time));
                }
            }
        }

        out.sort_by_key(|(mid, _, _)| *mid);
        out
    }

    /// The largest tick-placement shift between two runs, in ticks.
    pub fn max_shift_ticks(&self, other: &Self) -> f64 {
        self.differences(other)
            .iter()
            .filter_map(|(_, a, b)| Some((a.as_ref()?, b.as_ref()?)))
            .map(|(a, b)| (a - b).abs() / TICK_SECS)
            .fold(0.0, f64::max)
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for e in &self.entries {
            let seq = match e.seq_delta {
                Some(d) => format!("d+{d}"),
                None => "first".to_string(),
            };
            let t = match e.rel_time {
                Some(t) => format!("+{t:09.5}"),
                None => "PRE-ANCHOR".to_string(),
            };
            let _ = writeln!(out, "{:04x} {:<6} t={} len={}", e.msg_id, seq, t, e.len);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(msg_id: MsgId, seq: u16, secs: u32, subsecs: u16) -> TlmPacket {
        TlmPacket { msg_id, seq, secs, subsecs, len: 32 }
    }

    #[test]
    fn anchors_on_the_first_periodic_packet_not_the_event_stream() {
        let mut t = Transcript::new();
        t.record(&pkt(EVS_EVENT_MID, 1, 100, 0)); // async, pre-anchor
        t.record(&pkt(0x0800, 1, 110, 0)); // first periodic -> t=0
        let t = t.finish();

        let evs = t.entries().iter().find(|e| e.msg_id == EVS_EVENT_MID).unwrap();
        let hk = t.entries().iter().find(|e| e.msg_id == 0x0800).unwrap();

        assert_eq!(evs.rel_time, None, "event stream must not anchor the run");
        assert_eq!(hk.rel_time, Some(0.0));
    }

    #[test]
    fn absolute_epoch_shift_does_not_change_the_transcript() {
        // cFE TIME's boot epoch lands on a 5-second quantum that varies run to
        // run. The same run at a different epoch must render identically.
        let build = |base: u32| {
            let mut t = Transcript::new();
            t.record(&pkt(0x0800, 1, base, 0));
            t.record(&pkt(0x0801, 1, base + 1, 0));
            t.finish()
        };

        assert_eq!(build(1_001_001).render(), build(1_001_006).render());
    }

    #[test]
    fn sequence_deltas_survive_a_different_boot_history() {
        // Absolute counters differ (boot emitted a different number of events);
        // the deltas -- which is what actually detects a dropped packet -- do not.
        let build = |start: u16| {
            let mut t = Transcript::new();
            t.record(&pkt(0x0800, start, 10, 0));
            t.record(&pkt(0x0800, start + 1, 11, 0));
            t.finish()
        };

        assert!(build(32).same_stream(&build(79)));
    }

    #[test]
    fn a_dropped_packet_is_still_caught() {
        let mut good = Transcript::new();
        good.record(&pkt(0x0800, 1, 10, 0));
        good.record(&pkt(0x0800, 2, 11, 0));

        let mut dropped = Transcript::new();
        dropped.record(&pkt(0x0800, 1, 10, 0));
        dropped.record(&pkt(0x0800, 3, 11, 0)); // gap: seq jumped

        assert!(!good.finish().same_stream(&dropped.finish()), "seq gap must fail");
    }

    #[test]
    fn same_tick_order_is_not_asserted_but_cross_tick_order_is() {
        // Two packets in the SAME tick, emitted in opposite order.
        let mut a = Transcript::new();
        a.record(&pkt(0x0800, 1, 10, 0));
        a.record(&pkt(0x08b0, 1, 10, 0));
        a.record(&pkt(0x0801, 1, 10, 0));

        let mut b = Transcript::new();
        b.record(&pkt(0x0800, 1, 10, 0));
        b.record(&pkt(0x0801, 1, 10, 0));
        b.record(&pkt(0x08b0, 1, 10, 0));

        assert!(a.finish().same_stream(&b.finish()), "same-instant order is the scheduler's choice");
    }

    #[test]
    fn a_slipped_packet_does_not_cascade_into_a_false_stream_mismatch() {
        // The trap: entries are sorted by time, so if one packet slips a tick it
        // lands on the other side of its neighbour. Comparing the two runs
        // positionally then reports EVERY subsequent packet as different, and a
        // pure scheduling artifact masquerades as a broken telemetry stream.
        // Grouping by MID first is what keeps the two questions separate.
        let mut a = Transcript::new();
        a.record(&pkt(0x0880, 1, 10, 0)); // before 0x0800
        a.record(&pkt(0x0800, 1, 10, 655));
        let a = a.finish();

        let mut b = Transcript::new();
        b.record(&pkt(0x0880, 1, 10, 1310)); // slipped past 0x0800
        b.record(&pkt(0x0800, 1, 10, 655));
        let b = b.finish();

        assert!(a.same_stream(&b), "a tick slip is not a stream change");
        assert_eq!(a.differences(&b).len(), 1, "only the slipped packet is reported");
    }

    #[test]
    fn one_tick_of_jitter_keeps_the_stream_but_shows_up_as_a_shift() {
        let mut a = Transcript::new();
        a.record(&pkt(0x0800, 1, 10, 0));
        a.record(&pkt(0x0880, 1, 11, 0));
        let a = a.finish();

        // 0x0880 slipped one tick (10 ms = 655.36 subsecond units).
        let mut b = Transcript::new();
        b.record(&pkt(0x0800, 1, 10, 0));
        b.record(&pkt(0x0880, 1, 11, 655));
        let b = b.finish();

        assert!(a.same_stream(&b), "the stream is unchanged");
        assert_eq!(a.differences(&b).len(), 1);
        assert!((a.max_shift_ticks(&b) - 1.0).abs() < 0.01, "exactly one tick");
    }
}
