# AGENTS.md

## Rule zero: no shortcuts. Ever.

**Implement the operator's plan as specified. In full. No
exceptions.**

If the spec says compute a checksum, compute the checksum.
If the spec says thread the operator-entered AP, thread the
operator-entered AP. If the spec says emit a per-axis σ,
emit a per-axis σ. The cost of generating the correct code
is the operator's to pay, not yours to optimise away.

The following are **forbidden** and will be treated as
bugs at review time:

- Stubbing a required field with `""`, `0`, `null`, `0.0`,
  `"unknown"`, `"TODO"`, or any other sentinel because the
  real value is inconvenient to obtain.
- Hardcoding a placeholder (e.g. `lat = 0.0, lon = 0.0`)
  in a code path that is supposed to read from operator
  input, sensor data, or upstream state.
- Skipping a verification, validation, hash, signature,
  or schema check "for now," "until later," or "as a
  follow-up."
- Inventing a default σ, default accuracy, default timeout,
  or default any-measurement to dodge an unknown.
- Catching an error and swallowing it so the caller sees
  success.
- Marking a task `DONE` when any sub-requirement is unmet.
- Writing "this will be wired up in a follow-up PR" without
  the follow-up PR being a hard blocker on merge.
- Calling a function "working" when it returns plausible-
  looking values that are not the real values.
- Deleting or weakening a test to make a change pass.
- Reaching for `unwrap()` / `expect()` / `.unwrap_or(<made-
  up value>)` to avoid handling a case the spec covers.

When a requirement genuinely cannot be implemented — see
"Stopping is also a shortcut" below for the strict definition
of *genuinely* — **stop and ask the operator.** Never "stub
it and file a follow-up." Never "emit a placeholder and
document it." But also: never stop just because the work is
tedious or the design is ambiguous. The next section draws
that line.

## Stopping is also a shortcut

The other failure mode of this rule is the inverse one:
stopping to ask the operator about every minor tradeoff in
order to avoid generating code. That is also forbidden.

If there is a workable path forward — even one that
involves a tradeoff, an extra refactor, a longer
implementation, more test scaffolding, or a design choice
you'd prefer the operator weigh in on — **take it.** Note
the tradeoff in the PR description, keep moving. The
operator is paying you to generate code, not to generate
questions.

The only legitimate reason to stop and ask is a **concrete
blocker** with no workable path:

- A required upstream API does not exist and cannot
  reasonably be written in this PR.
- A required piece of operator input (a real AP, a real
  calibration, credentials, a hardware decision) is
  genuinely unobtainable and no honest default exists.
- Two valid implementations diverge on a *user-visible*
  contract (wire format, on-disk schema, public API) and
  picking wrong would force a later breaking change.
- A spec is internally contradictory and you cannot tell
  which side is authoritative.

Things that are **not** blockers and must not become "stop
and ask" moments:

- A choice between two reasonable internal implementations
  with no user-visible difference. Pick one, note it, move
  on.
- A refactor that would be cleaner if done a different
  way. Do the refactor if it's in scope; defer it as a
  follow-up if it isn't; either way, keep implementing.
- An additional test that would be nice to have. Write it.
- A naming preference. Pick one.
- Uncertainty about whether the operator wants the
  faster-but-uglier or slower-but-cleaner version. Pick
  the cleaner one; note the tradeoff.
- A dependency that *could* be added but isn't strictly
  required. Don't add it; note the option.

Note tradeoffs in the PR description under a "Tradeoffs"
or "Choices" section. The operator reviews and corrects.
That is cheaper than blocking on every fork in the road.

The rule combines: **never ship a shortcut, never stop
short of a real blocker.** If you find yourself drafting a
question, ask whether it's a concrete blocker (above list)
or a tradeoff (note it and continue). Default: continue.

The rare deviation that the operator explicitly approves
must be recorded in *all three* of:

