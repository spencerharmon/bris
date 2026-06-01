package io.github.spencerharmon.bris.engine

import android.content.Context
import android.hardware.Sensor
import android.hardware.SensorEvent
import android.hardware.SensorEventListener
import android.hardware.SensorManager
import android.view.Surface
import kotlin.math.atan2
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow

/**
 * Derives a discrete 0/90/180/270 rotation hint from the
 * accelerometer's gravity reading.
 *
 * Why this instead of `display.rotation`:
 * `DisplayManager.DisplayListener` only fires when the
 * *display* rotates. With the system-wide rotate-lock toggle
 * on, `display.rotation` is pinned to whatever orientation
 * the lock froze, even if the operator physically rotates
 * the device. CameraX's `ImageAnalysis.setTargetRotation`
 * driven from `display.rotation` therefore stays stuck and
 * the analyzer receives sensor-orientation frames whose
 * gravity points sideways. On-device verification 2026-06-01
 * (Cat S62 Pro) showed 42 of 74 frames had sideways gravity
 * for exactly this reason.
 *
 * The accelerometer is the physical-orientation source of
 * truth. This class exposes the derived rotation as a
 * [StateFlow]; the camera-bind code feeds it to
 * `setTargetRotation` directly.
 *
 * Hysteresis: rotation changes only when the dominant
 * gravity axis dwells in the new orientation for
 * [debounceMs]. Avoids ping-ponging when the device is
 * tilted near 45°.
 *
 * Pure-JVM helpers ([orientationFromGravity], [hysteresis])
 * are testable without instantiating the SensorManager.
 */
class DeviceOrientationSource(
    private val sensorManager: SensorManager,
    private val debounceMs: Long = 250L,
) : SensorEventListener {

    private val accel: Sensor? = sensorManager.getDefaultSensor(Sensor.TYPE_ACCELEROMETER)
    private val _rotation = MutableStateFlow(Surface.ROTATION_0)
    val rotation: StateFlow<Int> = _rotation

    private var pendingRotation: Int = Surface.ROTATION_0
    private var pendingSinceMs: Long = 0L

    /** Start sampling. Safe to call multiple times. */
    fun start() {
        val s = accel ?: return
        sensorManager.registerListener(this, s, SensorManager.SENSOR_DELAY_NORMAL)
    }

    /** Stop sampling. */
    fun stop() {
        sensorManager.unregisterListener(this)
    }

    override fun onSensorChanged(event: SensorEvent) {
        if (event.sensor.type != Sensor.TYPE_ACCELEROMETER) return
        val proposed = orientationFromGravity(
            ax = event.values[0],
            ay = event.values[1],
            az = event.values[2],
        ) ?: return
        val nowMs = System.currentTimeMillis()
        _rotation.value = hysteresis(
            current = _rotation.value,
            proposed = proposed,
            pending = pendingRotation,
            pendingSinceMs = pendingSinceMs,
            nowMs = nowMs,
            debounceMs = debounceMs,
            onPendingChanged = { newPending, atMs ->
                pendingRotation = newPending
                pendingSinceMs = atMs
            },
        )
    }

    override fun onAccuracyChanged(sensor: Sensor?, accuracy: Int) = Unit

    companion object {
        /** Mount over a Context. */
        fun forContext(context: Context): DeviceOrientationSource {
            val sm = context.getSystemService(Context.SENSOR_SERVICE) as SensorManager
            return DeviceOrientationSource(sm)
        }

        /**
         * Map a gravity vector (m/s², device frame) to a
         * [Surface] rotation constant.
         *
         * Returns null when the device is nearly face-up or
         * face-down (|gz| dominates), where there is no
         * meaningful azimuthal orientation.
         *
         * Convention: the returned value is the rotation the
         * surface (= analyzer) needs to land at gravity-down.
         * Matches `ImageAnalysis.setTargetRotation` semantics
         * on Android.
         */
        fun orientationFromGravity(ax: Float, ay: Float, az: Float): Int? {
            // Face-up/face-down: gravity dominantly on z.
            // Use a 2x guard (|gz| > 2 * max(|gx|,|gy|)) so we
            // don't flap when the operator is roughly
            // horizontal.
            val ah = kotlin.math.max(kotlin.math.abs(ax), kotlin.math.abs(ay))
            if (kotlin.math.abs(az) > 2f * ah) return null

            // Angle of the gravity vector in the device XY
            // plane, measured CCW from +X axis.
            val angleRad = atan2(ay.toDouble(), ax.toDouble())
            val angleDeg = Math.toDegrees(angleRad)
            // Quantize to nearest 90°, mapped to Surface
            // rotation enums.
            // Phone held portrait, top-up: gravity along -Y,
            // angle ≈ -90°  → ROTATION_0.
            // Phone landscape, USB-C right: gravity along -X,
            // angle ≈ ±180° → ROTATION_90.
            // Phone upside-down portrait: gravity along +Y,
            // angle ≈ +90°  → ROTATION_180.
            // Phone landscape, USB-C left: gravity along +X,
            // angle ≈ 0°    → ROTATION_270.
            return when {
                angleDeg in -135.0..-45.0 -> Surface.ROTATION_0
                angleDeg < -135.0 || angleDeg > 135.0 -> Surface.ROTATION_90
                angleDeg in 45.0..135.0 -> Surface.ROTATION_180
                else -> Surface.ROTATION_270
            }
        }

        /**
         * Debounce a proposed rotation: change only when
         * `proposed` has dwelled (matched `pending`) for
         * `debounceMs`. Pure function; on-change callback
         * updates the caller's bookkeeping.
         */
        fun hysteresis(
            current: Int,
            proposed: Int,
            pending: Int,
            pendingSinceMs: Long,
            nowMs: Long,
            debounceMs: Long,
            onPendingChanged: (newPending: Int, atMs: Long) -> Unit,
        ): Int {
            if (proposed == current) {
                // Reset pending: back to current rotation
                // before any new one had time to commit.
                if (pending != current) onPendingChanged(current, nowMs)
                return current
            }
            if (proposed != pending) {
                // New proposal differs from the last pending
                // candidate; restart the dwell timer.
                onPendingChanged(proposed, nowMs)
                return current
            }
            // proposed == pending, != current; check dwell.
            return if (nowMs - pendingSinceMs >= debounceMs) {
                proposed
            } else {
                current
            }
        }
    }
}
