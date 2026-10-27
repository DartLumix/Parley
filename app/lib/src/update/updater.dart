//! In-app updates: ask the release channel, download the new build, check it
//! against the release's own `SHA256SUMS`, then hand the swap to a small
//! script and restart into it.
//!
//! A running app cannot replace its own binary and libraries, so the last
//! step belongs to a script that outlives the process: it waits for this
//! exact pid to disappear, unpacks the staged download next to (and then
//! over) the install directory, and starts the new build. Nothing is removed
//! until the unpacking has succeeded, and a failed rename restores the old
//! directory — the app either comes back newer or comes back as it was.
//! Everything the script does is appended to `update.log` beside the
//! download (see [updateRoot]), which is the first place to look when the
//! app does not come back.
//!
//! The channel is this repository's own release page: the release workflow
//! attaches the Linux tarball, the Windows zip and a `SHA256SUMS` to every
//! `v*` tag, which is exactly what the updater reads — so a release needs no
//! extra publishing step. `releases/latest` needs no token, GitHub skips
//! drafts and prereleases for it, and every download is verified against the
//! release's checksums before anything is installed.

import 'dart:convert';
import 'dart:io';

import 'package:crypto/crypto.dart' show sha256;
import 'package:flutter/foundation.dart' show visibleForTesting;

import '../version.dart';
import '../window.dart' show closeWindow;

/// `owner/repo` the updater reads releases from. Override it for a fork or a
/// test channel without touching this file:
///
///     flutter build linux --dart-define=PARLEY_UPDATE_REPO=owner/repo
const String _defaultRepo = 'DartLumix/Parley';
const String _repoOverride = String.fromEnvironment('PARLEY_UPDATE_REPO');

/// The release repository in use.
String get updateRepo => _repoOverride.isEmpty ? _defaultRepo : _repoOverride;

/// GitHub wants a User-Agent on every API call; this one also says which
/// build is asking, which is all a rate limit would ever show.
const String _userAgent = 'parley-updater/$parleyVersion';

/// A `vX.Y.Z[-pre]` release tag.
///
/// Parsing is tolerant on purpose: the updater must never crash or refuse to
/// compare because a tag looks odd. Anything that is not a number counts as
/// zero, and a pre-release (`0.4.4-rc1`) sorts below its own release.
class Version implements Comparable<Version> {
  const Version(this.major, this.minor, this.patch, [this.pre = '']);

  factory Version.parse(String tag) {
    var text = tag.trim();
    if (text.startsWith('v') || text.startsWith('V')) text = text.substring(1);
    // Build metadata (`+15`) never orders anything.
    final plus = text.indexOf('+');
    if (plus >= 0) text = text.substring(0, plus);
    var pre = '';
    final dash = text.indexOf('-');
    if (dash >= 0) {
      pre = text.substring(dash + 1);
      text = text.substring(0, dash);
    }
    final parts = text.split('.');
    int at(int index) =>
        index < parts.length ? (int.tryParse(parts[index].trim()) ?? 0) : 0;
    return Version(at(0), at(1), at(2), pre);
  }

  final int major;
  final int minor;
  final int patch;

  /// Everything after the first `-`; empty for a release.
  final String pre;

  @override
  int compareTo(Version other) {
    if (major != other.major) return major.compareTo(other.major);
    if (minor != other.minor) return minor.compareTo(other.minor);
    if (patch != other.patch) return patch.compareTo(other.patch);
    if (pre.isEmpty != other.pre.isEmpty) return pre.isEmpty ? 1 : -1;
    return pre.compareTo(other.pre);
  }

  bool operator <(Version other) => compareTo(other) < 0;
  bool operator >(Version other) => compareTo(other) > 0;
  bool operator <=(Version other) => compareTo(other) <= 0;
  bool operator >=(Version other) => compareTo(other) >= 0;

  @override
  bool operator ==(Object other) =>
      other is Version &&
      major == other.major &&
      minor == other.minor &&
      patch == other.patch &&
      pre == other.pre;