- **Code**: a `TODO(operator-approved):` comment that names
  the unmet requirement, the reason, and the date of the
  approval. Not `TODO:`. Not `// FIXME`. The exact prefix
  so it greps cleanly.
- **PR description**: a "Deviations from spec" section
  enumerating every unmet requirement with the operator's
  reason. No deviations section ⇒ no deviations claimed ⇒
  any later-discovered deviation is a bug.
- **`plan.org` / `progress.md`**: the task is `PARTIAL`,
  never `DONE`, with a sub-bullet describing the gap and
  linking the operator's approval.

The canonical bug pattern this rule exists to prevent: the
Android writer shipped `first_frame_blake3 = ""` because
computing the checksum was inconvenient; the Rust verifier
treated `Some("")` as a real checksum; every bundle produced
over the following weeks failed replay verification before
the engine even started, and nobody noticed until the
operator pulled a capture off the phone. One silent
shortcut, multiplied across every capture, ate the entire
corpus until it was caught. That is the failure mode. It is
not acceptable. It will not be acceptable next time either.

This rule overrides convenience, schedule pressure, model
uncertainty, context-window limits, and your own judgment
about whether a shortcut is "obviously fine." It is not
obviously fine. Generate the correct code.

Guidance for AI coding agents working in this repository. Humans:
read `readme.org`, `plan.org`, `progress.md`, and `CONTRIBUTING.md`
first — those are the source of truth for project intent. This
file translates those documents into conventions an agent needs
to operate without re-deriving them every session.

## Repository orientation

Bris is a portable celestial-navigation system. The core is a
Rust workspace (`crates/`); platform shells (Android, eventually
iOS, embedded Linux appliance) wrap that core via FFI. Read
order for a new agent session:

1. `readme.org` — project concept, accuracy model, design
   decisions.
2. `plan.org` — phased roadmap with task-level granularity.
   Authoritative for what's done, partial, and pending.
3. `progress.md` — current snapshot of work in progress.
4. `docs/design/` — architecture documents for individual
   subsystems (pipeline, frame scheduling, diagnostic collection).
5. `docs/protocol/pbris.md` — the `$PBRIS` diagnostic-sentence
   contract. Versioned; treat as a stable interface.
6. `docs/operator/` — user-facing documentation. Reflects the
   *current* operator-visible behavior.

`plan.org` line states (`DONE`, `PARTIAL`, `TODO`) are
load-bearing. If you change a state, update `progress.md` in
the same change.

## Hard rules

These rules come from `plan.org`'s "Decisions baked in" block and
are non-negotiable without explicit operator approval:

- **No telemetry, no analytics, no automatic network calls.** The
  diagnostic-collection subsystem is the one network surface and
  it is *explicitly* user-initiated (debug mode + per-submission
  review). Do not add silent or scheduled network requests.
- **Honest uncertainty everywhere.** Every measurement carries a
  1σ contribution. Fixes carry a full position covariance. Do
  not silently fudge, smooth, or fuse without recording the
  resulting σ inflation.
- **License: GPL-3.0-or-later.** `cargo deny check` enforces
  license-compatible dependencies. Adding a dep that fails
  `cargo deny` is a stop-the-work event; resolve before merging.
- **`unsafe_code = "forbid"` workspace-wide.** Do not relax this
  per-crate without a written rationale and operator approval.
- **Pi Zero 2W (aarch64) is the minimum embedded target.** Code
  that compiles for `x86_64-unknown-linux-gnu` but not
  `aarch64-unknown-linux-gnu` is broken.
- **Android builds run in CI only.** Do not install the Android
  SDK / NDK / `cargo-ndk` / Android target stdlibs locally and
  do not attempt `./gradlew :app:assembleDebug` on the dev
  workstation. The Android tooling footprint (~2.7 GiB system
  + ~300 MiB rustup targets + ~600 MiB transient build dirs)
  is large enough that the operator has chosen to keep it off
  the workstation entirely. CI is authoritative for APK
  production; the rolling `nightly` GitHub release publishes
  every push to `main`. See "Where work runs" below.

