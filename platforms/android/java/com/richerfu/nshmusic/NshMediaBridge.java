package com.richerfu.nshmusic;

import android.app.Activity;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.PackageManager;
import android.media.session.MediaSession;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;

import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.util.Map;

/**
 * Runtime JNI bridge loaded via {@code InMemoryDexClassLoader} by {@code platforms/android/mod.rs}.
 *
 * <p>Compiled from source during Android Cargo builds ({@code platforms/build.rs}) so no binary
 * {@code .dex} artifacts are committed to version control.
 */
public final class NshMediaBridge {
    public static final String ACTION_MEDIA = "com.richerfu.nshmusic.MEDIA_ACTION";
    public static final String EXTRA_CMD = "cmd";

    public static final int CMD_PLAY = 1;
    public static final int CMD_PAUSE = 2;
    public static final int CMD_STOP = 3;
    public static final int CMD_NEXT = 4;
    public static final int CMD_PREV = 5;
    public static final int CMD_SEEK = 6;
    public static final int CMD_FAVORITE = 7;
    public static final int CMD_LOOP = 8;
    public static final int CMD_TOGGLE_PLAY_PAUSE = 9;

    private static final int RECEIVER_NOT_EXPORTED = 4;
    private static final int PERMISSION_REQUEST_CODE = 0x4E53;
    private static final String PERMISSION_POST_NOTIFICATIONS =
            "android.permission.POST_NOTIFICATIONS";

    private NshMediaBridge() {}

    /**
     * Native callback registered explicitly via JNI {@code RegisterNatives} right after
     * {@code InMemoryDexClassLoader.loadClass(...)}.
     */
    public static native void nativeOnAction(int action, long arg);

    /**
     * Attaches an interactive {@link MediaSession.Callback} handling transport controls,
     * lock-screen scrubber seeking, Bluetooth/headset media keys, and custom actions.
     */
    public static MediaSession.Callback attachCallback(MediaSession session) {
        MediaSession.Callback callback = new MediaSession.Callback() {
            @Override
            public void onPlay() {
                nativeOnAction(CMD_PLAY, 0L);
            }

            @Override
            public void onPause() {
                nativeOnAction(CMD_PAUSE, 0L);
            }

            @Override
            public void onStop() {
                nativeOnAction(CMD_STOP, 0L);
            }

            @Override
            public void onSkipToNext() {
                nativeOnAction(CMD_NEXT, 0L);
            }

            @Override
            public void onSkipToPrevious() {
                nativeOnAction(CMD_PREV, 0L);
            }

            @Override
            public void onSeekTo(long pos) {
                nativeOnAction(CMD_SEEK, pos);
            }

            @Override
            public void onCustomAction(String action, Bundle extras) {
                if ("favorite".equals(action)) {
                    nativeOnAction(CMD_FAVORITE, 0L);
                } else if ("loop".equals(action)) {
                    nativeOnAction(CMD_LOOP, 0L);
                }
            }
        };
        session.setCallback(callback, new Handler(Looper.getMainLooper()));
        return callback;
    }

    /**
     * Registers an app-private {@link BroadcastReceiver} for interactive notification action
     * buttons ({@code Previous} / {@code Play|Pause} / {@code Next}).
     */
    public static BroadcastReceiver registerActionReceiver(Context context) {
        BroadcastReceiver receiver = new BroadcastReceiver() {
            @Override
            public void onReceive(Context ctx, Intent intent) {
                if (intent == null || !ACTION_MEDIA.equals(intent.getAction())) {
                    return;
                }
                int cmd = intent.getIntExtra(EXTRA_CMD, 0);
                if (cmd != 0) {
                    nativeOnAction(cmd, 0L);
                }
            }
        };
        IntentFilter filter = new IntentFilter(ACTION_MEDIA);
        if (Build.VERSION.SDK_INT >= 33) {
            context.registerReceiver(receiver, filter, RECEIVER_NOT_EXPORTED);
        } else {
            context.registerReceiver(receiver, filter);
        }
        return receiver;
    }

    /**
     * Requests Android 13+ {@code POST_NOTIFICATIONS} runtime permission on the foreground
     * {@link Activity} UI thread. Returns {@code true} if a permission request was dispatched.
     */
    public static boolean requestPostNotifications(Context context) {
        if (Build.VERSION.SDK_INT < 33 || context == null) {
            return false;
        }
        if (context.checkSelfPermission(PERMISSION_POST_NOTIFICATIONS)
                == PackageManager.PERMISSION_GRANTED) {
            return false;
        }
        final Activity activity = findForegroundActivity();
        if (activity == null || activity.isFinishing()) {
            return false;
        }
        new Handler(Looper.getMainLooper()).post(new Runnable() {
            @Override
            public void run() {
                try {
                    activity.requestPermissions(
                            new String[] { PERMISSION_POST_NOTIFICATIONS },
                            PERMISSION_REQUEST_CODE);
                } catch (Throwable ignored) {
                    // Best-effort runtime prompt; never crash audio playback.
                }
            }
        });
        return true;
    }

    private static Activity findForegroundActivity() {
        try {
            Class<?> atClass = Class.forName("android.app.ActivityThread");
            Method currentAt = atClass.getMethod("currentActivityThread");
            Object activityThread = currentAt.invoke(null);
            if (activityThread == null) {
                return null;
            }
            Field activitiesField = atClass.getDeclaredField("mActivities");
            activitiesField.setAccessible(true);
            Object mapObj = activitiesField.get(activityThread);
            if (!(mapObj instanceof Map<?, ?>)) {
                return null;
            }
            Map<?, ?> activities = (Map<?, ?>) mapObj;
            Activity fallback = null;
            for (Object record : activities.values()) {
                if (record == null) {
                    continue;
                }
                Class<?> recClass = record.getClass();
                Field actField = recClass.getDeclaredField("activity");
                actField.setAccessible(true);
                Object actObj = actField.get(record);
                if (!(actObj instanceof Activity)) {
                    continue;
                }
                Activity act = (Activity) actObj;
                if (act.isFinishing()) {
                    continue;
                }
                Field pausedField = recClass.getDeclaredField("paused");
                pausedField.setAccessible(true);
                if (!pausedField.getBoolean(record)) {
                    return act;
                }
                if (fallback == null) {
                    fallback = act;
                }
            }
            return fallback;
        } catch (Throwable ignored) {
            return null;
        }
    }
}