  @override
  int get hashCode => Object.hash(major, minor, patch, pre);

  @override
  String toString() =>
      pre.isEmpty ? '$major.$minor.$patch' : '$major.$minor.$patch-$pre';
}

/// The asset names the release workflow publishes, for the platform this
/// build runs on. The mirror must carry the same names.
String assetNameFor(String version, {required bool windows}) => windows
    ? 'parley-$version-windows-x86_64.zip'
    : 'parley-$version-linux-x86_64.tar.gz';

/// Which platform's build a file describes — and with it, whether the
/// platform can unpack it itself (see the swap scripts).
bool get _windows => Platform.isWindows;

enum UpdateStatus {
  /// Nothing newer is published.
  upToDate,

  /// A newer release with a verified, installable build for this platform.
  available,

  /// A newer release exists but cannot be installed here; [UpdateCheck.detail]
  /// says why (a read-only install, a missing asset, no checksums).
  blocked,

  /// The channel itself could not be read.
  failed,
}

/// What the release channel says about this build.
class UpdateCheck {
  const UpdateCheck._({
    required this.status,
    required this.current,
    this.latest,
    this.assetName,
    this.assetUrl,
    this.sumsUrl,
    this.detail,
  });

  const UpdateCheck.upToDate(String current)
      : this._(status: UpdateStatus.upToDate, current: current);

  const UpdateCheck.available({
    required String current,
    required String latest,
    required String assetName,
    required String assetUrl,
    required String sumsUrl,
  }) : this._(
          status: UpdateStatus.available,
          current: current,
          latest: latest,
          assetName: assetName,
          assetUrl: assetUrl,
          sumsUrl: sumsUrl,
        );

  const UpdateCheck.blocked({
    required String current,
    required String latest,
    required String detail,
  }) : this._(
          status: UpdateStatus.blocked,
          current: current,
          latest: latest,
          detail: detail,
        );

  const UpdateCheck.failed({required String current, required String detail})
      : this._(status: UpdateStatus.failed, current: current, detail: detail);

  final UpdateStatus status;

  /// The version this build reports (`dev` for a source build).
  final String current;

  /// The published version, when it is newer than [current].
  final String? latest;

  final String? assetName;
  final String? assetUrl;
  final String? sumsUrl;

  /// A sentence for the user, for [UpdateStatus.blocked] and
  /// [UpdateStatus.failed].
  final String? detail;
}

/// Turn a `GET /repos/{repo}/releases/latest` body into a decision.
///
/// Pure, so the interesting cases — an equal version, a release without a
/// build for this platform, a release without checksums — are tested without
/// a network.
UpdateCheck evaluateRelease(
  Map<String, dynamic> release, {
  required String current,
  required bool windows,
  String repo = '',
  String? blocker,
}) {
  final latest = Version.parse((release['tag_name'] as String?) ?? '');
  if (latest <= Version.parse(current)) return UpdateCheck.upToDate(current);

  final version = latest.toString();
  final name = assetNameFor(version, windows: windows);
  final assets = (release['assets'] as List?) ?? const [];

  Map<String, dynamic>? find(String wanted) {
    for (final asset in assets) {
      if (asset is Map && asset['name'] == wanted) {
        return Map<String, dynamic>.from(asset);
      }
    }
    return null;
  }

  if (blocker != null) {
    return UpdateCheck.blocked(
      current: current,
      latest: version,
      detail: blocker,
    );
  }

  final url = find(name)?['browser_download_url'] as String?;
  if (url == null || url.isEmpty) {
    return UpdateCheck.blocked(
      current: current,
      latest: version,
      detail: 'release $version has no $name'
          '${repo.isEmpty ? '' : ' at $repo'} — publish it, or install this '
              'build by hand',
    );
  }

  final sums = find('SHA256SUMS')?['browser_download_url'] as String?;
  if (sums == null || sums.isEmpty) {
    return UpdateCheck.blocked(
      current: current,
      latest: version,
      detail: 'release $version has no SHA256SUMS, so the download could not '
          'be verified',
    );
  }

  return UpdateCheck.available(
    current: current,
    latest: version,
    assetName: name,
    assetUrl: url,
    sumsUrl: sums,
  );
}

