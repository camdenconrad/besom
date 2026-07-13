//! **besom** — a deterministic simulation harness and ground station for NASA's
//! core Flight System (cFS).
//!
//! Besom owns cFS's clock. Real flight software runs against it, but simulated
//! time advances only when Besom grants a tick, so a scripted scenario produces
//! the same telemetry every run — which is what makes it usable for regression
//! testing, and what a harness paced to wall time can never give you.
//!
//! The mechanism is a PSP timebase module (`timebase_besom`) whose OSAL sync
//! function blocks on a socket. It requires **no changes to cFE core**. See
//! `patches/` and `docs/phase0.md`.
//!
//! # What is reproducible, and what is not
//!
//! The packet *stream* — which messages, in what order, with what lengths, with
//! no gaps or duplicates — is exactly reproducible. A minority of packets land a
//! tick or two early or late: within a single granted tick cFE's tasks are
//! simultaneous in simulated time, and the **host** scheduler decides who runs
//! first. Removing that would take a cooperative scheduler inside OSAL, not a
//! better clock. Assert with [`Transcript::same_stream`], never on raw equality.

pub mod ccsds;
pub mod clock;
pub mod dynamics;
pub mod evs;
pub mod fsw;
pub mod quiesce;
pub mod run;
pub mod session;
pub mod transcript;

#[cfg(feature = "rune")]
pub mod chrome;
#[cfg(feature = "app")]
pub mod gui;
#[cfg(feature = "app")]
pub mod theme;
#[cfg(feature = "app")]
pub mod view3d;

pub use ccsds::{MsgId, TlmPacket};
pub use clock::{Clock, TICK_USEC};
pub use dynamics::{Orbit, Vehicle};
pub use evs::Event;
pub use run::{Cfs, Config};
pub use session::{Session, State};
pub use transcript::{Entry, Transcript};
