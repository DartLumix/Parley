//! Diagnostic reports: a self-contained folder with the client logs, the
//! previous session (the one before a crash), the Rust device log, the
//! update log, environment/driver details and an optional screenshot.
//!
//! GitHub requires an account to create an issue, so the primary action is
//! a prefilled issue in the browser and the offline fallback is the saved
//! folder. Nothing here uploads anything by itself.

import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';
import 'dart:ui' as ui;

import 'package:flutter/rendering.dart';
import 'package:flutter/widgets.dart' show GlobalKey;

import 'app_log.dart';
import 'update/updater.dart';
import 'version.dart';

/// Wraps the whole app so a report can include what the user actually saw,
/// including an open dialog or settings page.
final GlobalKey reportBoundaryKey = GlobalKey(debugLabel: 'parley-report');

class ReportResult {
  const ReportResult({
    required this.directory,
    required this.summary,
    required this.issueUrl,
    required this.files,
  });

  final Directory directory;

  /// The human-readable environment summary, also shown in the settings card.
  final String summary;

  final Uri issueUrl;
  final List<String> files;

  String get path => directory.path;
}

/// Build and write a report. All inputs are best-effort: a missing log or a
/// failed command is recorded as unavailable rather than failing the report.
Future<ReportResult> buildReport({
  required String description,
  required bool includeLogs,
  required bool includeScreenshot,
}) async {
  AppLog.flush();
  final stamp = DateTime.now()
      .toUtc()
      .toIso8601String()
      .replaceAll(':', '-')
      .split('.')
      .first;
  final directory = Directory(
    '${reportDir().path}${Platform.pathSeparator}parley-report-$stamp',
  )..createSync(recursive: true);

  final summary = await collectEnvironment();
  final files = <String>[];

  String copyIfPresent(File source, String name) {
    try {
      if (!source.existsSync()) return '';
      final target = File('${directory.path}${Platform.pathSeparator}$name');
      source.copySync(target.path);
      files.add(name);
      return name;
    } on Object catch (e) {
      AppLog.write('report', 'could not copy ${source.path}: $e');
      return '';
    }
  }

  String writeText(String name, String text) {
    try {
      File('${directory.path}${Platform.pathSeparator}$name')
          .writeAsStringSync(text, flush: true);
      files.add(name);
      return name;
    } on Object catch (e) {
      AppLog.write('report', 'could not write $name: $e');
      return '';
    }
  }

  final separator = Platform.pathSeparator;
  final logs = logDir();
  if (includeLogs) {
    copyIfPresent(
      File('${logs.path}${separator}session-current.log'),
      'session-current.log',
    );
    copyIfPresent(
      File('${logs.path}${separator}session-previous.log'),
      'session-previous.log',
    );
    copyIfPresent(
      File('${logs.path}${separator}rust-current.log'),
      'rust-current.log',
    );
    copyIfPresent(
      File('${logs.path}${separator}rust-previous.log'),
      'rust-previous.log',
    );
    copyIfPresent(
      File('${updateRoot().path}${separator}update.log'),
      'update.log',
    );
  }

  writeText('system.txt', summary);

  if (includeScreenshot) {
    try {
      final png = await captureScreenshot();
      if (png != null) {
        final name = 'screenshot.png';
        File('${directory.path}$separator$name')
            .writeAsBytesSync(png, flush: true);
        files.add(name);
      }
    } on Object catch (e) {
      AppLog.write('report', 'could not capture screenshot: $e');
    }
  }

  final markdown = StringBuffer()
    ..writeln('# Parley bug report')
    ..writeln()
    ..writeln(
      '- Parley: $parleyVersion${parleyBuild.isEmpty ? '' : ' ($parleyBuild)'}',
    )
    ..writeln('- Created: ${DateTime.now().toIso8601String()}')
    ..writeln('- Files: ${files.isEmpty ? 'none' : files.join(', ')}')
    ..writeln()
    ..writeln('## What happened')
    ..writeln()
    ..writeln(
      description.trim().isEmpty ? '_(not filled in)_' : description.trim(),
    )
    ..writeln()
    ..writeln('## Environment')
    ..writeln()
    ..writeln('```text')
    ..writeln(summary)
    ..writeln('```');
  writeText('report.md', markdown.toString());

  final issueUrl = githubIssueUrl(description, summary, directory.path);
  AppLog.write('report', 'saved ${directory.path} (${files.length} files)');
  return ReportResult(
    directory: directory,
    summary: summary,
    issueUrl: issueUrl,
    files: files,
  );
}

