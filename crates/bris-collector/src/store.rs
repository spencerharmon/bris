//! Filesystem store and SQLite index for received submissions.
//!
//! Truth lives on disk under `<data_root>/submissions/...`;
//! `<data_root>/index.sqlite` is a rebuildable cache for the
//! review UI's list/filter queries. See
//! `docs/design/diagnostic_collection.md` for the layout.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Duration, Utc};
use rand::RngCore;
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

use crate::auth::constant_time_eq;
use crate::manifest::Manifest;

/// Number of random bytes in a freshly-minted per-device token,
/// hex-encoded to a 64-character string. 256 bits of entropy —
/// comfortably infeasible to guess or brute-force.
const DEVICE_TOKEN_BYTES: usize = 32;

/// Name of the sidecar file written into a submission directory
/// when it is soft-deleted. Its contents are the RFC3339
/// soft-delete timestamp, so it is both the marker and the
/// authoritative value — the SQLite mirror is rebuildable from
/// it, never the other way around.
const DELETED_MARKER: &str = "deleted_at.txt";

/// Errors returned by the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Filesystem error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization error when writing the manifest.
    #[error("manifest serialize: {0}")]
    ManifestSerialize(#[from] serde_json::Error),
    /// SQLite error from the index mirror.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A submitted_at field that doesn't parse as RFC3339.
    #[error("invalid submitted_at: {0}")]
    InvalidSubmittedAt(String),
}

/// Local filesystem store with a SQLite mirror.
///
/// Construct via [`Store::open`]. The store is `Send + Sync`
/// via the underlying connection's `Mutex`; cloning the
/// `Arc<Store>` gives shared access from many request handlers.
#[derive(Debug)]
pub struct Store {
    data_root: PathBuf,
    index: std::sync::Mutex<Connection>,
}

impl Store {
    /// Open (and create if missing) a store rooted at
    /// `data_root`. Creates the directory tree and opens the
    /// SQLite index, running migrations if needed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] for filesystem failures and
    /// [`StoreError::Sqlite`] for index-mirror failures.
    pub fn open(data_root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let data_root = data_root.into();
        std::fs::create_dir_all(&data_root)?;
        std::fs::create_dir_all(data_root.join("submissions"))?;
        let index_path = data_root.join("index.sqlite");
        let conn = Connection::open(&index_path)?;
        Self::migrate(&conn)?;
        Ok(Self {
            data_root,
            index: std::sync::Mutex::new(conn),
        })
    }

