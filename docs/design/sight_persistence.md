# Sight Persistence

Status: design. Implements on-disk persistence of the
opportunistic sight pool so accumulated sights survive
process restart, app backgrounding, and OS-initiated kills.
Lives in a new module `crates/bris-streaming/src/store.rs`;
surfaced through the existing `Engine` constructor and a
small set of new FFI getters.

This document specifies the on-disk format, the file
layout, the engine integration points, the retention
policy, and the operator-visible behaviour. It is not a
discussion of design alternatives.

## Scope

The store persists **reduced sights**. It does not persist
frames, body records, horizon records, intermediate Stage
A–D products, or pixel data. Those continue to live in the
in-memory `Storage` ring with the existing
`stitching_window_seconds` lifetime (default 2 s) and are
gone on process exit.

A sight is the smallest self-contained unit that retains
navigational value across process boundaries: it has the
body's GP via its time + body id, an Ho, an azimuth, an
LOP, and a σ. Everything needed to reproduce a fix on
restart.

## File layout

Under the configured data root (`EngineConfig::
data_root: PathBuf`, default `~/.bris/` on Linux,
app-private internal storage on Android):

```
<data-root>/
  sights/
    current.log              active append-only log
    current.log.lock         advisory exclusive lock during writes
    archive/
      2026-05-25T00.log      hourly-rotated closed log
      2026-05-24T23.log
      ...
  fixes/
    current.log              same-shaped log for published fixes
    archive/
      2026-05-25T00.log
      ...
```

`fixes/` is structurally identical to `sights/` and used for
post-hoc audit / display of the fix history. Same code path,
different record type.

## Record format

Records are little-endian, fixed-width, versioned. The
on-disk struct is `#[repr(C)]` and serialized via
`bincode` with the workspace's existing dep (no new
workspace deps required; `bincode` is added on first use of
the store via `workspace.dependencies`).

```rust
#[repr(C)]
struct SightRecord {
    /// Magic + version. 0x42525353 'BRSS' || u32 version.
    /// Bumping version invalidates older logs; the loader
    /// archives them with a `.unsupported_v<N>` suffix
    /// rather than failing to start.
    magic_version: u64,
    /// Sight anchor time, TT seconds since J2000 (f64
    /// matching the in-memory Tt).
    anchor_tt_seconds: f64,
    /// Walltime when this record was written, UTC seconds
    /// since UNIX epoch. Forensic only; not used for
    /// retention or geometry.
    written_at_unix_seconds: f64,
    /// Body discriminant. 0 = SolarSystem, 1 = Star.
    body_kind: u8,
    /// 7 bytes padding to align body_payload to 8.
    _pad0: [u8; 7],
    /// For SolarSystem: SolarSystemBody as u32, zero-extended.
    /// For Star: HR number as u32, zero-extended.
    body_payload: u64,
    /// LOP fields, in radians and nm (matching in-memory).
    assumed_lat_rad: f64,
    assumed_lon_rad: f64,
    azimuth_rad: f64,
    intercept_nm: f64,
    intercept_sigma_nm: f64,
    /// Per-sight altitude σ, radians.
    altitude_sigma_rad: f64,
    /// Provenance string, max 16 bytes ASCII, null-padded.
    /// Mirrors HorizonProvenance::Display.
    provenance: [u8; 16],
}
```

Size: 96 bytes per record. A week of continuous scanning at
1 sight/s = 58 MB; a day = 8.3 MB.

`FixRecord` follows the same shape with appropriate fields
(lat, lon, covariance, ellipse, sight_count). Approximately
the same size.

Records are appended with `O_APPEND` so concurrent writers
from the same process do not interleave; cross-process
writing is prevented by the advisory lock. Each append is
followed by `fsync` on the file but not the directory
(durability of the record vs cost of fdatasync per write —
losing the last few seconds of records on hard reset is
acceptable; losing earlier records is not).

## Rotation

`current.log` rotates when **either** of:
- File size exceeds `rotation_size_bytes` (default 8 MB,
  ~24 hr at typical rate).
- Walltime crosses an hour boundary (UTC).

On rotation, `current.log` is renamed to
`archive/<rfc3339-hour>.log` atomically, a new `current.log`
is opened, and the engine continues. The archive directory
is scanned on startup for retention pruning (below).

## Retention

Two retention windows are tracked independently:

- **Operational retention**: how long sights remain in the
  in-memory pool for the fix solver. Today this is the
  Stage E `sight_window_seconds` (default 600 s). Unchanged
  by this design — the in-memory pool still trims to this
  window for active use.
- **Archive retention**: how long sights remain on disk for
  audit / replay / forensics. Default `retention_days = 7`.
  Files in `archive/` older than this are deleted on engine
  startup and on each rotation. `current.log` is never
  deleted while active.

