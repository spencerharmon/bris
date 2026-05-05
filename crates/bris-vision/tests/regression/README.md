# Bris vision regression test corpus

A small set of real captured frames used to detect behavior changes
in the vision pipeline. The corpus grows over time as new edge cases
are discovered; once a frame is in here, the corresponding test
asserts the pipeline still handles it correctly.

## What goes here

Each subdirectory is one test case:

```
crates/bris-vision/tests/regression/
  <case_name>/
    case.toml            (metadata + expected behavior — required)
    frame.png            (or per-frame names listed in [case].frames)
    README.md            (optional: prose description)
```

Frames must be small (< 200 KB each) and few (no more than 10 per
case). The corpus is committed to the repo so CI runs against
exactly the same bytes every time. The segmentation model
(`crates/bris-vision/data/segmentation.onnx`) is gitignored at ~14.5
MB; tests that need it skip cleanly with an `eprintln!` when it's
absent.

## Why not bigger / more frames?

A few hundred KB total keeps git clones fast. The corpus is
*regression* tests, not validation: each case represents a *known
hard scene* we've fixed or a *known failure mode* we want to catch
if it ever changes. For broader validation use the field-capture
infrastructure (Phase 8; not yet built).

## Three case kinds

Each case declares one of:

- `kind = "working"` — pipeline should produce a usable result.
- `kind = "expected_failure"` — pipeline should refuse to produce a
  result on this scene. One or more pipeline stages should return a
  typed error or the resulting fix should be flagged invalid. These
  are critical: they catch the regression where Bris starts
  fabricating fixes from scenes that don't have enough information.
- `kind = "expected_low_confidence"` — pipeline should produce a
  result, but with σ above an operator-meaningful threshold. The
  case asserts the σ floor is exceeded.

## How tests are generated

`build.rs` walks this directory at build time, parses each
`case.toml`, and emits one `mod case_<name>` block into
`$OUT_DIR/cases_generated.rs`. The integration test target
`tests/regression_test.rs` includes the generated file.

Each declared expectation table in `case.toml` produces one
`#[test] fn`, named after the check it performs. A failure
therefore points at the exact case and check, e.g.
`case_marina::horizon_segmentation_outcome`.

**Adding a new case is a TOML-write, not a Rust-write.** Drop a new
directory in here with a `case.toml` and frame(s); the next `cargo
test` regenerates the test dispatch.

## case.toml schema

The full schema is documented in code in `tests/regression_test.rs`
(module `harness`). Quick reference:

```toml
[case]
name                = "my_scene"          # must match directory name
description         = "..."
kind                = "working"           # working | expected_failure | expected_low_confidence
frame_count         = 1
frame_width         = 640
frame_height        = 360
source_rotation_deg = 0                   # 0 | 90 | 180 | 270; loader rotation TBD
frames              = ["frame.png"]       # optional; defaults to ["frame.png"]

[reference_observer]
lat_deg      = 25.0
lon_deg      = -80.0
eye_height_m = 2.0
capture_utc  = "2024-03-15T15:00:00Z"
body         = "sun"

[expected_classifier]                     # optional; classifier module pending
condition      = "day"                    # day | night | twilight
min_confidence = 0.7

[expected_centroid_frame0]                # optional
x_px         = 98.8
y_px         = 47.7
tolerance_px = 5.0

[horizon.gradient]                        # optional; one table per method
outcome             = "ok"                # ok | err
slope               = -0.166              # required when outcome = "ok"
intercept           = 309.9               # required when outcome = "ok"
slope_tolerance     = 0.05                # default 0.05
intercept_tolerance = 15.0                # default 15.0
inlier_count_min    = 100                 # optional lower bound
error_variant       = "InsufficientCandidates"  # required when outcome = "err"
correctness         = "wrong"             # documentation only
notes               = "..."

[horizon.sky_region]   # same shape as [horizon.gradient]
[horizon.segmentation] # same shape as [horizon.gradient]

[segmentation.transition_counts]          # optional; runner stubbed
col_sky_to_sea_min          = 100
col_sky_to_obstr_to_sea_min = 100

[fix]                                     # optional; runner pending
outcome              = "ok"               # ok | err | low_confidence
sigma_nm_min         = 0.5                # for low_confidence
sigma_nm_max         = 5.0                # for ok (rarely set)
dominant_source_in   = ["horizon", "centroiding"]
```

## What the tests assert

In priority order:

1. **Pipeline runs without panic.** Every case must load and run
   the declared pipeline stages without crashing. Always-on via the
   `frames_load` test that the build script emits for every case.

2. **Outputs are within tolerance of recorded values** (for
   `kind = "working"`). When a case is added, the *current* output is
   recorded in `case.toml`. The test asserts the current run produces
   values within tolerance.

3. **Specific failure modes** (for `kind = "expected_failure"`).
   The case declares which detector should fail and optionally which
   error variant. Catches regressions where the pipeline starts
   fabricating output from a scene that doesn't support a fix.

4. **Confidence floor exceeded** (for `kind = "expected_low_confidence"`).
   Case declares a σ floor; the test fails if Bris reports a fix
   tighter than that floor on a scene where it shouldn't be that
   confident.

## Adding a new case

1. Save the frame(s) at < 200 KB each.
2. Pick a directory name describing the *scene*, not the *bug*
   (e.g. `sailing_sun_upper_left`, not `fixes_issue_42`).
3. Run the pipeline once to observe its current behavior. The
   simplest way is to write a minimal `case.toml` (just `[case]`
   plus `kind`) and a placeholder expectation, run `cargo test`, and
   read the failure output.
4. Update `case.toml` to record what the pipeline actually does
   *and* what you intend to assert. For `working` cases that's
   "what it does today, plus tolerance"; for `expected_failure`
   cases that's "the typed error variant or the σ floor."
5. Optional but encouraged: a short `README.md` explaining why this
   scene is hard.
6. Commit the frame(s), the `case.toml`, and (optionally) the
   `README.md` together.

No Rust code change is required. `cargo build` regenerates the test
dispatch; `cargo test -p bris-vision --test regression_test` runs
the new tests.

## Running the corpus

```
cargo test -p bris-vision --test regression_test
```

To run a single case:

```
cargo test -p bris-vision --test regression_test case_sailing_sun_upper_left
```

To run a single check across all cases:

```
cargo test -p bris-vision --test regression_test horizon_segmentation
```

CI runs the full corpus on every push.
