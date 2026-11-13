package io.github.spencerharmon.bris.upload

import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONObject
import java.io.IOException

/** Result of a per-device registration attempt. */
sealed interface RegistrationResult {
    /** Collector issued a per-device token. */
    data class Registered(val deviceToken: String) : RegistrationResult {
        override fun toString() = "Registered(deviceToken=${maskToken(deviceToken)})"
    }

    /**
     * Collector refused the entered token for registration
     * (401/403): it is not an enrollment/admin token. The app
     * keeps using the entered token as-is (it may already be a
     * per-device token).
     */
    data class NotEnrollmentToken(val httpStatus: Int) : RegistrationResult

    /** Network / server failure; retry later. */
    data class Failed(val message: String) : RegistrationResult
}

/**
 * Client for the collector's `POST /v1/devices/register`
 * (collector-per-device-tokens). Called once with the
 * operator-entered enrollment token; the issued per-device token
 * is then stored and used for every submission.
 */
class DeviceRegistrar(private val client: OkHttpClient = OkHttpClient()) {

    fun register(baseUrl: String, enrollmentToken: String, deviceUuid: String): RegistrationResult {
        val body = JSONObject().put("device_uuid", deviceUuid).toString()
            .toRequestBody("application/json".toMediaType())
        val req = Request.Builder()
            .url(baseUrl.trimEnd('/') + "/v1/devices/register")
            .header("Authorization", "Bearer $enrollmentToken")
            .post(body)
            .build()
        return try {
            client.newCall(req).execute().use { resp ->
                when {
                    resp.code == 401 || resp.code == 403 -> RegistrationResult.NotEnrollmentToken(resp.code)
                    !resp.isSuccessful -> RegistrationResult.Failed("HTTP ${resp.code}")
                    else -> {
                        val json = JSONObject(resp.body?.string() ?: "")
                        val token = json.optString("token", "")
                        if (token.isBlank()) RegistrationResult.Failed("register response missing token")
                        else RegistrationResult.Registered(token)
                    }
                }
            }
        } catch (e: IOException) {
            RegistrationResult.Failed("${e.javaClass.simpleName}: ${e.message}")
        } catch (e: org.json.JSONException) {
            RegistrationResult.Failed("bad register response: ${e.message}")
        }
    }
}
