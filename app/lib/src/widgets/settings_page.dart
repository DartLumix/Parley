import 'package:flutter/material.dart';

import '../rust/api/settings.dart' as settings_api;
import '../theme.dart';
import 'update_section.dart';

/// Audio, video and account preferences.
///
/// Every control saves immediately — there is no Save button to forget,
/// and the Rust side applies the value where it takes effect (the next
/// share's encoder, the next voice connection). The page is pushed as a
/// route so the window can be tall and scrollable without disturbing the
/// session underneath; the session state itself stays in `HomePage`.
class SettingsPage extends StatefulWidget {
  const SettingsPage({
    super.key,
    required this.settings,
    required this.hwEncodeSupported,
    this.server,
    this.user,
    this.onSignOut,
    this.onSave,
    this.loadAudioDevices,
  });

  final settings_api.AppSettings settings;
  final bool hwEncodeSupported;

  /// The connected server/account, when there is one.
  final String? server;
  final String? user;

  /// Explicit sign-out (deletes the remembered token). Null while
  /// disconnected — there is nothing to sign out of.
  final Future<void> Function()? onSignOut;

  /// Persists a change. Defaults to the Rust `save`; tests inject their
  /// own so the page's wiring is exercised without Flutter bindings.
  final Future<void> Function(settings_api.AppSettings)? onSave;

  /// Enumerates the host's devices. Injected in tests, where there is no
  /// native bridge; a failure still leaves the default options usable.
  final Future<settings_api.AudioDevices> Function()? loadAudioDevices;

  @override
  State<SettingsPage> createState() => _SettingsPageState();
}

class _SettingsPageState extends State<SettingsPage> {
  late settings_api.AppSettings _settings = widget.settings;
  late final Future<settings_api.AudioDevices> _audioDevices =
      _loadAudioDevices();
  bool _saving = false;
  String? _error;

  Future<settings_api.AudioDevices> _loadAudioDevices() async {
    return (widget.loadAudioDevices ?? settings_api.audioDevices)();
  }

