//! Filesystem store and SQLite index for received submissions.
//!
//! Truth lives on disk under `<data_root>/submissions/...` as
//! bris-bundle-v1-shaped artifacts (`manifest.json` + `media/`
//! plus optional `pbris.log` / `calibration/` / `debug/`).
//! `<data_root>/index.sqlite` is a rebuildable cache for the
//! review UI's list/filter queries — never the source of truth.
//! See `docs/design/diagnostic_collection.md` for the layout.
//!
//! # Crash safety
//!
//! A submission is assembled in a private staging directory
//! under `<data_root>/tmp/<id>/` and only becomes visible at its
//! final `submissions/<yyyy>/<mm>/<dd>/<id>/` path via a single
//! `rename(2)`, which POSIX guarantees is atomic within the same
//! filesystem. A crash before the rename leaves an orphaned
//! staging directory (harmless; swept by [`Store::sweep_tmp`])
//! and no partial submission is ever visible at its final path.
//! The SQLite insert happens only after the rename succeeds, so
//! a crash between rename and insert leaves a submission that is
//! present on disk but missing from the index — exactly the case
//! [`Store::rebuild_index`] exists to repair.
//!
//! # Soft-delete
//!
//! Soft-delete never touches the submission's `manifest.json`
//! (submitted captures are precious primary data, never
//! rewritten). Instead it writes a sidecar `deleted.json` marker
//! containing the UTC timestamp of the delete. The index mirror
//! and [`Store::rebuild_index`] both honor this marker. Hard
//! deletion (actually removing the directory) only ever happens
//! via [`Store::retention_sweep`], an explicit operator-invoked
//! action — never a side effect of ingest, startup, or the
//! review API.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Duration, Utc};
use rusqlite::{params, Connection};

use crate::manifest::Manifest;

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
    /// A submission id was not found on disk (rebuild / sweep).
    #[error("submission not found: {0}")]
    NotFound(String),
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

/// Result of an [`Store::rebuild_index`] run.
#[derive(Debug, Clone, Copy, Default)]
pub struct RebuildStats {
    /// Number of submission directories found on disk.
    pub scanned: usize,
    /// Number of rows written into the (cleared) index.
    pub indexed: usize,
    /// Number of submission directories that failed to parse
    /// (corrupt / unreadable manifest.json) and were skipped.
    pub skipped: usize,
}

/// Result of a [`Store::retention_sweep`] run.
#[derive(Debug, Clone, Copy, Default)]
pub struct SweepStats {
    /// Number of soft-deleted submissions past the retention
    /// window that were hard-deleted.
    pub hard_deleted: usize,
    /// Number of soft-deleted submissions still inside the
    /// retention window, left in place.
    pub retained: usize,
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
        std::fs::create_dir_all(data_root.join("tmp"))?;
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
    /// it. Returns the assigned ULID's final directory.
    ///
    /// `files` is a list of `(filename, bytes)` tuples; the
    /// manifest's `media` array must reference exactly these
    /// filenames. Validation is the caller's responsibility
    /// (see [`Manifest::validate`]).
    ///
    /// Crash-safe: the submission is fully assembled in a
    /// staging directory under `<data_root>/tmp/` and only made
    /// visible at its final path via a single atomic `rename`.
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

        // Assemble everything in a private staging directory
        // first. Nothing under `submissions/` is touched until
        // the final `rename` below.
        let staging_dir = self.data_root.join("tmp").join(id);
        if staging_dir.exists() {
            std::fs::remove_dir_all(&staging_dir)?;
        }
        std::fs::create_dir_all(&staging_dir)?;
        std::fs::create_dir_all(staging_dir.join("media"))?;

        for (name, bytes) in files {
            let safe = sanitize_filename(name);
            let p = staging_dir.join("media").join(safe);
            std::fs::write(&p, bytes)?;
        }
        let json = serde_json::to_vec_pretty(manifest)?;
        std::fs::write(staging_dir.join("manifest.json"), json)?;

