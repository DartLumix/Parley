package dev.parley.app

import android.Manifest
import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.graphics.ImageFormat
import android.graphics.PixelFormat
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.Image
import android.media.ImageReader
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.util.DisplayMetrics
import android.util.Log
import android.util.Range
import android.util.Size
import android.view.Display
import android.view.Surface
import android.view.WindowManager
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import java.nio.ByteBuffer
import kotlin.math.min

/**
 * Android's capture foreground service.
 *
 * MediaProjection consent belongs to an Activity, but the capture must
 * outlive the Activity to survive a screen-off or a trip to the home
 * screen. MainActivity asks for consent and parks the result here; this
 * service starts foreground with the mediaProjection (or camera) type
 * before it calls getMediaProjection, which is required on Android 14+.
 *
 * Frames go to the Rust `parley_client::screen` encoder through JNI. The
 * service never encodes or sends anything itself; it only produces RGBA
 * frames with a row stride and a terminal event when the platform source
 * stops or fails.
 */
class ShareService : Service() {
    private val tag = "ParleyShare"

    private var thread: HandlerThread? = null
    private var handler: Handler? = null
    private var displayManager: DisplayManager? = null

    private var projection: MediaProjection? = null
    private var virtualDisplay: VirtualDisplay? = null
    private var screenReader: ImageReader? = null
    private var screenSize: Size? = null
    private var screenDensityDpi = 0

    private var cameraManager: CameraManager? = null
    private var cameraDevice: CameraDevice? = null
    private var cameraSession: CameraCaptureSession? = null
    private var cameraReader: ImageReader? = null
    private var cameraSensorOrientation = 0
    private var cameraLensFacing = CameraCharacteristics.LENS_FACING_BACK
    private var cameraRotation = 0
    private var cameraMirror = false
    private var cameraAfMode: Int? = null
    private var cameraFpsRange: Range<Int>? = null
    private var cameraReceivedFrame = false
    private var cameraWatchdog: Runnable? = null
    private var cameraGeneration = 0

    private var active = false
    private var cameraMode = false
    private var stopping = false

    private val displayListener = object : DisplayManager.DisplayListener {
        override fun onDisplayAdded(displayId: Int) {}

        override fun onDisplayRemoved(displayId: Int) {}

        override fun onDisplayChanged(displayId: Int) {
            if (displayId != Display.DEFAULT_DISPLAY) return
            handler?.post {
                if (!active || stopping) return@post
                if (cameraMode) updateCameraRotation() else resizeScreen()
            }
        }
    }

    override fun onCreate() {
        super.onCreate()
        createChannel()
        thread = HandlerThread("parley-share").also { it.start() }
        handler = Handler(thread!!.looper)
        displayManager = getSystemService(DisplayManager::class.java).also {
            it.registerDisplayListener(displayListener, handler)
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            endCapture("stop requested from the notification")
            stopSelf()
            return START_NOT_STICKY
        }
        if (intent?.action == ACTION_SWITCH_CAMERA) {
            if (active && cameraMode && !stopping) {
                handler?.post { switchCamera() }
            }
            return START_NOT_STICKY
        }
        if (active) return START_NOT_STICKY

        cameraMode = intent?.getBooleanExtra(EXTRA_CAMERA, false) ?: false
        if (cameraMode && !cameraPermissionGranted()) {
            fail("camera permission was not granted")
            return START_NOT_STICKY
        }
        try {
            startForegroundNow()
        } catch (e: Exception) {
            fail("could not start the share foreground service: ${e.message}")
            return START_NOT_STICKY
        }
        active = true
        try {
            if (cameraMode) startCamera() else startScreen()
        } catch (e: Exception) {
            fail("could not start the capture: ${e.message}")
        }
        return START_NOT_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onTaskRemoved(rootIntent: Intent?) {
        if (active) endCapture("the Parley task was removed")
        stopSelf()
        super.onTaskRemoved(rootIntent)
    }

    override fun onDestroy() {
        stopCapture()
        displayManager?.unregisterDisplayListener(displayListener)
        displayManager = null
        thread?.quitSafely()
        thread = null
        handler = null
        super.onDestroy()
    }

    private fun startForegroundNow() {
        val type = if (cameraMode) {
            ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA
        } else {
            ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIFICATION_ID, buildNotification(), type)
        } else {
            startForeground(NOTIFICATION_ID, buildNotification())
        }
    }

