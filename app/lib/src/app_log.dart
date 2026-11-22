//! Persistent client logging.
//!
//! Every line written through [AppLog.write] (and therefore `debugPrint`)
//! goes to a file as it happens, so a crash does not need a graceful shutdown
//! to leave diagnostics behind. At startup the previous `session-current.log`
//! is rotated to `session-previous.log`; the report page includes both, which
//! is what makes "last session before the crash" possible.
//!
//! The location follows the Rust side (`PARLEY_DATA_DIR`, then `%APPDATA%`
//! on Windows, `$XDG_CONFIG_HOME` or `~/.config` elsewhere), so settings,
//! remembered login and logs all live under the same `parley` directory.

import 'dart:io';

import 'package:flutter/foundation.dart';

/// The application data directory shared with the Rust side.
Directory appDataDir() {
  String? env(String name) {
    final value = Platform.environment[name];
    return (value == null || value.isEmpty) ? null : value;
  }

  final override = env('PARLEY_DATA_DIR');
  if (override != null) {
    return Directory('$override${Platform.pathSeparator}parley');
  }
  if (Platform.isWindows) {
    final appdata = env('APPDATA');
    if (appdata != null) {
      return Directory('$appdata${Platform.pathSeparator}parley');
    }
  }
  final xdg = env('XDG_CONFIG_HOME');
  if (xdg != null) return Directory('$xdg${Platform.pathSeparator}parley');
  final home = env('HOME');
  if (home != null) {
    return Directory(
      '$home${Platform.pathSeparator}.config${Platform.pathSeparator}parley',
    );
  }
  return Directory('${Directory.current.path}${Platform.pathSeparator}parley');
}

Directory logDir() =>
    Directory('${appDataDir().path}${Platform.pathSeparator}logs');

Directory reportDir() =>
    Directory('${appDataDir().path}${Platform.pathSeparator}reports');

/// Process-wide client log. Static on purpose: a crash can happen anywhere,
/// and the error handlers must not depend on a widget being mounted.
class AppLog {
  AppLog._();

  static RandomAccessFile? _file;
  static bool _started = false;
  static String? _currentPath;

  /// The current session log, or null before [start].
  static String? get currentPath => _currentPath;

  /// Start the session log and install Flutter/async error handlers.
  ///
  /// Safe to call more than once; only the first call does the work.
  static void start() {
    if (_started) return;
    _started = true;
    try {
      final dir = logDir()..createSync(recursive: true);
      final current = File(
        '${dir.path}${Platform.pathSeparator}session-current.log',
      );
      final previous = File(
        '${dir.path}${Platform.pathSeparator}session-previous.log',
      );
      if (current.existsSync()) {
        try {
          if (previous.existsSync()) previous.deleteSync();
          current.renameSync(previous.path);
        } on Object {
          // A locked old log must not stop this session from logging.
        }
      }
      _file = File(current.path).openSync(mode: FileMode.write);
      _currentPath = current.path;
      write(
        'app',
        'session started (${Platform.operatingSystem} '
            '${Platform.operatingSystemVersion})',
      );
    } on Object {
      // Without a writable data directory the console still works; the
      // report page says logs are unavailable instead of crashing.
      _file = null;
    }

    final originalDebugPrint = debugPrint;
    debugPrint = (String? message, {int? wrapWidth}) {
      originalDebugPrint(message, wrapWidth: wrapWidth);
      if (message != null) write('dart', message);
    };

    final originalFlutterError = FlutterError.onError;
    FlutterError.onError = (details) {
      write(
        'flutter',
        '${details.exceptionAsString()}\n${details.stack ?? ''}',
      );
      originalFlutterError?.call(details);
    };

    PlatformDispatcher.instance.onError = (error, stack) {
      write('dart', 'uncaught async error: $error\n$stack');
      return false;
    };
  }

  /// Append one line. Never throws: logging must not take the app down.
  static void write(String tag, String message) {
    final file = _file;
    if (file == null) return;
    final stamp = DateTime.now().toIso8601String();
    final text = message.endsWith('\n') ? message : '$message\n';
    try {
      file.writeStringSync('$stamp [$tag] $text');
    } on Object {
      // Disk full, removed directory, closed handle: keep running.
    }
  }

  /// A synchronous flush point for error handlers and the report writer.
  static void flush() {
    try {
      _file?.flushSync();
    } on Object {
      // Best effort.
    }
  }
}