    /// Schema migration. Idempotent.
    fn migrate(conn: &Connection) -> Result<(), StoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS submissions (
                 id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 submitted_at TEXT NOT NULL,
                 captured_at TEXT NOT NULL,
                 device_uuid TEXT NOT NULL,
                 app_version TEXT NOT NULL,
                 bris_core_version TEXT NOT NULL,
                 has_gps INTEGER NOT NULL,
                 note_present INTEGER NOT NULL,
                 manifest_path TEXT NOT NULL,
                 soft_deleted_at TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_submitted_at
                 ON submissions (submitted_at);
             CREATE INDEX IF NOT EXISTS idx_kind
                 ON submissions (kind);
             CREATE INDEX IF NOT EXISTS idx_device_uuid
                 ON submissions (device_uuid);
             CREATE TABLE IF NOT EXISTS devices (
                 device_uuid TEXT PRIMARY KEY,
                 token_hash TEXT NOT NULL,
                 issued_at TEXT NOT NULL
             );
             ",
        )?;
        Ok(())
    }

    /// Compute the on-disk directory for a submission given its
    /// declared `submitted_at` and the generated ULID.
    fn submission_dir(&self, submitted_at: &DateTime<Utc>, id: &str) -> PathBuf {
        self.data_root
            .join("submissions")
            .join(format!("{:04}", submitted_at.year()))
            .join(format!("{:02}", submitted_at.month()))
            .join(format!("{:02}", submitted_at.day()))
            .join(id)
    }

    /// Persist a freshly-received submission to disk and index
    /// it. Returns the assigned ULID.
    ///
    /// `files` is a list of `(filename, bytes)` tuples; the
    /// manifest's `media` array must reference exactly these
    /// filenames. Validation is the caller's responsibility
    /// (see [`Manifest::validate`]).
    ///
    /// Crash-safe: all files and the manifest are written into
    /// a staging directory under `<data_root>/submissions/.staging/`
    /// first, then the whole directory is atomically renamed
    /// (a single `rename(2)`, same filesystem) into its final
    /// `<yyyy>/<mm>/<dd>/<ulid>/` location. A crash before the
    /// rename leaves only an orphaned staging directory — never
    /// a partially-written entry under `submissions/<yyyy>/...`
    /// that the index or a future scan could mistake for a real
    /// submission. The index insert happens only after the
    /// rename succeeds, so a crash between rename and insert
    /// leaves a submission on disk with no index row — exactly
    /// what `rebuild_index` exists to repair (the index is a
    /// rebuildable cache, never the source of truth).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] or [`StoreError::Sqlite`] as
    /// applicable.
    pub fn save_submission(
        &self,
        id: &str,
        manifest: &Manifest,
        files: &[(String, Vec<u8>)],
    ) -> Result<PathBuf, StoreError> {
        let submitted_at = manifest
            .submitted_at
            .parse::<DateTime<Utc>>()
            .map_err(|e| StoreError::InvalidSubmittedAt(format!("{e}")))?;
        let final_dir = self.submission_dir(&submitted_at, id);

        let staging_root = self.data_root.join("submissions").join(".staging");
        std::fs::create_dir_all(&staging_root)?;
        let staging_dir = staging_root.join(id);
        // Defensive: a retried ULID (should not happen, ULIDs
        // are monotonic-random) could leave a stale staging dir
        // from a prior crashed attempt. Clear it first so the
        // rename below can't fail on a non-empty destination.
        if staging_dir.exists() {
            std::fs::remove_dir_all(&staging_dir)?;
        }
        std::fs::create_dir_all(staging_dir.join("media"))?;
        std::fs::create_dir_all(staging_dir.join("calibration"))?;

        let by_filename: std::collections::HashMap<&str, &crate::manifest::MediaItem> = manifest
            .media
            .iter()
            .map(|item| (item.filename.as_str(), item))
            .collect();
        for (name, bytes) in files {
            let safe = sanitize_filename(name);
            let role = by_filename
                .get(name.as_str())
                .map(|item| item.role.as_str());
            let rel = media_destination(manifest.submission_kind, role, &safe);
            let p = staging_dir.join(&rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&p, bytes)?;
        }
        let json = serde_json::to_vec_pretty(manifest)?;
        std::fs::write(staging_dir.join("manifest.json"), json)?;

        // Atomic finalize: one rename from staging into the
        // real, dated location. Ensure the parent (yyyy/mm/dd)
        // exists first; the rename itself is atomic on a single
        // filesystem (the staging dir and data_root share one).
        if let Some(parent) = final_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&staging_dir, &final_dir)?;
        let manifest_path = final_dir.join("manifest.json");

        // Insert into the index mirror.
        let kind = match manifest.submission_kind {
            crate::manifest::SubmissionKind::Fix => "fix",
            crate::manifest::SubmissionKind::Calibration => "calibration",
            crate::manifest::SubmissionKind::DebugCapture => "debug_capture",
        };
        let conn = self.index.lock().expect("index mutex poisoned");
        conn.execute(
            "INSERT INTO submissions (
                 id, kind, submitted_at, captured_at,
                 device_uuid, app_version, bris_core_version,
                 has_gps, note_present, manifest_path
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                id,
                kind,
                manifest.submitted_at,
                manifest.captured_at,
                manifest.device.uuid,
                manifest.versions.app,
                manifest.versions.bris_core,
                i32::from(manifest.gps.is_some()),
                i32::from(manifest.note.is_some()),
                manifest_path.to_string_lossy().into_owned(),
            ],
        )?;
        Ok(final_dir)
    }

    /// First-contact device registration: mint a fresh
    /// per-device bearer token, persist only its SHA-256 hash
    /// (never the token itself) keyed by `device_uuid`, and
    /// return the raw token to hand back to the caller — the one
    /// and only time it exists outside the device's own memory.
    ///
    /// Idempotent-by-rotation: calling this again for an
    /// already-registered device mints and persists a *new*
    /// token, immediately invalidating the old one (an admin-
    /// token-authenticated caller re-registering a device is
    /// treated as an explicit rotation, e.g. after a suspected
    /// leak — never silently returns the previous token, since
    /// the previous raw token is not retrievable anyway).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] on an index failure.
    pub fn register_device(&self, device_uuid: &str) -> Result<String, StoreError> {
        let mut token_bytes = [0u8; DEVICE_TOKEN_BYTES];
        rand::rng().fill_bytes(&mut token_bytes);
        let token = hex::encode(token_bytes);
        let token_hash = hex::encode(Sha256::digest(token.as_bytes()));
        let issued_at = Utc::now().to_rfc3339();

        let conn = self.index.lock().expect("index mutex poisoned");
        conn.execute(
            "INSERT INTO devices (device_uuid, token_hash, issued_at)
             VALUES (?, ?, ?)
             ON CONFLICT(device_uuid) DO UPDATE SET
                 token_hash = excluded.token_hash,
                 issued_at = excluded.issued_at",
            params![device_uuid, token_hash, issued_at],
        )?;
        Ok(token)
    }

    /// Verify a bearer token presented for `device_uuid` against
    /// the hash persisted by [`Self::register_device`]. Hashes
    /// the presented token and compares the two hashes in
    /// constant time — an unregistered device (no row) always
    /// returns `Ok(false)` rather than erroring, so a probing
    /// caller can't distinguish "unregistered" from "wrong token"
    /// by timing or error shape.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] on an index failure other
    /// than "no such device".
    pub fn verify_device_token(&self, device_uuid: &str, token: &str) -> Result<bool, StoreError> {
        let stored_hash: Option<String> = {
            let conn = self.index.lock().expect("index mutex poisoned");
            match conn.query_row(
                "SELECT token_hash FROM devices WHERE device_uuid = ?",
                [device_uuid],
                |r| r.get(0),
            ) {
                Ok(hash) => Some(hash),
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(e) => return Err(StoreError::Sqlite(e)),
            }
        };
        let Some(stored_hash) = stored_hash else {
            return Ok(false);
        };
        let presented_hash = hex::encode(Sha256::digest(token.as_bytes()));
        Ok(constant_time_eq(
            presented_hash.as_bytes(),
            stored_hash.as_bytes(),
        ))
    }

    /// Filesystem root.
    #[must_use]
    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    /// Borrow the index connection mutex for cache-only queries.
    /// `routes` handlers use it for list queries without going
    /// through a save method; tests use it to inspect or
    /// deliberately corrupt the cache (e.g. to exercise
    /// `rebuild_index`) without racing a second sqlite
    /// connection onto the same file.
    pub fn lock_index(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.index.lock().expect("index mutex poisoned")
    }

    /// Rebuild the SQLite index from scratch by walking
    /// `<data_root>/submissions/` for `manifest.json` files.
    /// The filesystem (manifest + optional
    /// [`DELETED_MARKER`] sidecar) is the source of truth; this
    /// makes the index a genuinely rebuildable cache rather
    /// than a claim. Operator-driven only (a CLI subcommand),
    /// never run automatically on startup or as a deploy side
    /// effect.
    ///
    /// Returns the number of submissions indexed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] for a filesystem failure while
    /// walking, or [`StoreError::Sqlite`] for an index failure.
    /// A submission directory whose `manifest.json` fails to
    /// parse is skipped with a warning logged, not fatal to the
    /// whole rebuild.
    pub fn rebuild_index(&self) -> Result<usize, StoreError> {
        let submissions_root = self.data_root.join("submissions");
        let mut found = Vec::new();
        if submissions_root.exists() {
            walk_manifests(&submissions_root, &mut found)?;
        }

        let mut conn = self.index.lock().expect("index mutex poisoned");
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM submissions", [])?;
        let mut count = 0usize;
        for manifest_path in &found {
            let Some(dir) = manifest_path.parent() else {
                continue;
            };
            let bytes = match std::fs::read(manifest_path) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(path = %manifest_path.display(), error = %e, "rebuild_index: read failed, skipping");
                    continue;
                }
            };
            let manifest: Manifest = match serde_json::from_slice(&bytes) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(path = %manifest_path.display(), error = %e, "rebuild_index: parse failed, skipping");
                    continue;
                }
            };
            let Some(id) = dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let kind = match manifest.submission_kind {
                crate::manifest::SubmissionKind::Fix => "fix",
                crate::manifest::SubmissionKind::Calibration => "calibration",
                crate::manifest::SubmissionKind::DebugCapture => "debug_capture",
            };
            let soft_deleted_at = read_deleted_marker(dir)?;
            tx.execute(
                "INSERT INTO submissions (
                     id, kind, submitted_at, captured_at,
                     device_uuid, app_version, bris_core_version,
                     has_gps, note_present, manifest_path, soft_deleted_at
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    id,
                    kind,
                    manifest.submitted_at,
                    manifest.captured_at,
                    manifest.device.uuid,
                    manifest.versions.app,
                    manifest.versions.bris_core,
                    i32::from(manifest.gps.is_some()),
                    i32::from(manifest.note.is_some()),
                    manifest_path.to_string_lossy().into_owned(),
                    soft_deleted_at,
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }

    /// Soft-delete a submission: writes the [`DELETED_MARKER`]
    /// sidecar (the durable, on-disk record) and mirrors it into
    /// the index row. Files remain on disk; only the review UI's
    /// default listing hides it (`list_submissions` filters
    /// `soft_deleted_at IS NULL`). Reserved for operator-driven
    /// use (a CLI subcommand) — never triggered as a deploy side
    /// effect.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] if the submission id is
    /// unknown or the marker can't be written, or
    /// [`StoreError::Sqlite`] on an index failure.
    pub fn soft_delete(&self, id: &str) -> Result<(), StoreError> {
        let dir = {
            let conn = self.index.lock().expect("index mutex poisoned");
            let manifest_path: String = conn
                .query_row(
                    "SELECT manifest_path FROM submissions WHERE id = ?",
                    [id],
                    |r| r.get(0),
                )
                .map_err(|_| {
                    StoreError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("submission {id} not found in index"),
                    ))
                })?;
            PathBuf::from(manifest_path)
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| {
                    StoreError::Io(std::io::Error::other("manifest_path has no parent"))
                })?
        };
        let now = Utc::now().to_rfc3339();
        std::fs::write(dir.join(DELETED_MARKER), &now)?;
        let conn = self.index.lock().expect("index mutex poisoned");
        conn.execute(
            "UPDATE submissions SET soft_deleted_at = ? WHERE id = ?",
            params![now, id],
        )?;
        Ok(())
    }

    /// Hard-delete every submission whose [`DELETED_MARKER`]
    /// timestamp is older than `retention`, permanently removing
    /// its directory and index row. Operator-driven only — the
    /// retention sweep is never run automatically; it must be
    /// invoked explicitly (e.g. the `retention-sweep` CLI
    /// subcommand) so a precious primary-data wipe is always a
    /// deliberate act, never a side effect of a deploy or
    /// restart.
    ///
    /// With `dry_run = true`, reports what would be removed
    /// without touching disk or the index.
    ///
    /// Returns the ids that were (or, in dry-run, would be)
    /// removed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] or [`StoreError::Sqlite`] on
    /// failure. A per-submission failure aborts the sweep rather
    /// than silently skipping precious data.
    pub fn retention_sweep(
        &self,
        retention: Duration,
        dry_run: bool,
    ) -> Result<Vec<String>, StoreError> {
        let cutoff = Utc::now() - retention;
        let candidates: Vec<(String, String)> = {
            let conn = self.index.lock().expect("index mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT id, manifest_path FROM submissions WHERE soft_deleted_at IS NOT NULL",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };

        let mut removed = Vec::new();
        for (id, manifest_path) in candidates {
            let dir = match Path::new(&manifest_path).parent() {
                Some(d) => d.to_path_buf(),
                None => continue,
            };
            let marker_path = dir.join(DELETED_MARKER);
            let Ok(deleted_at) = std::fs::read_to_string(&marker_path) else {
                continue; // no marker on disk; nothing to sweep
            };
            let Ok(deleted_at) = deleted_at.trim().parse::<DateTime<Utc>>() else {
                continue;
            };
            if deleted_at > cutoff {
                continue; // still within the retention window
            }
            if !dry_run {
                std::fs::remove_dir_all(&dir)?;
                let conn = self.index.lock().expect("index mutex poisoned");
                conn.execute("DELETE FROM submissions WHERE id = ?", params![id])?;
            }
            removed.push(id);
        }
        Ok(removed)
    }
}

