package net.planet.eudora27

import android.content.Context
import androidx.work.Constraints
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.NetworkType
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.Worker
import androidx.work.WorkerParameters
import java.util.concurrent.TimeUnit

/** Eudora's Rust code (src/android.rs in the eudora-app crate). */
object EudoraNative {
    init {
        System.loadLibrary("eudora_app_lib")
    }

    /** One pass over every account: 0 done, 1 the app is open (it downloads itself), 2 failed. */
    @JvmStatic
    external fun checkOnce(context: Context, dataDir: String): Int
}

/**
 * Checks for new mail while Eudora is closed: Android runs it about every 15 minutes when
 * there's a network (polling, as decided 2026-10-27; WorkManager chooses the moment to spare
 * the battery). When Eudora is open, its own download is already checking.
 */
class CheckWorker(context: Context, params: WorkerParameters) : Worker(context, params) {
    override fun doWork(): Result = try {
        when (EudoraNative.checkOnce(applicationContext, applicationContext.dataDir.absolutePath)) {
            2 -> Result.retry()
            else -> Result.success()
        }
    } catch (e: Throwable) {
        Result.retry()
    }

    companion object {
        private const val NAME = "eudora-check-mail"

        /** Asks Android for the periodic check (kept if it's already asked for). */
        fun schedule(context: Context) {
            val request = PeriodicWorkRequestBuilder<CheckWorker>(15, TimeUnit.MINUTES)
                .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
                .build()
            WorkManager.getInstance(context).enqueueUniquePeriodicWork(NAME, ExistingPeriodicWorkPolicy.KEEP, request)
        }
    }
}