## Workspace structure

```
crates/
  bris-core/         pure types; angles, time scales, σ
  bris-almanac/      ephemerides + star catalog
  bris-vision/       image processing primitives (no I/O policy)
  bris-platesolve/   star pattern matching
  bris-nav/          sight reduction + fix combination
  bris-nmea/         NMEA 0183 formatting + transport
  bris-streaming/    continuous-operation engine (Phase 3.5)
  bris-capture/      V4L2 capture (Linux only)
  bris-calibrate/    lens calibration workflow
  bris-cli/          headless reference frontend
  bris-ffi/          UniFFI bindings layer (Phase 7 on-ramp)
  bris-collector/    diagnostic-submission HTTP service (spike)

bris-android/        Kotlin Android shell (Phase 7 on-ramp)
docs/                design, protocol, operator
scripts/             data import + ML training scaffolding
test_video/          captured footage corpus (large; .gitignored
                     for the bulk, with curated subsets promoted
                     into per-crate tests/regression/)
```

`bris-ffi` and `bris-collector` and `bris-android/` were added
together with the diagnostic-collection spike. Their design is
captured in `docs/design/diagnostic_collection.md`.

## Per-component conventions

### Rust crates (all of `crates/`)

- Toolchain pinned by `rust-toolchain.toml`. Do not bump
  silently.
- Edition 2021, Rust 1.94 minimum.
- Workspace lints (`Cargo.toml` `[workspace.lints]`) are
  authoritative: `unsafe_code = "forbid"`,
  `missing_debug_implementations = "warn"`, `missing_docs =
  "warn"`, `clippy::pedantic` warn-level. Do not suppress
  per-file or per-crate without writing why in a comment.
- Use `workspace.dependencies` for shared deps; don't add a new
  version of an already-shared dep in a single crate.
- Module-level docs on every public module explaining what the
  module is for; item-level docs on public items.
- Tests live in `src/` (`#[cfg(test)] mod tests`) for unit
  scope, `tests/` for integration scope, and
  `tests/regression/<scene>/case.toml` for the TOML-driven
  scene corpus (see `crates/bris-vision/tests/regression/`).
- Run before claiming work is done:
  ```
  cargo fmt --all
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo test --workspace --all-features
  cargo deny check
  ```

### Build-cache / disk hygiene

The workspace is large (~12 crates, axum + tokio + image
stack). A naive `target/` reaches 20+ GiB. Standing policy:

- **Dev profile uses `debug = "line-tables-only"`** (set in
  the root `Cargo.toml`). Panic backtraces still resolve to
  file:line. If you need full DWARF for a gdb/lldb session,
  override per-invocation: `CARGO_PROFILE_DEV_DEBUG=2 cargo
  build`. Do not change the workspace default.
- **Per-worktree `target/`, not a shared `CARGO_TARGET_DIR`.**
  Cave's `implementer` agent runs in isolated git worktrees and
  parallel agents may build concurrently. Cargo takes a build
  lock per target dir, so a shared target serializes all
  builds and thrashes the incremental cache when branches
  differ. `.cargo/config.toml` documents this; do not add
  `build.target-dir` there.
- **`sccache` is the cross-worktree cache.** Install once
  (`pacman -S sccache`) and export `RUSTC_WRAPPER=sccache` in
  your shell. It's content-addressed and safe under concurrent
  builds across worktrees / branches. Not wired into the
  repo config because CI and the Pi build don't have it and
  shouldn't be forced to.
- **`cargo-sweep` for periodic GC.** `cargo install
  cargo-sweep`, then `cargo sweep --time 30` from the repo
  root drops artifacts untouched for 30 days. Safe; rebuilds
  on next use (sccache absorbs most of the cost).
