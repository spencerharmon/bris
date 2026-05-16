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
    data class Operator(val intrinsics: CalibrationStore.PersistedIntrinsics) : CalibrationSource()

    /**
     * A shipped factory profile matched the device + lens +
     * resolution. Good-enough day-one intrinsics; operators
     * can override by running their own calibration.
     */
    data class Factory(
        val intrinsics: CalibrationStore.PersistedIntrinsics,
        val label: String,
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
    store.latestIntrinsicsFor(lensId, width, height)?.let {
        return CalibrationSource.Operator(it)
    }
    FactoryCalibration.lookup(lensId, width, height)?.let { profile ->
        return CalibrationSource.Factory(profile.intrinsics, profile.label)
    }
    return CalibrationSource.Placeholder
}
