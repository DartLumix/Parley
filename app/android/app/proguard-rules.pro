# The irondash engine-context plugin resolves its Java entry points by
# name from Rust. R8 must not rename the methods, and it must not rename
# the Flutter types that appear in their JNI descriptors:
#
#   getTextureRegistry(J)Lio/flutter/view/TextureRegistry;
#
# Renaming TextureRegistry to a short obfuscated name changes the
# descriptor Rust looks up, so the first remote video surface dies with
# NoSuchMethodError. Keep both sides of every descriptor stable.
-keep class dev.irondash.engine_context.IrondashEngineContextPlugin { *; }
-keep class io.flutter.view.TextureRegistry { *; }
-keep interface io.flutter.plugin.common.BinaryMessenger { *; }
-keep class io.flutter.embedding.engine.FlutterEngine { *; }
-keep class io.flutter.embedding.android.FlutterView { *; }

# The capture service's native methods are resolved by their Java names
# from Rust (`Java_dev_parley_app_ShareService_*`). Keep the class and
# the method names even if a future R8 version changes its defaults.
-keep class dev.parley.app.ShareService { *; }

# Standard JNI safety net for any other plugin that uses a native
# method's declaring class or signature.
-keepclasseswithmembernames class * {
    native <methods>;
}
