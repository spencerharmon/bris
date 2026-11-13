package io.github.spencerharmon.bris.upload

import android.content.Context
import androidx.datastore.preferences.preferencesDataStore
import androidx.work.BackoffPolicy
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import io.github.spencerharmon.bris.Prefs
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import java.io.File
import java.util.concurrent.TimeUnit

private val Context.collectorStore by preferencesDataStore(name = "bris_collector")

/** App-wide wiring for the collector upload path. */
object Upload {
    private val initialization = Mutex()
    private var shared: UploadCoordinator? = null

    fun settings(context: Context) = CollectorSettingsStore(context.applicationContext.collectorStore)

    suspend fun coordinator(context: Context): UploadCoordinator = initialization.withLock {
        shared?.let { return@withLock it }
        val app = context.applicationContext
        val prefs = Prefs(app)
        val uuid = prefs.deviceUuid()
        withContext(Dispatchers.IO) {
            UploadCoordinator(
                FfiSubmitterPort(FfiSubmitterPort.queueRoot(app), uuid),
                settings(app),
                scheduleFlush = { scheduleFlush(app) },
                deviceUuid = { uuid },
            ).also {
                it.setDebugMode(prefs.debugModeFlow.first())
                shared = it
                if (it.counts().first > 0) scheduleFlush(app)
            }
        }
    }

    suspend fun saveSettings(context: Context, baseUrl: String, token: String) =
        withContext(Dispatchers.IO) { coordinator(context).saveSettings(baseUrl, token) }

    suspend fun register(context: Context): RegistrationResult = coordinator(context).register()

    suspend fun setAutomaticUpload(context: Context, enabled: Boolean) =
        withContext(Dispatchers.IO) { coordinator(context).setAutomaticUpload(enabled) }

    suspend fun setDebugMode(context: Context, enabled: Boolean) = withContext(Dispatchers.IO) {
        coordinator(context).setDebugMode(enabled, Prefs(context.applicationContext)::persistDebugMode)
    }

    /** Persist before completion returns; never a dialog, toast, or capture error. */
    suspend fun captureCompleted(context: Context, bundleDir: File) = captureCompleted(context, bundleDir.absolutePath)

    suspend fun captureCompleted(context: Context, bundleDir: String) = withContext(Dispatchers.IO) {
        try {
            coordinator(context).captureCompleted(bundleDir)
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            settings(context).update {
                it.copy(lastError = "automatic upload unavailable (${e.javaClass.simpleName})")
            }
        }
    }

    /** Schedule a network-constrained, retrying background flush. */
    fun scheduleFlush(context: Context) {
        val req = OneTimeWorkRequestBuilder<FlushWorker>()
            .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
            .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, 30, TimeUnit.SECONDS)
            .build()
        WorkManager.getInstance(context.applicationContext)
            // Append closes the enqueue-at-drain-completion race without cancelling
            // a blocking native call. Failed/cancelled chains start a fresh leaf.
            .enqueueUniqueWork(FlushWorker.NAME, ExistingWorkPolicy.APPEND_OR_REPLACE, req)
    }
}

/** Background flush of the durable submission queue. */
class FlushWorker(ctx: Context, params: WorkerParameters) : CoroutineWorker(ctx, params) {
    override suspend fun doWork(): Result = try {
        val summary = Upload.coordinator(applicationContext).flush()
        if (summary.needsRetry) Result.retry() else Result.success()
    } catch (e: CancellationException) {
        throw e
    } catch (e: Exception) {
        Upload.settings(applicationContext).update {
            it.copy(lastError = "upload worker failed (${e.javaClass.simpleName}); will retry")
        }
        Result.retry()
    }

    companion object {
        const val NAME = "bris-collector-flush"
    }
}
