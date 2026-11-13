# CLI Automatic Upload

Automatic upload is OFF by default and independent of Debug mode. Configure the
collector and per-device credentials in `bris --config /path/to/bris.toml`:

```toml
[upload]
automatic_upload = false
debug = false
collector_url = "https://collector.example.com"
# Supply the token registered for this device, not a shared public credential.
token = "your-device-token"
device_uuid = "your-registered-device-uuid"
queue_root = "/var/lib/bris/submit-queue"
drain_seconds = 120
```

Protect the file (for example mode 0600). Tokens are not printed in status.
Use `bris uploads set --automatic-upload true` to enable and
`bris uploads set --automatic-upload false` to disable. Both honor `--config`.
OFF persists revocation even when no upload driver was running; it returns only
after the active driver has released its lock. Keep one configuration file per
queue root. Do not rapidly edit OFF then ON by hand while no CLI process runs:
no background service exists to observe intermediate file versions.

The CLI session lifecycle is `session new`, `session attach`, then
`session complete --session <uuid> --corpus <root>`. Completion snapshots the
attached finalized capture bundles in canonical `sessions/<uuid>/captures/<id>`
layout. This is a local completion action, not an upload review. When ON, every
newly completed capture is queued silently, including Debug-off zero-frame
captures with their log. When OFF, completion records local receipts only;
enabling later does not retroactively send them. Repeating completion does not
enqueue the same capture twice. Sessions remain reusable groupings: additional
captures can be attached and completed later. An `attach --in-place` capture must
already occupy the canonical layout before completion.

`capture` records raw camera PNGs and `serve` streams navigation; neither currently
creates finalized bundle artifacts. They are not falsely treated as upload-ready
events. Prepare a valid bris-bundle v1 capture before attaching/completing it.
The completion path never rewrites its manifest, AP, GPS truth, or frame checksums.

Completion and `bris uploads retry` drive the Android/shared `SubmissionQueue`
in-process for at most `drain_seconds`. Set a budget large enough for the largest
capture on your connection. No detached process remains. Retry/backoff persists
across invocations; run `uploads retry` again or complete the next session to
drain eligible entries. `bris uploads status` reports pending and failed counts
without sending. Permanent rejection and revoked consent are retained under
`automatic/dead`; capture files are never deleted.

The active driver observes config changes every 25ms and cancels HTTP on OFF,
missing/invalid config, or destination/token/device/root changes. Bytes already
accepted by a collector cannot be recalled. Like the shared queue, delivery is
at-least-once: loss of an acknowledgement may result in a repeated POST, not an
exactly-once guarantee. Changing the endpoint revokes old entries rather than
retargeting them. Config writes should use atomic replacement.

Manual submissions retain the legacy queue at `queue_root`, isolated from the
automatic queue directory. They require Debug (`upload.debug=true` or `--debug`)
and individual review via `bris submit --bundle <capture> --yes`. Without `--yes`
the command only prints the review. `--enqueue-only` retains that approval;
`--flush-only --debug --yes` drains previously reviewed entries. Automatic consent
never approves or drains manual submissions.