- **Prefer `cargo check` / `-p <crate>` in tight loops.**
  `--workspace --all-features` rebuilds everything and is the
  right thing only before commit. During iteration, scope to
  the crate you're editing.
- **Delete cross-compile dirs when not in use.**
  `target/aarch64-*/` and `target/*-android/` survive
  `cargo clean -p` and add up fast. `rm -rf` them when you're
  not actively testing the Pi or Android paths.

### `crates/bris-streaming` — the streaming engine

- `EngineConfig` / `push_frame` / `fix_stream` / `diagnostics` is
  the **load-bearing public surface**. Phase 3.5 is DONE through
  commit 9 (61 tests). Treat the names and signatures of these
  as stable.
- `EngineDiagnostics` is consumed by both the CLI and the FFI.
  Adding fields is fine; renaming or removing requires a
  coordinated update in `bris-ffi` and `bris-android/`.
- Worker-thread model is single-threaded today. Do not introduce
  parallelism without empirical justification on the Pi Zero 2W.
- `EngineConfig::lock_ap_for_replay` is a **diagnostic-only**
  flag for `bris-cli replay --ap-lock-truth`. Production
  callers must leave it `false`. See
  `docs/design/replay_modes.md`.
- `EngineDiagnostics::cross_frame_sights_emitted` counts
  Stage E sights whose body and horizon came from different
  frames (the `panorama_altitude_for_pair` path). The
  executed Kabsch RMS residual is the authoritative stitch
  σ at sight-emission time; the cheap time-gap estimate in
  `stage_e::STITCH_SIGMA_PER_SECOND_RAD` is only used for
  pair ranking.

### `crates/bris-ffi` — UniFFI bindings (spike)

- **Proc-macro mode** (`uniffi = { features = ["build"] }`,
  `#[uniffi::export]`). No separate `.udl` file.
- Public surface lives in `src/lib.rs`. Each exported type or
  function carries doc comments — they propagate into the
  generated Kotlin/Swift bindings.
- Types crossing the FFI boundary are *value types* (owned, no
  borrows) unless they're explicitly `Arc<T>`-shared handles
  (the `Engine` itself is the canonical example).
- The FFI **does not duplicate** logic that exists in core
  crates. Everything is a thin wrapper. If you find yourself
  writing real logic in `bris-ffi`, it belongs in
  `bris-streaming` (or the appropriate core crate) and the FFI
  is just exposing it.
- Cross-compile for `aarch64-linux-android` and
  `x86_64-linux-android` (emulator). Targets are installed via
  `rustup target add`. The Gradle build in `bris-android/`
  invokes `cargo build --target ...` per ABI.

### `crates/bris-bundle` — debug-bundle schema

- **Shared schema** between Android capture, `bris-cli replay`,
  and `bris-collector` ingest. Pure serde + a couple of
  filesystem helpers; **never** executes engine logic.
- Public types: `BundleManifest`, `DeviceInfo`, `CaptureInfo`,
  `IntrinsicsRecord`, `IntrinsicsSource`, `Distortion`,
  `ApInput` / `ApProvenance` / `ApDerivationTrace`, `GpsTruth`,
  `AtmosphereHint`, `FrameSidecar`.
- Three-axis design (AP / GPS-truth / derivation) is
  load-bearing; see `docs/design/debug_bundle_schema.md` for
  the rationale.
- Within `schema_version: 1` only additive changes are allowed.
  Breaking changes bump the version and the loader rejects
  mismatches with `BundleError::UnsupportedSchema`.

### `crates/bris-collector` — diagnostic submission service

- **Filesystem store, not a database.** Layout under
  `<data-root>/submissions/<yyyy>/<mm>/<dd>/<ulid>/`:
  ```
  manifest.json   schema-versioned metadata (the searchable index)
  media/          uploaded images / video / log files
  pbris.log       raw $PBRIS sentence window if included
  calibration/    if the submission is a calibration bundle
  debug/          if debug-capture content is included
  ```