/// Ask the channel. Never throws: a missing or private mirror, a rate limit
/// or a dead network all come back as [UpdateStatus.failed] with a reason the
/// settings page can show.
Future<UpdateCheck> checkForUpdate({HttpClient? client, String? current}) async {
  final now = current ?? parleyVersion;
  final repo = updateRepo;
  final owned = client ?? HttpClient();
  try {
    final answer = await _get(
      owned,
      Uri.parse('https://api.github.com/repos/$repo/releases/latest'),
    );
    if (answer.status == 404) {
      return UpdateCheck.failed(
        current: now,
        detail: 'no releases at $repo — the mirror is missing or private',
      );
    }
    if (answer.status == 403) {
      return UpdateCheck.failed(
        current: now,
        detail: 'GitHub refused the request (rate limit?); try again later',
      );
    }
    if (answer.status != 200) {
      return UpdateCheck.failed(
        current: now,
        detail: 'the release channel answered HTTP ${answer.status}',
      );
    }
    final decoded = jsonDecode(answer.body);
    if (decoded is! Map) {
      return UpdateCheck.failed(
        current: now,
        detail: 'unexpected answer from $repo',
      );
    }
    return evaluateRelease(
      Map<String, dynamic>.from(decoded),
      current: now,
      windows: _windows,
      repo: repo,
      blocker: await installBlocker(),
    );
  } on Object catch (e) {
    return UpdateCheck.failed(
      current: now,
      detail: 'could not reach github.com: $e',
    );
  } finally {
    if (client == null) owned.close(force: true);
  }
}

/// The install directory: where the running binary actually lives — the
/// unpacked tarball for a run-in-place copy, `~/.local/opt/parley` for
/// `install.sh`, `/opt/parley` for the AUR package.
Directory installDir() => File(Platform.resolvedExecutable).parent;

/// Why this copy cannot replace itself, or null when it can. A package
/// manager owns `/opt/parley`: pacman updates that one, not the app.
Future<String?> installBlocker() async {
  final dir = installDir();
  if (!dir.existsSync()) {
    return 'cannot find the install directory (${dir.path})';
  }
  final probe = File('${dir.path}$_sep.parley-update-probe');
  try {
    await probe.writeAsString('probe', flush: true);
    await probe.delete();
  } on Object catch (_) {
    return 'this copy is not writable by your user (${dir.path}) — a '
        'package-manager install is updated with pacman/yay, not from here';
  }
  return null;
}

/// Where downloads, the swap script and its log live. The cache directory,
/// never the install directory: the swap is a rename, so both must be on one
/// filesystem, and a half-finished download must not look like an install.
Directory updateRoot() {
  String? env(String name) {
    final value = Platform.environment[name];
    return (value == null || value.isEmpty) ? null : value;
  }

  final base = _windows
      ? (env('LOCALAPPDATA') ?? Directory.systemTemp.path)
      : (env('XDG_CACHE_HOME') ?? '${env('HOME') ?? Directory.systemTemp.path}/.cache');
  return Directory('$base${_sep}parley');
}