/// A screenshot of [reportBoundaryKey], or null when it is not mounted (for
/// example a headless test). The PNG is a convenience, never a requirement.
Future<Uint8List?> captureScreenshot() async {
  final context = reportBoundaryKey.currentContext;
  final boundary = context?.findRenderObject();
  if (boundary is! RenderRepaintBoundary) return null;
  final image = await boundary.toImage(pixelRatio: 1.5);
  try {
    final data = await image.toByteData(format: ui.ImageByteFormat.png);
    return data?.buffer.asUint8List();
  } finally {
    image.dispose();
  }
}

/// The prefilled issue. The report folder itself cannot be attached through
/// a URL, so the body tells the reporter where it is and asks for the files.
Uri githubIssueUrl(String description, String summary, String reportPath) {
  final firstLine = description
      .split('\n')
      .map((line) => line.trim())
      .firstWhere((line) => line.isNotEmpty, orElse: () => 'Problem report');
  final title =
      '[Bug] ${firstLine.length > 90 ? '${firstLine.substring(0, 90)}…' : firstLine}';
  final compact = summary.length > 2200
      ? '${summary.substring(0, 2200)}\n…'
      : summary;
  final body =
      '''
### What happened
${description.trim().isEmpty ? '_Please describe the problem._' : description.trim()}

### Build and environment
```text
$compact
```

### Diagnostic report
A report folder was saved at:
`$reportPath`

Please attach the files from that folder, especially `session-current.log`,
`session-previous.log`, `screenshot.png` and `system.txt`, by dragging them
into this issue. They stay on this machine until you do.

---
Created from Parley's Report a problem page.
''';
  return Uri.https('github.com', '/$updateRepo/issues/new', {
    'title': title,
    'body': body,
  });
}

/// Open a URL in the platform's browser without adding a URL-launcher
/// dependency. Returns false when no opener is available.
Future<bool> openExternalUrl(Uri url) async {
  try {
    final Process process;
    if (Platform.isWindows) {
      process = await Process.start('cmd', ['/c', 'start', '', url.toString()]);
    } else if (Platform.isMacOS) {
      process = await Process.start('open', [url.toString()]);
    } else if (Platform.isAndroid) {
      process = await Process.start('am', [
        'start',
        '-a',
        'android.intent.action.VIEW',
        '-d',
        url.toString(),
      ]);
    } else {
      process = await Process.start('xdg-open', [url.toString()]);
    }
    // Do not wait for a browser to exit; just detect an immediate failure.
    final code = await process.exitCode.timeout(
      const Duration(seconds: 2),
      onTimeout: () => 0,
    );
    return code == 0;
  } on Object catch (e) {
    AppLog.write('report', 'could not open $url: $e');
    return false;
  }
}

