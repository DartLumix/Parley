import 'dart:async';
import 'dart:io' show Platform;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'theme.dart';

/// The GTK side's window channel (linux/runner/my_application.cc).
///
/// The window is undecorated: the app owns the title bar. Fullscreen is
/// still the WINDOW's, not the page's — the same call every native
/// player makes (`gtk_window_fullscreen` → xdg_toplevel /
/// `_NET_WM_STATE_FULLSCREEN`). That is what makes the system recognise
/// it, instead of a widget merely covering our own window.
const MethodChannel windowChannel = MethodChannel('parley/window');

/// Ask the window manager for real fullscreen. Answers with the state
/// AFTER the call, so a compositor that ignored it is visible in the
/// log rather than looking like a dead button.
Future<bool?> setWindowFullscreen(bool on, {bool probe = false}) async {
  try {
    final state = await windowChannel.invokeMethod<bool>('setFullscreen', on);
    debugPrint(
      'window: fullscreen ${on ? 'on' : 'off'} — state now: '
      '${state == true ? 'fullscreen' : 'not fullscreen'}'
      '${probe ? ' (channel ready)' : ''}',
    );
    return state;
  } on MissingPluginException {
    // Not the Linux runner (tests, other platforms): the Flutter view
    // still fills the window, so this is cosmetic only.
    if (probe) {
      debugPrint('window: no platform channel — fullscreen stays window-filling');
    }
    return null;
  } catch (e) {
    debugPrint('window: fullscreen $on failed: $e');
    return null;
  }
}

Future<bool> minimizeWindow() async {
  try {
    return await windowChannel.invokeMethod<bool>('minimize') ?? false;
  } on MissingPluginException {
    return false;
  }
}

/// Toggle maximize and return the state the window is in afterwards.
Future<bool> toggleMaximizeWindow() async {
  try {
    return await windowChannel.invokeMethod<bool>('toggleMaximize') ?? false;
  } on MissingPluginException {
    return false;
  }
}

Future<void> closeWindow() async {
  try {
    await windowChannel.invokeMethod<bool>('close');
  } on MissingPluginException {
    // No native window (tests): nothing to close.
  }
}

/// Best-effort native move drag. Returns false when the compositor did
/// not take it (Wayland needs a pointer serial Flutter owns); tiling
/// compositors move windows with their own binding anyway.
Future<bool> startWindowDrag() async {
  try {
    return await windowChannel.invokeMethod<bool>('startDrag') ?? false;
  } on MissingPluginException {
    return false;
  }
}

/// A thin window toolbar that slides down when the pointer reaches the
/// very top of the window and hides again afterwards.
///
/// The window is undecorated (the GTK header bar is gone — on a tiling
/// desktop it was a white strip above the page). That leaves no native
/// close/minimise control, so this is the replacement: 6 px of dead
/// space at the top reveals it, the strip itself holds the app name, a
/// drag region, and minimise / maximise / close.
class WindowToolbar extends StatefulWidget {
  const WindowToolbar({super.key, this.enabled = true});

  /// False while the fullscreen viewer is up: that page may have no
  /// chrome of any kind.
  final bool enabled;

  @override
  State<WindowToolbar> createState() => _WindowToolbarState();
}

const double _barHeight = 36;
const double _hotZoneHeight = 6;

class _WindowToolbarState extends State<WindowToolbar> {
  bool _visible = false;
  Timer? _hide;

  @override
  void dispose() {
    _hide?.cancel();
    super.dispose();
  }

  void _reveal() {
    _hide?.cancel();
    if (!_visible && mounted) setState(() => _visible = true);
  }

  void _scheduleHide() {
    _hide?.cancel();
    _hide = Timer(const Duration(milliseconds: 1200), () {
      if (mounted && _visible) setState(() => _visible = false);
    });
  }

  Future<void> _drag() async {
    final moved = await startWindowDrag();
    if (!moved) {
      debugPrint(
        'window: drag not taken by the compositor '
        '(use its own move binding on Wayland)',
      );
    }
  }

  Widget _button(IconData icon, String tooltip, VoidCallback onTap) {
    return IconButton(
      tooltip: tooltip,
      icon: Icon(icon, size: 16),
      color: Colors.white70,
      hoverColor: Colors.white.withValues(alpha: 0.08),
      padding: EdgeInsets.zero,
      visualDensity: VisualDensity.compact,
      constraints: const BoxConstraints.tightFor(width: 38, height: 28),
      onPressed: onTap,
    );
  }

  @override
  Widget build(BuildContext context) {
    if (!widget.enabled || !Platform.isLinux) return const SizedBox.shrink();

    final bar = Container(
      height: _barHeight,
      width: double.infinity,
      decoration: BoxDecoration(
        color: barSurface.withValues(alpha: 0.97),
        border: const Border(
          bottom: BorderSide(color: Colors.white12),
        ),
      ),
      child: Row(
        children: [
          const SizedBox(width: 12),
          const Text(
            'Parley',
            style: TextStyle(
              fontSize: 12,
              fontWeight: FontWeight.w600,
              color: Colors.white70,
              letterSpacing: 0.2,
            ),
          ),
          // Drag region: double-click toggles maximize, like a title bar.
          Expanded(
            child: GestureDetector(
              behavior: HitTestBehavior.opaque,
              onPanStart: (_) => _drag(),
              onDoubleTap: () => toggleMaximizeWindow(),
              child: const SizedBox.expand(),
            ),
          ),
          _button(Icons.remove, 'Minimize', () => minimizeWindow()),
          _button(Icons.crop_square, 'Maximize', () => toggleMaximizeWindow()),
          _button(Icons.close, 'Close', () => closeWindow()),
          const SizedBox(width: 4),
        ],
      ),
    );

    return Stack(
      children: [
        // Always-present hot zone: the only way to summon the toolbar.
        Positioned(
          top: 0,
          left: 0,
          right: 0,
          height: _hotZoneHeight,
          child: MouseRegion(
            onEnter: (_) => _reveal(),
            onHover: (_) => _reveal(),
            child: const SizedBox.expand(),
          ),
        ),
        IgnorePointer(
          ignoring: !_visible,
          child: AnimatedSlide(
            offset: Offset(0, _visible ? 0 : -1),
            duration: const Duration(milliseconds: 160),
            curve: Curves.easeOutCubic,
            child: MouseRegion(
              onEnter: (_) => _reveal(),
              onHover: (_) => _reveal(),
              onExit: (_) => _scheduleHide(),
              child: SizedBox(height: _barHeight, width: double.infinity, child: bar),
            ),
          ),
        ),
      ],
    );
  }
}