The two windows are decoupled deliberately. The
in-memory pool may have 10 sights spanning the last 10 min
even though the disk has thousands spanning a week. The
operational fix solver only sees what's in the pool;
operator-initiated replay can reach back into the archive.

## Engine integration

### Startup

`Engine::new(cfg, store_path)` opens or creates the store:

1. Open `<data-root>/sights/current.log`. If it doesn't
   exist, create it with a fresh magic header.
2. Read the file end-to-back, parsing `SightRecord`s.
3. For each record whose `anchor_tt` is within
   `cfg.sight_window_seconds` of the *current* clock,
   reconstruct an in-memory `Sight` and insert into the
   `SightWindow`. (Older records stay on disk but are not
   in the operational pool.)
4. Open `<data-root>/fixes/current.log`. Locate the most
   recent `FixRecord` whose `anchor_tt` is within
   `cfg.position_prior_max_age_seconds` (default 300) and,
   if found, install it as the engine's startup
   `PositionPrior`. This closes the "AP gap reopens on
   restart" hole.
5. Prune archive directory per `retention_days`.

Startup is bounded: read time is `O(size of current.log)`,
worst-case 8 MB sequential read. Hydration completes in
<100 ms on Pi Zero 2W and well under the camera's
first-frame latency on Android.

### Steady state

Every `SightWindow::try_insert` that returns `true` triggers
a `SightRecord` append. Every successful `try_publish`
triggers a `FixRecord` append. Both are synchronous on the
Stage E thread; the I/O is bounded (96 bytes + fsync) and
empirically <1 ms on commodity SSDs and <10 ms on Pi Zero
2W microSD. If this becomes a hotspot a per-store writer
thread with a bounded channel is the documented escalation,
but the default is the simple synchronous path.

Replacement-on-insert (capacity gate) also appends: the
displaced sight stays on disk for archive purposes, only
the in-memory pool replaces. This is intentional —
operational and archive retention are independent.

Eviction (age) does **not** trigger any disk operation. The
in-memory pool shrinks; the on-disk record persists until
archive-retention pruning sweeps it.

### Shutdown

`Engine::shutdown()` (and `Drop` for safety) flushes the
write buffer, releases the lock, and returns. No flush-then-
close ceremony beyond standard file semantics. The file is
always consistent at any point during normal operation
because each record is a complete unit and the magic header
is written on create.

If the process is killed mid-record-write, the next startup
sees a partial trailing record. The loader detects the
short read at the tail, logs a `tracing::warn!`, truncates
the partial bytes, and continues. No data corruption
propagates.

## Query surface

The store exposes a small read API beyond steady-state
hydration:

```rust
impl SightStore {
    /// All sights with anchor_tt in [start, end], oldest first.
    /// Reads from current.log and any archive files whose
    /// hour overlaps the range.
    pub fn query_range(&self, start: Tt, end: Tt)
        -> std::io::Result<Vec<Sight>>;

    /// Most recent N sights regardless of age, newest first.
    /// Used by the Android "session view" surfacing.
    pub fn most_recent(&self, n: usize)
        -> std::io::Result<Vec<Sight>>;

    /// All fixes published in the given range.
    pub fn query_fixes(&self, start: Tt, end: Tt)
        -> std::io::Result<Vec<PublishedFix>>;
}
```

`Engine` exposes these as `engine.query_sights(...)` etc.;
the FFI surfaces them as `Vec<FfiSight>` and
`Vec<FfiPublishedFix>` getters.

The store does not build any persistent index. All queries
are linear scans of the affected log segments. With 96-byte
records and 8 MB segments, a 1-week range is at most
~80 MB of sequential read — well under a second on every
target platform. Query latency is not a hot path
(operator-initiated session view, not per-frame).

## Schema evolution

`magic_version` is the contract. To add a field:

1. Bump `version` by 1.
2. New code writes records with the new layout.
3. New code reading old-versioned records archives the
   file (`*.unsupported_v<N>`) on encounter and continues
   with a fresh `current.log` at the new version. Operator-
   visible warning in `tracing::warn!`.

This is deliberately strict — the format is small enough
that converting is cheaper to write than to maintain a
multi-version reader. The archived old file remains
accessible for forensic replay by tooling pinned to that
version.

## PII / privacy

Sights are low-PII by construction (time + body + altitude +
azimuth). They reveal *that* an observation was made and
roughly *where* the body was, but not where the observer
was. Fixes are higher-PII (latitude and longitude of the
observer).

Both stores live under the app-private data root on
Android (not accessible to other apps without root). On
Linux the default is the user's home directory; no
additional encryption layer in this design. The
diagnostic-submission subsystem (separate from this store)
retains its own per-submission operator review before any
data leaves the device; this store is local-only and never
uploads automatically.

## Failure modes

