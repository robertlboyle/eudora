package net.planet.eudora27

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper

/**
 * Keeps Eudora awake while you sign in in the browser. Microsoft's and Google's sign-in pages
 * send you back to Eudora on this phone (http://localhost), but Android freezes an app left in
 * the background: tried on Android 16, the answer reached Eudora 11 minutes late. A foreground
 * service (its notification says what it's for) keeps it answering. Started and stopped from Rust
 * (src/android.rs); it stops itself after 10 minutes whatever happens.
 */
class SignInService : Service() {
    private val handler = Handler(Looper.getMainLooper())

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val manager = getSystemService(NotificationManager::class.java)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            manager.createNotificationChannel(NotificationChannel(CHANNEL, "Signing in", NotificationManager.IMPORTANCE_LOW))
        }
        val builder = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) Notification.Builder(this, CHANNEL) else Notification.Builder(this)
        val notification = builder
            .setSmallIcon(R.mipmap.ic_launcher)
            .setContentTitle("Signing in")
            .setContentText("Finish signing in in your browser; Eudora is waiting for it.")
            .setOngoing(true)
            .build()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        } else {
            startForeground(ID, notification)
        }
        handler.removeCallbacksAndMessages(null)
        handler.postDelayed({ stopSelf() }, 10 * 60 * 1000L)
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        handler.removeCallbacksAndMessages(null)
        super.onDestroy()
    }

    companion object {
        private const val CHANNEL = "eudora-sign-in"
        private const val ID = 27
    }
}
