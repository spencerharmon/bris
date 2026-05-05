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
    frame.png            (or frames are named by capture order)
    case.toml            (metadata: known horizon, body, conditions)
    README.md            (optional: prose description)
```

Frames must be small (< 200 KB each) and few (no more than 10 per
case). The corpus is committed to the repo so CI runs against
exactly the same bytes every time.

## Why not bigger / more frames?

A few hundred KB total keeps git clones fast. The corpus is
*regression* tests, not validation: each case represents a *known
hard scene* we've fixed, not a thorough sweep of conditions. For
broader validation use the field-capture infrastructure (Phase 8;
not yet built).

## What the tests assert

Three categories, in priority order:

1. **Pipeline runs without panic.** Every case must load, run
   through horizon detection (each method) and body centroiding
   without crashing. This catches regressions in error handling,
   memory safety, etc.

2. **Outputs are within tolerance of recorded values.** When a
   case is added, the *current* output is recorded in `case.toml`.
   The test asserts the current run produces values within
   tolerance. This catches algorithm changes that drift from
   known-good behavior.

3. **Specific behavior assertions** (per case). For example,
   "the segmentation detector should find at least N
   sky→sea candidate columns in this scene," or "the gradient
   detector should fail with InsufficientCandidates on this
   scene." Each case can assert what's load-bearing about it.

## Adding a new case

1. Capture or save the frame(s) at < 200 KB each.
2. Pick a directory name describing the *scene*, not the *bug*
   (e.g. `sailing_sun_upper_left`, not `fixes_issue_42`).
3. Run the pipeline manually with `bris replay` and record the
   horizon line, body centroid, etc. in `case.toml`.
4. Add a test in `regression_test.rs` that asserts the recorded
   values reproduce.
5. Commit the frame(s), the metadata, and the test.

## Running the corpus

The regression tests run as part of `cargo test -p bris-vision`.
There is no special invocation. CI runs the full corpus on every
push.

For interactive debugging, `bris replay` accepts any of the case
directories directly:

```
cargo run -p bris-cli --release -- replay \
    --frames crates/bris-vision/tests/regression/sailing_sun_upper_left \
    --assumed-lat 25.0 --assumed-lon -80.0 \
    --body sun \
    --capture-utc 2024-03-15T15:00:00Z \
    --horizon-method segmentation
```
