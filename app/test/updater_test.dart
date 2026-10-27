import 'package:flutter_test/flutter_test.dart';
import 'package:parley/src/update/notes.dart';
import 'package:parley/src/update/updater.dart';

/// A release body shaped like `GET /repos/{repo}/releases/latest`.
Map<String, dynamic> releaseBody(
  String tag, {
  bool linuxAsset = true,
  bool windowsAsset = true,
  bool sums = true,
  String sumsName = 'SHA256SUMS',
}) {
  final version = tag.startsWith('v') ? tag.substring(1) : tag;
  return {
    'tag_name': tag,
    'name': 'Parley $tag',
    'assets': [
      if (linuxAsset)
        {
          'name': 'parley-$version-linux-x86_64.tar.gz',
          'browser_download_url': 'https://example.invalid/linux.tar.gz',
        },
      if (windowsAsset)
        {
          'name': 'parley-$version-windows-x86_64.zip',
          'browser_download_url': 'https://example.invalid/windows.zip',
        },
      if (sums)
        {
          'name': sumsName,
          'browser_download_url': 'https://example.invalid/$sumsName',
        },
    ],
  };
}

void main() {
  group('version order', () {
    test('a newer patch wins, including two digits', () {
      expect(Version.parse('v0.4.4') > Version.parse('0.4.3'), isTrue);
      expect(Version.parse('v0.4.10') > Version.parse('0.4.9'), isTrue);
      expect(Version.parse('0.4.3') > Version.parse('0.4.4'), isFalse);
    });

    test('the tag prefix and build metadata do not count', () {
      expect(Version.parse('v0.4.3') == Version.parse('0.4.3+15'), isTrue);
      expect(Version.parse('0.4') == Version.parse('0.4.0'), isTrue);
    });

    test('a prerelease sorts below its own release', () {
      expect(Version.parse('0.4.4-rc1') < Version.parse('0.4.4'), isTrue);
      expect(Version.parse('v0.5.0-rc1') > Version.parse('0.4.4'), isTrue);
    });

    test('an unparsable tag is not an accident', () {
      expect(Version.parse('dev'), const Version(0, 0, 0));
      expect(Version.parse('') < Version.parse('0.0.1'), isTrue);
    });
  });

  group('release decision', () {
    test('an equal or older release is up to date', () {
      final same = evaluateRelease(
        releaseBody('v0.4.3'),
        current: '0.4.3',
        windows: false,
      );
      expect(same.status, UpdateStatus.upToDate);
      expect(same.latest, isNull);

      final older = evaluateRelease(
        releaseBody('v0.4.2'),
        current: '0.4.3',
        windows: false,
      );
      expect(older.status, UpdateStatus.upToDate);
    });

    test('a newer release offers this platform\'s build', () {
      final linux = evaluateRelease(
        releaseBody('v0.4.4'),
        current: '0.4.3',
        windows: false,
        repo: 'DartLumix/Parley',
      );
      expect(linux.status, UpdateStatus.available);
      expect(linux.latest, '0.4.4');
      expect(linux.assetName, 'parley-0.4.4-linux-x86_64.tar.gz');
      expect(linux.assetUrl, 'https://example.invalid/linux.tar.gz');
      expect(linux.sumsUrl, 'https://example.invalid/SHA256SUMS');

      final windows = evaluateRelease(
        releaseBody('v0.4.4'),
        current: '0.4.3',
        windows: true,
      );
      expect(windows.assetName, 'parley-0.4.4-windows-x86_64.zip');
      expect(windows.assetUrl, 'https://example.invalid/windows.zip');
    });

    test('a release with no build for this platform is blocked, not failed',
        () {
      final check = evaluateRelease(
        releaseBody('v0.4.4', windowsAsset: false),
        current: '0.4.3',
        windows: true,
        repo: 'DartLumix/Parley',
      );
      expect(check.status, UpdateStatus.blocked);
      expect(check.detail, contains('parley-0.4.4-windows-x86_64.zip'));
      expect(check.detail, contains('DartLumix/Parley'));
    });

    test('an unverifiable release is blocked', () {
      final check = evaluateRelease(
        releaseBody('v0.4.4', sums: false),
        current: '0.4.3',
        windows: false,
      );
      expect(check.status, UpdateStatus.blocked);
      expect(check.detail, contains('SHA256SUMS'));
    });

    test('a read-only install blocks the install, not the check', () {
      final check = evaluateRelease(
        releaseBody('v0.4.4'),
        current: '0.4.3',
        windows: false,
        blocker: 'this copy is not writable by your user (/opt/parley)',
      );
      expect(check.status, UpdateStatus.blocked);
      expect(check.latest, '0.4.4');
      expect(check.detail, contains('/opt/parley'));
    });
  });

  group('checksums', () {
    const sums = '''
f0dc243ab2be56f634ec4fc9b8ac0be19c9c37ce776d7c453413f91e71bece6d  parley-0.4.4-linux-x86_64.tar.gz
0d9ba46b5e6b8d9f5b0b0f0e0d0c0b0a0908070605040302010f0e0d0c0b0a09 *parley-0.4.4-windows-x86_64.zip

not a checksum line
''';

    test('finds an entry by file name', () {
      expect(
        sha256FromSums(sums, 'parley-0.4.4-linux-x86_64.tar.gz'),
        'f0dc243ab2be56f634ec4fc9b8ac0be19c9c37ce776d7c453413f91e71bece6d',
      );
      expect(
        sha256FromSums(sums, 'parley-0.4.4-windows-x86_64.zip'),
        '0d9ba46b5e6b8d9f5b0b0f0e0d0c0b0a0908070605040302010f0e0d0c0b0a09',
      );
    });

    test('a file that is not listed is not hashed', () {
      expect(sha256FromSums(sums, 'parley-0.4.5-linux-x86_64.tar.gz'), isNull);
      expect(sha256FromSums('', 'parley-0.4.4-linux-x86_64.tar.gz'), isNull);
      expect(sha256FromSums('not a checksum line', 'x'), isNull);
    });
  });

  group('swap scripts', () {
    test('the Linux script waits for this pid and quotes every path', () {
      final script = linuxSwapScript(
        pid: 1234,
        dest: '/home/a b/.local/opt/parley',
        archive: '/home/a b/.cache/parley/update-0.4.4/download/linux.tar.gz',
        newDir: '/home/a b/.cache/parley/update-0.4.4/new',
        bin: 'parley',
        log: '/home/a b/.cache/parley/update.log',
      );

      expect(script, contains('APP_PID=1234'));
      expect(script, contains("DEST='/home/a b/.local/opt/parley'"));
      expect(script, contains("LOG='/home/a b/.cache/parley/update.log'"));
      expect(script, contains('while kill -0 "\$APP_PID"'));
      // The staged download is the whole tarball, so the install directory is
      // its single top-level entry.
      expect(script, contains('tar -xzf "\$ARCHIVE" -C "\$NEW" --strip-components=1'));
      // A swap that cannot happen must restore rather than leave a hole.
      expect(script, contains('mv "\$OLD" "\$DEST"'));
      expect(script, contains('setsid "\$DEST/\$BIN"'));
    });

    test('a path with a quote stays one word for the shell', () {
      final script = linuxSwapScript(
        pid: 1,
        dest: "/home/o'brien/parley",
        archive: '/a',
        newDir: '/b',
        bin: 'parley',
        log: '/c',
      );
      expect(script, contains("DEST='/home/o'\\''brien/parley'"));
    });

    test('the Windows script retries the move and starts the new build', () {
      final script = windowsSwapScript(
        pid: 42,
        dest: r'C:\Users\a b\Parley',
        archive: r'C:\Users\a b\AppData\Local\parley\update-0.4.4\download\windows.zip',
        newDir: r'C:\Users\a b\AppData\Local\parley\update-0.4.4\new',
        bin: 'parley.exe',
        log: r'C:\Users\a b\AppData\Local\parley\update.log',
      );

      expect(script, contains(r'$appPid = 42'));
      expect(script, contains(r"$dest = 'C:\Users\a b\Parley'"));
      expect(script, contains('Get-Process -Id \$appPid'));
      expect(script, contains('Move-Item -LiteralPath \$dest'));
      expect(script, contains('Move-Item -LiteralPath \$old -Destination \$dest'));
      expect(script, contains('Start-Process'));
    });
  });

  group('notes', () {
    test('the authoring comment is not shown to the reader', () {
      expect(
        stripNotesComments('<!-- write these\nbefore tagging -->\n# Hi\n- one'),
        '# Hi\n- one',
      );
      expect(stripNotesComments('<!-- only a comment -->'), isEmpty);
    });
  });
}
