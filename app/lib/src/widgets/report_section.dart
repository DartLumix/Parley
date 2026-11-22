import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../app_log.dart';
import '../report.dart';
import '../version.dart';

/// The Support row's destination: a full page so the form does not turn the
/// settings list into a second scrollable and so a long description has room.
class ReportPage extends StatelessWidget {
  const ReportPage({super.key});

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Report a problem')),
      body: ListView(
        padding: const EdgeInsets.fromLTRB(20, 12, 20, 28),
        children: const [ReportSection()],
      ),
    );
  }
}

/// Collect the machine, logs and an optional screenshot, save a report
/// folder, and open a prefilled GitHub issue.
///
/// GitHub itself refuses anonymous issue creation. The card says so and
/// offers the saved folder as the fallback, so a user without an account is
/// never sent to a dead end.
class ReportSection extends StatefulWidget {
  const ReportSection({super.key});

  @override
  State<ReportSection> createState() => _ReportSectionState();
}

class _ReportSectionState extends State<ReportSection> {
  final TextEditingController _problem = TextEditingController();
  bool _logs = true;
  bool _screenshot = true;
  bool _busy = false;
  String? _error;
  ReportResult? _result;
  String _savedDescription = '';

  @override
  void dispose() {
    _problem.dispose();
    super.dispose();
  }

  Future<ReportResult?> _save() async {
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      final result = await buildReport(
        description: _problem.text,
        includeLogs: _logs,
        includeScreenshot: _screenshot,
      );
      if (!mounted) return result;
      setState(() {
        _result = result;
        _savedDescription = _problem.text;
      });
      return result;
    } on Object catch (e) {
      AppLog.write('report', 'save failed: $e');
      if (mounted) setState(() => _error = 'Could not save the report: $e');
      return null;
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  Future<void> _openIssue() async {
    var result = _result;
    // Reuse the just-saved folder only while the description still matches;
    // otherwise the issue would describe something different from the logs.
    if (result == null || _savedDescription != _problem.text) {
      result = await _save();
    }
    if (result == null || !mounted) return;
    final opened = await openExternalUrl(result.issueUrl);
    if (!opened && mounted) {
      setState(
        () => _error =
            'Could not open a browser. The report is saved at ${result!.path}; '
            'open GitHub and paste the description there.',
      );
    }
  }

  Future<void> _copySummary() async {
    final result = _result;
    final text = [
      'Parley ${_versionLine()}',
      if (result != null) 'Report: ${result.path}',
      '',
      _problem.text.trim(),
      if (result != null) '',
      if (result != null) result.summary,
    ].join('\n');
    await Clipboard.setData(ClipboardData(text: text));
    if (!mounted) return;
    ScaffoldMessenger.of(context)
        .showSnackBar(const SnackBar(content: Text('Report summary copied')));
  }

  String _versionLine() {
    final result = _result;
    if (result == null) return parleyVersion;
    return '$parleyVersion diagnostic report (${result.files.length} files)';
  }

  @override
  Widget build(BuildContext context) {
    final result = _result;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const Text(
          'Something broken or behaving strangely? Describe it below. The '
          'report includes this session and the previous one (so a crash is '
          'not lost), the Rust audio/device log, the update log, and the '
          'machine, desktop and audio/video driver details.',
          style: TextStyle(color: Colors.white54, fontSize: 12, height: 1.35),
        ),
        const SizedBox(height: 8),
        TextField(
          controller: _problem,
          minLines: 3,
          maxLines: 6,
          maxLength: 4000,
          decoration: const InputDecoration(
            labelText: 'What happened?',
            hintText:
                'What did you do, what did you expect, and what happened '
                'instead? Include the channel or screen if it matters.',
            border: OutlineInputBorder(),
          ),
        ),
        CheckboxListTile(
          contentPadding: EdgeInsets.zero,
          dense: true,
          value: _logs,
          onChanged: (v) => setState(() => _logs = v ?? true),
          title: const Text('Include logs'),
          subtitle: const Text(
            'Current and previous client sessions, Rust audio log, update log.',
            style: TextStyle(fontSize: 11),
          ),
        ),
        CheckboxListTile(
          contentPadding: EdgeInsets.zero,
          dense: true,
          value: _screenshot,
          onChanged: (v) => setState(() => _screenshot = v ?? true),
          title: const Text('Include a screenshot of the app'),
          subtitle: const Text(
            'Only what is inside Parley — useful for visual glitches.',
            style: TextStyle(fontSize: 11),
          ),
        ),
        const SizedBox(height: 4),
        Wrap(
          spacing: 8,
          runSpacing: 8,
          children: [
            FilledButton.icon(
              onPressed: _busy ? null : _save,
              icon: _busy
                  ? const SizedBox(
                      width: 16,
                      height: 16,
                      child: CircularProgressIndicator(strokeWidth: 2),
                    )
                  : const Icon(Icons.save_alt, size: 18),
              label: const Text('Save report'),
            ),
            OutlinedButton.icon(
              onPressed: _busy ? null : _openIssue,
              icon: const Icon(Icons.bug_report_outlined, size: 18),
              label: const Text('Open GitHub issue'),
            ),
            TextButton.icon(
              onPressed: _busy ? null : _copySummary,
              icon: const Icon(Icons.copy_all_outlined, size: 18),
              label: const Text('Copy summary'),
            ),
          ],
        ),
        const SizedBox(height: 6),
        const Text(
          'GitHub requires an account to submit an issue, so the button opens '
          'a prefilled page in your browser. If you cannot sign in, use Save '
          'report and send the folder (or Copy summary) wherever you report '
          'the problem. Nothing is uploaded automatically.',
          style: TextStyle(color: Colors.white38, fontSize: 11, height: 1.35),
        ),
        if (_error != null) ...[
          const SizedBox(height: 8),
          Text(
            _error!,
            style: const TextStyle(color: Colors.redAccent, fontSize: 12),
          ),
        ],
        if (result != null) ...[
          const SizedBox(height: 10),
          Container(
            width: double.infinity,
            padding: const EdgeInsets.all(10),
            decoration: BoxDecoration(
              color: Colors.white.withValues(alpha: 0.04),
              borderRadius: BorderRadius.circular(8),
              border: Border.all(color: Colors.white12),
            ),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                const Text(
                  'Report saved',
                  style: TextStyle(fontWeight: FontWeight.w600, fontSize: 12),
                ),
                const SizedBox(height: 4),
                SelectableText(
                  result.path,
                  style: const TextStyle(fontSize: 11, color: Colors.white70),
                ),
                const SizedBox(height: 4),
                Text(
                  'Files: ${result.files.isEmpty ? 'none' : result.files.join(', ')}\n'
                  'Attach these files to the issue (drag them in).',
                  style: const TextStyle(fontSize: 11, color: Colors.white38),
                ),
                const SizedBox(height: 6),
                Row(
                  children: [
                    TextButton.icon(
                      onPressed: () async {
                        final messenger = ScaffoldMessenger.of(context);
                        await Clipboard.setData(
                          ClipboardData(text: result.path),
                        );
                        if (!mounted) return;
                        messenger.showSnackBar(
                          const SnackBar(content: Text('Report path copied')),
                        );
                      },
                      icon: const Icon(Icons.copy, size: 16),
                      label: const Text('Copy path'),
                    ),
                  ],
                ),
              ],
            ),
          ),
        ],
      ],
    );
  }
}