| Failure | Behaviour |
|---|---|
| Disk full on append | `tracing::error!`, increment `EngineDiagnostics::store_append_failures`, drop the record on the floor, in-memory pool unaffected, engine continues. |
| Corrupted record mid-file | Loader logs `tracing::warn!`, skips the record, continues. Single bad record never disables the store. |
| Missing data root directory | `Engine::new` creates it (recursively). Fails with `EngineError::StoreUnusable` only if creation fails (permissions). |
| Lock contention | `Engine::new` refuses to start if another `Engine` holds the lock. `tracing::error!` with the pid path. Operator must stop the other instance. |
| Clock skew (system clock jump) | `anchor_tt_seconds` is derived from the frame timestamp, not system walltime, so frame-derived sights are immune. `written_at_unix_seconds` is informational only. |

## Diagnostics

`EngineDiagnostics` gains:

- `sights_persisted_total: u64`
- `sights_loaded_on_start: u64`
- `fixes_persisted_total: u64`
- `store_append_failures: u64`
- `store_corrupted_records_skipped: u64`
- `store_archive_files_pruned: u64`
- `store_current_log_bytes: u64`

These surface via the existing `EngineDiagnostics` snapshot
and the `EngineDiagnostics` FFI mirror.

## FFI surface additions

```rust
// crates/bris-ffi/src/lib.rs additions

#[uniffi::export]
impl Engine {
    /// Sights currently in the operational pool. Lean —
    /// reads the in-memory window, not the disk.
    pub fn pool_sights(&self) -> Vec<FfiSight>;

    /// Most recent N sights from the store (including
    /// archived). Reads from disk.
    pub fn recent_sights(&self, n: u32)
        -> Result<Vec<FfiSight>, FfiEngineError>;

    /// Most recent fix on disk. Survives restart. Distinct
    /// from the push-subscribe path which only delivers
    /// in-process fixes.
    pub fn last_persisted_fix(&self)
        -> Result<Option<FfiPublishedFix>, FfiEngineError>;
}
```

The existing `subscribe_fixes` push API is unchanged;
`last_persisted_fix` is the cold-open getter that closes the
"app reopened, latest fix gone" gap.

## Configuration

`EngineConfig` additions:

```rust
pub struct StoreConfig {
    /// Root directory for sight/fix logs.
    pub data_root: std::path::PathBuf,
    /// Days to retain archive segments. 0 = no archive,
    /// only current.log.
    pub retention_days: u32,           // default 7
    /// Bytes per rotation. Files larger than this rotate
    /// immediately on the next append.
    pub rotation_size_bytes: u64,      // default 8 MB
    /// If false, the store is disabled and the engine runs
    /// in-memory-only (legacy behaviour). Tests use this.
    pub enabled: bool,                  // default true
}

impl EngineConfig {
    pub store: StoreConfig,
}
```

Disabling the store is a single config flag and reverts to
pre-design behaviour; tests that exercise non-persistence
behaviour use `StoreConfig { enabled: false, .. }`.

## Test corpus

Unit tests in `crates/bris-streaming/src/store.rs`:

1. **Round-trip**: write 100 sights, reopen, read 100.
2. **Partial trailing record**: write 99 sights + a half
   record, reopen, read 99 + warning logged.
3. **Magic mismatch**: write a file with wrong magic,
   reopen, file archived as `unsupported`, new current.log
   begins.
4. **Rotation by size**: configure 1 KB rotation, write
   ~20 sights, verify rotation occurred and archive file
   exists.
5. **Rotation by hour**: mock clock crossing hour boundary,
   verify rotation.
6. **Retention pruning**: place a 30-day-old file in
   archive/, start engine, verify file removed.
7. **Concurrent open**: open store, attempt second open,
   verify lock-held error.
8. **Disk full**: mock write failure on append, verify
   diagnostics counter increments and engine continues.
9. **Hydration with mixed ages**: write sights at t-3000,
   t-300, t-30 with `sight_window_seconds=600`; verify only
   the t-300 and t-30 enter the pool.
10. **Position-prior recovery**: write a fix, restart,
    verify the engine's `last_published_fix` is populated
    on first `push_frame`.

Integration test in `crates/bris-streaming/tests/
sight_persistence.rs`:

- Start an engine with a tempdir store, push frames until
  several sights accumulate.
- Drop the engine.
- Open a new engine on the same tempdir.
- Verify pool sights match, position prior is set, and the
  next reduction uses the recovered prior without re-asking
  for an AP.

## Out of scope

- Cross-device sync of the store. The diagnostic-collection
  submission path covers the upload case; that is operator-
  initiated and uses its own format (`docs/design/
  diagnostic_collection.md`).
- Encryption at rest. App-private storage on Android is the
  current trust boundary; full-disk encryption (Android's
  default on modern devices) covers the rest.
- Multi-process writer. One writer at a time; the lock
  enforces this.
- Streaming export to a third-party logger (Prometheus,
  syslog, etc.). `tracing` already emits per-record
  events at `debug` for callers who want their own sink.
