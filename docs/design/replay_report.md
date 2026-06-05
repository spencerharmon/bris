# Bris replay report schema

`bris replay --render-frames` writes two JSON artefacts that
together form the input to the corpus explorer
(`tools/corpus-explorer/`) and any other replay-diagnostic
consumer:

- `<corpus>/sessions/<UUID>/bris-replay-report.json` — one per
  session.
- `<corpus>/index.json` — a corpus-root catalogue of every
  session report on disk, written when replay runs with
  `--all-sessions --render-frames`.

Both files carry an integer `schema_version` at the top level.
Within a version, the on-disk schema is additive: consumers
must ignore unknown fields. Breaking changes (renames,
type changes, semantic shifts) bump the version and the
explorer falls back to a compatibility banner.

The first schema is `1`. Reference implementation:
`crates/bris-cli/src/replay_report.rs`.

## `bris-replay-report.json`

```jsonc
{
  "schema_version": 1,
  "session_id": "508197ac-…",
  "session_title": "Marina, dusk",
  "generated_unix_ms": 1700000000000,
  "engine_build": {
    "git_sha": "abc123def456",      // optional; omitted if git unavailable
    "git_describe": "v0.1-3-gabc",   // optional
    "crate_version": "0.0.1"         // bris-cli's CARGO_PKG_VERSION
  },
  "captures": [
    {
      "capture_id": "0019e87174c5",
      "bundle_dir": "captures/0019e87174c5/",
      "app_version": "e8a7211",       // optional, from bundle.json.device
      "frame_count": 7,                // enumerated frames
      "frames_pushed": 7,              // engine.push_frame() accepted
      "fixes_published": 0,
      "sights_inserted_total": 0,
      "stage_e_rejection_counts": {
        "BelowHorizon": 28,
        "NonFinite": 0
      },
      "frames": [
        {
          "seq": 0,
          "captured_unix_ms": 1700000000000,
          "render_path": "captures/0019.../frames/00000000-render.png",
          "pgm_path":   "captures/0019.../frames/00000000.pgm",
          "classification": "Twilight",
          "horizon": {
            "provider": "vertical-line",
            "intercept_px": 583.6,
            "slope": 0.0058,
            "sigma_rad": 0.001
          },
          "body_centroid": {
            "x": 1743.2, "y": 2979.9, "sigma_px": 0.5,
            "area_px": 1779, "secondaries": 0
          },
          "stage_e_outcomes": [
            { "kind": "Err", "error": "BelowHorizon" }
          ],
          "sight_emitted": false
        }
      ]
    }
  ]
}
```

Field semantics:

- `horizon`, `body_centroid`: omitted (or `null`) when the
  frame produced no detection. `provider` strings are stable
  identifiers (`"gradient"`, `"sky-region"`, `"night-gradient"`,
  `"night-textured"`, `"segmentation"`, `"reflection-pair"`,
  `"vertical-line"`, `"vanishing-point"`, `"fused"`).
- `stage_e_outcomes` is one entry per (body, horizon) pair
  Stage E attempted to reduce. `kind` is `"Ok"` or `"Err"`;
  the discriminated union mirrors
  `bris_streaming::StageEOutcomeSnapshot`. The `Ok` variant
  carries `altitude_rad` (observed altitude) and `sigma_rad`
  (1σ). The `Err` variant carries a stable `error`
  identifier (e.g. `"BelowHorizon"`, `"NonFinite"`,
  `"FrameEvicted"`, `"Stitch"`).
- `sight_emitted` is `true` iff at least one Stage E attempt
  on that frame succeeded.
- `render_path` and `pgm_path` are paths relative to the
  **session root** (i.e. `sessions/<UUID>/…`). The explorer
  resolves them against the corpus root by prepending
  `sessions/<UUID>/`.

## `index.json`

```jsonc
{
  "schema_version": 1,
  "generated_unix_ms": 1700000000000,
  "sessions": [
    {
      "session_id": "508197ac-…",
      "session_title": "Marina, dusk",
      "report_path": "sessions/508197ac-…/bris-replay-report.json",
      "capture_count": 3
    }
  ]
}
```

The explorer fetches this file once, then lazily fetches each
session report when the operator opens it.