- A SQLite mirror (`<data-root>/index.sqlite`) is the
  list/filter index for the review UI. Truth is the manifest on
  disk; the SQLite mirror is rebuildable from the filesystem
  and treated as a cache.
- HTTP: axum. Bearer token from a config file or env var.
  Single shared token for the spike (documented as
  spike-grade).
- No PII in logs. The collector logs request IDs, submission
  IDs, and sizes — never the bearer token, never raw GPS
  coordinates from a submission, never the contents of free-text
  notes.
- Submission manifests are append-only on disk. Soft-delete
  (per `plan.org` Phase 6 / Phase 7 retention model) flips a
  flag in the manifest and the SQLite mirror; the underlying
  files stay for the retention window.

### `bris-android/` — Kotlin app

- Kotlin only. No Java sources.
- Gradle Kotlin DSL (`*.gradle.kts`). No Groovy.
- `minSdk = 26`, `targetSdk` = current (bump as Android
  releases; document the bump in the changelog).
- CameraX for capture. `STRATEGY_KEEP_ONLY_LATEST` backpressure
  (see `docs/design/diagnostic_collection.md` for the rationale).
- UniFFI-generated Kotlin bindings live in
  `bris-android/app/build/generated/source/uniffi/` and are
  produced by a Gradle task that invokes the Rust build. They
  are **not committed** to the repository.
- Native libraries for the bound Rust core are packaged per ABI
  under `app/src/main/jniLibs/<abi>/libbris_ffi.so`. The
  Gradle build orchestrates the cross-compile.
- The "Debug mode" toggle in settings is the **single
  operator surface controlling per-frame debug archival**.
  With Debug OFF the engine runs normally and each capture
  persists only its operator-facing artifacts:
  `manifest.json` (sight-log entry), `bundle.json` (replay
  manifest), `pbris.log`, plus the 1–3 fix-frame PGMs that
  backed any published fix — sidecared
  `retention: "fix_frame"`. KB to ~50 MB per capture.

  With Debug ON the analyzer additionally taps every frame
  during Start→Stop into the same `captures/<id>/frames/`
  directory with sidecar `retention: "debug"`. Fix frames
  are promoted in place (no file copy) at finalize.
  `bundle.json` gets the full frame catalog +
  `gps_truth` attachment. `index.jsonl` is added.
  ~4 MB × fps × duration per capture.

  This is the **only** debug-related save path. One toggle,
  one capture directory layout, one frame directory per
  capture; differentiation is by sidecar retention
  metadata, not by location. A single Settings **Share
  sessions** action SAF-zips the entire
  `<external-files>/sessions/` tree for off-device
  transfer. The on-disk tree IS the canonical corpus
  layout; `unzip -n <zip> -d <corpus>/` lands it correctly.

  Engine cross-restart persistence (`bris_streaming::
  SightStore`) is session-scoped via its `data_root`: each
  session has its own
  `<external-files>/sessions/<UUID>/engine-store/`. The
  96-byte binary record format is session-blind;
  session-awareness is purely the path the caller picks.
  `SessionHolder` rebuilds the engine when the active
  session changes.

## Where work runs

The workstation is for **Rust**. Anything Android-side is
authored locally in source files but **built only in CI**.

| Task | Local | CI |
|------|-------|----|
| Edit Rust source / tests | yes | — |
| `cargo check` / `cargo test` / `cargo clippy` / `cargo fmt` | yes | also runs |
| `cargo deny check` | yes (if installed) | runs |
| Cross-compile bris-ffi for Android (`cargo ndk …`) | **no** | runs |
| Generate UniFFI Kotlin bindings | **no** | runs |
| Edit Kotlin source under `bris-android/` | yes | — |
| `./gradlew :app:assembleDebug` | **no** | runs |
| `./gradlew :app:uniffiBindgen` | **no** | runs |
| Inspect / install the resulting APK | yes (`adb install`) | publishes |

