import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:parley/src/rust/api/settings.dart';
import 'package:parley/src/widgets/settings_page.dart';
import 'package:parley/src/theme.dart';

/// The settings page's job is to move one persisted value per interaction
/// and never to leave the UI and the file disagreeing. The Rust half is
/// injected, so these tests are about the wiring — the round-trip through
/// `settings.toml` is covered by the Rust test next to the module.
void main() {
  const initial = AppSettings(
    hwEncode: false,
    showSharePreview: true,
    shareHeight: 0,
    shareFps: 0,
    echoCancel: true,
  );

  Widget host(List<AppSettings> saved, {bool hw = true}) {
    return MaterialApp(
      theme: buildParleyTheme(),
      home: SettingsPage(
        settings: initial,
        hwEncodeSupported: hw,
        server: 'chat.example:4443',
        user: 'elliot',
        onSave: (s) async => saved.add(s),
      ),
    );
  }

  testWidgets('toggling a switch persists exactly that change',
      (tester) async {
    final saved = <AppSettings>[];
    await tester.pumpWidget(host(saved));
    await tester.pumpAndSettle();

    await tester.tap(find.text('Echo cancellation'));
    await tester.pumpAndSettle();

    expect(saved, hasLength(1));
    expect(saved.single.echoCancel, isFalse);
    // Everything else is carried over, not reset to defaults.
    expect(saved.single.shareHeight, initial.shareHeight);
    expect(saved.single.showSharePreview, initial.showSharePreview);
  });

  testWidgets('the preview lives on the screen tile, not in Settings',
      (tester) async {
    await tester.pumpWidget(host(<AppSettings>[]));
    await tester.pumpAndSettle();
    expect(find.text('Show my own stream while sharing'), findsNothing);
    expect(find.textContaining('small preview'), findsNothing);
  });

  testWidgets('resolution and frame-rate choices persist', (tester) async {
    final saved = <AppSettings>[];
    await tester.pumpWidget(host(saved));
    await tester.pumpAndSettle();

    // Open the resolution dropdown (the first one on the page).
    await tester.tap(find.byType(DropdownButton<int>).first);
    await tester.pumpAndSettle();
    await tester.tap(find.text('720p').last);
    await tester.pumpAndSettle();

    expect(saved.last.shareHeight, 720);

    await tester.tap(find.byType(DropdownButton<int>).last);
    await tester.pumpAndSettle();
    await tester.tap(find.text('30 fps').last);
    await tester.pumpAndSettle();

    expect(saved.last.shareFps, 30);
    expect(saved.last.shareHeight, 720, reason: 'the two choices compose');
  });

  testWidgets('the account section shows the live session and sign-out',
      (tester) async {
    var signedOut = false;
    await tester.pumpWidget(
      MaterialApp(
        theme: buildParleyTheme(),
        home: SettingsPage(
          settings: initial,
          hwEncodeSupported: false,
          server: 'chat.example:4443',
          user: 'elliot',
          onSave: (_) async {},
          onSignOut: () async => signedOut = true,
        ),
      ),
    );
    await tester.pumpAndSettle();

    expect(find.text('chat.example:4443'), findsOneWidget);
    expect(find.text('elliot'), findsOneWidget);
    // No hardware switch when the build has no VAAPI backend.
    expect(find.text('Hardware encoding (VAAPI)'), findsNothing);

    // The account section is at the bottom of a scrollable page.
    await tester.scrollUntilVisible(find.text('Sign out'), 200);
    await tester.tap(find.text('Sign out'));
    await tester.pumpAndSettle();
    expect(signedOut, isTrue);
  });
}
