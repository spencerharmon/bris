package io.github.spencerharmon.bris.location

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.location.Location
import android.location.LocationManager
import androidx.core.content.ContextCompat
import io.github.spencerharmon.bris.upload.GpsInfo

/**
 * Coarse-only GPS access for diagnostic submissions.
 *
 * Bris does not use the operator's location for anything except
 * (1) the observer position consumed by the streaming engine
 * (entered manually in settings; not GPS) and (2) optional
 * stamping of diagnostic submissions for collector-side
 * triage.
 *
 * This helper covers (2) only. It requests a *coarse* fix from
 * `LocationManager` without pulling in Google Play services,
 * and runs only when the operator has both Debug mode on and
 * has granted the runtime location permission. There is no
 * background location collection; there is no live-updates
 * subscription. The only API is `getLastKnown` — whatever the
 * OS already has cached. If nothing is cached, the submission
 * proceeds without a GPS field.
 */
object CoarseLocation {

    /**
     * Return the most recent cached coarse location, or `null`
     * if no permission is granted, no provider is enabled, or
     * no location is cached.
     */
    fun getLastKnown(context: Context): GpsInfo? {
        val granted = ContextCompat.checkSelfPermission(
            context, Manifest.permission.ACCESS_COARSE_LOCATION
        ) == PackageManager.PERMISSION_GRANTED
        if (!granted) return null

        val lm = context.getSystemService(Context.LOCATION_SERVICE) as? LocationManager
            ?: return null

        // Try the network provider (coarse) first; fall back to
        // GPS provider's last cached fix if network is empty.
        val candidates = listOfNotNull(
            tryLastKnown(lm, LocationManager.NETWORK_PROVIDER, "network"),
            tryLastKnown(lm, LocationManager.GPS_PROVIDER, "gps"),
        )
        return candidates.maxByOrNull { it.timeMs }?.toGpsInfo()
    }

    private fun tryLastKnown(
        lm: LocationManager,
        provider: String,
        sourceLabel: String,
    ): TimedLocation? {
        if (!lm.isProviderEnabled(provider)) return null
        return try {
            @Suppress("MissingPermission") // Caller checked above.
            val loc = lm.getLastKnownLocation(provider) ?: return null
            TimedLocation(loc, sourceLabel, loc.time)
        } catch (_: SecurityException) {
            null
        }
    }

    private data class TimedLocation(
        val loc: Location,
        val source: String,
        val timeMs: Long,
    ) {
        fun toGpsInfo(): GpsInfo = GpsInfo(
            latDeg = loc.latitude,
            lonDeg = loc.longitude,
            horizontalAccuracyM = if (loc.hasAccuracy()) loc.accuracy.toDouble() else 0.0,
            source = source,
        )
    }
}