/// Download the release's build, verify it against `SHA256SUMS`, stage the
/// swap and restart into it.
///
/// A successful install ends this process — the script brings the new build
/// up — so this only returns (throws) when nothing was installed.
Future<void> installAndRestart(
  UpdateCheck check, {
  void Function(double? progress)? onProgress,
  HttpClient? client,
}) async {
  if (check.status != UpdateStatus.available) {
    throw const UpdateException('there is nothing to install');
  }
  final stage = Directory(
    '${updateRoot().path}${_sep}update-${check.latest}',
  );
  if (stage.existsSync()) stage.deleteSync(recursive: true);
  final downloads = Directory('${stage.path}${_sep}download')
    ..createSync(recursive: true);

  final owned = client ?? HttpClient();
  try {
    final sums = await _get(owned, Uri.parse(check.sumsUrl!));
    if (sums.status != 200) {
      throw UpdateException('SHA256SUMS answered HTTP ${sums.status}');
    }
    final expected = sha256FromSums(sums.body, check.assetName!);
    if (expected == null) {
      throw UpdateException(
        'SHA256SUMS does not list ${check.assetName!}; nothing was installed',
      );
    }

    final archive = await _download(
      owned,
      Uri.parse(check.assetUrl!),
      downloads,
      onProgress: onProgress,
    );
    // No percentage is meaningful here, and saying so is better than a bar
    // that sits at 100 % while a 40 MB file is hashed.
    onProgress?.call(null);
    final actual = await _sha256Of(archive);
    if (actual != expected) {
      throw UpdateException(
        'the download does not match SHA256SUMS (expected $expected, got '
        '$actual); nothing was installed',
      );
    }

    final root = updateRoot()..createSync(recursive: true);
    final log = File('${root.path}${_sep}update.log');
    final script = _windows
        ? _windowsScript(stage: stage, archive: archive, log: log)
        : _linuxScript(stage: stage, archive: archive, log: log);
    // The app writes the first line itself: when the swap never happens, a log
    // that ends here says the script did not start, and one that stops after
    // the next line says it did and hit something.
    log.writeAsStringSync(
      '${DateTime.now().toIso8601String()} parley $parleyVersion: staged '
      '${check.latest} in ${archive.path}; handing over to ${script.path}\n',
      mode: FileMode.append,
    );
    await _spawn(script);
  } finally {
    if (client == null) owned.close(force: true);
  }

  await _quit();
}

/// The swap, as a shell script: waits for this pid, unpacks the download over
/// the install directory, starts the new build.
File _linuxScript({
  required Directory stage,
  required File archive,
  required File log,
}) {
  final script = File('${stage.path}${_sep}update.sh');
  script.writeAsStringSync(
    linuxSwapScript(
      pid: pid,
      dest: installDir().path,
      archive: archive.path,
      newDir: '${stage.path}${_sep}new',
      bin: _binaryName,
      log: log.path,
    ),
  );
  return script;
}

/// The same for Windows, in PowerShell: `Move-Item` can fail while a handle
/// is still closing, so the move is retried instead of reported as a broken
/// install.
File _windowsScript({
  required Directory stage,
  required File archive,
  required File log,
}) {
  final script = File('${stage.path}${_sep}update.ps1');
  script.writeAsStringSync(
    windowsSwapScript(
      pid: pid,
      dest: installDir().path,
      archive: archive.path,
      newDir: '${stage.path}${_sep}new',
      bin: _binaryName,
      log: log.path,
    ),
  );
  return script;
}

/// The Linux swap script as text. Public because the quoting is the part a
/// test can check without running it: one unquoted space in a path and the
/// script replaces the wrong directory.
@visibleForTesting
String linuxSwapScript({
  required int pid,
  required String dest,
  required String archive,
  required String newDir,
  required String bin,
  required String log,
}) =>
    _fill(_linuxTemplate, {
      'PID': '$pid',
      'DEST': dest,
      'ARCHIVE': archive,
      'NEW': newDir,
      'BIN': bin,
      'LOG': log,
    }, powershell: false, raw: const {'PID'});

/// The Windows swap script as text (see [linuxSwapScript]).
@visibleForTesting
String windowsSwapScript({
  required int pid,
  required String dest,
  required String archive,
  required String newDir,
  required String bin,
  required String log,
}) =>
    _fill(_windowsTemplate, {
      'PID': '$pid',
      'DEST': dest,
      'ARCHIVE': archive,
      'NEW': newDir,
      'BIN': bin,
      'LOG': log,
    }, powershell: true, raw: const {'PID'});

