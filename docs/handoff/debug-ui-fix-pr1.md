# Handoff: Debug UI Fix — PR #1

Audience: an implementer agent picking this up cold. Read this
whole document before touching any files. Read `AGENTS.md` first
if you haven't already; the conventions there are binding.

## Context

Bris's Android app has a "Debug capture" feature that records
every analyzed frame (PGM + per-frame diagnostic JSON) into an
app-private rolling LRU buffer. The buffer is the corpus-capture
mechanism for the engine team — we promote selected clips into
`crates/bris-vision/tests/regression/<scene>/` as ground-truth
fixtures.

The current UI has several gaps that surfaced during a real
corpus-capture attempt (a reflection-pair scene: a celestial
body and its reflection in a horizontal mirror/puddle, both in
the same frame):

1. **Export is fix-gated.** The only way to extract frames from
   the on-device buffer to external storage is through "Send fix
   (debug)" → `PreUploadReviewScreen` → "Save locally". A scene
   that doesn't produce a successful fix (which is precisely the
   case for debugging *why* fixes fail) cannot be exported
   through the UI at all. Workaround during capture session was
   `adb exec-out run-as io.github.spencerharmon.bris tar cf - files/debug-capture`,
   which is not acceptable as an operator workflow.
2. **No visual confirmation the buffer is recording.** The
   Settings "Debug capture" toggle is the only feedback; the
   operator cannot tell from the Live view whether frames are
   actually accumulating. We wasted multiple capture attempts to
   this ambiguity.
3. **Toggle/capture relationship is non-obvious.** Debug capture
   only records when the sight session is also active (operator
   tapped "Start capture"). This relationship is correct
   (protects the filesystem from runaway recording) but
   undocumented in the UI, so "toggle on + no Start press = no
   frames" looks like a bug.
4. **No way to clear the buffer from the UI.** Requires
   `adb shell run-as ... rm -rf files/debug-capture`.
5. **Save path is fixed.** Current export goes to
   `<external-files>/exports/...` which is owner-app-private on
   modern Android; only `adb pull` or MTP browsing reaches it.
   Operator should be able to pick Downloads, an SD card, or
   USB-OTG once and have subsequent saves go there.
6. **Remote-submit UI is shipped but the collector is a spike,
   not production.** Showing the operator a "Send to collector"
   button creates the impression of a working feature that
   isn't. Honest approach: hide the UI behind a build flag, keep
   the code paths for later.

## Branch

Create `debug-ui-fix-pr1` from `main`. PR title: `bris-android:
operator-visible debug buffer state + save without fix`. PR
description must reference this document and the plan-org task
(if a relevant TODO exists; if not, add one under Phase 7 work).

## Hard constraints (do not violate)

- **No silent network calls.** `AGENTS.md` "no telemetry" rule.
  Stubbing remote-submit UI behind a flag is the cleanest way
  to enforce this for now; do not delete the code paths.
- **Toggle/capture relationship stays.** Start/Stop still gates
  whether frames are pushed to the engine and to the buffer.
  This is the filesystem-protection mechanism. The HUD chip's
  job is to *explain* this state, not change it.
- **No Rust changes, no FFI changes.** This PR is Kotlin-only.
  If you find yourself touching `bris-ffi` or any `crates/*`,
  stop and reconsider — the scope is wrong.
- **`appendFrame` already works.** Verified live: 7 frames
  captured into `files/debug-capture/frames/` with
  `index.jsonl` at 756 bytes when start/stop is used correctly.
  The recording path itself is not broken.

## Implementer decisions (locked by the operator)

These were resolved during the planning conversation; do not
re-litigate:

- **HUD chip placement:** inline at top of `DiagnosticOverlay`
  (single Composable to edit; easiest).
- **Clear-buffer UX:** confirmation dialog with frame count +
  byte count in the prompt (e.g. "Delete 47 frames (561 MB)?").
- **Save scope:** local save exports **everything** in the
  buffer (no `MAX_FRAMES_PER_SUBMISSION` cap for the local
  path). The cap stays for the stubbed collector path.
- **State updates:** event-driven via
  `MutableStateFlow<BufferState>` owned by `DebugCaptureBuffer`,
  emitted from `appendFrame` / `appendPbris` / eviction / clear.
  No polling.
