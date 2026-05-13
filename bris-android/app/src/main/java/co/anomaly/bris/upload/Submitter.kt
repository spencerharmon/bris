package co.anomaly.bris.upload

import okhttp3.MediaType.Companion.toMediaTypeOrNull
import okhttp3.MultipartBody
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import java.util.concurrent.TimeUnit

/**
 * One file part of a multipart submission.
 *
 * @property filename Name referenced from the manifest's `media`
 *                    array. Also the part name (UniFFI side
 *                    enforces uniqueness).
 * @property contentType MIME type — `image/png`, `image/jpeg`,
 *                       `text/plain`, `application/toml`, etc.
 * @property bytes Raw file bytes.
 */
data class MediaPart(
    val filename: String,
    val contentType: String,
    val bytes: ByteArray,
)

/**
 * Result of a submission attempt.
 */
sealed interface SubmitResult {
    /** Submission accepted; `id` is the server-assigned ULID. */
    data class Accepted(val id: String) : SubmitResult
    /** Server rejected with a 4xx; `detail` is the error body. */
    data class Rejected(val statusCode: Int, val detail: String) : SubmitResult
    /** Network or 5xx; retryable. */
    data class TransientFailure(val message: String) : SubmitResult
}

/**
 * HTTP client for diagnostic submissions to the collector.
 *
 * The collector's contract is documented in
 * `docs/design/diagnostic_collection.md`: one
 * `multipart/form-data` POST to `{base}/v1/submissions` with a
 * `manifest` part (JSON) and one part per media file (part name
 * = filename = `media[i].filename` in the manifest).
 *
 * Spike-grade: synchronous, no retry, no offline queue. The
 * production version will run in WorkManager with exponential
 * backoff and persist queued submissions across app restarts.
 */
class Submitter(
    private val baseUrl: String,
    private val bearerToken: String,
) {
    private val client = OkHttpClient.Builder()
        .connectTimeout(30, TimeUnit.SECONDS)
        .writeTimeout(5, TimeUnit.MINUTES)
        .readTimeout(2, TimeUnit.MINUTES)
        .build()

    /**
     * POST one submission.
     *
     * @param manifestJson Serialized manifest matching
     *                     `bris_collector::manifest::Manifest`.
     * @param media Media parts referenced by the manifest's
     *              `media` array.
     */
    fun submit(manifestJson: String, media: List<MediaPart>): SubmitResult {
        val body = MultipartBody.Builder()
            .setType(MultipartBody.FORM)
            .addFormDataPart(
                "manifest",
                "manifest.json",
                manifestJson.toRequestBody("application/json".toMediaTypeOrNull()),
            )
        for (part in media) {
            body.addFormDataPart(
                part.filename,
                part.filename,
                part.bytes.toRequestBody(part.contentType.toMediaTypeOrNull()),
            )
        }
        val request = Request.Builder()
            .url("${baseUrl.trimEnd('/')}/v1/submissions")
            .addHeader("Authorization", "Bearer $bearerToken")
            .post(body.build())
            .build()
        return try {
            client.newCall(request).execute().use { resp ->
                when {
                    resp.isSuccessful -> {
                        // Body is `{"id": "..."}`; parse minimally
                        // to avoid pulling in a JSON library here.
                        val s = resp.body?.string().orEmpty()
                        val id = Regex("\"id\"\\s*:\\s*\"([^\"]+)\"").find(s)?.groupValues?.get(1)
                            ?: ""
                        SubmitResult.Accepted(id)
                    }
                    resp.code in 400..499 -> SubmitResult.Rejected(
                        resp.code,
                        resp.body?.string().orEmpty(),
                    )
                    else -> SubmitResult.TransientFailure(
                        "http ${resp.code}: ${resp.body?.string().orEmpty().take(200)}",
                    )
                }
            }
        } catch (e: Exception) {
            SubmitResult.TransientFailure(e.message ?: e.toString())
        }
    }
}
