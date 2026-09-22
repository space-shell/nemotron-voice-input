package dev.jamesnicholls.nemotronvoice;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.os.Build;
import android.os.IBinder;
import android.util.Log;

/**
 * Microphone-typed foreground service that runs in the {@code :ime} process
 * for the lifetime of a dictation. Background recording is a default-on
 * feature: when the keyboard hides mid-recording, the process would
 * otherwise drop to cached, where OEM app freezers (Samsung Freecess) freeze
 * it mid-inference and Android 14+ may silence background microphone
 * capture. A foreground service with type {@code microphone} keeps the
 * process at FGS level — freeze-ineligible and silencing-exempt — and puts a
 * visible, honest indicator in the drawer while the mic is hot (#8).
 *
 * <p>Started when a recording begins (while the IME window is still visible,
 * which is a permitted state for mic-FGS start) and stopped wherever the
 * recording ends. The notification needs no POST_NOTIFICATIONS permission:
 * foreground-service notifications are shown regardless.
 */
public class MicForegroundService extends Service {

    private static final String TAG = "OfflineVoiceInput";
    private static final String CHANNEL_ID = "voice_input_mic";
    private static final int NOTIFICATION_ID = 1;

    @Override
    public void onCreate() {
        super.onCreate();
        NotificationManager nm = getSystemService(NotificationManager.class);
        if (nm != null && nm.getNotificationChannel(CHANNEL_ID) == null) {
            // Low importance: silent, no vibration — the mic is already
            // indicated by the system; this is the honesty channel, not an
            // alert.
            NotificationChannel channel = new NotificationChannel(CHANNEL_ID,
                    "Voice input", NotificationManager.IMPORTANCE_LOW);
            channel.setDescription("Shown while a voice recording is in progress");
            nm.createNotificationChannel(channel);
        }
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        Notification notification = new Notification.Builder(this, CHANNEL_ID)
                .setSmallIcon(R.drawable.icon)
                .setContentTitle("Voice input is active")
                .setContentText("Microphone is in use for dictation")
                .setOngoing(true)
                .setContentIntent(PendingIntent.getActivity(this, 0,
                        new Intent(this, MainActivity.class),
                        PendingIntent.FLAG_IMMUTABLE))
                .build();
        try {
            if (Build.VERSION.SDK_INT >= 30) {
                // The typed overload asserts the microphone type explicitly;
                // on 29 the manifest type applies on its own, and below 29
                // types don't exist.
                startForeground(NOTIFICATION_ID, notification,
                        ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE);
            } else {
                startForeground(NOTIFICATION_ID, notification);
            }
        } catch (Throwable t) {
            // Android 14+ denies microphone-type foreground promotion when
            // the app is not in a while-in-use state — e.g. auto-record
            // firing during the keyboard show transition, before the system
            // registers the IME window as visible (observed crash-looping
            // the :ime process on an S23: SecurityException out of
            // startForeground, service restart, repeat). NEVER let this
            // kill the process: degrade to running the recording without
            // FGS protection — the freeze resilience carries it — and
            // retry the promotion from onWindowShown once the window is
            // genuinely visible.
            Log.w(TAG, "mic foreground promotion denied; recording continues unprotected", t);
            stopSelf();
            return START_NOT_STICKY;
        }
        // Not sticky: if the process dies there is no recording left to
        // resume — the user simply taps record again.
        return START_NOT_STICKY;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    /** Promote to a mic foreground service for the recording's lifetime. */
    public static void start(Context context) {
        try {
            context.startForegroundService(
                    new Intent(context, MicForegroundService.class));
        } catch (Throwable t) {
            // e.g. ForegroundServiceStartNotAllowedException if the state
            // changed under us — recording still works, it is just not
            // freeze/silencing-exempt this time.
            Log.w(TAG, "mic foreground service start failed", t);
        }
    }

    /** Demote once the recording has ended for any reason. */
    public static void stop(Context context) {
        try {
            context.stopService(new Intent(context, MicForegroundService.class));
        } catch (Throwable t) {
            Log.w(TAG, "mic foreground service stop failed", t);
        }
    }
}
