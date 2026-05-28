# AGENTS.md

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
- The "Debug mode" toggle in settings is the **only operator
  surface for diagnostic submission**. When off, no submission
  UI is visible anywhere in the app. When on, three contextual
  actions appear: debug capture (rolling on-device buffer of
  all processed frames + logs), send fix (uploads the on-device
  retained data for a single fix), send calibration (uploads
  the calibration session bundle). Every send action shows a
  one-screen pre-upload review.

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
