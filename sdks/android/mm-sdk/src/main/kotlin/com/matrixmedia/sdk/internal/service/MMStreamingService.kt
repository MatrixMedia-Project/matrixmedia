package com.matrixmedia.sdk.internal.service

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat

/**
 * Foreground service that keeps the audio stream alive when the app is backgrounded.
 *
 * ## Why This Is Required
 *
 * Android 8+ (API 26) aggressively kills background processes. Without a foreground
 * service, the audio stream disconnects within seconds of the app going to the
 * background. This was a critical missing piece in PTT v1.
 *
 * ## Permissions Required
 *
 * - `FOREGROUND_SERVICE` (declared in manifest, auto-granted)
 * - `FOREGROUND_SERVICE_MEDIA_PLAYBACK` (API 34+, declared in manifest)
 * - `FOREGROUND_SERVICE_MICROPHONE` (API 34+, declared in manifest)
 * - `POST_NOTIFICATIONS` (API 33+, must be requested at runtime by the host app)
 *
 * ## Foreground Service Type
 *
 * On API 34+ (Android 14), the service declares both `mediaPlayback` and
 * `microphone` foreground service types:
 * - `mediaPlayback` for viewers (receiving audio)
 * - `microphone` for hosts (capturing and transmitting audio)
 *
 * ## Usage
 *
 * The SDK starts this service automatically when a stream begins and stops it
 * when the stream ends. Host apps do not need to interact with this service directly.
 *
 * ```kotlin
 * // Start (called internally by MMClient)
 * val intent = Intent(context, MMStreamingService::class.java).apply {
 *     action = ACTION_START
 *     putExtra(EXTRA_STREAM_TITLE, "My Stream")
 * }
 * ContextCompat.startForegroundService(context, intent)
 *
 * // Stop (called internally by MMStream.leave/stop)
 * val stopIntent = Intent(context, MMStreamingService::class.java).apply {
 *     action = ACTION_STOP
 * }
 * context.startService(stopIntent)
 * ```
 */
class MMStreamingService : Service() {

    companion object {
        const val CHANNEL_ID = "matrixmedia_streaming"
        const val CHANNEL_NAME = "MatrixMedia Streaming"
        const val NOTIFICATION_ID = 1001

        const val ACTION_START = "com.matrixmedia.sdk.START_STREAMING"
        const val ACTION_STOP = "com.matrixmedia.sdk.STOP_STREAMING"
        const val EXTRA_STREAM_TITLE = "stream_title"
    }

    override fun onCreate() {
        super.onCreate()
        createNotificationChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_START -> {
                val title = intent.getStringExtra(EXTRA_STREAM_TITLE) ?: "MatrixMedia Stream"
                val notification = buildNotification(title)

                if (Build.VERSION.SDK_INT >= 34) {
                    startForeground(
                        NOTIFICATION_ID,
                        notification,
                        ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK or
                            ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
                    )
                } else {
                    startForeground(NOTIFICATION_ID, notification)
                }
            }

            ACTION_STOP -> {
                stopForeground(STOP_FOREGROUND_REMOVE)
                stopSelf()
            }
        }

        return START_NOT_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                CHANNEL_ID,
                CHANNEL_NAME,
                NotificationManager.IMPORTANCE_LOW
            ).apply {
                description = "Shows when MatrixMedia is streaming audio"
                setShowBadge(false)
            }

            val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            manager.createNotificationChannel(channel)
        }
    }

    private fun buildNotification(title: String): Notification {
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle(title)
            .setContentText("Streaming in progress")
            .setSmallIcon(android.R.drawable.ic_media_play)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setOngoing(true)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .build()
    }
}
