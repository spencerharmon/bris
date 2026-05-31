//! Build script: capture git provenance + generate `UniFFI` scaffolding.
//!
//! Records the git SHA, `git describe` output, dirty-tree flag,
//! commit count, and build timestamp at compile time so the
//! produced `bris-ffi` shared object can report exactly which
//! engine code it is. Exposed via `env!` in `src/lib.rs`.
//!
//! Hand-rolled (no `vergen`) to avoid pulling `git2`. Falls
//! back to `"unknown"` strings when not building inside a git
//! worktree (released tarballs, vendored builds) so the build
//! never breaks on environments without `git` available.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_owned())
    }
}

fn main() {
    let sha = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let describe =
        git(&["describe", "--always", "--tags", "--dirty"]).unwrap_or_else(|| "unknown".into());
    let count = git(&["rev-list", "--count", "HEAD"]).unwrap_or_else(|| "0".into());
    let dirty = if describe.ends_with("-dirty") {
        "true"
    } else {
        "false"
    };
    let timestamp = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => iso8601_utc(d.as_secs()),
        Err(_) => "unknown".into(),
    };

    println!("cargo:rustc-env=BRIS_GIT_SHA={sha}");
    println!("cargo:rustc-env=BRIS_GIT_DESCRIBE={describe}");
    println!("cargo:rustc-env=BRIS_GIT_DIRTY={dirty}");
    println!("cargo:rustc-env=BRIS_GIT_COMMIT_COUNT={count}");
    println!("cargo:rustc-env=BRIS_BUILD_TIMESTAMP={timestamp}");

    // Re-run when HEAD moves or the index changes (covers
    // checkout, commit, and worktree-dirty transitions).
    println!("cargo:rerun-if-changed=build.rs");
    if std::path::Path::new("../../.git/HEAD").exists() {
        println!("cargo:rerun-if-changed=../../.git/HEAD");
        println!("cargo:rerun-if-changed=../../.git/index");
    }
}

/// Format `secs` (Unix epoch) as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Civil-from-days algorithm by Howard Hinnant (public domain),
/// adapted. Inline to avoid a dep on `chrono`/`time`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::many_single_char_names
)]
fn iso8601_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let sod = secs % 86_400;
    let hour = (sod / 3600) as u32;
    let min = ((sod / 60) % 60) as u32;
    let sec = (sod % 60) as u32;

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = y + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}
