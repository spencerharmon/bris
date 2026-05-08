//! NMEA 0183 sentence formatting (standard `$GP*` and proprietary `$PBRIS,*`)
//! and transport-layer adapters (TCP server, UDP broadcast, serial).
//!
//! Every emitted sentence is logged at `debug` level via [`tracing`].
//! Under `RUST_LOG=bris_nmea=debug` (or via the journald subscriber on
//! the embedded image) you can observe exactly what's going on the wire.
//!
//! See `plan.org` Phase 5 and the `$PBRIS` protocol spec at
//! `docs/protocol/pbris.md`.

pub mod checksum;
pub mod pbris;
pub mod standard;

pub use checksum::{checksum, format_sentence};
pub use pbris::{
    pbris_err, pbris_fix, pbris_full, pbris_sight, pbris_time, pbris_unc, pbris_ver,
    ErrorCounters, FixSummary, TimeDiagnostic, UncertaintyBudget, PBRIS_SCHEMA_VERSION,
};
pub use standard::{gpgga, gpgll, gpgst, gprmc, FixQuality, QualityThresholds};
