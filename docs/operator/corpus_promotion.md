# Promote a Reviewed Capture

Run promotion against an unpacked bris-bundle v1 capture containing
`bundle.json` and PGM/JSON pairs under `frames/` or `media/`:

```sh
bris corpus promote --capture ./reviewed-capture \
  --output crates/bris-vision/tests/regression/reviewed_scene \
  --description 'Reviewed horizon capture'
bris replay --case crates/bris-vision/tests/regression/reviewed_scene/case.toml \
  --disable-store
```

The output directory must not exist. Its final component supplies the case name;
an explicit `--name` must match it. Promotion validates the bundle version and
recorded first-frame checksum before generating output.

The skeleton contains vision expectations from the earliest frame, a lossless
`frame.png`, and a companion `bundle/` preserving that frame's original PGM,
sidecar, intrinsics, AP and GPS truth. Rotation is declared, not guessed.
`[expected_replay]` records frame/fix counts from a real default-mode engine run.
`bris replay --case` resolves the companion path relative to the TOML file, so
the original submission is not required after promotion.

Review every generated expectation against the scene before committing it.
The vision harness consumes the detector expectations; replay runs the capture
but does not enforce the review-pending `[expected_replay]` counts as a baseline.
A no-fix replay is an honest outcome, not proof that navigation succeeded.
The case represents frame 0 only, without earlier session history or optional
external model overrides. Add further frames or astronomical reference inputs
manually when the regression requires them. Raw bundle metadata can contain
sensitive field data: review it before publishing the corpus.