const String _linuxTemplate = r'''
#!/bin/sh
# Written by the Parley updater. Runs after the app exits, replaces the
# install directory with the staged download and starts the new build.
# Nothing is removed until the unpacking has succeeded, and a failed install
# puts the previous directory back.
set -u

APP_PID=@PID@
DEST=@DEST@
ARCHIVE=@ARCHIVE@
NEW=@NEW@
BIN=@BIN@
LOG=@LOG@

exec >>"$LOG" 2>&1
echo "[$(date -Is 2>/dev/null || date)] parley update: replacing $DEST"

waited=0
while kill -0 "$APP_PID" 2>/dev/null; do
    waited=$((waited + 1))
    if [ "$waited" -gt 120 ]; then
        echo "parley did not exit within 60 s; leaving the install alone"
        exit 1
    fi
    sleep 0.5
done

rm -rf "$NEW"
if ! mkdir -p "$NEW"; then
    echo "cannot create $NEW"
    exit 1
fi
if ! tar -xzf "$ARCHIVE" -C "$NEW" --strip-components=1; then
    echo "cannot unpack $ARCHIVE (tar failed); the install is untouched"
    exit 1
fi
if [ ! -f "$NEW/$BIN" ]; then
    echo "the package does not contain $BIN; refusing to install"
    exit 1
fi
chmod +x "$NEW/$BIN" 2>/dev/null

OLD="$DEST.old.$$"
rm -rf "$OLD"
if ! mv "$DEST" "$OLD"; then
    echo "cannot move $DEST aside (read-only install?); the install is untouched"
    exit 1
fi
if mv "$NEW" "$DEST"; then
    rm -rf "$OLD"
    echo "installed; starting the new build"
    if command -v setsid >/dev/null 2>&1; then
        setsid "$DEST/$BIN" </dev/null >/dev/null 2>&1 &
    else
        nohup "$DEST/$BIN" </dev/null >/dev/null 2>&1 &
    fi
    exit 0
fi
echo "install failed; restoring the previous build"
mv "$OLD" "$DEST"
exit 1
''';

const String _windowsTemplate = r'''
# Written by the Parley updater. Runs after the app exits, replaces the
# install directory with the staged download and starts the new build.
$ErrorActionPreference = 'Stop'

$appPid = @PID@
$dest = @DEST@
$archive = @ARCHIVE@
$new = @NEW@
$bin = @BIN@
$log = @LOG@

function Write-Log([string]$message) {
    "$(Get-Date -Format o) $message" | Add-Content -LiteralPath $log
}

Write-Log "parley update: replacing $dest"

$deadline = (Get-Date).AddSeconds(90)
while (Get-Process -Id $appPid -ErrorAction SilentlyContinue) {
    if ((Get-Date) -gt $deadline) {
        Write-Log 'parley did not exit within 90 s; leaving the install alone'
        exit 1
    }
    Start-Sleep -Milliseconds 500
}

try {
    Remove-Item -Recurse -Force $new -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $new | Out-Null
    # The zip holds the bundle contents at its top level, so it unpacks
    # straight into the new directory.
    if (Get-Command tar.exe -ErrorAction SilentlyContinue) {
        & tar.exe -xf $archive -C $new
    } else {
        Expand-Archive -LiteralPath $archive -DestinationPath $new -Force
    }
    if (-not (Test-Path (Join-Path $new $bin))) {
        Write-Log "the package does not contain $bin; refusing to install"
        exit 1
    }

    $old = "$dest.old"
    Remove-Item -Recurse -Force $old -ErrorAction SilentlyContinue
    $moved = $false
    for ($i = 0; $i -lt 20 -and -not $moved; $i++) {
        try {
            Move-Item -LiteralPath $dest -Destination $old
            $moved = $true
        } catch {
            Start-Sleep -Milliseconds 500
        }
    }
    if (-not $moved) {
        Write-Log "cannot move $dest aside (still in use?); the install is untouched"
        exit 1
    }
    try {
        Move-Item -LiteralPath $new -Destination $dest
    } catch {
        Write-Log "install failed: $_; restoring the previous build"
        Move-Item -LiteralPath $old -Destination $dest
        exit 1
    }
    Remove-Item -Recurse -Force $old -ErrorAction SilentlyContinue
    Write-Log 'installed; starting the new build'
    Start-Process -FilePath (Join-Path $dest $bin) -WorkingDirectory $dest
    exit 0
} catch {
    Write-Log "update failed: $_"
    exit 1
}
''';

