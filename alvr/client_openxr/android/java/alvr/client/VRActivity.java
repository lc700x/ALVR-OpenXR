package alvr.client;

import android.app.Activity;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.os.BatteryManager;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.HandlerThread;
import android.util.Log;
import android.view.Surface;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.Window;
import android.view.WindowManager;

import java.util.concurrent.Semaphore;

// Official-style ALVR QiYu VR activity. The QiYu SXR SDK (qiyu_Init -> QVR_Init) must be
// driven from a real Android Activity on a dedicated rendering thread; a NativeActivity's
// android_main thread crashes it. The native methods are implemented in
// libalvr_client_openxr.so (Java_alvr_client_VRActivity_*).
//
// This is a single universal APK. VRActivity is the launcher, but only QiYu devices use the
// SXR path here: on any other device it immediately delegates to android.app.NativeActivity
// (the standard OpenXR path) and finishes, so Quest/Pico/HVR/YVR behaviour is unchanged.
public class VRActivity extends Activity {
    static {
        System.loadLibrary("alvr_client_openxr");
    }

    static final String TAG = "VRActivity";

    static boolean isQiyu() {
        String m = (Build.MANUFACTURER == null ? "" : Build.MANUFACTURER).toUpperCase();
        String mo = (Build.MODEL == null ? "" : Build.MODEL).toUpperCase();
        String d = (Build.DEVICE == null ? "" : Build.DEVICE).toUpperCase();
        String p = (Build.PRODUCT == null ? "" : Build.PRODUCT).toUpperCase();
        return m.contains("IQIYI") || m.contains("QIYU")
                || mo.contains("IQIYI") || mo.contains("QIYU")
                || d.contains("IQIYI") || d.contains("QIYU")
                || p.contains("IQIYI") || p.contains("QIYU");
    }

    // Load a QiYu SXR native library via System.loadLibrary so the Android runtime invokes its
    // JNI_OnLoad. QVR_Init (in libqiyivrsdkcore.so) dereferences globals that are only set up by
    // that library's JNI_OnLoad; loading it with dlopen from native code does NOT trigger
    // JNI_OnLoad, which is why QVR_Init crashed with a null-pointer dereference.
    static void loadQiyuLibrary(String name) {
        try {
            System.loadLibrary(name);
            Log.i(TAG, "loaded QiYu library " + name);
        } catch (Throwable t) {
            Log.e(TAG, "loadLibrary " + name + " failed: " + t);
        }
    }

    class RenderingCallbacks implements SurfaceHolder.Callback {
        @Override
        public void surfaceCreated(SurfaceHolder holder) {
            mScreenSurface = holder.getSurface();
            maybeResume();
        }

        @Override
        public void surfaceChanged(SurfaceHolder holder, int fmt, int w, int h) {
            maybePause();
            mScreenSurface = holder.getSurface();
            maybeResume();
        }

        @Override
        public void surfaceDestroyed(SurfaceHolder holder) {
            maybePause();
            mScreenSurface = null;
        }
    }

    final BroadcastReceiver mBatInfoReceiver = new BroadcastReceiver() {
        @Override
        public void onReceive(Context ctxt, Intent intent) {
            onBatteryChangedNative(intent.getIntExtra(BatteryManager.EXTRA_LEVEL, 0),
                    intent.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0) != 0);
        }
    };

    boolean mResumed = false;
    Handler mRenderingHandler;
    HandlerThread mRenderingHandlerThread;
    Surface mScreenSurface;

    final Runnable mRenderRunnable = this::render;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        // Non-QiYu devices: delegate to the standard NativeActivity (OpenXR) and exit.
        if (!isQiyu()) {
            Log.i(TAG, "Non-QiYu device, delegating to NativeActivity (OpenXR path).");
            Intent intent = new Intent();
            intent.setClassName(getPackageName(), "android.app.NativeActivity");
            intent.addFlags(Intent.FLAG_ACTIVITY_NO_ANIMATION);
            startActivity(intent);
            finish();
            return;
        }

        // Load the QiYu SXR libraries here (QiYu-only) so their JNI_OnLoad runs and initializes
        // the VRSDKCore globals required by QVR_Init. Order matters: the core/base libraries must
        // be loaded before qiyuapi, which depends on them.
        loadQiyuLibrary("qiyivrsdkcore");
        loadQiyuLibrary("sxrapi");
        loadQiyuLibrary("ashreader");
        loadQiyuLibrary("qiyuapi");

        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_FULLSCREEN);
        requestWindowFeature(Window.FEATURE_NO_TITLE);

        // Build a SurfaceView programmatically to avoid depending on generated R resources.
        SurfaceView surfaceView = new SurfaceView(this);
        setContentView(surfaceView);

        mRenderingHandlerThread = new HandlerThread("Rendering thread");
        mRenderingHandlerThread.start();
        mRenderingHandler = new Handler(mRenderingHandlerThread.getLooper());
        mRenderingHandler.post(this::initializeNative);

        SurfaceHolder holder = surfaceView.getHolder();
        holder.addCallback(new RenderingCallbacks());

        registerReceiver(mBatInfoReceiver, new IntentFilter(Intent.ACTION_BATTERY_CHANGED));
    }

    @Override
    protected void onDestroy() {
        super.onDestroy();
        if (mRenderingHandler == null) {
            return; // delegated path, nothing to tear down
        }
        final Semaphore sem = new Semaphore(1);
        try {
            sem.acquire();
        } catch (InterruptedException e) {
            e.printStackTrace();
        }
        mRenderingHandler.post(() -> {
            Log.i(TAG, "Destroying QiYu SXR state.");
            destroyNative();
            sem.release();
        });
        mRenderingHandlerThread.quitSafely();
        try {
            sem.acquire();
            sem.release();
        } catch (InterruptedException e) {
            e.printStackTrace();
        }
    }

    @Override
    protected void onResume() {
        super.onResume();
        if (mRenderingHandler == null) {
            return; // delegated path
        }
        mResumed = true;
        maybeResume();
    }

    void maybeResume() {
        if (mResumed && mScreenSurface != null) {
            mRenderingHandler.post(() -> {
                onResumeNative(mScreenSurface);
                mRenderingHandler.post(mRenderRunnable);
            });
        }
    }

    @Override
    protected void onPause() {
        if (mRenderingHandler != null) {
            maybePause();
            mResumed = false;
        }
        super.onPause();
    }

    void maybePause() {
        if (mResumed && mScreenSurface != null) {
            mRenderingHandler.post(this::onPauseNative);
        }
    }

    private void render() {
        if (mResumed && mScreenSurface != null) {
            renderNative();
            mRenderingHandler.removeCallbacks(mRenderRunnable);
            mRenderingHandler.postDelayed(mRenderRunnable, 2);
        }
    }

    native void initializeNative();

    native void destroyNative();

    native void onResumeNative(Surface screenSurface);

    native void onPauseNative();

    native void onStreamStartNative();

    native void onStreamStopNative();

    native void renderNative();

    native void onBatteryChangedNative(int battery, boolean plugged);

    @SuppressWarnings("unused")
    public void onStreamStart() {
        mRenderingHandler.post(this::onStreamStartNative);
    }

    @SuppressWarnings("unused")
    public void onStreamStop() {
        mRenderingHandler.post(this::onStreamStopNative);
    }
}
