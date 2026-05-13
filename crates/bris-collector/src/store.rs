//! Filesystem store and SQLite index for received submissions.
//!
//! Truth lives on disk under `<data_root>/submissions/...`;
//! `<data_root>/index.sqlite` is a rebuildable cache for the
//! review UI's list/filter queries. See
//! `docs/design/diagnostic_collection.md` for the layout.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Utc};
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
        let dir = self.submission_dir(&submitted_at, id);
        std::fs::create_dir_all(&dir)?;
        std::fs::create_dir_all(dir.join("media"))?;

        // Write all files first; only then write the manifest
        // so a partial write doesn't leave an indexed-but-
        // incomplete submission.
        for (name, bytes) in files {
            let safe = sanitize_filename(name);
            let p = dir.join("media").join(safe);
            std::fs::write(&p, bytes)?;
        }
        let manifest_path = dir.join("manifest.json");
        let json = serde_json::to_vec_pretty(manifest)?;
        std::fs::write(&manifest_path, json)?;

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
        Ok(dir)
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

    #[test]
    fn sanitize_strips_separators() {
        assert_eq!(sanitize_filename("../etc/passwd"), "etc_passwd");
        assert_eq!(sanitize_filename("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_filename("normal.png"), "normal.png");
    }
}
