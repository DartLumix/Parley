package dev.parley.app

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import android.system.Os
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import androidx.core.content.FileProvider
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel
import java.io.File

/**
 * The small Android surface Flutter cannot supply itself: the app's private
 * directories, runtime permissions, the microphone foreground service and
 * handing a downloaded APK to Android's package installer.
 */
class MainActivity : FlutterActivity() {
    private val channelName = "parley/platform"
    private val permissionRequestCode = 4701
    private val screenCaptureRequestCode = 4702
    private var pendingPermissions: MethodChannel.Result? = null
    private var pendingScreenCapture: MethodChannel.Result? = null
    private var requestedPermissions: Array<String> = emptyArray()

    // Loading the Rust library from the JVM (rather than only from the
    // Dart VM) is what lets Oboe find the Android application context
    // through ndk-context. FRB's own load is idempotent.
    private external fun initAndroidContext(context: Context)

    override fun onCreate(savedInstanceState: Bundle?) {
        // The Rust side resolves its config/session files through this
        // variable. Set it before the Flutter engine (and FRB) starts, so
        // the remembered login and settings land in app-private storage
        // instead of an unwritable desktop path.
        Os.setenv("PARLEY_DATA_DIR", filesDir.absolutePath, true)
        super.onCreate(savedInstanceState)
        initAndroidContext(applicationContext)
    }

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        MethodChannel(
            flutterEngine.dartExecutor.binaryMessenger,
            channelName,
        ).setMethodCallHandler { call, result ->
            when (call.method) {
                "paths" -> result.success(
                    mapOf(
                        "files" to filesDir.absolutePath,
                        "cache" to cacheDir.absolutePath,
                        "device" to "${Build.MANUFACTURER} ${Build.MODEL}".trim(),
                    ),
                )

                "requestPermissions" -> {
                    @Suppress("UNCHECKED_CAST")
                    val wanted = (call.arguments as? List<*>)?.filterIsInstance<String>().orEmpty()
                    requestPermissions(wanted, result)
                }

                "installApk" -> {
                    val path = call.argument<String>("path")
                    if (path.isNullOrBlank()) {
                        result.error("bad-argument", "APK path is missing", null)
                    } else {
                        installApk(File(path), result)
                    }
                }

                "openInstallSettings" -> {
                    openInstallSettings()
                    result.success(true)
                }

                "startVoiceService" -> {
                    ContextCompat.startForegroundService(
                        this,
                        Intent(this, VoiceService::class.java),
                    )
                    result.success(true)
                }

                "stopVoiceService" -> {
                    stopService(Intent(this, VoiceService::class.java))
                    result.success(true)
                }

                "requestScreenCapture" -> requestScreenCapture(result)

                "hasCamera" -> result.success(
                    packageManager.hasSystemFeature(PackageManager.FEATURE_CAMERA_ANY),
                )

                "startShareService" -> {
                    val camera = call.argument<Boolean>("camera") ?: false
                    ContextCompat.startForegroundService(
                        this,
                        Intent(this, ShareService::class.java).apply {
                            action = ShareService.ACTION_START
                            putExtra(ShareService.EXTRA_CAMERA, camera)
                        },
                    )
                    result.success(true)
                }

                "stopShareService" -> {
                    stopService(Intent(this, ShareService::class.java))
                    result.success(true)
                }

                "switchCamera" -> {
                    ContextCompat.startForegroundService(
                        this,
                        Intent(this, ShareService::class.java).apply {
                            action = ShareService.ACTION_SWITCH_CAMERA
                        },
                    )
                    result.success(true)
                }

                "moveTaskToBack" -> {
                    // Back is a MINIMIZE while a call/share is running:
                    // finishing the activity would tear down the
                    // foreground service with it.
                    moveTaskToBack(true)
                    result.success(true)
                }

                else -> result.notImplemented()
            }
        }
    }

    private fun requestPermissions(wanted: List<String>, result: MethodChannel.Result) {
        val needed = wanted
            .filter { it in KNOWN_PERMISSIONS }
            .filter {
                ContextCompat.checkSelfPermission(this, it) != PackageManager.PERMISSION_GRANTED
            }
            .toTypedArray()
        if (needed.isEmpty()) {
            result.success(true)
            return
        }
        if (pendingPermissions != null) {
            result.error("busy", "another permission request is already open", null)
            return
        }
        pendingPermissions = result
        requestedPermissions = needed
        ActivityCompat.requestPermissions(this, needed, permissionRequestCode)
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode != permissionRequestCode) return
        val allGranted = grantResults.isNotEmpty() &&
            grantResults.all { it == PackageManager.PERMISSION_GRANTED }
        pendingPermissions?.success(allGranted)
        pendingPermissions = null
        requestedPermissions = emptyArray()
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != screenCaptureRequestCode) return
        val result = pendingScreenCapture
        pendingScreenCapture = null
        if (resultCode == RESULT_OK) {
            // The service consumes this once it is in the foreground. The
            // Activity cannot start MediaProjection itself on Android 14+.
            ShareService.setScreenCapturePermission(resultCode, data)
            result?.success(true)
        } else {
            result?.success(false)
        }
    }

    private fun requestScreenCapture(result: MethodChannel.Result) {
        if (pendingScreenCapture != null) {
            result.error("busy", "another screen capture request is already open", null)
            return
        }
        val manager = getSystemService(MediaProjectionManager::class.java)
        pendingScreenCapture = result
        try {
            startActivityForResult(
                manager.createScreenCaptureIntent(),
                screenCaptureRequestCode,
            )
        } catch (e: Exception) {
            pendingScreenCapture = null
            result.error(
                "screen-capture",
                e.message ?: "could not open the system screen-capture prompt",
                null,
            )
        }
    }

    private fun installApk(apk: File, result: MethodChannel.Result) {
        if (!apk.isFile) {
            result.error("missing", "the downloaded APK no longer exists", null)
            return
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O &&
            !packageManager.canRequestPackageInstalls()
        ) {
            openInstallSettings()
            result.success(false)
            return
        }
        try {
            // Downloads live under code_cache, which FileProvider does
            // not expose. Its roots are files-path and cache-path, so
            // stage the APK under cacheDir first; the old direct URI
            // threw "Failed to find configured root" on every install.
            val staging = File(cacheDir, "updates").apply { mkdirs() }
            val staged = File(staging, "parley-update.apk")
            apk.copyTo(staged, overwrite = true)
            val uri: Uri = FileProvider.getUriForFile(
                this,
                "$packageName.fileprovider",
                staged,
            )
            val intent = Intent(Intent.ACTION_VIEW).apply {
                setDataAndType(uri, "application/vnd.android.package-archive")
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            }
            startActivity(intent)
            result.success(true)
        } catch (e: Exception) {
            result.error("install", e.message ?: "could not open the installer", null)
        }
    }

    private fun openInstallSettings() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val intent = Intent(
            Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
            Uri.parse("package:$packageName"),
        ).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        startActivity(intent)
    }

    private companion object {
        init {
            System.loadLibrary("parley_flutter")
        }

        val KNOWN_PERMISSIONS = setOf(
            Manifest.permission.RECORD_AUDIO,
            Manifest.permission.CAMERA,
            Manifest.permission.POST_NOTIFICATIONS,
            Manifest.permission.BLUETOOTH_CONNECT,
        )
    }
}
