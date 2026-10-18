import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

/// The fullscreen viewer: black background, the stream letterboxed to
/// its own aspect, and one translucent control cluster that fades after
/// a few idle seconds and comes back on pointer movement. Nothing else
/// — no app bar, no session chrome, no floating action bar, no window
/// toolbar.
class FullscreenShare extends StatefulWidget {
  const FullscreenShare({
    super.key,
    required this.title,
    required this.textureId,
    required this.width,
    required this.height,
    required this.onExit,
    required this.onStopWatching,
  });

  final String title;
  final int textureId;
  final int width;
  final int height;
  final VoidCallback onExit;
  final VoidCallback onStopWatching;

  @override
  State<FullscreenShare> createState() => _FullscreenShareState();
}

class _FullscreenShareState extends State<FullscreenShare> {
  bool _controls = true;
  Timer? _hide;

  @override
  void initState() {
    super.initState();
    _scheduleHide();
  }

  @override
  void dispose() {
    _hide?.cancel();
    super.dispose();
  }

  void _scheduleHide() {
    _hide?.cancel();
    _hide = Timer(const Duration(seconds: 3), () {
      if (mounted && _controls) setState(() => _controls = false);
    });
  }

  void _reveal() {
    if (!mounted) return;
    if (!_controls) setState(() => _controls = true);
    _scheduleHide();
  }

  Widget _button(IconData icon, String label, VoidCallback onTap) {
    return FilledButton.tonalIcon(
      onPressed: onTap,
      icon: Icon(icon, size: 18),
      label: Text(label),
      style: FilledButton.styleFrom(
        backgroundColor: Colors.black54,
        foregroundColor: Colors.white,
        padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 10),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    return CallbackShortcuts(
      bindings: {
        const SingleActivator(LogicalKeyboardKey.escape): widget.onExit,
      },
      child: Focus(
        autofocus: true,
        child: MouseRegion(
          cursor: _controls ? SystemMouseCursors.basic : SystemMouseCursors.none,
          onHover: (_) => _reveal(),
          child: Stack(
            fit: StackFit.expand,
            children: [
              Center(
                child: AspectRatio(
                  aspectRatio: widget.width / widget.height,
                  child: Texture(textureId: widget.textureId),
                ),
              ),
              Positioned(
                top: 16,
                right: 16,
                child: AnimatedOpacity(
                  opacity: _controls ? 1 : 0,
                  duration: const Duration(milliseconds: 180),
                  child: IgnorePointer(
                    ignoring: !_controls,
                    child: Row(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        _button(
                          Icons.visibility_off,
                          'Stop watching',
                          widget.onStopWatching,
                        ),
                        const SizedBox(width: 8),
                        _button(
                          Icons.fullscreen_exit,
                          'Exit fullscreen',
                          widget.onExit,
                        ),
                      ],
                    ),
                  ),
                ),
              ),
              Positioned(
                left: 20,
                bottom: 16,
                child: AnimatedOpacity(
                  opacity: _controls ? 1 : 0,
                  duration: const Duration(milliseconds: 180),
                  child: Text(
                    '${widget.title} · ${widget.width}×${widget.height} · Esc to exit',
                    style: const TextStyle(color: Colors.white60, fontSize: 12),
                  ),
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}