  Future<void> _update(settings_api.AppSettings next) async {
    setState(() {
      _settings = next;
      _saving = true;
      _error = null;
    });
    try {
      if (widget.onSave != null) {
        await widget.onSave!(next);
      } else {
        await settings_api.save(settings: next);
      }
    } catch (e) {
      // Keep the UI honest: the switch moved, the file did not.
      setState(() => _error = 'Could not save: $e');
    } finally {
      if (mounted) setState(() => _saving = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final s = _settings;
    return Scaffold(
      appBar: AppBar(
        title: const Text('Settings'),
        actions: [
          if (_saving)
            const Padding(
              padding: EdgeInsets.only(right: 16),
              child: Center(
                child: SizedBox(
                  width: 14,
                  height: 14,
                  child: CircularProgressIndicator(strokeWidth: 2),
                ),
              ),
            ),
        ],
      ),
      body: ListView(
        padding: const EdgeInsets.fromLTRB(20, 8, 20, 24),
        children: [
          if (_error != null)
            Padding(
              padding: const EdgeInsets.only(bottom: 12),
              child: Text(
                _error!,
                style: const TextStyle(color: Colors.redAccent, fontSize: 12),
              ),
            ),
          _section(theme, 'Video'),
          if (widget.hwEncodeSupported)
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Hardware encoding (VAAPI)'),
              subtitle: const Text(
                'Uses the GPU for the next share you start. Software is '
                'the safe default; switch back if the GPU misbehaves.',
              ),
              value: s.hwEncode,
              onChanged: (v) => _update(_copy(hwEncode: v)),
            ),
          const SizedBox(height: 8),
          _dropdown<int>(
            label: 'Share resolution',
            caption:
                'A cap for the next share — the capture is scaled down, '
                'never up.',
            value: s.shareHeight,
            items: const [
              (0, 'Source (native)'),
              (1440, '1440p'),
              (1080, '1080p'),
              (720, '720p'),
              (480, '480p'),
            ],
            onChanged: (v) => _update(_copy(shareHeight: v)),
          ),
          _dropdown<int>(
            label: 'Share frame rate',
            caption:
                'Capped further by what this machine can encode. Source '
                'follows the compositor.',
            value: s.shareFps,
            items: const [
              (0, 'Source'),
              (60, '60 fps'),
              (30, '30 fps'),
              (15, '15 fps'),
            ],
            onChanged: (v) => _update(_copy(shareFps: v)),
          ),
          const SizedBox(height: 16),
          _section(theme, 'Audio'),
          FutureBuilder<settings_api.AudioDevices>(
            future: _audioDevices,
            builder: (context, snapshot) {
              final devices = snapshot.data;
              if (snapshot.hasError) {
                return const Padding(
                  padding: EdgeInsets.only(bottom: 4),
                  child: Text(
                    'Could not list audio devices; system defaults are used.',
                    style: TextStyle(color: Colors.amberAccent, fontSize: 11),
                  ),
                );
              }
              return Column(
                children: [
                  _deviceDropdown(
                    label: 'Microphone',
                    caption:
                        'Capture device for voice. Applies to the next '
                        'channel you join; a removed device falls back to '
                        'the system default.',
                    value: s.inputDevice,
                    items: _deviceItems(
                      devices?.inputs ?? const [],
                      s.inputDevice,
                    ),
                    onChanged: (v) => _update(_copy(inputDevice: v)),
                  ),
                  _deviceDropdown(
                    label: 'Speakers / output',
                    caption:
                        'Where voices and shared audio play. Applies to '
                        'the next channel you join.',
                    value: s.outputDevice,
                    items: _deviceItems(
                      devices?.outputs ?? const [],
                      s.outputDevice,
                    ),
                    onChanged: (v) => _update(_copy(outputDevice: v)),
                  ),
                  if (snapshot.connectionState == ConnectionState.waiting)
                    const Padding(
                      padding: EdgeInsets.only(bottom: 4),
                      child: LinearProgressIndicator(minHeight: 2),
                    ),
                ],
              );
            },
          ),
          SwitchListTile(
            contentPadding: EdgeInsets.zero,
            title: const Text('Echo cancellation'),
            subtitle: const Text(
              'Removes your speakers from your microphone on speech '
              'channels. Music channels never use it. Applies to the '
              'next channel you join.',
            ),
            value: s.echoCancel,
            onChanged: (v) => _update(_copy(echoCancel: v)),
          ),
          SwitchListTile(
            contentPadding: EdgeInsets.zero,
            title: const Text('Microphone auto gain'),
            subtitle: const Text(
              'Lifts a quiet microphone and turns a hot one down before '
              'encoding. Off by default; it changes the signal, so a good '
              'interface can stay untouched. Applies to the next channel '
              'you join.',
            ),
            value: s.micAutogain,
            onChanged: (v) => _update(_copy(micAutogain: v)),
          ),
          if (s.micAutogain)
            Padding(
              padding: const EdgeInsets.only(top: 2, bottom: 4),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Row(
                    children: [
                      const Expanded(
                        child: Text(
                          'Target level',
                          style: TextStyle(fontSize: 14),
                        ),
                      ),
                      Text(
                        '${s.micAutogainTargetDb} dBFS',
                        style: const TextStyle(
                          color: Colors.white70,
                          fontSize: 12,
                        ),
                      ),
                    ],
                  ),
                  Slider(
                    value: s.micAutogainTargetDb.clamp(-36, -6).toDouble(),
                    min: -36,
                    max: -6,
                    divisions: 30,
                    label: '${s.micAutogainTargetDb} dBFS',
                    onChanged: (v) => setState(
                      () => _settings = _copy(micAutogainTargetDb: v.round()),
                    ),
                    onChangeEnd: (v) =>
                        _update(_copy(micAutogainTargetDb: v.round())),
                  ),
                  const Text(
                    'The auto gain aims for this average level. Lower it if '
                    'your voice sounds too loud to peers; raise it for a '
                    'quiet headset.',
                    style: TextStyle(color: Colors.white38, fontSize: 11),
                  ),
                ],
              ),
            ),
          const SizedBox(height: 16),
          _section(theme, 'Account'),
          ListTile(
            contentPadding: EdgeInsets.zero,
            leading: const Icon(Icons.dns_outlined, size: 20),
            title: const Text('Server'),
            subtitle: Text(widget.server ?? 'not connected'),
          ),
          ListTile(
            contentPadding: EdgeInsets.zero,
            leading: const Icon(Icons.person_outline, size: 20),
            title: const Text('Signed in as'),
            subtitle: Text(widget.user ?? 'nobody'),
          ),
          const SizedBox(height: 8),
          if (widget.onSignOut != null)
            OutlinedButton.icon(
              onPressed: () async {
                await widget.onSignOut!();
                if (context.mounted) Navigator.of(context).pop();
              },
              icon: const Icon(Icons.logout, size: 18),
              label: const Text('Sign out'),
              style: OutlinedButton.styleFrom(
                foregroundColor: Colors.redAccent,
                side: BorderSide(
                  color: Colors.redAccent.withValues(alpha: 0.5),
                ),
                alignment: Alignment.centerLeft,
              ),
            ),
          const SizedBox(height: 6),
          const Text(
            'Signing out forgets the saved session on this machine. '
            'Closing the window does not — it reconnects on the next launch.',
            style: TextStyle(color: Colors.white38, fontSize: 11),
          ),
          const SizedBox(height: 20),
          _section(theme, 'Updates'),
          const UpdateSection(),
        ],
      ),
    );
  }

  /// A copy with one field changed — `AppSettings` is immutable generated
  /// glue, deliberately: the saved file and the UI can never drift by
  /// aliasing the same object.
  settings_api.AppSettings _copy({
    bool? hwEncode,
    int? shareHeight,
    int? shareFps,
    bool? echoCancel,
    String? inputDevice,
    String? outputDevice,
    bool? micAutogain,
    int? micAutogainTargetDb,
  }) {
    final s = _settings;
    return settings_api.AppSettings(
      hwEncode: hwEncode ?? s.hwEncode,
      showSharePreview: s.showSharePreview,
      shareHeight: shareHeight ?? s.shareHeight,
      shareFps: shareFps ?? s.shareFps,
      echoCancel: echoCancel ?? s.echoCancel,
      inputDevice: inputDevice ?? s.inputDevice,
      outputDevice: outputDevice ?? s.outputDevice,
      micAutogain: micAutogain ?? s.micAutogain,
      micAutogainTargetDb: micAutogainTargetDb ?? s.micAutogainTargetDb,
    );
  }

  List<(String, String)> _deviceItems(List<String> names, String selected) {
    final seen = <String>{''};
    final items = <(String, String)>[('', 'System default')];
    for (final name in names) {
      if (name.isNotEmpty && seen.add(name)) items.add((name, name));
    }
    // A saved device may have disappeared since the list was loaded. Keep
    // it visible so the dropdown can render the user's actual value.
    if (selected.isNotEmpty && seen.add(selected)) {
      items.add((selected, '$selected (unavailable)'));
    }
    return items;
  }

  Widget _deviceDropdown({
    required String label,
    required String caption,
    required String value,
    required List<(String, String)> items,
    required ValueChanged<String> onChanged,
  }) {
    return _dropdown<String>(
      label: label,
      caption: caption,
      value: value,
      items: items,
      onChanged: onChanged,
    );
  }

  Widget _section(ThemeData theme, String title) {
    return Padding(
      padding: const EdgeInsets.only(top: 8, bottom: 4),
      child: Text(
        title.toUpperCase(),
        style: TextStyle(
          color: accentSoft,
          fontSize: 11,
          letterSpacing: 1.2,
          fontWeight: FontWeight.w700,
        ),
      ),
    );
  }

  Widget _dropdown<T>({
    required String label,
    required String caption,
    required T value,
    required List<(T, String)> items,
    required ValueChanged<T> onChanged,
  }) {
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 6),
      child: Row(
        children: [
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(label, style: const TextStyle(fontSize: 14)),
                const SizedBox(height: 2),
                Text(
                  caption,
                  style: const TextStyle(color: Colors.white38, fontSize: 11),
                ),
              ],
            ),
          ),
          const SizedBox(width: 16),
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
      ),
    );
  }
}