/// Start the swap script in its own session, so it survives this process —
/// which is the point: it is waiting for exactly that.
Future<void> _spawn(File script) async {
  if (_windows) {
    await Process.start(
      'powershell.exe',
      [
        '-NoProfile',
        '-ExecutionPolicy',
        'Bypass',
        '-WindowStyle',
        'Hidden',
        '-File',
        script.path,
      ],
      mode: ProcessStartMode.detached,
    );
  } else {
    await Process.start(
      '/bin/sh',
      [script.path],
      mode: ProcessStartMode.detached,
    );
  }
}

/// Close the window the way the toolbar does, then make sure the process is
/// really gone — the script is watching for that.
Future<void> _quit() async {
  if (!_windows) {
    await closeWindow();
  }
  await Future<void>.delayed(const Duration(milliseconds: 500));
  exit(0);
}

Future<File> _download(
  HttpClient client,
  Uri url,
  Directory into, {
  void Function(double? progress)? onProgress,
}) async {
  final name = url.pathSegments.isEmpty ? 'download' : url.pathSegments.last;
  final file = File('${into.path}$_sep$name');
  final request = await client.getUrl(url);
  request.headers.set(HttpHeaders.userAgentHeader, _userAgent);
  final response = await request.close();
  if (response.statusCode != 200) {
    throw UpdateException('the download answered HTTP ${response.statusCode}');
  }
  final total = response.contentLength;
  final sink = file.openWrite();
  var received = 0;
  try {
    await for (final chunk in response) {
      sink.add(chunk);
      received += chunk.length;
      onProgress?.call(total > 0 ? received / total : null);
    }
    await sink.flush();
  } finally {
    await sink.close();
  }
  onProgress?.call(1);
  return file;
}

Future<String> _sha256Of(File file) async {
  final digest = await sha256.bind(file.openRead()).first;
  return digest.toString();
}

/// One entry out of a `sha256sum`-style file, as lowercase hex, or null when
/// the file does not list [name].
String? sha256FromSums(String sums, String name) {
  for (final line in const LineSplitter().convert(sums)) {
    final match =
        RegExp(r'^([0-9a-fA-F]{64})\s+\*?(.+)$').firstMatch(line.trim());
    if (match == null) continue;
    final file = match.group(2)!.trim();
    if (file == name || file.endsWith('/$name')) {
      return match.group(1)!.toLowerCase();
    }
  }
  return null;
}

/// A failure the settings page shows verbatim.
class UpdateException implements Exception {
  const UpdateException(this.message);

  final String message;

  @override
  String toString() => message;
}

class _Response {
  const _Response(this.status, this.body);

  final int status;
  final String body;
}

Future<_Response> _get(HttpClient client, Uri url) async {
  final request = await client.getUrl(url);
  request.headers.set(HttpHeaders.userAgentHeader, _userAgent);
  request.headers.set(HttpHeaders.acceptHeader, 'application/vnd.github+json');
  request.followRedirects = true;
  final response = await request.close();
  return _Response(
    response.statusCode,
    await response.transform(utf8.decoder).join(),
  );
}

String _fill(
  String template,
  Map<String, String> values, {
  required bool powershell,
  Set<String> raw = const {},
}) {
  var text = template;
  values.forEach((key, value) {
    text = text.replaceAll(
      '@$key@',
      raw.contains(key) ? value : _quoted(value, powershell: powershell),
    );
  });
  return text;
}

/// Single quotes for `sh`, doubled ones for PowerShell — both keep a path
/// with spaces (or a stray quote) intact.
String _quoted(String value, {required bool powershell}) => powershell
    ? "'${value.replaceAll("'", "''")}'"
    : "'${value.replaceAll("'", "'\\''")}'";

String get _binaryName => File(Platform.resolvedExecutable).uri.pathSegments.last;

String get _sep => Platform.pathSeparator;
