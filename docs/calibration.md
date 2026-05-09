# Lens calibration

Bris's vision pipeline depends on a calibrated camera model — the
relationship between sensor pixels and ray directions through space.
Without calibration, every measured altitude is wrong by the
calibration error. A few pixels of distortion at the lens edge
translates to several arcminutes of altitude error at typical fields
of view, which is the dominant absolute-altitude error after
atmospheric refraction.

This guide walks through the calibration workflow end-to-end:
prepare a target, capture frames, run `bris calibrate`, inspect the
result, and configure `bris serve` to use it.

## When you need to calibrate

- **The first time you run Bris on a camera.** `bris serve` falls
  back to placeholder intrinsics with a loud warning when no
  calibration is configured. Fixes still publish, but they're
  uniformly biased by the calibration error — typically *tens of
  nautical miles*. Calibrate before trusting any fix you take.
- **After changing anything optical.** A new lens, a re-mounted
  sensor, a focus shift, even cleaning the lens with the wrong
  cloth: any change that perturbs the lens-to-sensor geometry
  changes the intrinsics. Re-calibrate.
- **After changing the capture resolution.** Focal length scales
  with sensor crop and binning. A 640×480 calibration silently
  produces wrong altitudes when applied to 1280×720 capture.
  Calibrate at the resolution you'll be running. Bris refuses to
  load a calibration whose recorded resolution doesn't match the
  current capture.

## What you need

- **The Bris CLI** built and installed (`cargo build --release -p bris-cli`).
- **A printed checkerboard target.** Standard options:
  - Generate one with OpenCV's `gen_pattern.py`, calib.io's
    [printable templates](https://calib.io/pages/camera-calibration-pattern-generator),
    or any other calibration-template tool.
  - Default Bris parameters expect a board with **7×11 inner
    corners** (an 8×12 squares board) and **25 mm squares**, but
    any size between roughly 5×7 and 9×13 inner corners works
    fine. Override with `--rows`, `--cols`, `--square-size-mm`.
  - **Mount the print on a rigid flat surface.** A thick piece of
    foamcore, MDF, or aluminium. A wavy paper print introduces
    0.5–2 px of residual error that's impossible to distinguish
    from real lens distortion.
  - **Measure the actual square size** with a caliper after
    printing. Printer scaling routinely produces ±1 mm of error
    relative to the source PDF; this matters because the absolute
    scale of the board feeds the absolute scale of the recovered
    intrinsics.
- **A way to capture frames** at the resolution you'll be running
  Bris at:
  - `bris capture --frames N --output ./calib-frames` for a USB
    camera through V4L2.
  - Any other tool that saves grayscale or color image files
    (PNG/JPEG/PPM) — the file format is fine, the pixel format is
    fine. Bris's calibrate path opens them as 8-bit grayscale.

## Capture procedure

The calibration solve estimates focal length, principal point, and
five distortion coefficients (radial k1/k2/k3, tangential p1/p2)
plus per-frame board pose. That's a lot of unknowns; the more
**varied** views you give it, the better-conditioned the solve.

The rule of thumb:

- **20–30 frames** is the comfortable regime. Fewer than 10 makes
  the solve brittle; more than 40 doesn't help much.
- Vary the **distance** from the camera. Get some frames with the
  board filling most of the FOV (close), some with it filling
  about a third (medium), and some smaller (far). The far-away
  frames constrain focal length, the close ones constrain
  distortion.
- Vary the **tilt**. Don't keep the board parallel to the sensor
  — Zhang's planar method needs out-of-plane rotation to separate
  focal length from board pose. Tilt the board 30–45° around both
  axes for at least half the frames.
- Vary the **position in the FOV**. Move the board to the center,
  each corner, top, bottom, sides. Distortion coefficients fit
  best when corners hit every region of the sensor.
- **Hold the board steady** for each frame. Motion blur turns
  sharp corners into smudges; the corner detector then finds them
  in the wrong place, and the residuals balloon.
- **Sharp focus** on the board. If you can't see the inner
  corners crisply by eye in the saved frames, the detector won't
  either.

A typical capture session takes 5–10 minutes including setup. If
you're capturing through `bris capture`, use a long enough
`--duration` or large `--frames` to let yourself reposition the
board between captures.

### Example capture command

```bash
bris capture \
  --device /dev/video0 \
  --width 640 --height 480 \
  --output ./calib-frames \
  --frames 30 \
  --exposure-us 5000
```

This records 30 frames at the camera's natural rate (typically
30 fps, so 1 second total) with a short exposure to minimize
motion blur. For a real session you want closer to **5 seconds
between board moves**, which means either `--frames 1` per move
(running the command repeatedly) or capturing continuously and
letting Bris discard the frames where the board didn't change
position.

The simplest workflow: run `bris capture --duration 60` (one
minute), and during that minute slowly cycle the board through 25
or so distinct positions, holding each for 2 seconds.

## Run calibration

```bash
bris calibrate \
  --frames ./calib-frames \
  --rows 7 --cols 11 \
  --square-size-mm 25 \
  --output ~/.local/share/bris/intrinsics.toml
```

Output (typical successful run):

```
INFO bris-calibrate: scanning frames    directory=./calib-frames candidate_frames=30 rows=7 cols=11
INFO bris-calibrate: detection complete  successful_views=27 skipped_no_board=2 skipped_wrong_size=1 skipped_io=0
INFO bris-calibrate: solve complete      fx=612.34 fy=612.71 cx=318.91 cy=240.50 k1=-0.0823 k2=0.1421 rms_px=0.31 views=27 observations=2079

Calibration written to: /home/operator/.local/share/bris/intrinsics.toml
  RMS reprojection: 0.310 px
  Views used:       27
  Observations:     2079
  Diagnosis:        OK

Use the file with `bris serve` by setting `[camera] intrinsics = "..."` ...
```