This is a deliberate constraint. The Android tooling footprint
(~2.7 GiB system packages + ~300 MiB rustup target stdlibs +
several hundred MiB of transient Gradle / cargo build dirs)
adds up to ~3-4 GiB of disk for tooling that the
diagnostic-collection spike needs only to produce the APK.
Producing the APK is what CI is for.

### Practical workflow for Android-touching changes

1. Edit Kotlin (and/or Rust) locally.
2. Run `cargo check --workspace` and `cargo clippy --workspace
   --all-targets -- -D warnings` to catch FFI surface
   regressions and lint issues.
3. Commit and push.
4. The `android` workflow at
   `.github/workflows/android.yml` cross-builds
   `bris-ffi` for both Android ABIs, generates UniFFI Kotlin
   bindings, runs `./gradlew :app:assembleDebug`, uploads the
   APK as a workflow artifact, and republishes the rolling
   `nightly` GitHub Release.
5. Install via `adb install` from the artifact or, more
   conveniently, the stable URL:
   `https://github.com/spencerharmon/bris/releases/download/nightly/bris-app-debug-latest.apk`.

### When you really do need a local Android build

If a CI feedback loop is too slow for a particular debugging
session — for instance, chasing a JNA crash that only repros on
device — the operator may temporarily install the Android
toolchain. **Don't do this in a normal session.** Treat it as
a one-off, and remove the tools (~3.5 GiB) when done:

```sh
# install (operator-approved one-off)
sudo pacman -S android-ndk android-sdk-build-tools \
                android-platform android-sdk-cmdline-tools-latest
rustup target add aarch64-linux-android x86_64-linux-android
cargo install cargo-ndk

# … your Android-debugging session …

# tear down (back to baseline)
sudo pacman -R android-ndk android-sdk-build-tools \
                android-platform android-sdk-cmdline-tools-latest
rustup target remove aarch64-linux-android x86_64-linux-android
cargo uninstall cargo-ndk
rm -rf ~/.android-sdk-bris ~/.android-sdk-overlay ~/.bris-build
rm -f bris-android/local.properties
```

If you find yourself needing a local Android build *often*,
the right fix is making CI faster (build cache, smaller
artifacts), not making the operator's workstation bigger.

## Commit and PR hygiene

- One logical change per PR. Mixed "scaffold + behavior" PRs
  are hard to review; split them.
- PR description references the `plan.org` task it advances and
  flips that task's state (or notes why it can't yet).
- Tests are not optional. New behavior gets new tests; new
  surfaces get integration tests; new corpus entries (in
  `tests/regression/`) get a `case.toml` that asserts current
  behavior (success *or* expected failure — refusal is a valid
  assertion).

## Don'ts (common agent traps)

- **Don't add a dependency without checking the workspace
  graph.** `Cargo.toml` has `[workspace.dependencies]`. Reuse,
  don't fork.
- **Don't introduce async without a reason.** The streaming
  engine is sync-threaded by design. The collector is async
  (axum requires it). `bris-ffi` is sync at the FFI boundary;
  async happens on the Kotlin side via coroutines.
- **Don't fuse data sources silently.** Bris's invariant is that
  every reported value comes with a documented σ. If you find
  yourself averaging two estimates without recording the
  resulting σ, stop.
- **Don't auto-rotate images.** `Frame::source_rotation` is
  opt-in (`plan.org` Phase 2.5 has the reasoning). The capture
  path must declare rotation explicitly; the pipeline does not
  guess.
- **Don't bypass the regression-test harness for "quick"
  testing.** `tests/regression/*/case.toml` is the canonical
  way to assert on real footage. One-off Rust tests against
  vendored frames bit-rot quickly.
- **Don't write to `progress.md` or `plan.org` casually.** Both
  are operator-readable status documents. Edits should be
  precise and reflect actual work done.

