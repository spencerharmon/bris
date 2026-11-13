//! Bounded CLI driver over the same consent-aware queue used by Android.
//! Capture completion is explicit; replay and arbitrary command exits are not consent.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use bris_bundle::{BundleManifest, SessionManifest};
use bris_submit::{build_submission, CollectorEndpoint, SubmissionQueue, SubmissionSource};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{load_config, UploadConfig};
use crate::SessionCompleteArgs;

#[derive(Debug, Clone, Copy, clap::Subcommand)]
pub(crate) enum UploadCommand {
    /// Show pending/failed counts without sending anything.
    Status,
    /// Retry eligible entries within `upload.drain_seconds`, then return.
    Retry,
    /// Persist the toggle and revoke old consent, including while idle.
    Set {
        /// Explicitly turn automatic uploads on or off.
        #[arg(long, action = clap::ArgAction::Set)]
        automatic_upload: bool,
    },
}

#[derive(Default, Deserialize, Serialize)]
struct Consent {
    fingerprint: Option<String>,
    scope: Option<String>,
}

fn root(config: &UploadConfig) -> PathBuf {
    config
        .queue_root
        .clone()
        .unwrap_or_else(|| crate::default_data_root().join("submit-queue"))
        .join("automatic")
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("JSON path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(&serde_json::to_vec_pretty(value)?)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}

fn endpoint(config: &UploadConfig) -> Result<CollectorEndpoint> {
    if config.drain_seconds == 0 {
        bail!("upload.drain_seconds must be positive");
    }
    let required = |value: &Option<String>, name| {
        value
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .with_context(|| format!("automatic upload requires upload.{name}"))
    };
    Ok(CollectorEndpoint::new(
        required(&config.collector_url, "collector_url")?,
        required(&config.token, "token")?,
    )
    .with_device_uuid(required(&config.device_uuid, "device_uuid")?))
}

fn same_consent(a: &UploadConfig, b: &UploadConfig) -> bool {
    a.automatic_upload == b.automatic_upload
        && a.collector_url == b.collector_url
        && a.token == b.token
        && a.device_uuid == b.device_uuid
        && root(a) == root(b)
}

fn configure(queue: &SubmissionQueue, config: &UploadConfig) -> Result<Option<String>> {
    let path = root(config).join("upload-state.json");
    let mut consent: Consent = read_json(&path)?;
    if !config.automatic_upload {
        atomic_json(&path, &Consent::default())?;
        queue.configure_uploads(None, None)?;
        return Ok(None);
    }
    if let Err(e) = endpoint(config) {
        atomic_json(&path, &Consent::default())?;
        queue.configure_uploads(None, None)?;
        return Err(e);
    }
    let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&(
        &config.collector_url,
        &config.token,
        &config.device_uuid,
    ))?));
    if consent.fingerprint.as_ref() != Some(&fingerprint) || consent.scope.is_none() {
        atomic_json(&path, &Consent::default())?;
        queue.configure_uploads(None, None)?;
        consent = Consent {
            fingerprint: Some(fingerprint),
            scope: Some(uuid::Uuid::new_v4().to_string()),
        };
        atomic_json(&path, &consent)?;
    }
    queue.configure_uploads(None, consent.scope.clone())?;
    Ok(consent.scope)
}

// The shared queue's coordinator is process-local. Hold an OS lock for the
// entire transaction/drain so concurrent CLI invocations cannot double-send.
fn lock(config: &UploadConfig) -> Result<File> {
    let dir = root(config);
    fs::create_dir_all(&dir)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("driver.lock"))?;
    file.try_lock()
        .context("automatic upload driver is already running; retry later")?;
    Ok(file)
}

fn status(queue: &SubmissionQueue, config: &UploadConfig) -> Result<()> {
    println!(
        "automatic_upload={} pending={} failed={}",
        if config.automatic_upload { "on" } else { "off" },
        queue.pending_ids()?.len(),
        queue.dead_ids()?.len()
    );
    Ok(())
}

pub(crate) fn run(cmd: UploadCommand, path: Option<&Path>) -> Result<()> {
    if let UploadCommand::Set { automatic_upload } = cmd {
        return set_toggle(path, automatic_upload);
    }
    let config = load_config(path)?.upload;
    let queue = SubmissionQueue::open(root(&config))?;
    if matches!(cmd, UploadCommand::Status) {
        // A running owner observes config changes itself; never race its writes.
        if let Ok(_lock) = lock(&config) {
            configure(&queue, &config)?;
        }
        return status(&queue, &config);
    }
    let _lock = lock(&config)?;
    if let Some(scope) = configure(&queue, &config)? {
        drive(&queue, &config, path, scope)?;
    }
    status(&queue, &load_config(path)?.upload)
}