/// Recursively collect every `manifest.json` under `dir`,
/// skipping the `.staging` directory (orphaned in-flight
/// uploads, never real submissions).
fn walk_manifests(dir: &Path, found: &mut Vec<PathBuf>) -> Result<(), StoreError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some(".staging") {
                continue;
            }
            walk_manifests(&path, found)?;
        } else if path.file_name().and_then(|n| n.to_str()) == Some("manifest.json") {
            found.push(path);
        }
    }
    Ok(())
}

/// Read the soft-delete marker for a submission directory, if
/// present.
fn read_deleted_marker(dir: &Path) -> Result<Option<String>, StoreError> {
    let marker = dir.join(DELETED_MARKER);
    if !marker.exists() {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(marker)?.trim().to_owned()))
}

/// Strip path separators and any leading `.` from a filename to
/// keep submissions confined to their submission dir. The
/// filename comes from a Bris-controlled Android app, but
/// defense in depth is cheap.
fn sanitize_filename(name: &str) -> String {
    let trimmed = name.trim_start_matches(['.', '/', '\\']);
    trimmed.replace(['/', '\\'], "_")
}

/// Route a received file to its on-disk destination (relative
/// to the submission directory) per
/// `docs/design/diagnostic_collection.md`'s server-side layout:
/// `pbris_log`-role files land at the submission root as a
/// single `pbris.log`; calibration-role files (or any file in a
/// `Calibration`-kind submission) land under `calibration/`;
/// everything else lands under `media/`.
fn media_destination(
    submission_kind: crate::manifest::SubmissionKind,
    role: Option<&str>,
    sanitized_filename: &str,
) -> PathBuf {
    use crate::manifest::SubmissionKind;

    if role == Some("pbris_log") {
        return PathBuf::from("pbris.log");
    }
    let is_calibration_role = matches!(role, Some("calibration_frame" | "intrinsics_toml"));
    if is_calibration_role || submission_kind == SubmissionKind::Calibration {
        return PathBuf::from("calibration").join(sanitized_filename);
    }
    PathBuf::from("media").join(sanitized_filename)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_separators() {
        assert_eq!(sanitize_filename("../etc/passwd"), "etc_passwd");
        assert_eq!(sanitize_filename("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_filename("normal.png"), "normal.png");
    }

    #[test]
    fn register_device_issues_token_verifiable_only_with_correct_uuid_and_token() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Store::open(tmp.path()).expect("store open");

        let token = store.register_device("device-a").expect("register_device");
        assert!(!token.is_empty());

        assert!(store
            .verify_device_token("device-a", &token)
            .expect("verify"));
        assert!(!store
            .verify_device_token("device-a", "wrong-token")
            .expect("verify"));
        assert!(!store
            .verify_device_token("device-b", &token)
            .expect("verify: unregistered device"));
    }

    #[test]
    fn register_device_rotates_and_invalidates_the_prior_token() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Store::open(tmp.path()).expect("store open");

        let first = store.register_device("device-a").expect("register_device");
        let second = store
            .register_device("device-a")
            .expect("register_device (rotation)");

        assert_ne!(first, second);
        assert!(!store
            .verify_device_token("device-a", &first)
            .expect("verify"));
        assert!(store
            .verify_device_token("device-a", &second)
            .expect("verify"));
    }
}
