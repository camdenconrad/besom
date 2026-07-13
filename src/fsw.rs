//! The link to `besom_io`, the sensor bridge running inside cFS.
//!
//! This is what closes the loop. Besom pushes simulated vehicle state into the
//! flight software; `besom_io` publishes it on the software bus; TO_LAB
//! downlinks it; the ground station reads it back. The state on screen has then
//! travelled *through* cFS rather than being Besom's own copy of it — which is
//! the difference between a simulation and a picture next to a simulation.
//!
//! It also gives the harness something it could not otherwise assert: that the
//! flight software saw what we sent it.

use crate::ccsds::build_command;
use crate::dynamics::Vehicle;

/// Where `besom_io` listens for vehicle state.
pub const STATE_PORT: u16 = 5010;

/// What `besom_io` publishes on the software bus.
pub const STATE_TLM_MID: u16 = 0x08F0;

const TO_LAB_CMD_MID: u16 = 0x1880;
const TO_LAB_ADD_PKT_CC: u8 = 2;

/// The state wire format, matching `BESOM_IO_State_t` exactly.
///
/// Native little-endian doubles, no CCSDS framing: this is a host-to-host
/// simulation link, not a spacecraft downlink. Adding byte-order conversion
/// would be ceremony that buys nothing and could silently disagree with the C
/// side.
pub fn encode_state(v: &Vehicle) -> Vec<u8> {
    let (lat, lon) = v.orbit.subpoint_deg();

    let fields = [
        v.orbit.pos.x,
        v.orbit.pos.y,
        v.orbit.pos.z,
        v.orbit.vel.x,
        v.orbit.vel.y,
        v.orbit.vel.z,
        v.orbit.altitude_km(),
        lat,
        lon,
        v.attitude.roll,
    ];

    fields.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Vehicle state as the FLIGHT SOFTWARE reports it, decoded from `0x08F0`.
///
/// If this ever disagrees with Besom's own model, the loop is broken — and that
/// is a far more useful thing to be able to see than either number alone.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FswState {
    pub alt_km: f64,
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub roll: f64,
    /// State datagrams cFS has accepted. Should climb steadily; a stall means
    /// the flight software has stopped hearing us.
    pub rx_count: u32,
    pub rx_err_count: u32,
}

impl FswState {
    /// Layout after the 16-byte telemetry header (see `evs.rs` for why it is 16,
    /// not 12): ten f64 of state, then two u32 counters. All little-endian.
    pub fn parse(msg_id: u16, buf: &[u8]) -> Option<Self> {
        const HDR: usize = 16;
        const N_F64: usize = 10;
        const NEED: usize = HDR + N_F64 * 8 + 8;

        if msg_id != STATE_TLM_MID || buf.len() < NEED {
            return None;
        }

        let f = |i: usize| -> f64 {
            let o = HDR + i * 8;
            f64::from_le_bytes(buf[o..o + 8].try_into().unwrap())
        };
        let u = |o: usize| -> u32 { u32::from_le_bytes(buf[o..o + 4].try_into().unwrap()) };

        let counters = HDR + N_F64 * 8;

        Some(Self {
            alt_km: f(6),
            lat_deg: f(7),
            lon_deg: f(8),
            roll: f(9),
            rx_count: u(counters),
            rx_err_count: u(counters + 4),
        })
    }
}

/// Tell TO_LAB to downlink a stream it does not already carry.
///
/// TO_LAB's subscription table is baked in at build time and does not include
/// `besom_io`'s telemetry, so the ground station subscribes it at runtime — which
/// is exactly what a real operator would do, and exercises a command path beyond
/// the usual NOOP.
///
/// Payload is `TO_LAB_AddPacket_Payload_t`: a `CFE_SB_MsgId_t` (u32), a
/// `CFE_SB_Qos_t` (two u8), and a u8 buffer limit — then padding to the struct's
/// 4-byte alignment.
pub fn add_packet_command(msg_id: u16) -> Vec<u8> {
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&u32::from(msg_id).to_le_bytes());
    payload.push(0); // Qos.Priority
    payload.push(0); // Qos.Reliability
    payload.push(4); // BufLimit
    payload.push(0); // padding

    build_command(TO_LAB_CMD_MID, TO_LAB_ADD_PKT_CC, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_through_the_wire_format() {
        // The C side reads these bytes as a packed struct of f64. If the encoder
        // and the decoder ever disagree the loop silently reports garbage, so
        // pin the layout from both ends.
        let v = Vehicle::default();
        let bytes = encode_state(&v);
        assert_eq!(bytes.len(), 80, "ten f64");

        // Rebuild a telemetry packet the way besom_io publishes it.
        let mut pkt = vec![0u8; 16];
        pkt.extend_from_slice(&bytes);
        pkt.extend_from_slice(&7u32.to_le_bytes()); // RxCount
        pkt.extend_from_slice(&0u32.to_le_bytes()); // RxErrCount

        let s = FswState::parse(STATE_TLM_MID, &pkt).unwrap();

        let (lat, lon) = v.orbit.subpoint_deg();
        assert!((s.alt_km - v.orbit.altitude_km()).abs() < 1e-9);
        assert!((s.lat_deg - lat).abs() < 1e-9);
        assert!((s.lon_deg - lon).abs() < 1e-9);
        assert_eq!(s.rx_count, 7);
    }

    #[test]
    fn ignores_other_telemetry() {
        assert!(FswState::parse(0x0883, &[0u8; 200]).is_none());
    }

    #[test]
    fn add_packet_targets_to_lab() {
        let cmd = add_packet_command(STATE_TLM_MID);
        assert_eq!(&cmd[..2], &0x1880u16.to_be_bytes(), "TO_LAB command MID");
        assert_eq!(cmd[6], TO_LAB_ADD_PKT_CC);
        assert_eq!(
            u32::from_le_bytes(cmd[8..12].try_into().unwrap()),
            u32::from(STATE_TLM_MID)
        );
    }
}