    private fun createChannel() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(
                CHANNEL,
                "Screen and camera sharing",
                NotificationManager.IMPORTANCE_LOW,
            ).apply {
                description = "Keeps a Parley share alive while the app is in the background"
                setShowBadge(false)
            },
        )
    }

    private fun buildNotification(): Notification {
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java).addFlags(
                Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP,
            ),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val stop = PendingIntent.getService(
            this,
            1,
            Intent(this, ShareService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val switch = PendingIntent.getService(
            this,
            2,
            Intent(this, ShareService::class.java).setAction(ACTION_SWITCH_CAMERA),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val builder = NotificationCompat.Builder(this, CHANNEL)
            .setSmallIcon(android.R.drawable.ic_menu_share)
            .setContentTitle(if (cameraMode) "Camera share" else "Screen share")
            .setContentText("Parley is capturing your ${if (cameraMode) "camera" else "screen"}")
            .setContentIntent(open)
            .addAction(
                android.R.drawable.ic_menu_close_clear_cancel,
                "Stop sharing",
                stop,
            )
        if (cameraMode) {
            builder.addAction(
                android.R.drawable.ic_menu_camera,
                "Switch camera",
                switch,
            )
        }
        return builder
            .setOngoing(true)
            .setSilent(true)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .build()
    }

    // --- MediaProjection -------------------------------------------------

    private fun startScreen() {
        val permission = takeScreenCapturePermission()
        if (permission == null) {
            fail("screen capture permission was not granted")
            return
        }
        val manager = getSystemService(MediaProjectionManager::class.java)
        val mediaProjection = try {
            manager.getMediaProjection(permission.resultCode, permission.data)
        } catch (e: Exception) {
            fail("could not start screen capture: ${e.message}")
            return
        }
        if (mediaProjection == null) {
            fail("Android returned no screen capture projection")
            return
        }
        projection = mediaProjection
        // Registering the callback before createVirtualDisplay is required
        // on Android 14; it also tells us when the user stops the share
        // from the system privacy indicator.
        mediaProjection.registerCallback(
            object : MediaProjection.Callback() {
                override fun onStop() {
                    endCapture("screen capture stopped by Android")
                }
            },
            handler,
        )

        screenDensityDpi = resources.displayMetrics.densityDpi
        val size = captureSize(displaySize())
        if (size == null) {
            fail("could not determine the screen size")
            return
        }
        screenSize = size
        val reader = newScreenReader(size)
        val display = mediaProjection.createVirtualDisplay(
            "parley-screen",
            size.width,
            size.height,
            screenDensityDpi,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
            reader.surface,
            null,
            handler,
        )
        if (display == null) {
            reader.close()
            fail("Android returned no virtual display")
            return
        }
        virtualDisplay = display
        Log.i(tag, "screen capture started at ${size.width}x${size.height}")
    }

    private fun newScreenReader(size: Size): ImageReader {
        val reader = ImageReader.newInstance(
            size.width,
            size.height,
            PixelFormat.RGBA_8888,
            2,
        )
        reader.setOnImageAvailableListener({ r -> pushScreenFrame(r) }, handler)
        return reader
    }

    private fun pushScreenFrame(reader: ImageReader) {
        val image = try {
            reader.acquireLatestImage()
        } catch (e: Exception) {
            Log.e(tag, "screen acquire failed", e)
            return
        } ?: return
        try {
            if (!active || stopping) return
            val plane = image.planes[0]
            val buffer = plane.buffer
            if (buffer.isDirect) {
                nativePushFrame(image.width, image.height, plane.rowStride, buffer)
            } else {
                val copy = ByteBuffer.allocateDirect(buffer.remaining())
                copy.put(buffer)
                copy.flip()
                nativePushFrame(image.width, image.height, plane.rowStride, copy)
            }
        } catch (e: Exception) {
            Log.e(tag, "screen frame failed", e)
        } finally {
            image.close()
        }
    }

    /**
     * A rotation changes the display's dimensions. Keep the virtual display
     * and its ImageReader in step so a portrait share becomes a landscape
     * one instead of being stretched or letterboxed for the rest of the
     * call.
     */
    private fun resizeScreen() {
        val oldSize = screenSize ?: return
        val size = captureSize(displaySize()) ?: return
        if (size == oldSize) return
        val display = virtualDisplay ?: return
        val reader = newScreenReader(size)
        try {
            display.resize(size.width, size.height, screenDensityDpi)
            display.surface = reader.surface
            val oldReader = screenReader
            screenReader = reader
            screenSize = size
            oldReader?.close()
            Log.i(tag, "screen capture resized to ${size.width}x${size.height}")
        } catch (e: Exception) {
            reader.close()
            Log.e(tag, "screen resize failed", e)
        }
    }

    // --- Camera2 ---------------------------------------------------------

    private fun startCamera() {
        if (!cameraPermissionGranted()) {
            fail("camera permission was not granted")
            return
        }
        val manager = getSystemService(CameraManager::class.java)
        cameraManager = manager
        val id = pickCamera(manager, CameraCharacteristics.LENS_FACING_BACK)
        if (id == null) {
            fail("no camera found")
            return
        }
        openCamera(id)
    }

    /// Flip the running camera without ending the share. The old device is
    /// closed before the new one opens; a late callback from it is ignored
    /// by the generation check below.
    private fun switchCamera() {
        if (!active || !cameraMode || stopping) return
        val manager = cameraManager ?: return
        val wanted = if (cameraLensFacing == CameraCharacteristics.LENS_FACING_FRONT) {
            CameraCharacteristics.LENS_FACING_BACK
        } else {
            CameraCharacteristics.LENS_FACING_FRONT
        }
        val id = pickCamera(manager, wanted) ?: return
        closeCamera()
        openCamera(id)
    }

    private fun closeCamera() {
        cameraGeneration += 1
        cameraWatchdog?.let { handler?.removeCallbacks(it) }
        cameraWatchdog = null
        try {
            cameraSession?.close()
        } catch (_: Exception) {
        }
        cameraSession = null
        try {
            cameraDevice?.close()
        } catch (_: Exception) {
        }
        cameraDevice = null
        cameraReader?.close()
        cameraReader = null
        cameraReceivedFrame = false
    }

    private fun openCamera(id: String) {
        val manager = cameraManager ?: return
        val characteristics = try {
            manager.getCameraCharacteristics(id)
        } catch (e: Exception) {
            fail("could not read the camera: ${e.message}")
            return
        }
        val map = characteristics.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
        val size = chooseSize(map?.getOutputSizes(ImageFormat.YUV_420_888))
        if (size == null) {
            fail("the camera has no YUV output size")
            return
        }
        cameraSensorOrientation =
            characteristics.get(CameraCharacteristics.SENSOR_ORIENTATION) ?: 0
        cameraLensFacing = characteristics.get(CameraCharacteristics.LENS_FACING)
            ?: CameraCharacteristics.LENS_FACING_BACK
        cameraMirror = cameraLensFacing == CameraCharacteristics.LENS_FACING_FRONT
        val modes = characteristics.get(CameraCharacteristics.CONTROL_AF_AVAILABLE_MODES)
        cameraAfMode = when {
            modes?.contains(CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_PICTURE) == true ->
                CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_PICTURE
            modes?.contains(CaptureRequest.CONTROL_AF_MODE_AUTO) == true ->
                CaptureRequest.CONTROL_AF_MODE_AUTO
            else -> null
        }
        cameraFpsRange = chooseFpsRange(
            characteristics.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES),
        )
        updateCameraRotation()

        val reader = ImageReader.newInstance(
            size.width,
            size.height,
            ImageFormat.YUV_420_888,
            2,
        )
        cameraReader = reader
        cameraReceivedFrame = false
        reader.setOnImageAvailableListener({ r -> pushCameraFrame(r) }, handler)
        cameraWatchdog = Runnable {
            if (active && !cameraReceivedFrame) {
                fail("the camera did not produce a frame")
            }
        }.also { handler?.postDelayed(it, CAMERA_START_TIMEOUT_MS) }
        val generation = ++cameraGeneration
        try {
            manager.openCamera(id, cameraStateCallbackFor(generation), handler)
        } catch (e: Exception) {
            fail("could not open the camera: ${e.message}")
        }
    }

    private fun cameraStateCallbackFor(generation: Int) = object : CameraDevice.StateCallback() {
        private fun stale() = !active || generation != cameraGeneration

        override fun onOpened(device: CameraDevice) {
            if (stale()) {
                device.close()
                return
            }
            cameraDevice = device
            val reader = cameraReader
            if (reader == null) {
                device.close()
                fail("the camera reader was closed before the camera opened")
                return
            }
            val surface = reader.surface
            try {
                device.createCaptureSession(
                    listOf(surface),
                    object : CameraCaptureSession.StateCallback() {
                        override fun onConfigured(session: CameraCaptureSession) {
                            if (stale()) {
                                session.close()
                                return
                            }
                            cameraSession = session
                            try {
                                val request = device.createCaptureRequest(
                                    CameraDevice.TEMPLATE_PREVIEW,
                                )
                                request.addTarget(surface)
                                cameraAfMode?.let {
                                    request.set(CaptureRequest.CONTROL_AF_MODE, it)
                                }
                                request.set(
                                    CaptureRequest.CONTROL_AE_MODE,
                                    CaptureRequest.CONTROL_AE_MODE_ON,
                                )
                                cameraFpsRange?.let {
                                    request.set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, it)
                                }
                                session.setRepeatingRequest(request.build(), null, handler)
                                Log.i(
                                    tag,
                                    "camera capture started (${cameraRotation}°, " +
                                        "fps ${cameraFpsRange ?: "auto"})",
                                )
                            } catch (e: Exception) {
                                if (stale()) {
                                    session.close()
                                } else {
                                    fail("camera request failed: ${e.message}")
                                }
                            }
                        }

                        override fun onConfigureFailed(session: CameraCaptureSession) {
                            session.close()
                            if (!stale()) fail("camera session could not be configured")
                        }
                    },
                    handler,
                )
            } catch (e: Exception) {
                if (!stale()) fail("camera session failed: ${e.message}")
            }
        }

        override fun onDisconnected(device: CameraDevice) {
            device.close()
            if (!stale()) endCapture("camera disconnected")
        }

        override fun onError(device: CameraDevice, error: Int) {
            device.close()
            if (!stale()) fail("camera error $error")
        }
    }

    private fun pushCameraFrame(reader: ImageReader) {
        val image = try {
            reader.acquireLatestImage()
        } catch (e: Exception) {
            Log.e(tag, "camera acquire failed", e)
            return
        } ?: return
        try {
            if (!active || stopping) return
            cameraReceivedFrame = true
            cameraWatchdog?.let { handler?.removeCallbacks(it) }
            cameraWatchdog = null
            val y = image.planes[0]
            val u = image.planes[1]
            val v = image.planes[2]
            // Hand the planes over as direct slices: Rust converts and
            // rotates them in one pass, so no full RGBA frame is built on
            // the camera thread (and no second full-frame swizzle follows).
            nativePushYuvFrame(
                image.width,
                image.height,
                y.buffer.slice(),
                y.rowStride,
                y.pixelStride,
                u.buffer.slice(),
                u.rowStride,
                u.pixelStride,
                v.buffer.slice(),
                v.rowStride,
                v.pixelStride,
                cameraRotation,
                cameraMirror,
            )
        } catch (e: Exception) {
            Log.e(tag, "camera frame failed", e)
        } finally {
            image.close()
        }
    }

    private fun pickCamera(manager: CameraManager, preferred: Int): String? {
        var fallback: String? = null
        var opposite: String? = null
        for (id in manager.cameraIdList) {
            if (fallback == null) fallback = id
            val facing = try {
                manager.getCameraCharacteristics(id).get(CameraCharacteristics.LENS_FACING)
            } catch (_: Exception) {
                null
            }
            if (facing == preferred) return id
            if (opposite == null && facing != null && facing != preferred) opposite = id
        }
        return opposite ?: fallback
    }

    private fun chooseSize(sizes: Array<Size>?): Size? {
        if (sizes == null || sizes.isEmpty()) return null
        val preferred = sizes
            .filter { it.width in 320..1280 && it.height in 240..720 }
            .maxByOrNull { it.width * it.height }
        return preferred ?: sizes.minByOrNull { it.width * it.height }
    }

    /**
     * Prefer a steady 30 fps range. Camera2's automatic range can expose
     * for a long time in dim light and settle at 5 fps, which looks like
     * a broken share even though every frame is fine. If the device has
     * no fixed-30 range, take the narrowest range that still reaches 30.
     */
    private fun chooseFpsRange(ranges: Array<Range<Int>>?): Range<Int>? {
        if (ranges == null || ranges.isEmpty()) return null
        ranges.firstOrNull { it.lower == 30 && it.upper == 30 }?.let { return it }
        return ranges
            .filter { it.upper >= 30 && it.lower >= 15 }
            .minByOrNull { it.upper - it.lower }
            ?: ranges.maxByOrNull { it.upper * 1000 + it.lower }
    }

    private fun updateCameraRotation() {
        val display = displayRotation()
        cameraRotation = if (cameraLensFacing == CameraCharacteristics.LENS_FACING_FRONT) {
            (cameraSensorOrientation + display) % 360
        } else {
            (cameraSensorOrientation - display + 360) % 360
        }
    }

    private fun displayRotation(): Int {
        val display = getSystemService(DisplayManager::class.java)
            .getDisplay(Display.DEFAULT_DISPLAY)
        return when (display?.rotation ?: Surface.ROTATION_0) {
            Surface.ROTATION_90 -> 90
            Surface.ROTATION_180 -> 180
            Surface.ROTATION_270 -> 270
            else -> 0
        }
    }

    private fun displaySize(): Size? {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val bounds = getSystemService(WindowManager::class.java)
                .maximumWindowMetrics.bounds
            if (bounds.width() > 0 && bounds.height() > 0) {
                return Size(bounds.width(), bounds.height())
            }
        }
        val metrics = DisplayMetrics()
        @Suppress("DEPRECATION")
        val display = getSystemService(WindowManager::class.java).defaultDisplay
        if (display != null) {
            @Suppress("DEPRECATION")
            display.getRealMetrics(metrics)
            if (metrics.widthPixels > 0 && metrics.heightPixels > 0) {
                return Size(metrics.widthPixels, metrics.heightPixels)
            }
        }
        val fallback = resources.displayMetrics
        return if (fallback.widthPixels > 0 && fallback.heightPixels > 0) {
            Size(fallback.widthPixels, fallback.heightPixels)
        } else {
            null
        }
    }

    /**
     * Cap the captured surface at 1080p on the short side and 1920 on the
     * long side, preserving the display aspect ratio. Even dimensions keep
     * the H.264 encoder's 4:2:0 chroma from carrying a padding row.
     */
    private fun captureSize(display: Size?): Size? {
        if (display == null || display.width <= 0 || display.height <= 0) return null
        val width = display.width
        val height = display.height
        val maxWidth = if (width >= height) MAX_LONG_SIDE else MAX_SHORT_SIDE
        val maxHeight = if (width >= height) MAX_SHORT_SIDE else MAX_LONG_SIDE
        val scale = min(
            1.0,
            min(maxWidth.toDouble() / width, maxHeight.toDouble() / height),
        )
        val scaledWidth = (width * scale).toInt() and -2
        val scaledHeight = (height * scale).toInt() and -2
        return Size(
            scaledWidth.coerceAtLeast(2),
            scaledHeight.coerceAtLeast(2),
        )
    }

    // --- teardown --------------------------------------------------------

    private fun endCapture(reason: String) {
        if (stopping) return
        Log.i(tag, reason)
        stopCapture()
        stopSelf()
    }

    private fun fail(message: String) {
        Log.e(tag, message)
        // Failure must win the race with the normal cleanup below: taking
        // the Rust sink here means stopCapture's nativeCaptureEnded() is a
        // no-op, so the UI gets share_failed with this reason instead of a
        // silent share_ended.
        nativeCaptureFailed(message)
        endCapture(message)
    }

    private fun stopCapture() {
        if (stopping) return
        stopping = true
        active = false
        closeCamera()
        nativeCaptureEnded()

        screenReader?.close()
        screenReader = null
        virtualDisplay?.release()
        virtualDisplay = null
        try {
            projection?.stop()
        } catch (_: Exception) {
        }
        projection = null
        cameraManager = null
        stopForeground(STOP_FOREGROUND_REMOVE)
        Log.i(tag, "capture stopped")
    }

    private fun cameraPermissionGranted(): Boolean =
        ContextCompat.checkSelfPermission(this, Manifest.permission.CAMERA) ==
            PackageManager.PERMISSION_GRANTED

    private external fun nativePushFrame(
        width: Int,
        height: Int,
        rowStride: Int,
        buffer: ByteBuffer,
    )

    private external fun nativePushYuvFrame(
        width: Int,
        height: Int,
        y: ByteBuffer,
        yRowStride: Int,
        yPixelStride: Int,
        u: ByteBuffer,
        uRowStride: Int,
        uPixelStride: Int,
        v: ByteBuffer,
        vRowStride: Int,
        vPixelStride: Int,
        rotation: Int,
        mirror: Boolean,
    )

    private external fun nativeCaptureEnded()

    private external fun nativeCaptureFailed(reason: String)

    companion object {
        const val ACTION_START = "dev.parley.app.action.START_SHARE"
        const val ACTION_STOP = "dev.parley.app.action.STOP_SHARE"
        const val ACTION_SWITCH_CAMERA = "dev.parley.app.action.SWITCH_CAMERA"
        const val EXTRA_CAMERA = "camera"

        private const val CHANNEL = "parley-share"
        private const val NOTIFICATION_ID = 4702
        private const val CAMERA_START_TIMEOUT_MS = 10_000L
        private const val MAX_LONG_SIDE = 1920
        private const val MAX_SHORT_SIDE = 1080

        private var pendingPermission: ScreenPermission? = null

        fun setScreenCapturePermission(resultCode: Int, data: Intent?) {
            pendingPermission = if (resultCode == Activity.RESULT_OK && data != null) {
                ScreenPermission(resultCode, data)
            } else {
                null
            }
        }

        private fun takeScreenCapturePermission(): ScreenPermission? =
            pendingPermission.also { pendingPermission = null }

        init {
            System.loadLibrary("parley_flutter")
        }
    }

    private data class ScreenPermission(val resultCode: Int, val data: Intent)
}