/// Collect the build, machine, desktop and audio/video driver details the
/// report needs. Commands are best-effort and bounded; a missing tool is
/// written as unavailable.
Future<String> collectEnvironment() async {
  final out = StringBuffer()
    ..writeln('Parley: $parleyVersion')
    ..writeln('Build: ${parleyBuild.isEmpty ? 'dev / unknown' : parleyBuild}')
    ..writeln('Executable: ${Platform.resolvedExecutable}')
    ..writeln(
      'OS: ${Platform.operatingSystem} ${Platform.operatingSystemVersion}',
    )
    ..writeln('Processors: ${Platform.numberOfProcessors}')
    ..writeln('Locale: ${Platform.localeName}')
    ..writeln('Dart VM: ${Platform.version}')
    ..writeln();

  for (final name in const [
    'XDG_CURRENT_DESKTOP',
    'XDG_SESSION_TYPE',
    'XDG_SESSION_DESKTOP',
    'DESKTOP_SESSION',
    'WAYLAND_DISPLAY',
    'DISPLAY',
    'LANG',
    'LC_ALL',
    'PIPEWIRE_RUNTIME_DIR',
    'PULSE_SERVER',
  ]) {
    final value = Platform.environment[name];
    if (value != null && value.isNotEmpty) out.writeln('$name=$value');
  }
  out.writeln();

  if (Platform.isLinux) {
    final osRelease = _readText(File('/etc/os-release'));
    if (osRelease.isNotEmpty) {
      out
        ..writeln('--- /etc/os-release ---')
        ..writeln(_releaseName(osRelease))
        ..writeln();
    }
    await _run(out, 'uname', ['-a']);
    await _run(out, 'lspci', ['-nnk']);
    await _run(out, 'aplay', ['-l']);
    await _run(out, 'arecord', ['-l']);
    await _run(out, 'pactl', ['info']);
    await _run(out, 'wpctl', ['status']);
    await _run(out, 'pipewire', ['--version']);
    await _run(out, 'wireplumber', ['--version']);
    await _run(out, 'vainfo', []);
    await _run(out, 'nvidia-smi', [
      '--query-gpu=name,driver_version',
      '--format=csv,noheader',
    ]);
    await _run(out, 'systemctl', [
      '--user',
      '--no-pager',
      'status',
      'pipewire',
      'wireplumber',
    ]);
  } else if (Platform.isWindows) {
    await _run(out, 'powershell.exe', [
      '-NoProfile',
      '-NonInteractive',
      '-Command',
      r'''
$ErrorActionPreference = 'SilentlyContinue'
'--- OS ---'
Get-CimInstance Win32_OperatingSystem | Select-Object Caption,Version,BuildNumber,OSArchitecture | Format-List
'--- CPU ---'
Get-CimInstance Win32_Processor | Select-Object Name,NumberOfCores,NumberOfLogicalProcessors | Format-List
'--- GPU / driver ---'
Get-CimInstance Win32_VideoController | Select-Object Name,DriverVersion,DriverDate,AdapterRAM | Format-List
'--- Audio devices ---'
Get-CimInstance Win32_SoundDevice | Select-Object Name,Manufacturer,Status,DeviceID | Format-List
'--- Audio endpoints ---'
Get-PnpDevice -Class AudioEndpoint | Select-Object FriendlyName,Status,InstanceId | Format-List
''',
    ], timeout: const Duration(seconds: 8));
  } else if (Platform.isMacOS) {
    await _run(out, 'system_profiler', [
      'SPHardwareDataType',
      'SPAudioDataType',
    ]);
  }

  final updateLog = File(
    '${updateRoot().path}${Platform.pathSeparator}update.log',
  );
  if (updateLog.existsSync()) {
    out
      ..writeln('--- update.log (tail) ---')
      ..writeln(_tail(_readText(updateLog), 80))
      ..writeln();
  }
  return out.toString();
}

String _releaseName(String text) {
  for (final line in const LineSplitter().convert(text)) {
    if (line.startsWith('PRETTY_NAME=')) {
      return line.substring('PRETTY_NAME='.length).replaceAll('"', '');
    }
  }
  return _tail(text, 8);
}

String _readText(File file) {
  try {
    return file.readAsStringSync();
  } on Object {
    return '';
  }
}

String _tail(String text, int lines) {
  final all = const LineSplitter().convert(text);
  return all.length <= lines
      ? text
      : all.sublist(all.length - lines).join('\n');
}

Future<void> _run(
  StringBuffer out,
  String executable,
  List<String> arguments, {
  Duration timeout = const Duration(seconds: 3),
}) async {
  out.writeln('--- $executable ${arguments.join(' ')} ---');
  try {
    final result = await Process.run(
      executable,
      arguments,
      stdoutEncoding: utf8,
      stderrEncoding: utf8,
      runInShell: false,
    ).timeout(timeout);
    final text = '${result.stdout}${result.stderr}'.trim();
    out.writeln(
      text.isEmpty
          ? '(no output, exit ${result.exitCode})'
          : '${text.length > 14000 ? '${text.substring(0, 14000)}\n…' : text}\n(exit ${result.exitCode})',
    );
  } on Object {
    out.writeln('(unavailable or timed out)');
  }
  out.writeln();
}
