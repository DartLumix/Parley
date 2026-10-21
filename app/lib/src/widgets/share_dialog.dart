import 'package:flutter/material.dart';

/// What the pre-share dialog returns: caps for one share. 0 means "the
/// capture's own" for both — the behaviour before the dialog existed.
typedef ShareOptions = ({int height, int fps});

/// Ask for resolution and frame rate before the portal opens.
///
/// Its own file so the choice can be tested without a session: the
/// dialog has no state of its own beyond the two selections, and the
/// caller decides what to do with the answer. `height`/`fps` are the
/// remembered defaults (settings), so the common case is one click.
Future<ShareOptions?> showShareOptionsDialog(
  BuildContext context, {
  required int height,
  required int fps,
}) {
  var chosenHeight = height;
  var chosenFps = fps;
  return showDialog<ShareOptions>(
    context: context,
    builder: (context) => StatefulBuilder(
      builder: (context, setDialog) => AlertDialog(
        title: const Text('Share screen'),
        content: SizedBox(
          width: 380,
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              const Text(
                'Pick the source in the desktop dialog that opens next. '
                'These are caps, not upscales.',
                style: TextStyle(color: Colors.white54, fontSize: 12),
              ),
              const SizedBox(height: 12),
              _choice<int>(
                label: 'Resolution',
                value: chosenHeight,
                items: const [
                  (0, 'Source (native)'),
                  (1440, '1440p'),
                  (1080, '1080p'),
                  (720, '720p'),
                  (480, '480p'),
                ],
                onChanged: (v) => setDialog(() => chosenHeight = v),
              ),
              const SizedBox(height: 8),
              _choice<int>(
                label: 'Frame rate',
                value: chosenFps,
                items: const [
                  (0, 'Source'),
                  (60, '60 fps'),
                  (30, '30 fps'),
                  (15, '15 fps'),
                ],
                onChanged: (v) => setDialog(() => chosenFps = v),
              ),
            ],
          ),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context),
            child: const Text('Cancel'),
          ),
          FilledButton.icon(
            onPressed: () => Navigator.pop<ShareOptions>(
              context,
              (height: chosenHeight, fps: chosenFps),
            ),
            icon: const Icon(Icons.screen_share, size: 18),
            label: const Text('Start sharing'),
          ),
        ],
      ),
    ),
  );
}

Widget _choice<T>({
  required String label,
  required T value,
  required List<(T, String)> items,
  required ValueChanged<T> onChanged,
}) {
  return Row(
    children: [
      Expanded(child: Text(label, style: const TextStyle(fontSize: 13))),
      DropdownButton<T>(
        value: value,
        underline: const SizedBox.shrink(),
        onChanged: (v) {
          if (v != null) onChanged(v);
        },
        items: [
          for (final (v, text) in items)
            DropdownMenuItem<T>(value: v, child: Text(text)),
        ],
      ),
    ],
  );
}