## Project boundaries (directory discipline)

The operator keeps **multiple checkouts of the same repo** under
distinct directory names so Emacs' project name (and therefore the
buffer title) reflects the *task* in progress, not the repo. Two
or more directories on disk may share the same `origin` remote.

Rules for agents:

- **Stay inside the cwd checkout.** Do all reads, edits, builds,
  and `cargo` invocations within the working directory you were
  launched in, even if another checkout of the same repo exists
  elsewhere on disk and would also satisfy the request.
- **Do not `cd` into a sibling checkout** to run commands.
  Crossing checkouts pollutes the wrong project's `target/`,
  can race with the operator's open buffers in that project,
  and defeats the point of the per-task directory naming.
- If a tool, crate, or file seems to be missing from the cwd
  checkout, check again (it almost certainly isn't) before
  reaching elsewhere. If you really do need something outside
  the cwd, **ask first.**
- This applies to subagents too. An `explore`/`implementer` run
  inherits the cwd; don't override it to point at a sibling
  checkout without operator approval.

## Pulling debug captures from the phone

### Importing into the corpus

The corpus (under `bris-corpus/` at the repo root, or any
directory the operator points `bris replay` at) is a tree of
sessions and captures:

```
<corpus-root>/
  sessions/
    <session-uuid>/
      session.json
      captures/
        <capture-id>/
          bundle.json
          frames/
            000000000001.pgm
            000000000001.json
            ...
          pbris.log
```

When pulling debug zips off the phone (per the
"Pulling debug captures" workflow below), the canonical
import step is **`unzip -n`**:

```sh
unzip -n bris-exports/incoming/bris-debug-<cap-id>.zip \
     -d <corpus-root>/
```

`-n` (never overwrite) is load-bearing: the same capture-id
will not produce different contents across pulls, so seeing
"file exists" warnings is the expected idempotent-import
signal. If you find yourself reaching for `-o` (overwrite),
stop — either the zip is corrupted, or you're about to clobber
an edited session.json with the on-device default.

Zips produced by the on-device **Share capture** action are
already in canonical layout:

```
sessions/<UUID>/
  session.json
  captures/<cap-id>/
    bundle.json
    index.jsonl
    pbris.log
    frames/...
```

so `unzip -n <zip> -d <corpus-root>/` drops them straight
into the corpus tree. No flat-layout fixup required. There
is exactly one on-device save path; the operator either has
Debug mode ON (frames persist, Share button visible on
that capture) or OFF (sight-log manifest only, nothing to
share).

A zip whose internal nesting predates the session-aware
writer (no `session_id` in `bundle.json`, or no
`session.json` alongside) lands directly under
`<corpus-root>/bris-debug-<cap-id>/` instead of under a
session. Use `scripts/synthesize_bundle_json.py` to
fabricate a stub session + move the capture into place.
**Never** rewrite the zip's internal structure to match —
the zip is the operator's backup; rewriting it loses
provenance.

Replay them with:

```sh
bris replay --bundle <corpus-root>/sessions/<uuid>/captures/<cap-id>/
```

or (once #4 of the testing-strategy stack lands):

```sh
bris replay --session <session-uuid>           # one session
bris replay --all-sessions --corpus <root>     # full corpus
```

The regression harness's K=3 σ-honesty pass rule (described in
`docs/design/testing_strategy.md`) is the single global gate;
there are no per-session expectations files.

## Pulling debug captures from the phone (mechanics)

The operator triggers an export by tapping the **Share
capture** button on a capture row in the on-device
SightLogScreen (visible only for captures recorded with
Debug mode ON — those have a `bundle.json` on disk; ones
recorded with Debug OFF carry only a sight-log manifest
and are nothing to share). The Share action writes a
canonical-layout zip via SAF; the operator picks a
destination, and the resulting file is named
`/sdcard/<picked-tree>/bris-session-<UUID>-cap-<id>.zip`
or similar (SAF-determined).

When asked to pull captures off the phone, use this
workflow:

1. **List zips on the phone.** Conventional spot the
   operator picks is the Documents tree:

   ```sh
   adb shell 'ls /sdcard/Documents/bris-session-*.zip 2>/dev/null'
   ```

2. **For each zip: dedupe by filename before pulling.** If
   `bris-exports/incoming/<zip-name>` already exists, skip
   that zip — already ingested. Do not re-pull; do not
   delete on-device just because the local copy is present
   (the operator may have deleted the extracted dir
   intentionally).

3. **Pull new zips into `bris-exports/incoming/`.**

   ```sh
   mkdir -p bris-exports/incoming
   adb pull /sdcard/Documents/bris-session-<UUID>-cap-<id>.zip bris-exports/incoming/
   ```

4. **Extract directly into the corpus root.** The zip's
   internal layout is canonical (`sessions/<UUID>/{session.json,
   captures/<id>/...}`), so `unzip -n` drops everything
   into place:

   ```sh
   unzip -n bris-exports/incoming/bris-session-<UUID>-cap-<id>.zip \
     -d <corpus-root>/
   ```

   `-n` (never overwrite) is load-bearing: re-pulling and
   re-extracting an already-imported capture is idempotent
   and prints "file exists" warnings, which is the
   expected success signal.

5. **Delete the on-device zip *only after* the extracted
   capture is verified non-empty.** Storage on the phone is
   precious; leaving stale zips around accumulates
   gigabytes.

   ```sh
   # sanity-check before delete: confirm bundle.json exists
   test -s <corpus-root>/sessions/<UUID>/captures/<id>/bundle.json \
     && adb shell rm /sdcard/Documents/bris-session-<UUID>-cap-<id>.zip
   ```

6. **The local `bris-exports/incoming/<zip>` may be kept**
   (it's a backup of what was on the phone) or deleted at
   the operator's discretion. Default: keep it; it's cheap
   insurance against a botched extract.

Notes:

- Pre-refactor zips (named `bris-debug-<ulid>.zip`, flat
  layout) require the manual fixup documented in
  `scripts/synthesize_bundle_json.py`. New captures all
  use the canonical layout.
- Never bulk-delete on-device zips without confirming the
  extracted bundle.json landed locally. An interrupted
  `adb pull` (USB unplug, phone screen-off-suspend) can
  produce a truncated local file that *looks* present but
  isn't the full payload — always verify with `unzip -t`
  or a `test -s` on the extracted `bundle.json` before
  deleting the source.

## Cave worktree hygiene

Subagents launched with `isolation: worktree` (the default for
`implementer`) create a fresh git worktree under
`.cave/worktrees/implementer-*` on a `cave/agent/implementer-*`
branch. Each worktree gets its own `target/` directory; a single
full build is 5–10 GiB. Twenty-odd parallel agent sessions will
silently consume 40–100 GiB.

Periodically — and always after a batch of merged agent PRs —
prune them:

```sh
# 1. remove all cave worktrees (forces; they are disposable)
git worktree list | awk '/\.cave\/worktrees/ {print $1}' \
  | xargs -r -n1 git worktree remove --force
git worktree prune

# 2. delete merged agent branches
git branch --merged main | grep '^  cave/agent/implementer-' \
  | xargs -r -n1 git branch -d

# 3. (optional) audit unmerged branches
git branch --no-merged main

# 4. reclaim packfile space if a lot was deleted
git gc --prune=now
```

Never bulk-delete unmerged branches — inspect first; an agent
branch with unique commits is the only record of that session's
work.

## When in doubt

Ask the operator. The repository is small enough and the
operator engaged enough that a clarifying question is cheaper
than a wrong implementation. The questions in
`docs/design/diagnostic_collection.md` (and similar design docs)
exist because the operator answered them; new design questions
deserve the same treatment.
