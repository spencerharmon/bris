package io.github.spencerharmon.bris.engine

/**
 * Source + content of the intrinsics the live engine will
 * use for a given `(lens, capture resolution)` combination.
 *
 * Resolved by [`resolveCalibration`] before each engine
 * binding. The variant carries enough provenance for the
 * diagnostic overlay to tell the operator honestly what
 * they're running on — operator-calibrated, factory
 * default, or fallback placeholder — without the call
 * sites having to re-derive that from three separate
 * nullables.
 */
sealed class CalibrationSource {
    /** Operator ran the in-app calibration workflow. Most trustworthy. */
    data class Operator(
        val intrinsics: CalibrationStore.PersistedIntrinsics,
        /**
         * Identifier of the calibration session that
         * produced these intrinsics. Stamped into
         * `bundle.intrinsics.source.calibration_id` so
         * captures can be traced back to their calibration
         * for debugging.
         *
         * One of:
         *  - A real `UUIDv4` string for calibrations
         *    captured by [`CalibrationStore.newSession`].
         *  - `"legacy:WxH"` for pre-#58 on-disk
         *    calibrations that have no recorded UUID;
         *    deliberately distinct from a real UUID and
         *    from the synthesised `operator-WxH`
         *    placeholder earlier builds shipped so
         *    downstream consumers can tell them apart.
         */
        val calibrationId: String,
    ) : CalibrationSource()

    /**
     * A shipped factory profile matched the device + lens +
     * resolution. Good-enough day-one intrinsics; operators
     * can override by running their own calibration.
     */
    data class Factory(
        val intrinsics: CalibrationStore.PersistedIntrinsics,
        val label: String,
        /** Stable baked-in UUID of the factory profile. */
        val calibrationId: String,
    ) : CalibrationSource()

    /**
     * No operator calibration and no matching factory
     * profile. The engine runs with synthetic placeholder
     * intrinsics and altitudes will be off by the
     * calibration error — the operator is warned via the
     * diagnostic overlay.
     */
    data object Placeholder : CalibrationSource()
}

/**
 * Resolve the best available intrinsics for the given
 * lens + capture-resolution combination.
 *
 * Lookup order:
 *
 *  1. Operator-calibrated session at exactly this
 *     (lensId, width, height) — see
 *     [`CalibrationStore.latestIntrinsicsFor`].
 *  2. Shipped factory profile for this device model + lens
 *     + resolution — see [`FactoryCalibration.lookup`].
 *  3. Otherwise [`CalibrationSource.Placeholder`].
 */
fun resolveCalibration(
    store: CalibrationStore,
    lensId: String,
    width: Int,
    height: Int,
): CalibrationSource {
    store.latestIntrinsicsFor(lensId, width, height)?.let { intr ->
        // `latestIntrinsicsFor` returning non-null guarantees
        // a session directory exists for this (lens, w, h),
        // so `latestCalibrationIdFor` is also non-null:
        // either the recorded UUID (new layout) or the
        // `legacy:WxH` migration marker (legacy layout /
        // missing `calibration_id` field). The Elvis here
        // is purely a static-types fallback that should be
        // unreachable in practice; the marker is the
        // honest answer for everything else.
        val id = store.latestCalibrationIdFor(lensId, width, height)
            ?: "legacy:${width}x${height}"
        return CalibrationSource.Operator(intrinsics = intr, calibrationId = id)
    }
    FactoryCalibration.lookup(lensId, width, height)?.let { profile ->
        return CalibrationSource.Factory(
            intrinsics = profile.intrinsics,
            label = profile.label,
            calibrationId = profile.calibrationId.toString(),
        )
    }
    return CalibrationSource.Placeholder
}
