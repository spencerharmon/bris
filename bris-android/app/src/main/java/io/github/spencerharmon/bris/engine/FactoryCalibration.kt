package io.github.spencerharmon.bris.engine

import android.os.Build

/**
 * Per-device factory-default calibration intrinsics.
 *
 * **Why this exists.** Calibration is the dominant absolute-
 * altitude error before the operator runs the in-app
 * checkerboard workflow. For known devices we can ship a
 * pre-solved calibration that's good enough for an operator
 * to take their first fix on day one, without ever opening
 * Settings → Calibration. Operators can still override by
 * running their own calibration; the override (a real
 * persisted session in [`CalibrationStore`]) takes
 * precedence over the factory entry.
 *
 * **Scope of a factory entry.** Calibration intrinsics are
 * keyed by `(device model, lens id, capture resolution)`.
 * They are *not portable*: a profile solved on one S62 main
 * camera at 4032×3024 may not be optimal for another S62 if
 * the per-unit lens variation is significant, and it
 * certainly does not apply to a different sensor on the
 * same device. The lookup is exact-match on all three keys.
 *
 * **Provenance honesty.** Factory entries are flagged
 * separately from operator-calibrated ones; the diagnostic
 * overlay shows `calib: factory (S62 main, rms 0.73)` so
 * the operator knows they're on a generic profile and can
 * choose to run their own calibration for better accuracy.
 *
 * **Failure mode.** If a major Android update changes the
 * camera HAL's built-in geometric correction (rare but
 * possible), the bake-in becomes wrong — recovered
 * `k1`/`k2` are the *residual* after the HAL's correction.
 * Operators who upgrade and see altitude drift should
 * re-calibrate. Docs (`docs/operator/calibration.md`)
 * mention this; the UI does not auto-detect it.
 *
 * **Adding a new device profile.** Capture a calibration
 * session, confirm `Diagnosis = OK` with sub-pixel RMS,
 * then copy the persisted intrinsics into a [`Profile`]
 * entry below with the device's `Build.MODEL` (or a list
 * of compatible models), the lens id reported by
 * `LensCatalog.enumerate(...).id` for the lens in
 * question, and the capture resolution as actually solved.
 */
object FactoryCalibration {

    /**
     * One factory-provenance calibration entry.
     *
     * Matches when `Build.MODEL` equals one of [`models`]
     * **and** the live screen's lens + capture resolution
     * match [`lensId`] / [`width`] / [`height`] exactly.
     */
    data class Profile(
        /** `Build.MODEL` strings this profile applies to. */
        val models: List<String>,
        /** Lens id as enumerated by `LensCatalog.enumerate`. */
        val lensId: String,
        /** Capture width in pixels the intrinsics were solved at. */
        val width: Int,
        /** Capture height in pixels the intrinsics were solved at. */
        val height: Int,
        /** Solved intrinsics. */
        val intrinsics: CalibrationStore.PersistedIntrinsics,
        /** Short label for the diagnostic overlay (e.g. `"S62 main"`). */
        val label: String,
        /**
         * Sensor analog conversion gain (electrons per ADU)
         * **at the camera's minimum ISO**. The runtime
         * scales this by `currentIso / minIso` to recover the
         * effective per-frame gain.
         *
         * "Measured" means analog gain only. Digital gain is
         * post-quantization and must not be folded in (see
         * `bris_core::SensorGain`).
         *
         * For the Cat S62 main camera the value below is a
         * datasheet-derived **placeholder** until a real
         * per-unit measurement is performed; see the
         * inline comment on the profile entry.
         */
        val gainEPerAduAtMinIso: Double,
    )

    /**
     * Lookup a factory profile for the current device + lens
     * + resolution combination. Returns `null` when no
     * matching profile exists; the caller should then fall
     * back to placeholder intrinsics and flag PLACEHOLDER.
     *
     * Match keys (all must agree):
     *
     * - `Build.MODEL` is one of [`Profile.models`].
     * - `lensId` matches [`Profile.lensId`] exactly. Lens
     *   ids are stable strings derived from the underlying
     *   Camera2 physical camera id (see [`LensCatalog`]),
     *   so a profile keyed for the S62 main camera does
     *   not accidentally apply to its selfie camera.
     * - `(width, height)` matches [`Profile.width`] /
     *   `.height` exactly. We could in principle scale
     *   intrinsics across resolutions using
     *   `bris_vision::Intrinsics::scaled_to`, but a
     *   factory profile is so cheap to add (one entry per
     *   resolution per lens) that exact-match is the
     *   right default for trustworthiness.
     */
    fun lookup(
        lensId: String,
        width: Int,
        height: Int,
        model: String = Build.MODEL ?: "",
    ): Profile? {
        if (model.isEmpty()) return null
        return PROFILES.firstOrNull { p ->
            p.models.any { it.equals(model, ignoreCase = true) } &&
                p.lensId == lensId &&
                p.width == width &&
                p.height == height
        }
    }

    /**
     * The full set of factory profiles compiled into the
     * app. Add new entries here.
     */
    private val PROFILES: List<Profile> = listOf(
        // ─────────────────────────────────────────────────────
        // Cat S62 (rugged-line, main rear camera, native 4:3)
        //
        // Source: in-app calibration session 2026-05-15.
        //   Target: 10×7 inner corners @ 19mm squares.
        //   15 views, 0 rejected, all 100% detection.
        //   Aggregate RMS = 0.733 px (≈47 arcsec at fx=3103).
        //   Diagnosis = OK, no issues.
        //   fx/fy symmetric to 0.4%; cx/cy within 1.4% of
        //   image center; tangential < 0.005; |k1| = 0.023,
        //   |k2| = 0.027 (small residual after CameraX's
        //   built-in geometric correction). Worst per-view
        //   RMS 1.43 px (frame 6, partial occlusion).
        //
        // Lens id "0" is Camera2's main rear camera on S62.
        // Verified against Build.MODEL = "S62 Pro" (Cat
        // markets the device under multiple model strings;
        // adding additional aliases is straightforward).
        // ─────────────────────────────────────────────────────
        Profile(
            models = listOf("S62", "S62 Pro", "S62Pro"),
            lensId = "0",
            width = 4032,
            height = 3024,
            label = "S62 main",
            intrinsics = CalibrationStore.PersistedIntrinsics(
                fx = 3103.4061281557006,
                fy = 3090.496744366685,
                cx = 2013.857097640865,
                cy = 1491.4983945221607,
                k1 = 0.02287385685683836,
                k2 = -0.027249189121853052,
                k3 = 0.0,
                p1 = -0.0020285902622051532,
                p2 = -0.004038950067724464,
                width = 4032,
                height = 3024,
                rmsPx = 0.7331791456580863,
            ),
            // Placeholder analog conversion gain. The S62
            // main sensor's per-unit e⁻/ADU at base ISO has
            // not been independently measured; 4.0 is a
            // representative figure for modern phone
            // back-illuminated CMOS at ISO ~50–80. Replace
            // with a measured value (e.g. via a photon-
            // transfer-curve session) when available.
            //
            // TODO: add a Robolectric test that asserts
            // FrameAnalyzer scales this by currentIso / minIso
            // when the test infrastructure grows.
            gainEPerAduAtMinIso = 4.0,
        ),
    )
}
