import 'package:flutter/material.dart';

import '../rust/api/players.dart';

/// What the pre-share dialog returns: caps for one share plus its audio
/// choice. 0 means "the capture's own" for both caps.
///
/// `audio` off ⇒ no audio at all. On ⇒ `audioPlayers` holds the PipeWire
/// node ids to capture (one or more); those are captured directly, so
/// this app's own voice is never part of the share.
typedef ShareOptions = ({
  int height,
  int fps,
  bool audio,
  List<int> audioPlayers,
});

/// Ask for resolution, frame rate and audio before the portal opens.
///
/// Its own file so the choice can be tested without a session: the
/// dialog has no state beyond the selections, and the caller decides
/// what to do with the answer. `height`/`fps` are the remembered
/// defaults (settings), so the common case is one click.
///
/// `players` is a snapshot taken just before opening (see
/// `listShareAudioPlayers`): the per-app tracking is what removes the
/// echo, so "entire system audio" is deliberately not an option here.
Future<ShareOptions?> showShareOptionsDialog(
  BuildContext context, {
  required int height,
  required int fps,
  List<ShareAudioPlayer> players = const [],
}) {
  var chosenHeight = height;
  var chosenFps = fps;
  // Everything playing is checked by default: the old whole-output
  // behaviour, minus this app itself.
  var audioOn = players.isNotEmpty;
  final selected = <int>{for (final p in players) p.id};
  return showDialog<ShareOptions>(
    context: context,
    builder: (context) => StatefulBuilder(
      builder: (context, setDialog) => AlertDialog(
        title: const Text('Share screen'),
        content: SizedBox(
          width: 380,
          // Scrollable: the audio list can grow past the dialog's
          // height budget on a small window.
          child: SingleChildScrollView(
            child: Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                const Text(
                  'Pick the source in the desktop dialog that opens next. '
                  'These are caps, not upscales — the stream runs at the '
                  'fastest rate the encoder can hold up to the frame rate '
                  'chosen here, and the screen tile shows the rate actually '
                  'being sent.',
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
                const Divider(height: 24),
                Row(
                  children: [
                    const Expanded(
                      child: Text(
                        'Share audio',
                        style: TextStyle(fontSize: 13),
                      ),
                    ),
                    Switch(
                      value: audioOn,
                      onChanged: players.isEmpty
                          ? null
                          : (v) => setDialog(() => audioOn = v),
                    ),
                  ],
                ),
                if (players.isEmpty)
                  const Text(
                    'No application is playing audio right now. Start the '
                    'playback you want to share, then share again.',
                    style: TextStyle(color: Colors.white54, fontSize: 12),
                  )
                else if (audioOn)
                  ConstrainedBox(
                    constraints: const BoxConstraints(maxHeight: 190),
                    child: SingleChildScrollView(
                      child: Column(
                        children: [
                          for (final player in players)
                            CheckboxListTile(
                              dense: true,
                              contentPadding: EdgeInsets.zero,
                              value: selected.contains(player.id),
                              onChanged: (v) => setDialog(() {
                                if (v == true) {
                                  selected.add(player.id);
                                } else {
                                  selected.remove(player.id);
                                }
                              }),
                              title: Text(
                                player.app,
                                style: const TextStyle(fontSize: 13),
                              ),
                              subtitle: Text(
                                player.description,
                                style: const TextStyle(
                                  color: Colors.white54,
                                  fontSize: 11,
                                ),
                              ),
                            ),
                        ],
                      ),
                    ),
                  ),
                if (players.isNotEmpty)
                  const Padding(
                    padding: EdgeInsets.only(top: 6),
                    child: Text(
                      'Only these apps are captured — Parley itself never is, '
                      'so participants do not hear their own voice back.',
                      style: TextStyle(color: Colors.white38, fontSize: 11),
                    ),
                  ),
              ],
            ),
          ),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context),
            child: const Text('Cancel'),
          ),
          FilledButton.icon(
            onPressed: () => Navigator.pop<ShareOptions>(context, (
              height: chosenHeight,
              fps: chosenFps,
              audio: audioOn && selected.isNotEmpty,
              audioPlayers: audioOn
                  ? selected.toList(growable: false)
                  : const <int>[],
            )),
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
