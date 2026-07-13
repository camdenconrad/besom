//! CCSDS space packets — the wire format cFS speaks.
//!
//! A packet is a 6-byte primary header followed by a secondary header and the
//! payload. Telemetry carries a 6-byte timestamp (4 bytes of seconds, 2 of
//! 1/65536 s); commands carry a function code and a checksum byte.

use anyhow::{bail, Result};

/// The apid/stream identifier cFS calls a "MsgId". Telemetry lives at
/// `0x08xx`, commands at `0x18xx`.
pub type MsgId = u16;

/// cFE's event message stream. It is asynchronous rather than periodic, and it
/// is emitted during un-gated startup, so a run's timestamps are anchored to
/// the first *periodic* packet instead of this one.
pub const EVS_EVENT_MID: MsgId = 0x0808;

/// A decoded telemetry packet. Only the fields a transcript needs to assert on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlmPacket {
    pub msg_id: MsgId,
    /// 14-bit CCSDS sequence counter. Compare *deltas*, never absolutes: the
    /// counter includes every transmit since boot, and boot runs before the
    /// simulated clock exists.
    pub seq: u16,
    /// Mission elapsed seconds from the packet's secondary header.
    pub secs: u32,
    /// Subseconds, in units of 1/65536 s.
    pub subsecs: u16,
    pub len: usize,
}

impl TlmPacket {
    /// Time in seconds. Note the 1/65536 field cannot always represent a whole
    /// number of 10 ms ticks exactly, so callers comparing runs should snap to
    /// the tick grid rather than compare this directly.
    pub fn time_secs(&self) -> f64 {
        f64::from(self.secs) + f64::from(self.subsecs) / 65536.0
    }

    pub fn parse(buf: &[u8]) -> Result<Self> {
        // 6-byte primary header + 6-byte timestamp
        if buf.len() < 12 {
            bail!("short packet: {} bytes", buf.len());
        }

        Ok(Self {
            msg_id: u16::from_be_bytes([buf[0], buf[1]]),
            seq: u16::from_be_bytes([buf[2], buf[3]]) & 0x3FFF,
            secs: u32::from_be_bytes([buf[6], buf[7], buf[8], buf[9]]),
            subsecs: u16::from_be_bytes([buf[10], buf[11]]),
            len: buf.len(),
        })
    }
}

/// Build a CCSDS command packet.
///
/// The checksum is the byte that makes an XOR over the whole packet come out to
/// 0xFF, per `CFE_MSG_ComputeCheckSum`. (cFS's own `cmdUtil` emits a byte that
/// does *not* satisfy this, and cFS accepts both — it does not verify command
/// checksums by default. We emit the spec-correct one.)
pub fn build_command(msg_id: MsgId, fn_code: u8, payload: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(8 + payload.len());

    // Primary header: msg id, sequence flags (0xC000 = unsegmented), length.
    // CCSDS length counts the bytes after the primary header, minus one.
    let body_len = 2 + payload.len();
    pkt.extend_from_slice(&msg_id.to_be_bytes());
    pkt.extend_from_slice(&0xC000u16.to_be_bytes());
    pkt.extend_from_slice(&((body_len - 1) as u16).to_be_bytes());

    // Command secondary header: function code + checksum placeholder.
    pkt.push(fn_code);
    pkt.push(0);
    pkt.extend_from_slice(payload);

    let xor = pkt.iter().fold(0u8, |acc, b| acc ^ b);
    pkt[7] = xor ^ 0xFF;

    pkt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_header_matches_the_wire() {
        // The TO_LAB "enable output" command. cmdUtil puts this on the wire:
        //   18 80 c0 00 00 11 06 b0  31 32 37 2e 30 2e 30 2e 31 00 ...
        // Everything but the checksum byte must match it exactly.
        let mut ip = [0u8; 16];
        ip[..9].copy_from_slice(b"127.0.0.1");

        let pkt = build_command(0x1880, 6, &ip);

        assert_eq!(&pkt[..6], &[0x18, 0x80, 0xC0, 0x00, 0x00, 0x11], "primary header");
        assert_eq!(pkt[6], 6, "function code");
        assert_eq!(&pkt[8..17], b"127.0.0.1", "payload");
        assert_eq!(pkt.len(), 24);
        assert_eq!(pkt.iter().fold(0u8, |a, b| a ^ b), 0xFF, "CFE_MSG_ComputeCheckSum invariant");
    }

    #[test]
    fn parses_a_telemetry_packet() {
        let mut buf = vec![0u8; 24];
        buf[0..2].copy_from_slice(&0x0883u16.to_be_bytes()); // SAMPLE_APP HK
        buf[2..4].copy_from_slice(&0xC005u16.to_be_bytes()); // seq 5, flags set
        buf[6..10].copy_from_slice(&7u32.to_be_bytes());
        buf[10..12].copy_from_slice(&32768u16.to_be_bytes()); // half a second

        let p = TlmPacket::parse(&buf).unwrap();

        assert_eq!(p.msg_id, 0x0883);
        assert_eq!(p.seq, 5, "sequence flags must be masked off");
        assert_eq!(p.time_secs(), 7.5);
    }

    #[test]
    fn rejects_a_runt() {
        assert!(TlmPacket::parse(&[0u8; 8]).is_err());
    }
}
