//! Release notes: the markdown bundled with every build
//! (`app/assets/patch_notes.md`), the dialog that shows it, and the marker
//! that limits the pop-up to the first launch of a version.
//!
//! The notes are written by hand before a release and travel inside the
//! bundle, so they describe exactly the build the user is running — they are
//! not the GitHub release body (that one is generated from `CHANGELOG.md`).

import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart' show rootBundle;

import '../theme.dart';
import '../version.dart';

/// The bundled notes, or null when this build carries none (a source build
/// without the asset) — the caller says so instead of showing an empty box.
Future<String?> loadPatchNotes({Future<String> Function(String key)? load}) async {
  try {
    final text = await (load ?? rootBundle.loadString)('assets/patch_notes.md');
    final notes = stripNotesComments(text);
    return notes.isEmpty ? null : notes;
  } on Object catch (e) {
    debugPrint('notes: cannot read the bundled notes: $e');
    return null;
  }
}

/// The file starts with a `<!-- … -->` block explaining how to write it; that
/// is for the author, not the reader.
String stripNotesComments(String text) =>
    text.replaceAll(RegExp(r'<!--.*?-->', dotAll: true), '').trim();

/// The first launch of a version shows what changed in it. A fresh install
/// counts too — the notes are as useful there as after an update.
Future<void> maybeShowPatchNotes(
  BuildContext context, {
  String? version,
  Future<String?> Function()? load,
  File? marker,
}) async {
  final current = version ?? parleyVersion;
  // A source build has no release identity and no previous launch to compare
  // with; only released builds announce themselves.
  if (current == 'dev') return;

  final seenFile = marker ?? _seenFile();
  String? seen;
  try {
    if (seenFile.existsSync()) seen = seenFile.readAsStringSync().trim();
  } on Object catch (e) {
    debugPrint('notes: cannot read the marker: $e');
  }
  if (seen == current) return;

  final notes = await (load ?? loadPatchNotes)();
  // Record BEFORE showing: a dialog that never comes back must not pop up on
  // every launch.
  try {
    seenFile.parent.createSync(recursive: true);
    seenFile.writeAsStringSync(current);
  } on Object catch (e) {
    debugPrint('notes: cannot record $current as seen: $e');
  }

  if (notes == null || !context.mounted) return;
  await showPatchNotesDialog(context, notes, version: current);
}

/// The notes as a dialog, behind Settings → *What's new* as well.
Future<void> showPatchNotesDialog(
  BuildContext context,
  String notes, {
  String? version,
}) {
  final shown = version ?? parleyVersion;
  return showDialog<void>(
    context: context,
    builder: (context) => AlertDialog(
      title: Text(shown == 'dev' ? "What's new" : "What's new in $shown"),
      content: SizedBox(
        width: 520,
        child: SingleChildScrollView(
          child: SelectionArea(
            child: PatchNotes(
              notes,
              // A dialog must be able to end without a second thought.
              onClose: () => Navigator.of(context).pop(),
            ),
          ),
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Close'),
        ),
      ],
    ),
  );
}

/// The notes as readable lines: headings, bullet lists and paragraphs, with
/// the text selectable and copyable like the chat.
class PatchNotes extends StatelessWidget {
  const PatchNotes(this.markdown, {super.key, this.onClose});

  final String markdown;

  /// Safe to ignore; closes the surrounding dialog when there is one.
  final VoidCallback? onClose;

  @override
  Widget build(BuildContext context) {
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        for (final line in markdown.split('\n')) _line(context, line),
      ],
    );
  }

  Widget _line(BuildContext context, String line) {
    final text = line.trimRight();
    final trimmed = text.trimLeft();
    if (trimmed.isEmpty) return const SizedBox(height: 8);

    if (trimmed.startsWith('#')) {
      final heading = _plain(trimmed.replaceFirst(RegExp(r'^#+\s*'), ''));
      final top = trimmed.startsWith('# ') || trimmed.startsWith('## ');
      return Padding(
        padding: EdgeInsets.only(top: top ? 8 : 12, bottom: 3),
        child: Text(
          heading,
          style: TextStyle(
            fontSize: top ? 15 : 13,
            fontWeight: FontWeight.w700,
            color: top ? accentSoft : Colors.white70,
          ),
        ),
      );
    }

    final bullet = RegExp(r'^[-*]\s+').firstMatch(trimmed);
    if (bullet != null) {
      return Padding(
        padding: const EdgeInsets.only(left: 2, bottom: 5),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            const Text('•  ', style: TextStyle(color: accentSoft)),
            Expanded(
              child: Text(
                _plain(trimmed.substring(bullet.end)),
                style: const TextStyle(fontSize: 13, height: 1.35),
              ),
            ),
          ],
        ),
      );
    }

    return Padding(
      padding: const EdgeInsets.only(bottom: 5),
      child: Text(
        _plain(text),
        style: const TextStyle(fontSize: 13, height: 1.35),
      ),
    );
  }

  /// Emphasis markers are noise in a dialog that has no markdown engine.
  String _plain(String text) => text.replaceAll(RegExp(r'\*\*|`'), '');
}

/// `<config>/parley/`, the directory the Rust side uses for `settings.toml`
/// and the remembered login (`app/rust/src/api/settings.rs`), so the marker
/// sits with the rest of this installation's state.
Directory _configDir() {
  String? env(String name) {
    final value = Platform.environment[name];
    return (value == null || value.isEmpty) ? null : value;
  }

  if (Platform.isWindows) {
    final appdata = env('APPDATA');
    if (appdata != null) return Directory(appdata);
  }
  final xdg = env('XDG_CONFIG_HOME');
  if (xdg != null) return Directory(xdg);
  final home = env('HOME');
  if (home != null) return Directory('$home/.config');
  return Directory('.');
}

File _seenFile() => File(
      '${_configDir().path}${Platform.pathSeparator}parley'
      '${Platform.pathSeparator}release_notes_seen',
    );
