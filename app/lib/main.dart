import 'dart:io' show Platform;

import 'package:flutter/material.dart';
import 'package:irondash_engine_context/irondash_engine_context.dart';

import 'src/app.dart';
import 'src/rust/api/video.dart' as video_api;
import 'src/rust/frb_generated.dart';
import 'src/window.dart';

Future<void> main() async {
  // Platform channels (EngineContext.getEngineHandle) need the services
  // binding before runApp initializes it for us.
  WidgetsFlutterBinding.ensureInitialized();
  await RustLib.init();
  // 7f: hand the Rust texture bridge the engine handle BEFORE any
  // session can start receiving shares. A failure is non-fatal for
  // the client (voice/chat still work; video will error later).
  try {
    final handle = await EngineContext.instance.getEngineHandle();
    await video_api.videoPrepare(engineHandle: handle);
    debugPrint('video: texture bridge ready (engine handle $handle)');
  } catch (e, st) {
    debugPrint('video prepare failed (video disabled): $e\n$st');
  }
  // One probe at startup. Whether the platform channel is reachable is
  // the difference between REAL fullscreen (the compositor's) and a page
  // that merely covers our window — and a silent failure only shows up
  // as "the fullscreen button does nothing".
  await setWindowFullscreen(false, probe: true);
  // Dev affordance (env-gated, like the harness flags): prove that the
  // native call actually reaches the compositor on this machine —
  // gtk_window_fullscreen → GDK_WINDOW_STATE_FULLSCREEN — rather than
  // only compiling. Run it as `PARLEY_FULLSCREEN_SELFTEST=1 ./parley`;
  // the log then reads "on — state now: fullscreen".
  if (Platform.environment['PARLEY_FULLSCREEN_SELFTEST'] == '1') {
    await setWindowFullscreen(true);
    await Future<void>.delayed(const Duration(milliseconds: 800));
    await setWindowFullscreen(false);
  }
  runApp(const ParleyApp());
}
