//! `bris submit`: operator-gated diagnostic submission from the
//! embedded/desktop frontend.
//!
//! This is the CLI counterpart to the Android Submitter — both
//! are thin frontends over the shared [`bris_submit`] crate
//! (payload build, the explicit operator gate, the durable
//! retrying queue). The command:
//!
//! 1. builds a `bris-bundle v1` submission from a capture
//!    directory (the `bundle.json` the engine ran against is
//!    shipped verbatim; `ap_input` honest, `gps_truth` carried
//!    only as ground-truth);
//! 2. prints a one-screen pre-upload review of the exact bytes
//!    about to leave the device;
//! 3. **stops there unless the operator passed `--yes`** — no
//!    submission is ever sent without an explicit operator
//!    action (Bris makes no automatic network calls);
//! 4. on `--yes`, enqueues the submission to a durable on-disk
//!    queue and (unless `--enqueue-only`) flushes the queue to
//!    the collector, retrying transient failures.
//!
//! The collector base URL and bearer token are RUNTIME config
//! (flags or `$BRIS_COLLECTOR_URL` / `$BRIS_COLLECTOR_TOKEN`),
//! never compiled into the binary.

use std::path::PathBuf;

use anyhow::{bail, Context};
use bris_submit::review::SubmissionReview;
use bris_submit::transport::{CollectorEndpoint, HttpTransport};
use bris_submit::{build_submission, AttemptResult, SubmissionQueue, SubmissionSource};

use crate::SubmitArgs;

/// Default queue root: `<data-root>/submit-queue`, i.e.
/// `~/.bris/submit-queue`.
fn default_queue_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map_or_else(|| PathBuf::from(".bris"), |h| h.join(".bris"))
        .join("submit-queue")
}

/// Entry point for `bris submit`.
pub(crate) fn run_submit(args: &SubmitArgs) -> anyhow::Result<()> {
    let queue_root = args.queue_root.clone().unwrap_or_else(default_queue_root);
    let queue = SubmissionQueue::open(&queue_root)
        .with_context(|| format!("open submission queue at {}", queue_root.display()))?;

    // Flush-only mode: skip building; just drive the queue.
    if args.flush_only {
        return flush(&queue, args);
    }

    // 1. Build the submission from the capture bundle.
    let source = SubmissionSource {
        device_uuid: args
            .device_uuid
            .clone()
            .unwrap_or_else(|| "cli-unset-device".to_owned()),
        device_model: "bris-cli".to_owned(),
        device_os: std::env::consts::OS.to_owned(),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        bris_core_version: env!("CARGO_PKG_VERSION").to_owned(),
        bris_data_version: None,
        note: args.note.clone(),
    };
    let submission = build_submission(&args.bundle, &source, args.calibration.as_deref())
        .with_context(|| format!("build submission from {}", args.bundle.display()))?;
    let review = SubmissionReview::new(submission);

    // 2. Print the one-screen pre-upload review.
    println!("── Pre-upload review ──────────────────────────────");
    for line in review.lines() {
        println!("  {:<18} {}", format!("{}:", line.label), line.value);
    }
    println!("───────────────────────────────────────────────────");

    // 3. Explicit-operator-action gate.
    if !args.yes {
        println!(
            "Not submitted. This is the pre-upload review only.\n\
             Re-run with --yes to confirm and upload (Bris never \
             uploads automatically)."
        );
        return Ok(());
    }

    // 4. Approve → enqueue (durable).
    let approved = review.approve();
    let queue_id = queue
        .enqueue(&approved)
        .context("enqueue approved submission")?;
    println!(
        "Enqueued submission {queue_id} (durable at {}).",
        queue_root.display()
    );

    if args.enqueue_only {
        println!("--enqueue-only: not flushing. Run `bris submit --flush-only` later to upload.");
        return Ok(());
    }

    flush(&queue, args)
}

/// Drive one flush pass over the queue against the runtime
/// collector endpoint, printing per-entry outcomes.
fn flush(queue: &SubmissionQueue, args: &SubmitArgs) -> anyhow::Result<()> {
    let pending = queue.pending_ids().context("list pending submissions")?;
    if pending.is_empty() {
        println!("No pending submissions to upload.");
        return Ok(());
    }

    let base_url = args.collector_url.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "collector URL not set. Pass --collector-url or set \
             $BRIS_COLLECTOR_URL (never compiled into the binary)."
        )
    })?;
    let token = args.token.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "collector token not set. Pass --token or set \
             $BRIS_COLLECTOR_TOKEN."
        )
    })?;
    let transport = HttpTransport::new(CollectorEndpoint::new(base_url, token));

    let results = queue
        .drain_once(&transport)
        .context("flush submission queue")?;
    let mut any_dead = false;
    for r in &results {
        match r {
            AttemptResult::Sent { id, collector_id } => {
                println!("  sent {id} -> collector id {collector_id}");
            }
            AttemptResult::Retrying { id, attempts } => {
                println!("  retrying {id} (attempt {attempts}); will retry later");
            }
            AttemptResult::DeadLettered { id, reason } => {
                any_dead = true;
                eprintln!("  REJECTED {id}: {reason}");
            }
            AttemptResult::Deferred { id } => {
                println!("  deferred {id} (backoff not elapsed)");
            }
        }
    }
    if any_dead {
        bail!("one or more submissions were permanently rejected (see dead-letter queue)");
    }
    Ok(())
}
