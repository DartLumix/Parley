plugins {
    id("com.android.application")
    // The Flutter Gradle Plugin must be applied after the Android and Kotlin Gradle plugins.
    id("dev.flutter.flutter-gradle-plugin")
}

// Release signing material is read from the environment, never from the
// repository: the keystore path, its password, the key alias and that
// key's password. CI decodes the base64 secret to a file first. A release
// build without them is refused below, so a tag can never quietly publish
// a debug-signed APK that Android would refuse to update over.
val releaseKeystore = System.getenv("PARLEY_ANDROID_KEYSTORE")?.takeIf { it.isNotBlank() }
val releaseSigningReady = releaseKeystore != null

fun requiredEnv(name: String): String =
    System.getenv(name)?.takeIf { it.isNotBlank() }
        ?: throw GradleException(
            "$name is required when PARLEY_ANDROID_KEYSTORE is set"
        )

android {
    namespace = "dev.parley.app"
    compileSdk = 37
    buildToolsVersion = "37.0.0"
    // AGP 9.5's default/supported NDK. The newer r30 LTS is not required
    // for API 37 and would be a separate toolchain migration.
    ndkVersion = "28.2.13676358"

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    defaultConfig {
        applicationId = "dev.parley.app"
        minSdk = 26
        targetSdk = 37
        ndk {
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
        // Uses the version code from pubspec.yaml. When using split APKs, 1000 * ABI_VERSION
        // is added automatically by Flutter. (https://developer.android.com/studio/build/configure-apk-splits#configure-APK-versions)
        // You can force using the value of versionCode by specifying the `-P force-version-code-ignoring-abi=true`
        // flag during build.
        versionCode = flutter.versionCode
        versionName = flutter.versionName
    }

    signingConfigs {
        if (releaseSigningReady) {
            create("release") {
                storeFile = file(releaseKeystore!!)
                storePassword = requiredEnv("PARLEY_ANDROID_KEYSTORE_PASSWORD")
                keyAlias = requiredEnv("PARLEY_ANDROID_KEY_ALIAS")
                keyPassword = requiredEnv("PARLEY_ANDROID_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        debug {
            // Side-by-side with the release-signed app: a debug build must
            // never replace the user's real install (or its data).
            applicationIdSuffix = ".debug"
            versionNameSuffix = "-debug"
        }
        release {
            signingConfig =
                if (releaseSigningReady) signingConfigs.getByName("release") else null
            // Flutter's release pipeline runs R8. The irondash plugin is
            // reached by JNI method name, so its entry points and the
            // Flutter types in their descriptors must survive shrinking.
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
    }
}

// `flutter build apk --release` runs assembleRelease; a missing keystore
// must stop there with this sentence rather than after a 20-minute NDK
// build that produces an installable-looking APK signed by nobody.
gradle.taskGraph.whenReady {
    val releaseBuild = allTasks.any {
        it.name == "assembleRelease" || it.name == "bundleRelease"
    }
    check(!releaseBuild || releaseSigningReady) {
        "Android release signing is not configured: set PARLEY_ANDROID_KEYSTORE, " +
            "PARLEY_ANDROID_KEYSTORE_PASSWORD, PARLEY_ANDROID_KEY_ALIAS and " +
            "PARLEY_ANDROID_KEY_PASSWORD (see docs/release.md)"
    }
}

kotlin {
    compilerOptions {
        jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17
    }
}

// Build the Rust bridge for the two ABIs the Android app ships. Keeping
// this in Gradle means `flutter build apk` and `flutter run` cannot
// silently package an old or missing libparley_flutter.so.
val rustFeatures = "audio-device,aec-rust,video,screen-capture"
val buildRustAndroid by tasks.registering(Exec::class) {
    workingDir = file("../../rust")
    val ndk = android.ndkDirectory.absolutePath
    environment("ANDROID_NDK_HOME", ndk)
    environment("ANDROID_NDK", ndk)
    environment("CMAKE_ANDROID_NDK", ndk)
    environment("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
    commandLine(
        "cargo", "ndk",
        "--platform", "26",
        "-t", "arm64-v8a",
        "-t", "x86_64",
        "-o", file("src/main/jniLibs").absolutePath,
        "build", "--release", "--locked",
        "-p", "parley_flutter",
        "--features", rustFeatures,
    )
    // cargo-ndk emits libparley_flutter.so but not the NDK's C++ runtime.
    // Oboe/OpenH264 pull in libc++_shared, and without it dlopen fails
    // before Flutter can leave the launch screen. Package the matching
    // runtime for each ABI alongside the Rust libraries.
    doLast {
        val sysroot = "$ndk/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib"
        project.copy {
            from("$sysroot/aarch64-linux-android/libc++_shared.so")
            into(file("src/main/jniLibs/arm64-v8a"))
        }
        project.copy {
            from("$sysroot/x86_64-linux-android/libc++_shared.so")
            into(file("src/main/jniLibs/x86_64"))
        }
    }
}

tasks.named("preBuild").configure {
    dependsOn(buildRustAndroid)
}

dependencies {
    implementation("androidx.core:core-ktx:1.19.1")
}

flutter {
    source = "../.."
}
