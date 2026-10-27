//! The Updates card on the settings page: what this build is, one button that
//! asks the release channel, the install-and-restart path, and *What's new*
//! for the notes bundled with this build.

import 'package:flutter/material.dart';

import '../theme.dart';
import '../update/notes.dart' as notes;
import '../update/updater.dart' as updater;
import '../version.dart';

class UpdateSection extends StatefulWidget {
  const UpdateSection({
    super.key,
    this.check,
    this.installAndRestart,
    this.loadNotes,
  });

  /// The defaults talk to GitHub, to the filesystem and to this process;
  /// tests inject the states they want to render instead.
  final Future<updater.UpdateCheck> Function()? check;
  final Future<void> Function(
    updater.UpdateCheck check,
    void Function(double? progress) onProgress,
  )? installAndRestart;
  final Future<String?> Function()? loadNotes;

  @override
  State<UpdateSection> createState() => _UpdateSectionState();
}

class _UpdateSectionState extends State<UpdateSection> {
  updater.UpdateCheck? _check;
  bool _busy = false;
  double? _progress;
  String _phase = '';
  String? _error;

  Future<void> _runCheck() async {
    setState(() {
      _busy = true;
      _check = null;
      _error = null;
      _phase = 'Asking ${updater.updateRepo}…';
    });
    try {
      final result = await (widget.check ?? updater.checkForUpdate)();
      if (mounted) setState(() => _check = result);
    } on Object catch (e) {
      if (mounted) setState(() => _error = 'the update check failed: $e');
    } finally {
      if (mounted) {
        setState(() {
          _busy = false;
          _phase = '';
        });
      }
    }
  }

  Future<void> _install() async {
    final check = _check;
    if (check == null || check.latest == null) return;
    final go = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text('Install ${check.latest} and restart?'),
        content: const Text(
          'Parley downloads the new build, checks it against the release '
          'checksums and restarts on it. A call or a screen share in '
          'progress ends with the restart.',
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(context).pop(false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.of(context).pop(true),
            child: const Text('Install and restart'),
          ),
        ],
      ),
    );
    if (go != true || !mounted) return;

    setState(() {
      _busy = true;
      _error = null;
      _progress = 0;
      _phase = 'Downloading…';
    });
    try {
      await (widget.installAndRestart ?? _defaultInstall)(check, (fraction) {
        if (!mounted) return;
        setState(() {
          _progress = fraction;
          _phase = fraction == null ? 'Verifying and installing…' : 'Downloading…';
        });
      });
      // The real install ends this process; only an injected one returns.
      if (mounted) setState(() => _phase = 'Restarting…');
    } on Object catch (e) {
      if (mounted) {
        setState(() {
          _error = 'the update failed: $e';
          _busy = false;
          _progress = null;
          _phase = '';
        });
      }
    }
  }

  Future<void> _defaultInstall(
    updater.UpdateCheck check,
    void Function(double? progress) onProgress,
  ) =>
      updater.installAndRestart(check, onProgress: onProgress);

  Future<void> _showNotes() async {
    final text = await (widget.loadNotes ?? notes.loadPatchNotes)();
    if (!mounted) return;
    if (text == null) {
      setState(() => _error = 'this build carries no release notes');
      return;
    }
    setState(() => _error = null);
    await notes.showPatchNotesDialog(context, text, version: parleyVersion);
  }

  @override
  Widget build(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        ListTile(
          contentPadding: EdgeInsets.zero,
          leading: const Icon(Icons.system_update_alt, size: 20),
          title: const Text('Version'),
          subtitle: Text(
            parleyVersion == 'dev'
                ? 'dev — a source build, not a release'
                : parleyVersion,
          ),
        ),
        const SizedBox(height: 4),
        Wrap(
          spacing: 8,
          runSpacing: 8,
          children: [
            OutlinedButton.icon(
              onPressed: _busy ? null : _runCheck,
              icon: const Icon(Icons.refresh, size: 18),
              label: const Text('Check for updates'),
            ),
            OutlinedButton.icon(
              onPressed: _busy ? null : _showNotes,
              icon: const Icon(Icons.article_outlined, size: 18),
              label: const Text("What's new"),
            ),
          ],
        ),
        const SizedBox(height: 6),
        Text(
          'Updates come from github.com/${updater.updateRepo}. Releases are '
          'checked against their SHA256SUMS; a copy installed by a package '
          'manager updates with it instead.',
          style: const TextStyle(color: Colors.white38, fontSize: 11),
        ),
        if (_busy) ...[
          const SizedBox(height: 10),
          if (_progress != null)
            LinearProgressIndicator(value: _progress, minHeight: 3)
          else
            const LinearProgressIndicator(minHeight: 3),
          const SizedBox(height: 6),
        ],
        if (_phase.isNotEmpty)
          Text(
            _phase,
            style: const TextStyle(color: Colors.white70, fontSize: 12),
          ),
        if (_error != null)
          Padding(
            padding: const EdgeInsets.only(top: 6),
            child: Text(
              _error!,
              style: const TextStyle(color: Colors.redAccent, fontSize: 12),
            ),
          ),
        if (!_busy && _check != null) _status(_check!),
        if (!_busy && _check?.status == updater.UpdateStatus.available) ...[
          const SizedBox(height: 10),
          FilledButton.icon(
            onPressed: _install,
            icon: const Icon(Icons.download, size: 18),
            label: Text('Install ${_check!.latest} and restart'),
          ),
        ],
      ],
    );
  }

  Widget _status(updater.UpdateCheck check) {
    switch (check.status) {
      case updater.UpdateStatus.upToDate:
        return _line(
          Colors.white70,
          'You are on the latest release (${check.current}).',
        );
      case updater.UpdateStatus.available:
        return _line(
          accentSoft,
          'Version ${check.latest} is available — you have ${check.current}.',
        );
      case updater.UpdateStatus.blocked:
        return _line(
          Colors.amberAccent,
          'Version ${check.latest} is available, but ${check.detail}.',
        );
      case updater.UpdateStatus.failed:
        return _line(Colors.redAccent, 'Could not check: ${check.detail}.');
    }
  }

  Widget _line(Color color, String text) => Padding(
        padding: const EdgeInsets.only(top: 6),
        child: Text(text, style: TextStyle(color: color, fontSize: 12)),
      );
}
