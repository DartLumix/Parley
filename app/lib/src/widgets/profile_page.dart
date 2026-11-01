import 'dart:typed_data';
import 'dart:ui' as ui;

import 'package:file_selector/file_selector.dart';
import 'package:flutter/material.dart';

import '../rust/api/soundboard.dart';
import '../theme.dart';

/// Pictures are square thumbnails, never full photos: the widest attempt
/// is 256 px and the encoder walks down until the PNG fits the wire limit.
/// 48 KiB leaves ample room inside the protocol's 64 KiB frame, and is
/// still a sharp 2x roster/chat thumbnail.
const int avatarEdgePx = 256;
const int maxAvatarBytes = 48 * 1024;

/// Save callback supplied by `HomePage`: it talks to the Rust bridge and
/// resolves with the server's error, or null on success. The page never
/// pretends a change was accepted before the server says so.
typedef SaveProfile =
    Future<String?> Function({
      required String currentPassword,
      required String newUsername,
      required String newPassword,
      required Uint8List? avatar,
      required bool clearAvatar,
    });

/// Decode any image Flutter can read and return a PNG that fits
/// [maxAvatarBytes]. A detailed photo may not compress well at 256 px, so
/// the edge walks down until it does; a flat graphic normally stays at
/// the full edge on the first try. PNG keeps the upload to one format and
/// avoids a second image encoder on the Rust side.
Future<Uint8List> prepareAvatar(Uint8List raw) async {
  Uint8List? smallest;
  for (final edge in const <int>[avatarEdgePx, 192, 128, 96, 64]) {
    final codec = await ui.instantiateImageCodec(raw, targetWidth: edge);
    try {
      final frame = await codec.getNextFrame();
      try {
        final data = await frame.image.toByteData(
          format: ui.ImageByteFormat.png,
        );
        if (data == null) {
          throw const FormatException('image produced no pixels');
        }
        final bytes = data.buffer.asUint8List();
        smallest = bytes;
        if (bytes.length <= maxAvatarBytes) return bytes;
      } finally {
        frame.image.dispose();
      }
    } finally {
      codec.dispose();
    }
  }
  // Even 64 px did not fit: refuse before anything reaches the wire.
  throw FormatException(
    'image is still ${smallest?.length ?? 0} bytes at 64 px',
  );
}

/// Name/photo/password settings for the signed-in account.
///
/// Every change is authorized by the current password; that is a server
/// rule, mirrored here by making the field required instead of failing
/// after a round trip.
class ProfilePage extends StatefulWidget {
  const ProfilePage({
    super.key,
    required this.username,
    required this.role,
    required this.avatar,
    required this.onSave,
    this.soundboard,
    this.onSoundboard,
  });

  final String username;
  final String role;
  final Uint8List? avatar;
  final SaveProfile onSave;
  /// Local files + the last applied toggles. Null keeps the section out
  /// (the widget tests that only exercise the account form).
  final SoundboardStatus? soundboard;
  final Future<String?> Function({
    required bool join,
    required bool leave,
    required bool afk,
  })?
  onSoundboard;

  @override
  State<ProfilePage> createState() => _ProfilePageState();
}

class _ProfilePageState extends State<ProfilePage> {
  late final TextEditingController _username = TextEditingController(
    text: widget.username,
  );
  final _currentPassword = TextEditingController();
  final _newPassword = TextEditingController();
  final _confirmPassword = TextEditingController();

  late Uint8List? _avatar = widget.avatar;
  bool _clearAvatar = false;
  bool _busy = false;
  String? _error;
  late bool _joinSound = widget.soundboard?.joinEnabled ?? false;
  late bool _leaveSound = widget.soundboard?.leaveEnabled ?? false;
  late bool _afkSound = widget.soundboard?.afkEnabled ?? false;
  bool _savingSounds = false;
  String? _soundStatus;

  @override
  void dispose() {
    _username.dispose();
    _currentPassword.dispose();
    _newPassword.dispose();
    _confirmPassword.dispose();
    super.dispose();
  }

