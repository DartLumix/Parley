import 'dart:io';

import 'package:flutter/services.dart';

/// The Android-only bridge. Every call is a no-op or a readable failure on
/// other platforms, so callers can use it without importing `dart:io`
/// themselves.
class AndroidPlatform {
  const AndroidPlatform._();

  static const MethodChannel _channel = MethodChannel('parley/platform');

  static bool get supported => Platform.isAndroid;

  /// App-private directories plus a human-readable device name.
  static Future<AndroidPaths?> paths() async {
    if (!supported) return null;
    final answer = await _channel.invokeMapMethod<String, String>('paths');
    if (answer == null) return null;
    return AndroidPaths(
      files: answer['files'] ?? '',
      cache: answer['cache'] ?? '',
      device: answer['device'] ?? 'Android',
    );
  }

  static Future<bool> requestPermissions(List<String> permissions) async {
    if (!supported) return false;
    return await _channel.invokeMethod<bool>(
          'requestPermissions',
          permissions,
        ) ??
        false;
  }

  /// Open Android's package installer. Returns false when the user still
  /// has to allow installs from this source; the settings page is opened
  /// in that case.
  static Future<bool> installApk(String path) async {
    if (!supported) return false;
    return await _channel.invokeMethod<bool>('installApk', {'path': path}) ??
        false;
  }

  static Future<void> openInstallSettings() async {
    if (!supported) return;
    await _channel.invokeMethod<void>('openInstallSettings');
  }

  static Future<void> startVoiceService() async {
    if (!supported) return;
    await _channel.invokeMethod<void>('startVoiceService');
  }

  static Future<void> stopVoiceService() async {
    if (!supported) return;
    await _channel.invokeMethod<void>('stopVoiceService');
  }
}

class AndroidPaths {
  const AndroidPaths({
    required this.files,
    required this.cache,
    required this.device,
  });

  final String files;
  final String cache;
  final String device;
}
