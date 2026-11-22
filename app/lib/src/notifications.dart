//! Desktop poke notifications.
//!
//! The in-app snackbar is enough while the window is in front; a poke is
//! exactly the thing that should find you in another window, though. When
//! Settings → Notifications › Poke notifications is on, the same poke also
//! goes to the desktop's notification service through the small native
//! channel below (GNotification on Linux, a notification-area balloon on
//! Windows), which is what puts Parley's name and icon on it and lets the
//! desktop keep it in the notification centre.
//!
//! The channel is deliberately fire-and-forget: a desktop without a
//! notification daemon, a test binary or the phone build simply has no
//! handler, and a failed banner must never affect the session or the cue.

import 'dart:io';

import 'package:flutter/services.dart';

import 'app_log.dart';

/// The platform channel served by `linux/runner/my_application.cc` and
/// `windows/runner/flutter_window.cpp`.
const MethodChannel notificationsChannel = MethodChannel(
  'parley/notifications',
);

/// Desktops with a notification backend in this build. Android keeps the
/// in-app notice only, so the switch is not offered there.
bool get systemNotificationsSupported => Platform.isLinux || Platform.isWindows;

/// The system notification's headline: the poke itself.
String pokeNotificationTitle(String from) => '$from poked you';

/// The line under it: where the poke came from, and what a click does.
/// Desktop banners give this one line, so it stays short.
String pokeNotificationBody({String? channel}) {
  final place = (channel == null || channel.isEmpty) ? null : channel;
  return place == null
      ? 'Open Parley to poke back'
      : 'In “$place” · Open Parley to poke back';
}

/// Raise the desktop notification for a poke. Silently does nothing on a
/// platform (or in a test) without the native channel.
Future<void> showPokeNotification({
  required String from,
  String? channel,
}) async {
  if (!systemNotificationsSupported) return;
  try {
    await notificationsChannel.invokeMethod<bool>('notify', {
      'title': pokeNotificationTitle(from),
      'body': pokeNotificationBody(channel: channel),
    });
  } on MissingPluginException {
    // Tests and runner builds without the channel: the snackbar is enough.
  } catch (e) {
    AppLog.write('dart', 'notifications: poke failed: $e');
  }
}
