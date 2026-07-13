//! cFE event messages (EVS long-format telemetry, MID `0x0808`).
//!
//! This is the flight software talking: app name, event id, severity, text. A
//! ground station that only renders numeric telemetry and drops these is
//! throwing away the one stream that says *why* something happened.
//!
//! Layout, verified against a real packet on the wire rather than read off the
//! struct — two things trip you up otherwise:
//!
//! * The telemetry header is **16 bytes**, not 12: `CFE_MSG_TelemetryHeader_t`
//!   carries a 4-byte spare after the timestamp.
//! * The payload fields are **little-endian** (host order). Only the CCSDS
//!   primary header is big-endian. Reading them big-endian yields plausible
//!   garbage rather than an obvious failure.
//!
//! ```text
//!   16: AppName[20]  36: EventID:u16le  38: EventType:u16le
//!   40: SpacecraftID:u32le  44: ProcessorID:u32le  48: Message[..]
//! ```

use crate::ccsds::EVS_EVENT_MID;

const HDR: usize = 16;
const APP_NAME: usize = 20;
const EVENT_ID: usize = HDR + APP_NAME; // 36
const EVENT_TYPE: usize = EVENT_ID + 2; // 38
const MSG_OFF: usize = EVENT_TYPE + 2 + 4 + 4; // 48

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Debug,
    Info,
    Error,
    Critical,
}

impl Severity {
    fn from_event_type(t: u16) -> Self {
        // CFE_EVS_EventType_*
        match t {
            1 => Self::Debug,
            2 => Self::Info,
            3 => Self::Error,
            4 => Self::Critical,
            _ => Self::Info,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Error => "ERROR",
            Self::Critical => "CRIT",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub app: String,
    pub event_id: u16,
    pub severity: Severity,
    pub text: String,
}

/// Decode an event packet. `None` if this is not one, or it is malformed.
pub fn parse(msg_id: u16, buf: &[u8]) -> Option<Event> {
    if msg_id != EVS_EVENT_MID || buf.len() < MSG_OFF {
        return None;
    }

    let app = cstr(&buf[HDR..HDR + APP_NAME]);
    let event_id = u16::from_le_bytes([buf[EVENT_ID], buf[EVENT_ID + 1]]);
    let event_type = u16::from_le_bytes([buf[EVENT_TYPE], buf[EVENT_TYPE + 1]]);
    let text = cstr(&buf[MSG_OFF..]);

    Some(Event {
        app,
        event_id,
        severity: Severity::from_event_type(event_type),
        text,
    })
}

/// Read a NUL-terminated, fixed-width field.
fn cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes captured off the wire from a real cFS instance — not synthesised
    /// from the struct definition. The header size and the endianness of the
    /// payload fields are both easy to get wrong in a way that yields plausible
    /// garbage instead of an obvious failure, so this pins them to reality.
    #[test]
    fn decodes_a_real_event_captured_from_cfs() {
        let mut buf = vec![0u8; 172];
        buf[0..16].copy_from_slice(&[
            0x08, 0x08, 0xc0, 0x14, 0x00, 0xa5, // primary header
            0x00, 0x0f, 0x46, 0x2a, 0x28, 0xf5, // timestamp
            0x00, 0x00, 0x00, 0x00, // the 4-byte spare that makes the header 16, not 12
        ]);
        buf[16..22].copy_from_slice(b"TO_LAB");
        buf[36..48].copy_from_slice(&[
            0x03, 0x00, // EventID = 3, LITTLE-endian
            0x02, 0x00, // EventType = 2 (Info), little-endian
            0x42, 0x00, 0x00, 0x00, // SpacecraftID = 66 -- the giveaway
            0x01, 0x00, 0x00, 0x00, // ProcessorID = 1
        ]);
        buf[48..64].copy_from_slice(b"TO telemetry out");

        let e = parse(EVS_EVENT_MID, &buf).unwrap();

        assert_eq!(e.app, "TO_LAB");
        assert_eq!(e.event_id, 3);
        assert_eq!(e.severity, Severity::Info);
        assert_eq!(e.text, "TO telemetry out");
    }

    #[test]
    fn ignores_non_event_telemetry() {
        assert!(parse(0x0883, &[0u8; 172]).is_none());
    }
}