- **Save location:** Storage Access Framework
  (`ACTION_OPEN_DOCUMENT_TREE`); persisted via
  `ContentResolver.takePersistableUriPermission`; stored in
  DataStore prefs. First Save without a stored URI prompts;
  subsequent Saves silent. "Change save location" in Settings
  re-prompts.
- **Remote-submit visibility:** feature flag
  `BuildConfig.ENABLE_REMOTE_SUBMIT = false` (default). All
  collector-upload buttons gated. `PreUploadReviewScreen` still
  reachable for the archive purpose, but its network actions
  are hidden.

## Existing surface (read these files first)

- `bris-android/app/src/main/java/io/github/spencerharmon/bris/engine/DebugCaptureBuffer.kt`
  — buffer implementation. Header comment (lines 13–50) is the
  authoritative spec. Has `appendFrame`, `appendPbris`,
  eviction by `DEFAULT_MAX_BYTES = 1 GiB`. Currently exposes no
  state flow.
- `bris-android/app/src/main/java/io/github/spencerharmon/bris/engine/Exporter.kt`
  — `exportDebugCapture(buffer, maxFrames)`. Writes to
  `<external-files>/exports/<yyyy>/<MM>/<dd>/debug-<ulid>/media/`.
  Uses reflection on `rootDir` to find the live buffer.
- `bris-android/app/src/main/java/io/github/spencerharmon/bris/engine/FrameAnalyzer.kt`
  — calls `debugBuffer.appendFrame` when
  `debugCaptureProvider()` returns true. Already correct.
- `bris-android/app/src/main/java/io/github/spencerharmon/bris/ui/LiveScreen.kt`
  — `debugCaptureEnabled` from `prefs.debugCaptureFlow`;
  `captureActive` from `SessionStatus`; analyzer bound only
  when `captureActive` (line ~291). Don't change this gating.
  `DiagnosticOverlay` invocation around line ~217.
- `bris-android/app/src/main/java/io/github/spencerharmon/bris/ui/SettingsScreen.kt`
  — current Debug capture toggle around line 155–162.
- `bris-android/app/src/main/java/io/github/spencerharmon/bris/ui/PreUploadReviewScreen.kt`
  — `MAX_FRAMES_PER_SUBMISSION`, "Save locally" (lines
  260–268), "Send to collector" (lines 115–227).
- `bris-android/app/src/main/java/io/github/spencerharmon/bris/data/Prefs.kt`
  (or wherever `debugCaptureFlow` is defined) — add the SAF URI
  pref alongside.

On-device paths (for testing):
- Live buffer (app-private): `/data/data/io.github.spencerharmon.bris/files/debug-capture/`
- Export dir (current): `/sdcard/Android/data/io.github.spencerharmon.bris/files/exports/`
- Pull command:
  `adb exec-out run-as io.github.spencerharmon.bris tar cf - files/debug-capture | tar xf - -C ./bris-debug-pull/`

## Scope

### 1. `DebugCaptureBuffer.stateFlow`

Add to `DebugCaptureBuffer`:

```kotlin
data class BufferState(
    val frameCount: Int,
    val totalBytes: Long,
    val lastAppendUnixMs: Long?,   // null if buffer empty
    val oldestFrameUnixMs: Long?,  // from index.jsonl head
    val newestFrameUnixMs: Long?,  // from index.jsonl tail
    val evictedSinceClear: Long,   // optional — see note
)

val stateFlow: StateFlow<BufferState>
```

Update points: end of `appendFrame`, end of `appendPbris`,
inside the eviction loop, end of any future `clear()`. Initial
value computed by scanning `index.jsonl` at construction. Keep
the recomputation cheap: maintain running counts as fields,
recompute totals only on construction and clear.

`evictedSinceClear`: nice-to-have; if it's not trivially cheap
to track, ship the counter as `0` and add a TODO. Don't let it
hold up PR #1.

### 2. HUD chip in `DiagnosticOverlay`

When `debugCaptureEnabled`:

- Pulsing red `REC` dot if `lastAppendUnixMs` within 1.5 s of
  now; static grey dot otherwise.
- `N frames · M MB` (use `Formatter.formatShortFileSize`).
- When `debugCaptureEnabled && !captureActive`: subdued chip
  reading `Debug armed — press Start capture to record`. This
  is the documentation fix for problem #3 above.
