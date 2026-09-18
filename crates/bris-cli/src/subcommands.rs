//! `bris update`: apply a signed `bris-data` payload.
//!
//! An update is a directory laid out as:
//!
//! ```text
//! <payload>/
//!   manifest.json     # UpdateManifest: version + per-file BLAKE3 digests
//!   manifest.sig      # raw 64-byte Ed25519 signature over manifest.json's bytes
//!   payload/          # the new data tree, replacing <data-dir> wholesale
//!     almanac/...
//!     catalog/...
//!     leap-seconds.txt
//! ```
//!
//! Applying it is a three-step, fail-closed operation:
//!
//! 1. **Verify the signature.** The detached `manifest.sig` must
//!    be a valid Ed25519 signature over `manifest.json`'s exact
//!    bytes under the operator-supplied trusted public key. A bad
//!    or missing signature aborts before anything is written.
//! 2. **Verify the payload against the manifest.** Every file the
//!    manifest lists must exist under `payload/` with a matching
//!    BLAKE3 digest, and the payload must contain no file the
//!    manifest does not list. This closes the gap where a valid
//!    signature covers a manifest whose payload was tampered with
//!    after signing.
//! 3. **Swap atomically.** The verified tree is copied to a
//!    staging dir alongside the destination, then swapped into
//!    place with a rename (old tree moved aside first, removed
//!    only after the new tree is in place). A failure at any point
//!    leaves the previous data intact — never a silent partial
//!    update.

use anyhow::{bail, Context};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::UpdateArgs;

/// Signed manifest describing an update payload.
///
/// `files` maps each payload-relative path (forward-slash
/// separated) to the hex BLAKE3 digest of its contents. The
/// manifest's serialized bytes are what the detached signature
/// covers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct UpdateManifest {
    /// Schema version of this manifest format.
    pub schema_version: u32,
    /// Human-readable version of the data payload (e.g. an
    /// almanac epoch or catalog release tag).
    pub data_version: String,
    /// payload-relative path -> hex BLAKE3 digest of the file.
    pub files: BTreeMap<String, String>,
}

/// This module's manifest schema version.
pub(crate) const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Compute the lowercase-hex BLAKE3 digest of a byte slice.
pub(crate) fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Recursively collect every regular file under `root`, keyed by
/// its `root`-relative forward-slash path.
fn collect_files(root: &Path) -> anyhow::Result<BTreeMap<String, PathBuf>> {
    let mut out = BTreeMap::new();
    walk_tree(root, root, &mut out)?;
    Ok(out)
}

fn walk_tree(base: &Path, dir: &Path, out: &mut BTreeMap<String, PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk_tree(base, &path, out)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(base)
                .expect("path is under base")
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.insert(rel, path);
        } else {
            bail!(
                "payload contains a non-regular file (symlink/device?): {}",
                path.display()
            );
        }
    }
    Ok(())
}

/// Verify the detached Ed25519 signature over `manifest.json`.
fn verify_signature(
    manifest_bytes: &[u8],
    sig_bytes: &[u8],
    pubkey_hex: &str,
) -> anyhow::Result<()> {
    let pk_raw = hex::decode(pubkey_hex.trim())
        .context("decode --pubkey-hex (expected hex-encoded 32-byte Ed25519 key)")?;
    let pk_arr: [u8; 32] = pk_raw.as_slice().try_into().map_err(|_| {
        anyhow::anyhow!("Ed25519 public key must be 32 bytes, got {}", pk_raw.len())
    })?;
    let vk = VerifyingKey::from_bytes(&pk_arr).context("parse Ed25519 public key")?;
    let sig_arr: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("signature must be 64 bytes, got {}", sig_bytes.len()))?;
    let sig = Signature::from_bytes(&sig_arr);
    vk.verify(manifest_bytes, &sig)
        .context("Ed25519 signature verification failed (untrusted or tampered payload)")?;
    Ok(())
}

/// Verify every payload file matches the signed manifest, and the
/// payload contains exactly the manifest's file set.
fn verify_payload_against_manifest(
    manifest: &UpdateManifest,
    payload_dir: &Path,
) -> anyhow::Result<()> {
    let on_disk = collect_files(payload_dir)?;
    // Every manifest entry present with a matching digest.
    for (rel, expected_hex) in &manifest.files {
        let path = on_disk
            .get(rel)
            .with_context(|| format!("manifest lists {rel} but it is missing from payload/"))?;
        let bytes =
            std::fs::read(path).with_context(|| format!("read payload file {}", path.display()))?;
        let actual = blake3_hex(&bytes);
        if &actual != expected_hex {
            bail!(
                "payload file {rel} digest mismatch: manifest={expected_hex} actual={actual} \
                 (tampered payload)"
            );
        }
    }
    // No extra files beyond what the manifest covers.
    for rel in on_disk.keys() {
        if !manifest.files.contains_key(rel) {
            bail!("payload contains {rel} which the signed manifest does not list");
        }
    }
    Ok(())
}

