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
    echoLevel: 2,
    noiseLevel: 2,
    inputDevice: '',
    outputDevice: '',
    micAutogain: false,
    micAutogainTargetDb: -18,
  );

  Future<AudioDevices> devices() async => const AudioDevices(
    inputs: ['USB Mic', 'Built-in Mic'],
    outputs: ['Headphones', 'Speakers'],
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
        loadAudioDevices: devices,
      ),
    );
  }

  testWidgets('toggling a switch persists exactly that change', (tester) async {
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

  testWidgets('the preview lives on the screen tile, not in Settings', (
    tester,
  ) async {
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

  testWidgets('the account section shows the live session and sign-out', (
    tester,
  ) async {
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
          loadAudioDevices: devices,
          onSignOut: () async => signedOut = true,
        ),
      ),
    );
    await tester.pumpAndSettle();

    await tester.scrollUntilVisible(find.text('chat.example:4443'), 200);
    expect(find.text('chat.example:4443'), findsOneWidget);
    await tester.scrollUntilVisible(find.text('Signed in as'), 200);
    expect(find.text('elliot'), findsOneWidget);
    // No hardware switch when the build has no VAAPI backend.
    expect(find.text('Hardware encoding (VAAPI)'), findsNothing);

    // The account section is at the bottom of a scrollable page.
    await tester.scrollUntilVisible(find.text('Sign out'), 200);
    await tester.tap(find.text('Sign out'));
    await tester.pumpAndSettle();
    expect(signedOut, isTrue);
  });

  testWidgets('input and output devices persist by their exact names', (
    tester,
  ) async {
    final saved = <AppSettings>[];
    await tester.pumpWidget(host(saved));
    await tester.pumpAndSettle();

    await tester.tap(find.byType(DropdownButton<String>).first);
    await tester.pumpAndSettle();
    await tester.tap(find.text('USB Mic').last);
    await tester.pumpAndSettle();
    expect(saved.last.inputDevice, 'USB Mic');

    await tester.tap(find.byType(DropdownButton<String>).last);
    await tester.pumpAndSettle();
    await tester.tap(find.text('Speakers').last);
    await tester.pumpAndSettle();
    expect(saved.last.outputDevice, 'Speakers');
    expect(saved.last.inputDevice, 'USB Mic', reason: 'the two compose');
  });

  testWidgets('auto gain is opt-in and its target is saved on release', (
    tester,
  ) async {
    final saved = <AppSettings>[];
    await tester.pumpWidget(host(saved));
    await tester.pumpAndSettle();

    await tester.scrollUntilVisible(find.text('Microphone auto gain'), 200);
    await tester.tap(find.text('Microphone auto gain'));
    await tester.pumpAndSettle();
    expect(saved.last.micAutogain, isTrue);
    await tester.scrollUntilVisible(find.text('Target level'), 200);
    expect(find.text('Target level'), findsOneWidget);

    await tester.drag(find.byType(Slider).last, const Offset(-80, 0));
    await tester.pumpAndSettle();
    expect(saved.last.micAutogainTargetDb, lessThan(-18));
    expect(saved.last.micAutogainTargetDb, greaterThanOrEqualTo(-36));
  });

  testWidgets('echo and noise strength are separate persisted levels', (
    tester,
  ) async {
    final saved = <AppSettings>[];
    await tester.pumpWidget(host(saved));
    await tester.pumpAndSettle();

    await tester.scrollUntilVisible(find.text('Noise reduction'), 200);
    expect(find.text('Noise reduction'), findsOneWidget);
    await tester.drag(find.byType(Slider).last, const Offset(-120, 0));
    await tester.pumpAndSettle();
    expect(saved.last.noiseLevel, lessThan(initial.noiseLevel));
    expect(
      saved.last.echoLevel,
      initial.echoLevel,
      reason: 'changing noise must not change echo strength',
    );
  });
}