## Inspecting the result

`bris calibrate` runs a diagnostic before declaring success.
Three severity levels:

- **OK** — calibration looks healthy. Use it.
- **WARN** — calibration is usable but a quality concern is
  noted. Consider re-shooting if accuracy matters.
- **ERROR** — calibration is unlikely to produce trustworthy
  fixes. Bris does *not* write the intrinsics file in this case;
  re-shoot before the file appears on disk.

Common warnings and what to do about them:

| Code                          | What it means                                                                                                                            | What to try                                                                                                                                 |
| ----------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `view_count_low`              | Calibrated against fewer than 10 views. The solve technically converges but is sensitive to outliers.                                    | Capture more frames if you're chasing sub-pixel residuals.                                                                                  |
| `view_count_too_low`          | Fewer than 5 views. Solve is unreliable.                                                                                                 | Re-shoot. Aim for 20+.                                                                                                                      |
| `reproj_error_elevated`       | Mean RMS reprojection > 1 px. Calibration is usable but could be tighter.                                                                | If accuracy matters, re-shoot the worst frames.                                                                                             |
| `reproj_error_high`           | Mean RMS reprojection > 2 px. Almost certainly wrong.                                                                                    | Re-shoot with sharper focus, less motion blur, board mounted on a flat rigid surface.                                                       |
| `focal_invalid`               | Focal length came out negative or non-finite. Solve diverged.                                                                            | Re-shoot with more view diversity (especially board tilts).                                                                                 |
| `focal_asymmetric`            | fx and fy differ by more than 10%. Real cameras have fx ≈ fy unless the sensor has rectangular pixels (extremely rare).                  | Likely a wavy board or insufficient view diversity. Re-mount the print rigidly; capture more tilts.                                         |
| `principal_point_off_center`  | Principal point is more than 20% of the image away from the center. Could be real (off-center lens mount) or could be the solve fitting noise. | Re-shoot with views distributed across the full FOV; if the warning persists, the lens may genuinely be off-center.                          |
| `k1_unusual`                  | \|k1\| > 0.5. Unusual for non-fisheye lenses.                                                                                            | Rectilinear lenses are fine. Fisheye lenses (FOV > ~120°) need a different distortion model than Bris uses; check your lens specifications. |
| `tangential_unusual`          | \|p1\| or \|p2\| > 0.01. Tangential terms typically capture sensor mounting tilt.                                                        | Could indicate a real mounting issue or insufficient view diversity. More views distributed across the FOV usually helps.                   |

## Use the calibration

Either point your config file at it:

```toml
# ~/.config/bris/config.toml
[camera]
intrinsics = "/home/operator/.local/share/bris/intrinsics.toml"
```

Or pass `--intrinsics` on the command line:

```bash
bris serve --intrinsics ~/.local/share/bris/intrinsics.toml --assumed-lat 47.6 --assumed-lon -122.3
```

`bris serve` logs the intrinsics file's quality summary at
startup so you can verify it loaded the file you expected. The
file itself is human-readable TOML — you can `cat` it to see the
recovered focal length, distortion coefficients, and reprojection
RMS.

## What sub-pixel residuals get you

The reprojection RMS is the *only* number worth chasing during
calibration. As a rough guide for the absolute-altitude error
contribution:

| RMS (px) | Approx. altitude error contribution (arcmin) |
| -------- | -------------------------------------------- |
| 0.3      | 0.3                                          |
| 0.5      | 0.5                                          |
| 1.0      | 1.0                                          |
| 2.0      | 2.0                                          |

This is the per-sight contribution from calibration alone; the
full sight uncertainty also includes horizon, refraction, dip,
and timing components. At sub-pixel residuals, calibration drops
out of the dominant-error position and refraction (in disturbed
atmospheres) or horizon (in clutter) becomes the limiting factor.

The published `$PBRIS,FIX` sentence reports which source
dominates the per-fix sigma; if it consistently says
`calibration`, your calibration RMS is the bottleneck and re-shooting
is the right next step.

## Known limitations

- **Rectilinear lenses only.** Bris's Brown-Conrady distortion
  model is the standard fit for normal and wide-angle lenses up
  to ~90° diagonal FOV. Fisheye lenses (>120°) need a different
  model (usually equidistant or stereographic projection); not
  yet implemented.
- **No multi-camera calibration.** Each camera is calibrated
  independently. Stereo or array setups need separate
  per-device intrinsics files.
- **No factory-default intrinsics.** Plan.org Phase 2 envisions
  shipping a factory-default per shipped hardware unit; right
  now every operator runs the workflow themselves. Until that
  ships, the per-operator workflow above is the only path.
- **No automatic recalibration scheduling.** Cameras drift over
  time (thermal, mechanical). Bris doesn't track when you last
  calibrated. Re-running every six months for static deployments
  or every couple weeks for handheld is a reasonable manual
  cadence; the diagnostic in `bris calibrate` will surface
  drift if it happens.

## See also

- `docs/design/frame_scheduling.md` — engine architecture; the
  σ accounting that makes calibration's contribution to the fix
  uncertainty explicit.
- `crates/bris-vision/src/lens.rs` — the math (pinhole +
  Brown-Conrady projection and undistortion).
- `crates/bris-calibrate/src/lib.rs` — the workflow code.
- `plan.org` Phase 2 — broader calibration roadmap (factory
  defaults, star-field self-calibration, ML-based
  fallback estimator).