        // Make the assembled submission visible atomically.
        if let Some(parent) = final_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&staging_dir, &final_dir)?;
        let manifest_path = final_dir.join("manifest.json");

        // Insert into the index mirror only after the rename
        // succeeded, so a crash mid-write never leaves an
        // indexed-but-incomplete submission.
        let kind = kind_label(manifest.submission_kind);
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

    /// Filesystem root.
    #[must_use]
    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    /// Borrow the index connection mutex for cache-only queries.
    /// Exposed within the crate so `routes` handlers can run
    /// list queries without going through a save method.
    pub(crate) fn lock_index(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.index.lock().expect("index mutex poisoned")
    }

    /// Remove any staging directories left behind by a crash
    /// between assembling a submission and its final `rename`.
    /// Safe to call at any time (staging dirs never overlap
    /// with a fully-rendered submission); not called
    /// automatically — an operator (or a maintenance job they
    /// wire up) invokes it explicitly.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] on a filesystem failure.
    pub fn sweep_tmp(&self) -> Result<usize, StoreError> {
        let tmp = self.data_root.join("tmp");
        let mut removed = 0;
        if !tmp.exists() {
            return Ok(0);
        }
        for entry in std::fs::read_dir(&tmp)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                std::fs::remove_dir_all(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Rebuild the SQLite index from scratch by walking
    /// `<data_root>/submissions/**/manifest.json`. The index is
    /// a cache; this is the operator-invoked repair path when
    /// it's lost, corrupted, or has drifted from disk (e.g.
    /// after a crash between a submission's rename and its
    /// index insert). Soft-delete markers (`deleted.json`
    /// sidecars) are honored and re-populate `soft_deleted_at`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] on an index-mirror
    /// failure. Individual unparsable submission directories are
    /// counted in [`RebuildStats::skipped`] rather than
    /// aborting the whole rebuild.
    pub fn rebuild_index(&self) -> Result<RebuildStats, StoreError> {
        let mut stats = RebuildStats::default();
        let submissions_root = self.data_root.join("submissions");
        let mut rows: Vec<(String, Manifest, PathBuf, Option<String>)> = Vec::new();

        if submissions_root.exists() {
            for day_dir in walk_leaf_dirs(&submissions_root, 3)? {
                for entry in std::fs::read_dir(&day_dir)? {
                    let entry = entry?;
                    if !entry.file_type()?.is_dir() {
                        continue;
                    }
                    let sub_dir = entry.path();
                    let id = sub_dir
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    stats.scanned += 1;
                    let manifest_path = sub_dir.join("manifest.json");
                    let Ok(bytes) = std::fs::read(&manifest_path) else {
                        stats.skipped += 1;
                        continue;
                    };
                    let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes) else {
                        stats.skipped += 1;
                        continue;
                    };
                    let deleted_marker = sub_dir.join("deleted.json");
                    let soft_deleted_at = if deleted_marker.exists() {
                        std::fs::read_to_string(&deleted_marker)
                            .ok()
                            .and_then(|s| serde_json::from_str::<DeletedMarker>(&s).ok())
                            .map(|m| m.soft_deleted_at)
                    } else {
                        None
                    };
                    rows.push((id, manifest, manifest_path, soft_deleted_at));
                }
            }
        }

        let mut conn = self.index.lock().expect("index mutex poisoned");
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM submissions", [])?;
        for (id, manifest, manifest_path, soft_deleted_at) in &rows {
            let kind = kind_label(manifest.submission_kind);
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
            stats.indexed += 1;
        }
        tx.commit()?;
        Ok(stats)
    }

    /// Soft-delete a submission: writes a `deleted.json` sidecar
    /// next to its `manifest.json` (the manifest itself is never
    /// rewritten) and flips `soft_deleted_at` in the index
    /// mirror. Files stay on disk untouched. Operator-driven
    /// only — never called from the ingest path.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] if the id isn't indexed,
    /// otherwise [`StoreError::Io`] / [`StoreError::Sqlite`].
    pub fn soft_delete(&self, id: &str) -> Result<(), StoreError> {
        let manifest_path: String = {
            let conn = self.lock_index();
            conn.query_row(
                "SELECT manifest_path FROM submissions WHERE id = ?",
                [id],
                |r| r.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => StoreError::NotFound(id.to_owned()),
                other => StoreError::Sqlite(other),
            })?
        };
        let sub_dir = PathBuf::from(&manifest_path)
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| StoreError::NotFound(id.to_owned()))?;
        let now = Utc::now().to_rfc3339();
        let marker = DeletedMarker {
            soft_deleted_at: now.clone(),
        };
        std::fs::write(sub_dir.join("deleted.json"), serde_json::to_vec(&marker)?)?;
        let conn = self.lock_index();
        conn.execute(
            "UPDATE submissions SET soft_deleted_at = ? WHERE id = ?",
            params![now, id],
        )?;
        Ok(())
    }

    /// Operator-invoked retention sweep: hard-deletes (removes
    /// the on-disk directory and the index row for) every
    /// submission whose `soft_deleted_at` is older than
    /// `retention_days`. Never runs implicitly — no deploy,
    /// startup path, or ingest call ever invokes this. Pass
    /// `dry_run = true` to compute [`SweepStats`] without
    /// touching anything.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] / [`StoreError::Sqlite`] on
    /// failure.
    pub fn retention_sweep(
        &self,
        retention_days: i64,
        dry_run: bool,
    ) -> Result<SweepStats, StoreError> {
        let mut stats = SweepStats::default();
        let cutoff = Utc::now() - Duration::days(retention_days);
        let rows: Vec<(String, String, String)> = {
            let conn = self.lock_index();
            let mut stmt = conn.prepare(
                "SELECT id, manifest_path, soft_deleted_at
                 FROM submissions
                 WHERE soft_deleted_at IS NOT NULL",
            )?;
            let mapped = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            mapped
        };

        for (id, manifest_path, soft_deleted_at) in rows {
            let Ok(deleted_at) = soft_deleted_at.parse::<DateTime<Utc>>() else {
                continue;
            };
            if deleted_at > cutoff {
                stats.retained += 1;
                continue;
            }
            stats.hard_deleted += 1;
            if dry_run {
                continue;
            }
            if let Some(sub_dir) = PathBuf::from(&manifest_path).parent() {
                if sub_dir.exists() {
                    std::fs::remove_dir_all(sub_dir)?;
                }
            }
            let conn = self.lock_index();
            conn.execute("DELETE FROM submissions WHERE id = ?", [&id])?;
        }
        Ok(stats)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DeletedMarker {
    soft_deleted_at: String,
}

fn kind_label(kind: crate::manifest::SubmissionKind) -> &'static str {
    match kind {
        crate::manifest::SubmissionKind::Fix => "fix",
        crate::manifest::SubmissionKind::Calibration => "calibration",
        crate::manifest::SubmissionKind::DebugCapture => "debug_capture",
    }
}