  Future<void> _pickAvatar() async {
    const images = XTypeGroup(
      label: 'Images',
      extensions: <String>['png', 'jpg', 'jpeg', 'webp', 'gif'],
    );
    final XFile? file;
    try {
      file = await openFile(acceptedTypeGroups: const <XTypeGroup>[images]);
    } catch (e) {
      if (mounted) setState(() => _error = 'Could not open the file picker: $e');
      return;
    }
    if (file == null || !mounted) return;
    try {
      final prepared = await prepareAvatar(await file.readAsBytes());
      if (prepared.length > maxAvatarBytes) {
        if (mounted) {
          setState(
            () => _error =
                'That picture is too large after resizing. Try a smaller image.',
          );
        }
        return;
      }
      if (!mounted) return;
      setState(() {
        _avatar = prepared;
        _clearAvatar = false;
        _error = null;
      });
    } catch (_) {
      if (mounted) {
        setState(() => _error = 'That file is not a readable image.');
      }
    }
  }

  void _removeAvatar() {
    setState(() {
      _avatar = null;
      _clearAvatar = true;
      _error = null;
    });
  }

  Future<void> _save() async {
    final username = _username.text.trim();
    if (username.isEmpty) {
      setState(() => _error = 'Username cannot be empty.');
      return;
    }
    if (username.length > 32) {
      setState(() => _error = 'Username must be 32 characters or fewer.');
      return;
    }
    if (_currentPassword.text.isEmpty) {
      setState(() => _error = 'Enter your current password to save changes.');
      return;
    }
    if (_newPassword.text != _confirmPassword.text) {
      setState(() => _error = 'The new passwords do not match.');
      return;
    }
    final renamed = username != widget.username;
    final avatarChanged = !_clearAvatar && !identical(_avatar, widget.avatar);
    if (!renamed &&
        _newPassword.text.isEmpty &&
        !avatarChanged &&
        !_clearAvatar) {
      setState(() => _error = 'Nothing to change.');
      return;
    }
    setState(() {
      _busy = true;
      _error = null;
    });
    final error = await widget.onSave(
      currentPassword: _currentPassword.text,
      newUsername: username,
      newPassword: _newPassword.text,
      // Re-sending an untouched picture would bump its revision and move
      // the bytes for no reason; only a real change goes over the wire.
      avatar: avatarChanged ? _avatar : null,
      clearAvatar: _clearAvatar,
    );
    if (!mounted) return;
    if (error != null) {
      setState(() {
        _busy = false;
        _error = error;
      });
      return;
    }
    Navigator.of(context).pop(true);
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final initial = widget.username.isEmpty
        ? '?'
        : widget.username[0].toUpperCase();
    return Scaffold(
      appBar: AppBar(
        title: const Text('Profile'),
        actions: [
          if (_busy)
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
        padding: const EdgeInsets.fromLTRB(20, 12, 20, 28),
        children: [
          Center(
            child: CircleAvatar(
              radius: 46,
              backgroundColor: barSurface,
              backgroundImage: _avatar == null ? null : MemoryImage(_avatar!),
              child: _avatar == null
                  ? Text(
                      initial,
                      style: const TextStyle(
                        fontSize: 32,
                        fontWeight: FontWeight.w600,
                        color: Colors.white70,
                      ),
                    )
                  : null,
            ),
          ),
          const SizedBox(height: 10),
          Row(
            mainAxisAlignment: MainAxisAlignment.center,
            children: [
              OutlinedButton.icon(
                onPressed: _busy ? null : _pickAvatar,
                icon: const Icon(Icons.add_photo_alternate_outlined, size: 18),
                label: Text(_avatar == null ? 'Add picture' : 'Change picture'),
              ),
              if (_avatar != null) ...[
                const SizedBox(width: 8),
                TextButton(
                  onPressed: _busy ? null : _removeAvatar,
                  style: TextButton.styleFrom(foregroundColor: Colors.redAccent),
                  child: const Text('Remove'),
                ),
              ],
            ],
          ),
          const SizedBox(height: 8),
          Center(
            child: Text(
              '${widget.role} account',
              style: const TextStyle(color: Colors.white38, fontSize: 11),
            ),
          ),
          const SizedBox(height: 20),
          _section(theme, 'Account'),
          TextField(
            controller: _username,
            enabled: !_busy,
            decoration: const InputDecoration(
              labelText: 'Username',
              helperText: 'Your name on the roster, in chat and on pokes.',
              isDense: true,
              border: OutlineInputBorder(),
            ),
          ),
          const SizedBox(height: 12),
          TextField(
            controller: _currentPassword,
            enabled: !_busy,
            obscureText: true,
            decoration: const InputDecoration(
              labelText: 'Current password',
              helperText: 'Required for every change below.',
              isDense: true,
              border: OutlineInputBorder(),
            ),
          ),
          const SizedBox(height: 12),
          TextField(
            controller: _newPassword,
            enabled: !_busy,
            obscureText: true,
            decoration: const InputDecoration(
              labelText: 'New password',
              helperText: 'Leave empty to keep the current one.',
              isDense: true,
              border: OutlineInputBorder(),
            ),
          ),
          const SizedBox(height: 12),
          TextField(
            controller: _confirmPassword,
            enabled: !_busy,
            obscureText: true,
            onSubmitted: (_) => _save(),
            decoration: const InputDecoration(
              labelText: 'Confirm new password',
              isDense: true,
              border: OutlineInputBorder(),
            ),
          ),
          if (_error != null) ...[
            const SizedBox(height: 14),
            Text(
              _error!,
              style: const TextStyle(color: Colors.redAccent, fontSize: 12),
            ),
          ],
          const SizedBox(height: 20),
          FilledButton.icon(
            onPressed: _busy ? null : _save,
            icon: const Icon(Icons.check, size: 18),
            label: Text(_busy ? 'Saving…' : 'Save changes'),
          ),
          if (widget.soundboard != null) ...[
            const SizedBox(height: 26),
            const Divider(),
            _section(theme, 'Soundboard'),
            Text(
              'Drop one MP3 per event in this folder and switch it on. '
              'You and everyone in the channel hear it when you join, '
              'leave or go AFK. Edited a file? Press Apply again.',
              style: const TextStyle(color: Colors.white54, fontSize: 12),
            ),
            const SizedBox(height: 8),
            SelectableText(
              widget.soundboard!.dir,
              style: const TextStyle(
                color: accentSoft,
                fontSize: 11,
                fontFamily: 'monospace',
              ),
            ),
            const SizedBox(height: 8),
            _soundRow(
              'Join sound',
              'join.mp3',
              _joinSound,
              widget.soundboard!.joinFile,
              (v) => setState(() => _joinSound = v),
            ),
            _soundRow(
              'Leave sound',
              'leave.mp3',
              _leaveSound,
              widget.soundboard!.leaveFile,
              (v) => setState(() => _leaveSound = v),
            ),
            _soundRow(
              'AFK sound',
              'afk.mp3',
              _afkSound,
              widget.soundboard!.afkFile,
              (v) => setState(() => _afkSound = v),
            ),
            const SizedBox(height: 10),
            OutlinedButton.icon(
              onPressed: _savingSounds ? null : _saveSounds,
              icon: const Icon(Icons.graphic_eq, size: 18),
              label: Text(_savingSounds ? 'Applying…' : 'Apply sounds'),
            ),
            if (_soundStatus != null) ...[
              const SizedBox(height: 8),
              Text(
                _soundStatus!,
                style: TextStyle(
                  color: _soundStatus == 'Sounds applied'
                      ? Colors.greenAccent
                      : Colors.redAccent,
                  fontSize: 12,
                ),
              ),
            ],
          ],
        ],
      ),
    );
  }

  Future<void> _saveSounds() async {
    final save = widget.onSoundboard;
    if (save == null) return;
    setState(() {
      _savingSounds = true;
      _soundStatus = null;
    });
    final error = await save(join: _joinSound, leave: _leaveSound, afk: _afkSound);
    if (!mounted) return;
    setState(() {
      _savingSounds = false;
      _soundStatus = error ?? 'Sounds applied';
    });
  }

  Widget _soundRow(
    String title,
    String file,
    bool enabled,
    bool present,
    ValueChanged<bool> onChanged,
  ) {
    return SwitchListTile(
      value: enabled,
      onChanged: _savingSounds ? null : onChanged,
      contentPadding: EdgeInsets.zero,
      dense: true,
      title: Text(title, style: const TextStyle(fontSize: 13)),
      subtitle: Text(
        present ? 'sounds/$file' : 'sounds/$file — missing',
        style: TextStyle(
          fontSize: 11,
          color: present ? Colors.white38 : Colors.orangeAccent,
        ),
      ),
    );
  }

  Widget _section(ThemeData theme, String title) {
    return Padding(
      padding: const EdgeInsets.only(top: 8, bottom: 10),
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
}