/// Recursively copy a directory tree.
fn copy_tree(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)
        .with_context(|| format!("create staging dir {}", dst.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("read_dir {}", src.display()))? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copy {} -> {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// Entry point for `bris update`.
pub(crate) fn run_update(args: &UpdateArgs) -> anyhow::Result<()> {
    apply_update(&args.payload, &args.data_dir, &args.pubkey_hex)
}

/// The full verify-and-atomically-swap operation, factored out of
/// `run_update` so tests can drive it directly with a temp dir.
pub(crate) fn apply_update(
    payload: &Path,
    data_dir: &Path,
    pubkey_hex: &str,
) -> anyhow::Result<()> {
    let manifest_path = payload.join("manifest.json");
    let sig_path = payload.join("manifest.sig");
    let payload_dir = payload.join("payload");

    let manifest_bytes = std::fs::read(&manifest_path)
        .with_context(|| format!("read manifest {}", manifest_path.display()))?;
    let sig_bytes = std::fs::read(&sig_path)
        .with_context(|| format!("read signature {}", sig_path.display()))?;

    // 1. Signature over the manifest bytes.
    verify_signature(&manifest_bytes, &sig_bytes, pubkey_hex)?;

    // 2. Manifest parses and payload matches it.
    let manifest: UpdateManifest =
        serde_json::from_slice(&manifest_bytes).context("parse manifest.json")?;
    if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        bail!(
            "unsupported update manifest schema_version {} (expected {})",
            manifest.schema_version,
            MANIFEST_SCHEMA_VERSION
        );
    }
    if !payload_dir.is_dir() {
        bail!("payload/ subtree missing under {}", payload.display());
    }
    verify_payload_against_manifest(&manifest, &payload_dir)?;

    // 3. Atomic swap. Stage the verified tree next to the
    //    destination, then rename into place. The staging +
    //    backup dirs live in the destination's PARENT so the
    //    final renames are same-filesystem (atomic).
    let parent = data_dir
        .parent()
        .context("--data-dir must have a parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create data-dir parent {}", parent.display()))?;

    let file_name = data_dir
        .file_name()
        .and_then(|s| s.to_str())
        .context("--data-dir must have a final path component")?;
    let staging = parent.join(format!(".{file_name}.bris-update-staging"));
    let backup = parent.join(format!(".{file_name}.bris-update-backup"));

    // Clean any leftovers from a prior interrupted run.
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("clear stale staging dir {}", staging.display()))?;
    }
    if backup.exists() {
        std::fs::remove_dir_all(&backup)
            .with_context(|| format!("clear stale backup dir {}", backup.display()))?;
    }

    copy_tree(&payload_dir, &staging)?;

    let had_existing = data_dir.exists();
    if had_existing {
        std::fs::rename(data_dir, &backup).with_context(|| {
            format!(
                "move existing data aside {} -> {}",
                data_dir.display(),
                backup.display()
            )
        })?;
    }
    // Move staged tree into place. On failure, restore the backup
    // so the operator is never left without data.
    if let Err(e) = std::fs::rename(&staging, data_dir).with_context(|| {
        format!(
            "swap staged data into place {} -> {}",
            staging.display(),
            data_dir.display()
        )
    }) {
        if had_existing {
            let _ = std::fs::rename(&backup, data_dir);
        }
        return Err(e);
    }
    // Success: drop the backup and any staging remnant.
    if backup.exists() {
        std::fs::remove_dir_all(&backup)
            .with_context(|| format!("remove backup {}", backup.display()))?;
    }
    tracing::info!(
        data_version = %manifest.data_version,
        files = manifest.files.len(),
        data_dir = %data_dir.display(),
        "bris update: applied signed payload"
    );
    println!(
        "applied update {} ({} files) to {}",
        manifest.data_version,
        manifest.files.len(),
        data_dir.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use tempfile::tempdir;

    /// Build a signed payload directory under `root` from a set
    /// of (payload-relative-path, contents) files. Returns the
    /// payload dir and the signer's public-key hex.
    fn make_signed_payload(
        root: &Path,
        signer: &SigningKey,
        files: &[(&str, &[u8])],
        tamper_manifest: bool,
    ) -> (PathBuf, String) {
        let payload = root.join("update");
        let payload_dir = payload.join("payload");
        let mut manifest = UpdateManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            data_version: "test-2026a".into(),
            files: BTreeMap::new(),
        };
        for (rel, contents) in files {
            let dst = payload_dir.join(rel);
            std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
            std::fs::write(&dst, contents).unwrap();
            manifest
                .files
                .insert((*rel).to_string(), blake3_hex(contents));
        }
        if tamper_manifest {
            // Sign a manifest, then write a DIFFERENT one to disk.
            let signed_bytes = serde_json::to_vec(&manifest).unwrap();
            let sig = signer.sign(&signed_bytes);
            std::fs::create_dir_all(&payload).unwrap();
            let mut tampered = manifest.clone();
            tampered.data_version = "evil".into();
            std::fs::write(
                payload.join("manifest.json"),
                serde_json::to_vec(&tampered).unwrap(),
            )
            .unwrap();
            std::fs::write(payload.join("manifest.sig"), sig.to_bytes()).unwrap();
        } else {
            let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
            let sig = signer.sign(&manifest_bytes);
            std::fs::create_dir_all(&payload).unwrap();
            std::fs::write(payload.join("manifest.json"), &manifest_bytes).unwrap();
            std::fs::write(payload.join("manifest.sig"), sig.to_bytes()).unwrap();
        }
        let pk_hex = hex::encode(signer.verifying_key().to_bytes());
        (payload, pk_hex)
    }

    fn signer() -> SigningKey {
        // Deterministic key for reproducible tests.
        SigningKey::from_bytes(&[7u8; 32])
    }

    #[test]
    fn valid_signed_payload_applies_atomically() {
        let dir = tempdir().unwrap();
        let sk = signer();
        let (payload, pk_hex) = make_signed_payload(
            dir.path(),
            &sk,
            &[("almanac/2026.dat", b"almanac-bytes"), ("leap.txt", b"37")],
            false,
        );
        let data_dir = dir.path().join("data");
        // Pre-existing data that must be replaced, not merged.
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("old-file.dat"), b"stale").unwrap();

        apply_update(&payload, &data_dir, &pk_hex).unwrap();

        // New files present, old file gone (wholesale replace).
        assert_eq!(
            std::fs::read(data_dir.join("almanac/2026.dat")).unwrap(),
            b"almanac-bytes"
        );
        assert_eq!(std::fs::read(data_dir.join("leap.txt")).unwrap(), b"37");
        assert!(!data_dir.join("old-file.dat").exists());
        // No staging/backup remnants.
        assert!(!dir.path().join(".data.bris-update-staging").exists());
        assert!(!dir.path().join(".data.bris-update-backup").exists());
    }

    #[test]
    fn wrong_pubkey_is_rejected_and_leaves_data_untouched() {
        let dir = tempdir().unwrap();
        let sk = signer();
        let (payload, _pk_hex) = make_signed_payload(dir.path(), &sk, &[("a.dat", b"x")], false);
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("keep.dat"), b"original").unwrap();

        // A different key than the one that signed.
        let wrong = hex::encode(
            SigningKey::from_bytes(&[9u8; 32])
                .verifying_key()
                .to_bytes(),
        );
        let err = apply_update(&payload, &data_dir, &wrong).unwrap_err();
        assert!(
            format!("{err:#}").contains("signature verification failed"),
            "unexpected error: {err:#}"
        );
        // Original data untouched.
        assert_eq!(
            std::fs::read(data_dir.join("keep.dat")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn tampered_payload_file_is_rejected() {
        let dir = tempdir().unwrap();
        let sk = signer();
        let (payload, pk_hex) = make_signed_payload(dir.path(), &sk, &[("a.dat", b"good")], false);
        // Corrupt a payload file after signing — signature over the
        // manifest still validates, but the digest check must fail.
        std::fs::write(payload.join("payload/a.dat"), b"evil").unwrap();
        let data_dir = dir.path().join("data");
        let err = apply_update(&payload, &data_dir, &pk_hex).unwrap_err();
        assert!(
            format!("{err:#}").contains("digest mismatch"),
            "unexpected error: {err:#}"
        );
        assert!(!data_dir.exists(), "no partial update must be written");
    }

    #[test]
    fn tampered_manifest_breaks_signature() {
        let dir = tempdir().unwrap();
        let sk = signer();
        // manifest.json on disk differs from what was signed.
        let (payload, pk_hex) = make_signed_payload(dir.path(), &sk, &[("a.dat", b"good")], true);
        let data_dir = dir.path().join("data");
        let err = apply_update(&payload, &data_dir, &pk_hex).unwrap_err();
        assert!(
            format!("{err:#}").contains("signature verification failed"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn extra_payload_file_not_in_manifest_is_rejected() {
        let dir = tempdir().unwrap();
        let sk = signer();
        let (payload, pk_hex) = make_signed_payload(dir.path(), &sk, &[("a.dat", b"good")], false);
        // Add a file the signed manifest does not cover.
        std::fs::write(payload.join("payload/sneaky.dat"), b"unlisted").unwrap();
        let data_dir = dir.path().join("data");
        let err = apply_update(&payload, &data_dir, &pk_hex).unwrap_err();
        assert!(
            format!("{err:#}").contains("does not list"),
            "unexpected error: {err:#}"
        );
        assert!(!data_dir.exists());
    }
}