/// Walk `depth` levels down from `root` (e.g. the
/// `yyyy/mm/dd` layers) and return every directory found at the
/// bottom level (the "day" directories that directly contain
/// submission-id directories). Missing intermediate levels are
/// simply skipped, not an error.
fn walk_leaf_dirs(root: &Path, depth: usize) -> Result<Vec<PathBuf>, StoreError> {
    let mut level: Vec<PathBuf> = vec![root.to_path_buf()];
    for _ in 0..depth {
        let mut next = Vec::new();
        for dir in &level {
            if !dir.exists() {
                continue;
            }
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    next.push(entry.path());
                }
            }
        }
        level = next;
    }
    Ok(level)
}

/// Strip path separators and any leading `.` from a filename to
/// keep submissions confined to their submission dir. The
/// filename comes from a Bris-controlled Android app, but
/// defense in depth is cheap.
fn sanitize_filename(name: &str) -> String {
    let trimmed = name.trim_start_matches(['.', '/', '\\']);
    trimmed.replace(['/', '\\'], "_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Device, Manifest, MediaItem, SubmissionKind, Versions};

    #[test]
    fn sanitize_strips_separators() {
        assert_eq!(sanitize_filename("../etc/passwd"), "etc_passwd");
        assert_eq!(sanitize_filename("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_filename("normal.png"), "normal.png");
    }

    fn sample_manifest(kind: SubmissionKind) -> Manifest {
        Manifest {
            schema_version: crate::manifest::SCHEMA_VERSION,
            submission_kind: kind,
            submitted_at: "2026-05-13T14:22:01Z".to_owned(),
            device: Device {
                uuid: "01HXYZTESTDEVICE0000000001".to_owned(),
                model: "Test Device".to_owned(),
                os: "Android 14".to_owned(),
            },
            versions: Versions {
                app: "0.1.0".to_owned(),
                bris_core: "0.0.1".to_owned(),
                bris_data: None,
                submission_schema: crate::manifest::SCHEMA_VERSION,
            },
            captured_at: "2026-05-13T14:18:55Z".to_owned(),
            gps: None,
            note: None,
            fix: Some(serde_json::json!({"lat_deg": 1.0, "lon_deg": 2.0})),
            calibration: None,
            debug_capture: None,
            media: vec![MediaItem {
                filename: "frame.png".to_owned(),
                role: "fix_frame".to_owned(),
                frame_index: None,
                captured_at: None,
                size_bytes: 4,
                checksum_blake3: blake3::hash(b"data").to_hex().to_string(),
            }],
        }
    }

    #[test]
    fn save_is_atomic_and_crash_orphans_are_sweepable() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let manifest = sample_manifest(SubmissionKind::Fix);
        let files = vec![("frame.png".to_owned(), b"data".to_vec())];
        let dir = store.save_submission("01ID", &manifest, &files).unwrap();
        assert!(dir.join("manifest.json").exists());
        assert!(dir.join("media/frame.png").exists());
        // No leftover staging directory after a successful save.
        assert_eq!(
            std::fs::read_dir(tmp.path().join("tmp")).unwrap().count(),
            0
        );

        // Simulate an orphaned staging dir from a crash and
        // confirm the sweep clears it without touching the real
        // submission.
        std::fs::create_dir_all(tmp.path().join("tmp/orphan-id")).unwrap();
        let removed = store.sweep_tmp().unwrap();
        assert_eq!(removed, 1);
        assert!(dir.join("manifest.json").exists());
    }

    #[test]
    fn rebuild_index_recovers_from_empty_index() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let manifest = sample_manifest(SubmissionKind::Fix);
        let files = vec![("frame.png".to_owned(), b"data".to_vec())];
        store.save_submission("01ID", &manifest, &files).unwrap();

        // Wipe the index to simulate loss/corruption.
        store
            .lock_index()
            .execute("DELETE FROM submissions", [])
            .unwrap();
        assert_eq!(
            store
                .lock_index()
                .query_row("SELECT COUNT(*) FROM submissions", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );

        let stats = store.rebuild_index().unwrap();
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.indexed, 1);
        assert_eq!(stats.skipped, 0);
        assert_eq!(
            store
                .lock_index()
                .query_row("SELECT COUNT(*) FROM submissions", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn soft_delete_leaves_files_and_retention_sweep_hard_deletes_after_window() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let manifest = sample_manifest(SubmissionKind::Fix);
        let files = vec![("frame.png".to_owned(), b"data".to_vec())];
        let dir = store.save_submission("01ID", &manifest, &files).unwrap();

        store.soft_delete("01ID").unwrap();
        assert!(dir.join("manifest.json").exists(), "manifest untouched");
        assert!(dir.join("media/frame.png").exists(), "media untouched");
        assert!(dir.join("deleted.json").exists());

        // Retention window not yet elapsed: dry-run and real run
        // both retain.
        let stats = store.retention_sweep(30, false).unwrap();
        assert_eq!(stats.hard_deleted, 0);
        assert_eq!(stats.retained, 1);
        assert!(dir.join("manifest.json").exists());

        // Retention window of 0 days: everything soft-deleted is
        // immediately eligible for hard delete.
        let stats = store.retention_sweep(0, true).unwrap();
        assert_eq!(stats.hard_deleted, 1);
        assert!(
            dir.join("manifest.json").exists(),
            "dry run must not delete"
        );

        let stats = store.retention_sweep(0, false).unwrap();
        assert_eq!(stats.hard_deleted, 1);
        assert!(!dir.exists(), "hard delete removes the directory");
    }
}