fn set_toggle(path: Option<&Path>, enabled: bool) -> Result<()> {
    let path = path
        .map(Path::to_path_buf)
        .or_else(crate::config::default_config_path)
        .context("no configuration path available; use --config")?
        .canonicalize()?;
    let toggle_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("upload-toggle.lock"))?;
    toggle_lock
        .try_lock()
        .context("another upload toggle operation is in progress")?;
    let mut config = load_config(Some(&path))?.upload;
    if enabled {
        endpoint(&config)?;
    }
    let mut document: toml::Value = toml::from_str(&fs::read_to_string(&path)?)?;
    let table = document
        .as_table_mut()
        .context("config must be a TOML table")?;
    let upload = table
        .entry("upload")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    upload
        .as_table_mut()
        .context("upload must be a TOML table")?
        .insert("automatic_upload".into(), toml::Value::Boolean(enabled));
    let parent = path.parent().context("config path has no parent")?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(toml::to_string_pretty(&document)?.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(&path)?;
    File::open(parent)?.sync_all()?;
    config.automatic_upload = enabled;
    // The active owner observes OFF and closes its socket before releasing
    // the lock. A successful OFF command therefore acknowledges revocation.
    let deadline = Instant::now() + Duration::from_secs(3);
    let guard = loop {
        match lock(&config) {
            Ok(guard) => break guard,
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(error);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    };
    let queue = SubmissionQueue::open(root(&config))?;
    configure(&queue, &config)?;
    let result = status(&queue, &config);
    drop(guard);
    result
}

pub(crate) fn complete(args: &SessionCompleteArgs, path: Option<&Path>) -> Result<()> {
    let config = load_config(path)?.upload;
    let _lock = lock(&config)?;
    let queue = SubmissionQueue::open(root(&config))?;
    let scope = configure(&queue, &config)?;
    let session_dir = args
        .corpus
        .clone()
        .unwrap_or_else(crate::default_corpus_root)
        .join("sessions")
        .join(args.session.to_string());
    let session = SessionManifest::load_from_dir(&session_dir)?;
    let receipt_path = session_dir.join("completed-captures.json");
    let mut completed: BTreeSet<String> = read_json(&receipt_path)?;
    for id in &session.ordered_capture_ids {
        if completed.contains(id) {
            continue;
        }
        if Path::new(id).components().count() != 1 || id == "." || id == ".." {
            bail!("invalid capture id in session");
        }
        let bundle = session_dir.join("captures").join(id);
        let manifest = BundleManifest::load_from_dir(&bundle)?;
        if manifest.session_id != Some(session.session_id)
            || manifest.capture.ended_unix_ms < manifest.capture.started_unix_ms
        {
            bail!("capture {id} is not finalized for this session");
        }
        if let Some(scope) = &scope {
            let source = SubmissionSource {
                device_uuid: config.device_uuid.clone().context("device UUID missing")?,
                device_model: "bris-cli".into(),
                device_os: std::env::consts::OS.into(),
                app_version: env!("CARGO_PKG_VERSION").into(),
                bris_core_version: env!("CARGO_PKG_VERSION").into(),
                bris_data_version: None,
                note: None,
            };
            let submission = build_submission(&bundle, &source, None)?;
            // Recover an enqueue whose completion receipt was interrupted by a
            // crash. Sent and revoked entries also count: never resurrect them.
            let checksum = hex::encode(Sha256::digest(fs::read(bundle.join("bundle.json"))?));
            if !already_queued(&root(&config), &checksum)? {
                let current = load_config(path)?.upload;
                if !same_consent(&config, &current) {
                    atomic_json(
                        &root(&config).join("upload-state.json"),
                        &Consent::default(),
                    )?;
                    queue.configure_uploads(None, None)?;
                    bail!(
                        "upload configuration changed during completion; automatic uploads stopped"
                    );
                }
                queue.enqueue_automatic(&submission, scope.clone())?;
            }
        }
        completed.insert(id.clone());
        atomic_json(&receipt_path, &completed)?;
    }
    if let Some(scope) = scope {
        drive(&queue, &config, path, scope)?;
    }
    status(&queue, &load_config(path)?.upload)
}

fn already_queued(root: &Path, checksum: &str) -> Result<bool> {
    for state in ["pending", "sent", "dead"] {
        for entry in fs::read_dir(root.join(state))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let state: bris_submit::queue::QueuedState =
                serde_json::from_slice(&fs::read(entry.path().join("state.json"))?)?;
            if state
                .parts
                .iter()
                .any(|p| p.filename == "bundle.json" && p.checksum_sha256 == checksum)
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn drive(
    queue: &SubmissionQueue,
    config: &UploadConfig,
    path: Option<&Path>,
    scope: String,
) -> Result<()> {
    let endpoint = endpoint(config)?;
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(config.drain_seconds))
        .context("upload.drain_seconds is too large")?;
    if !same_consent(config, &load_config(path)?.upload) {
        atomic_json(&root(config).join("upload-state.json"), &Consent::default())?;
        queue.configure_uploads(None, None)?;
        return Ok(());
    }
    std::thread::scope(|threads| -> Result<()> {
        let (send, receive) = mpsc::channel();
        threads.spawn(move || {
            let _ = send.send(queue.flush_scoped(endpoint, scope, true));
        });
        loop {
            match receive.recv_timeout(Duration::from_millis(25)) {
                Ok(result) => {
                    result?;
                    return Ok(());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("upload worker disconnected"),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            let current = load_config(path).map(|c| c.upload);
            if !current.as_ref().is_ok_and(|c| same_consent(config, c)) {
                queue.cancel_flush()?;
                atomic_json(&root(config).join("upload-state.json"), &Consent::default())?;
                queue.configure_uploads(None, None)?;
                return Ok(());
            }
            if Instant::now() >= deadline {
                queue.cancel_flush()?;
                return Ok(());
            }
        }
    })
}
