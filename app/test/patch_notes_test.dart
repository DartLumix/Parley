import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:parley/src/update/notes.dart';

/// A page with one button that runs the popup check, so a test can run it as
/// many times as a launch would.
Widget host({
  required File marker,
  required String version,
  required Future<String?> Function() load,
  required void Function() onRan,
}) =>
    MaterialApp(
      home: Builder(
        builder: (context) => Scaffold(
          body: Center(
            child: ElevatedButton(
              onPressed: () async {
                onRan();
                await maybeShowPatchNotes(
                  context,
                  version: version,
                  marker: marker,
                  load: load,
                );
              },
              child: const Text('launch'),
            ),
          ),
        ),
      ),
    );

void main() {
  late Directory dir;

  setUp(() => dir = Directory.systemTemp.createTempSync('parley-notes'));
  tearDown(() => dir.deleteSync(recursive: true));

  testWidgets('the notes show once per version', (tester) async {
    final marker = File('${dir.path}/release_notes_seen');
    var loads = 0;
    await tester.pumpWidget(
      host(
        marker: marker,
        version: '0.4.4',
        load: () async {
          loads++;
          return '# Fixed things\n- one';
        },
        onRan: () {},
      ),
    );

    await tester.tap(find.text('launch'));
    await tester.pumpAndSettle();

    expect(find.text('Fixed things'), findsOneWidget);
    expect(find.text('one'), findsOneWidget);
    expect(find.text("What's new in 0.4.4"), findsOneWidget);
    expect(marker.readAsStringSync().trim(), '0.4.4');

    await tester.tap(find.text('Close'));
    await tester.pumpAndSettle();

    // The next launch of the same version is an ordinary one.
    await tester.tap(find.text('launch'));
    await tester.pumpAndSettle();
    expect(find.text('Fixed things'), findsNothing);
    expect(loads, 1, reason: 'the notes are not even read again');
  });

  testWidgets('an update shows the notes of the version that started',
      (tester) async {
    final marker = File('${dir.path}/release_notes_seen')
      ..writeAsStringSync('0.4.3');
    await tester.pumpWidget(
      host(
        marker: marker,
        version: '0.4.4',
        load: () async => '- the new build',
        onRan: () {},
      ),
    );

    await tester.tap(find.text('launch'));
    await tester.pumpAndSettle();

    expect(find.text('the new build'), findsOneWidget);
    expect(marker.readAsStringSync().trim(), '0.4.4');
  });

  testWidgets('a build with no notes records the version anyway',
      (tester) async {
    final marker = File('${dir.path}/release_notes_seen');
    await tester.pumpWidget(
      host(
        marker: marker,
        version: '0.4.4',
        load: () async => null,
        onRan: () {},
      ),
    );

    await tester.tap(find.text('launch'));
    await tester.pumpAndSettle();

    expect(find.byType(AlertDialog), findsNothing);
    expect(marker.readAsStringSync().trim(), '0.4.4');
  });

  testWidgets('a source build never announces itself', (tester) async {
    final marker = File('${dir.path}/release_notes_seen');
    await tester.pumpWidget(
      host(
        marker: marker,
        version: 'dev',
        load: () async => '- should not be shown',
        onRan: () {},
      ),
    );

    await tester.tap(find.text('launch'));
    await tester.pumpAndSettle();

    expect(find.text('should not be shown'), findsNothing);
    expect(marker.existsSync(), isFalse);
  });

  testWidgets('the notes bundled with the build are readable', (tester) async {
    // The file is declared in pubspec.yaml; a build that loses it would pop
    // up an empty dialog on every update.
    final notes = await loadPatchNotes();
    expect(notes, isNotNull);
    expect(notes, isNot(contains('<!--')));
  });
}
