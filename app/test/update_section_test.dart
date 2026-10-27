import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:parley/src/update/updater.dart';
import 'package:parley/src/widgets/update_section.dart';

Widget host(Widget child) => MaterialApp(
      home: Scaffold(body: SingleChildScrollView(child: child)),
    );

const available = UpdateCheck.available(
  current: '0.4.3',
  latest: '0.4.4',
  assetName: 'parley-0.4.4-linux-x86_64.tar.gz',
  assetUrl: 'https://example.invalid/linux.tar.gz',
  sumsUrl: 'https://example.invalid/SHA256SUMS',
);

Future<void> check(WidgetTester tester) async {
  await tester.tap(find.text('Check for updates'));
  await tester.pumpAndSettle();
}

void main() {
  testWidgets('an up-to-date check says so and offers no install',
      (tester) async {
    await tester.pumpWidget(
      host(UpdateSection(check: () async => const UpdateCheck.upToDate('0.4.3'))),
    );
    await check(tester);

    expect(find.text('You are on the latest release (0.4.3).'), findsOneWidget);
    expect(find.text('Install 0.4.4 and restart'), findsNothing);
  });

  testWidgets('an available release installs after a confirmation',
      (tester) async {
    // The install stays pending, so the test can drive the progress the
    // download would report — from the test body, never from inside the
    // callback, which no longer counts as a guarded test call.
    final done = Completer<void>();
    void Function(double? progress) report = (double? _) {};
    var installed = false;
    await tester.pumpWidget(
      host(
        UpdateSection(
          check: () async => available,
          installAndRestart: (check, onProgress) {
            report = onProgress;
            installed = true;
            return done.future;
          },
        ),
      ),
    );
    await check(tester);

    expect(
      find.text('Version 0.4.4 is available — you have 0.4.3.'),
      findsOneWidget,
    );

    await tester.tap(find.text('Install 0.4.4 and restart'));
    await tester.pumpAndSettle();
    // The restart ends a call in progress, so it asks first.
    expect(find.text('Install 0.4.4 and restart?'), findsOneWidget);
    expect(installed, isFalse);

    await tester.tap(find.text('Install and restart'));
    // No pumpAndSettle from here: while the install runs the card shows an
    // indeterminate progress bar, which never settles by design.
    for (var i = 0; i < 4; i++) {
      await tester.pump(const Duration(milliseconds: 50));
    }
    expect(installed, isTrue);

    report(0.25);
    await tester.pump();
    expect(
      tester
          .widget<LinearProgressIndicator>(
            find.byType(LinearProgressIndicator),
          )
          .value,
      0.25,
    );
    expect(find.text('Downloading…'), findsOneWidget);

    // A null fraction is the verification step, which has no percentage.
    report(null);
    await tester.pump();
    expect(find.text('Verifying and installing…'), findsOneWidget);

    done.complete();
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 50));
    expect(find.text('Restarting…'), findsOneWidget);
  });

  testWidgets('cancelling the confirmation installs nothing', (tester) async {
    var installed = false;
    await tester.pumpWidget(
      host(
        UpdateSection(
          check: () async => available,
          installAndRestart: (check, onProgress) async => installed = true,
        ),
      ),
    );
    await check(tester);
    await tester.tap(find.text('Install 0.4.4 and restart'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();

    expect(installed, isFalse);
    expect(find.textContaining('Version 0.4.4 is available'), findsOneWidget);
  });

  testWidgets('a blocked release explains itself without an install button',
      (tester) async {
    await tester.pumpWidget(
      host(
        UpdateSection(
          check: () async => const UpdateCheck.blocked(
            current: '0.4.3',
            latest: '0.4.4',
            detail: 'this copy is not writable by your user (/opt/parley)',
          ),
        ),
      ),
    );
    await check(tester);

    expect(find.textContaining('Version 0.4.4 is available, but'), findsOneWidget);
    expect(find.textContaining('/opt/parley'), findsOneWidget);
    expect(find.text('Install 0.4.4 and restart'), findsNothing);
  });

  testWidgets('a failed check is reported, not hidden', (tester) async {
    await tester.pumpWidget(
      host(
        UpdateSection(
          check: () async => const UpdateCheck.failed(
            current: '0.4.3',
            detail: 'no releases at DartLumix/Parley',
          ),
        ),
      ),
    );
    await check(tester);

    expect(
      find.text('Could not check: no releases at DartLumix/Parley.'),
      findsOneWidget,
    );
  });

  testWidgets("What's new reads the notes bundled with this build",
      (tester) async {
    await tester.pumpWidget(
      host(
        UpdateSection(
          loadNotes: () async => '# Release notes\n- a selectable bullet',
        ),
      ),
    );
    await tester.tap(find.text("What's new"));
    await tester.pumpAndSettle();

    expect(find.text('Release notes'), findsOneWidget);
    expect(find.text('a selectable bullet'), findsOneWidget);
    await tester.tap(find.text('Close'));
    await tester.pumpAndSettle();
    expect(find.text('a selectable bullet'), findsNothing);
  });

  testWidgets('a build without notes says so', (tester) async {
    await tester.pumpWidget(host(UpdateSection(loadNotes: () async => null)));
    await tester.tap(find.text("What's new"));
    await tester.pumpAndSettle();

    expect(find.text('this build carries no release notes'), findsOneWidget);
  });
}
