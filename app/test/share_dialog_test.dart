import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:parley/src/rust/api/players.dart';
import 'package:parley/src/theme.dart';
import 'package:parley/src/widgets/share_dialog.dart';

/// The dialog is the only place a share's caps and audio are chosen.
/// These tests pin the behaviours that matter: the remembered defaults
/// come back unchanged on "Start sharing", a changed choice is what the
/// caller receives (the Rust side treats it as a cap), and the audio
/// picker hands back exactly the applications that stayed checked.
void main() {
  /// Pump a host page, open the dialog, and return a reader for the value
  /// the dialog will hand back once it is dismissed.
  Future<ShareOptions? Function()> open(
    WidgetTester tester, {
    int height = 0,
    int fps = 0,
    List<ShareAudioPlayer> players = const [],
  }) async {
    ShareOptions? result;
    await tester.pumpWidget(
      MaterialApp(
        theme: buildParleyTheme(),
        home: Builder(
          builder: (context) => Center(
            child: ElevatedButton(
              onPressed: () async {
                result = await showShareOptionsDialog(
                  context,
                  height: height,
                  fps: fps,
                  players: players,
                );
              },
              child: const Text('open'),
            ),
          ),
        ),
      ),
    );
    await tester.tap(find.text('open'));
    await tester.pumpAndSettle();
    return () => result;
  }

  testWidgets('defaults pass straight through', (tester) async {
    final result = await open(tester, height: 720, fps: 30);

    expect(find.text('720p'), findsOneWidget);
    expect(find.text('30 fps'), findsOneWidget);

    await tester.tap(find.text('Start sharing'));
    await tester.pumpAndSettle();
    final value = result()!;
    expect(value.height, 720);
    expect(value.fps, 30);
    // No app is playing: the audio switch is off and cannot be armed.
    expect(value.audio, isFalse);
    expect(value.audioPlayers, isEmpty);
  });

  testWidgets('changing both choices returns the new caps', (tester) async {
    final result = await open(tester);

    await tester.tap(find.byType(DropdownButton<int>).first);
    await tester.pumpAndSettle();
    await tester.tap(find.text('480p').last);
    await tester.pumpAndSettle();

    await tester.tap(find.byType(DropdownButton<int>).last);
    await tester.pumpAndSettle();
    await tester.tap(find.text('15 fps').last);
    await tester.pumpAndSettle();

    await tester.tap(find.text('Start sharing'));
    await tester.pumpAndSettle();
    final value = result()!;
    expect(value.height, 480);
    expect(value.fps, 15);
  });

  testWidgets('playing apps are all checked and unchecking one drops it', (
    tester,
  ) async {
    final players = [
      const ShareAudioPlayer(
        id: 101,
        app: 'Firefox',
        description: 'Firefox audio',
      ),
      const ShareAudioPlayer(id: 102, app: 'mpv', description: 'mpv audio'),
    ];
    final result = await open(tester, players: players);

    expect(find.text('Firefox'), findsOneWidget);
    expect(find.text('mpv'), findsOneWidget);
    expect(find.textContaining('Parley itself never is'), findsOneWidget);

    // Uncheck mpv; Firefox stays. The dialog scrolls when the window is
    // small, so bring the row into view first.
    final mpvCheckbox = find.descendant(
      of: find.widgetWithText(CheckboxListTile, 'mpv'),
      matching: find.byType(Checkbox),
    );
    await tester.ensureVisible(mpvCheckbox);
    await tester.pumpAndSettle();
    await tester.tap(mpvCheckbox);
    await tester.pumpAndSettle();

    await tester.tap(find.text('Start sharing'));
    await tester.pumpAndSettle();
    final value = result()!;
    expect(value.audio, isTrue);
    expect(value.audioPlayers, [101]);
  });

  testWidgets('cancel means no share', (tester) async {
    final result = await open(tester);

    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();
    expect(result(), isNull);
  });
}