- When `!debugCaptureEnabled`: chip hidden entirely.

Pass `BufferState` and `captureActive` into `DiagnosticOverlay`
(it already takes a snapshot; one more parameter is fine).
Don't introduce a separate overlay; the operator already looks
at `DiagnosticOverlay`.

### 3. Settings — Debug capture section

When `debugCaptureFlow` is true, expand the section to include:

- **Save buffer now** — `Button`. Calls into the shared
  `DebugBufferActions` (defined in this PR, see §5).
- **Clear buffer** — `OutlinedButton`. Opens
  `AlertDialog` ("Delete N frames (M MB)? This cannot be
  undone.") → on confirm, calls `DebugCaptureBuffer.clear()`
  (new method: delete `frames/`, truncate `index.jsonl`,
  truncate `pbris.log`, reset `.seq`, emit fresh
  `BufferState`).
- **Change save location** — `OutlinedButton`. Launches
  `ActivityResultContracts.OpenDocumentTree`. Stores resulting
  `Uri` in DataStore via a new `prefs.debugSaveLocationFlow`
  (nullable String). Take persistable URI permission via
  `contentResolver.takePersistableUriPermission(uri,
  FLAG_GRANT_READ|WRITE_URI_PERMISSION)`.
- **Buffer state detail** — read-only `Card` displaying:
  frame count, total bytes, oldest/newest timestamps
  (formatted `yyyy-MM-dd HH:mm:ss`), evictedSinceClear,
  on-device path
  (`/data/data/io.github.spencerharmon.bris/files/debug-capture/`),
  current save location (the stored URI's display name, or
  "Not set — pick on first save").

### 4. Live view debug-mode action row

In `LiveScreen.kt`'s `if (debugMode)` block (around line ~240):

- Add `OutlinedButton("Save buffer")` next to the existing
  "Send fix (debug)" button. Calls the same
  `DebugBufferActions.save(...)` path.
- The "Send fix (debug)" button stays. Its target
  (`PreUploadReviewScreen`) is still useful for the archive
  flow; remote-submit there is hidden in §6.

### 5. Shared `DebugBufferActions`

New file:
`bris-android/app/src/main/java/io/github/spencerharmon/bris/engine/DebugBufferActions.kt`

```kotlin
object DebugBufferActions {
    /**
     * Export the entire current debug buffer to the operator's
     * chosen Storage Access Framework destination. If no
     * destination is configured yet, this returns
     * SaveResult.NeedLocation and the caller is responsible
     * for launching the picker, persisting the URI, and
     * retrying.
     */
    suspend fun saveAll(
        context: Context,
        buffer: DebugCaptureBuffer,
        savedTreeUri: Uri?,
    ): SaveResult { ... }
}

sealed interface SaveResult {
    object NeedLocation : SaveResult
    data class Ok(val destinationDisplay: String, val frameCount: Int, val bytes: Long) : SaveResult
    data class Failed(val message: String) : SaveResult
}
```

Implementation: walk the buffer's `frames/`, `index.jsonl`,
`pbris.log`; copy each into a new `bris-debug-<ulid>/` subtree
under the chosen tree URI using `DocumentsContract` /
`DocumentFile`. Stream copies; do not slurp full files into
memory (PGMs are ~12 MB at 4032×3024).

`PreUploadReviewScreen`'s existing "Save locally" button is
refactored to call `DebugBufferActions.saveAll` so behaviour
stays unified. **Don't change its behaviour beyond the call
site.** The cap (`MAX_FRAMES_PER_SUBMISSION`) that previously
applied through `Exporter.exportDebugCapture` is dropped for
the local save path; if the operator wants a clipped
collector-bound bundle in some future PR, that's a separate
code path.

### 6. Feature flag — `ENABLE_REMOTE_SUBMIT`

- Add to `app/build.gradle.kts` under `defaultConfig`:
  ```kotlin
  buildConfigField("boolean", "ENABLE_REMOTE_SUBMIT", "false")
  ```
- Wrap every "Send to collector" / network-upload button in
  `if (BuildConfig.ENABLE_REMOTE_SUBMIT) { ... }`. Do not
  delete the code; just gate the UI. Search for:
  `PreUploadReviewScreen.kt` (the "Send to collector" path),
  any settings entries for collector endpoint/token, the
  `ManifestBuilder.debugCapture` callers.
- Leave the configuration (endpoint URL, bearer token fields)
  visible in Settings for now — they're harmless without a
  button to trigger them. Mark the section header
  "Collector (disabled in this build)".

### 7. Toasts and feedback

After every Save / Clear action, show a `Snackbar` (not Toast —
Material 3 idiom) confirming the result. On Save success:
`Saved 47 frames (561 MB) to <display name>`. On Clear:
`Cleared N frames`. On Save NeedLocation: trigger picker, then
retry automatically.

## Out of scope (do not do in this PR)

- Sharing the saved bundle via `Intent.ACTION_SEND_MULTIPLE`
  (PR #2).
- MediaStore registration (PR #4, possibly never).
- `pixels_hash` per frame (PR #3).
- Operator-facing docs in `docs/operator/` (PR #2).
- Updating `docs/design/diagnostic_collection.md` (PR #3).
- Background recording.
- Any Rust changes.

## Acceptance criteria

The PR is done when an operator can, **without using adb**:

1. Open Settings → toggle Debug capture on.
2. Open Live view, see the HUD chip read "Debug armed — press
   Start capture to record".
3. Press Start capture. HUD chip switches to `REC` + `N frames
   · M MB`, updating live.
4. Hold the scene for ~10 s. Press Stop capture.
5. Tap "Save buffer" in the Live view debug-mode action row.
   First time: picker appears; pick a folder (e.g. Downloads).
   Snackbar: "Saved 47 frames (561 MB) to Downloads".
6. Open the system Files app, navigate to Downloads, see
   `bris-debug-<ulid>/` containing `frames/`, `index.jsonl`,
   `pbris.log`.
7. Return to Settings → tap "Clear buffer" → confirm. Buffer
   state reads "0 frames · 0 B".
8. Confirm no "Send to collector" buttons are visible anywhere
   in the app.
9. Confirm Start/Stop capture still gates whether frames are
   pushed to the engine (existing behaviour preserved).

## Required local checks before pushing

Per `AGENTS.md`:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo deny check  # if installed
```

(These should all be no-ops since the PR is Kotlin-only, but
run them to confirm you haven't accidentally touched the
workspace.)

**Do not** attempt `./gradlew :app:assembleDebug` locally. Per
`AGENTS.md` "Where work runs": Android builds run in CI only.
Push and let `.github/workflows/android.yml` produce the APK;
the rolling `nightly` release will publish to
`https://github.com/spencerharmon/bris/releases/download/nightly/bris-app-debug-latest.apk`.

## Testing strategy

Without a local Android build, you cannot run instrumented
tests in this session. Acceptable:

- **Unit tests for `DebugCaptureBuffer.stateFlow`** — verify
  emissions on append, eviction, clear. Pure JVM tests; runs
  under Gradle's `test` task in CI.
- **Unit tests for `DebugBufferActions.saveAll`** —
  destination-URI mocking is hard with SAF; test the
  file-walking + manifest copy logic against a `Path`-based
  fake destination. SAF integration is end-to-end tested by
  the operator following the acceptance-criteria checklist.

CI will run JVM tests; the operator will manually validate
acceptance criteria once the nightly APK builds.

## Followups (not your job, but tag in the PR description)

- PR #2: shared `DebugBufferActions` consolidation, share
  intent, operator docs.
- PR #3: eviction counter polish, `pixels_hash`,
  `docs/design/diagnostic_collection.md` archive-vs-submit
  clarification.
- PR #4: MediaStore (evaluate need first).

## Questions to raise with the operator before merging

(Only if they actually come up during implementation; don't
manufacture questions.)

- If `DocumentFile`-based writes turn out to be unacceptably
  slow for 12-MB-per-frame PGMs (possible on some Android
  versions), fall back to `ContentResolver.openOutputStream`
  with direct file descriptors and document the workaround.
- If `BuildConfig.ENABLE_REMOTE_SUBMIT = false` causes any
  collector-bound code to fail to compile (e.g. a Composable
  that no longer has a button referencing it triggers an
  unused-symbol warning treated as error), wrap the *call
  sites* in the flag rather than deleting the buttons
  outright. Don't change the underlying components.

---

End of handoff. Read `AGENTS.md`, then the four files listed in
"Existing surface", then start with §1
(`DebugCaptureBuffer.stateFlow`). Everything else depends on it.
