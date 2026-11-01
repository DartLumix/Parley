import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:parley/src/rust/api/soundboard.dart';
import 'package:parley/src/theme.dart';
import 'package:parley/src/widgets/profile_page.dart';

/// The Soundboard section's contract: the toggles reach the bridge
/// unchanged, a missing file is visible before Apply, and the server's
/// answer is reported without pretending success.
void main() {
  const board = SoundboardStatus(
    dir: '/opt/parley/sounds',
    joinFile: true,
    leaveFile: false,
    afkFile: true,
    joinEnabled: false,
    leaveEnabled: false,
    afkEnabled: false,
  );

  Future<void> open(
    WidgetTester tester, {
    required Future<String?> Function({
      required bool join,
      required bool leave,
      required bool afk,
    })
    onSoundboard,
  }) async {
    await tester.binding.setSurfaceSize(const Size(1000, 1800));
    addTearDown(() => tester.binding.setSurfaceSize(null));
    await tester.pumpWidget(
      MaterialApp(
        theme: buildParleyTheme(),
        home: ProfilePage(
          username: 'alice',
          role: 'user',
          avatar: null,
          onSave: ({
            required currentPassword,
            required newUsername,
            required newPassword,
            required avatar,
            required clearAvatar,
          }) async => null,
          soundboard: board,
          onSoundboard: onSoundboard,
        ),
      ),
    );
    await tester.pumpAndSettle();
  }

  testWidgets('the section names the drop folder and flags missing MP3s',
      (tester) async {
    await open(
      tester,
      onSoundboard: ({required join, required leave, required afk}) async =>
          null,
    );
    // The page is a ListView; the section sits below the account form.
    await tester.scrollUntilVisible(find.text('SOUNDBOARD'), 200, scrollable: find.byType(Scrollable).first);
    expect(find.text('SOUNDBOARD'), findsOneWidget);
    expect(find.text('/opt/parley/sounds'), findsOneWidget);
    expect(find.text('sounds/join.mp3'), findsOneWidget);
    expect(find.text('sounds/leave.mp3 — missing'), findsOneWidget);
    expect(find.text('sounds/afk.mp3'), findsOneWidget);
  });

  testWidgets('Apply sends the toggles and reports the result',
      (tester) async {
    bool? sentJoin;
    bool? sentLeave;
    bool? sentAfk;
    await open(
      tester,
      onSoundboard: ({required join, required leave, required afk}) async {
        sentJoin = join;
        sentLeave = leave;
        sentAfk = afk;
        return null;
      },
    );
    await tester.scrollUntilVisible(find.text('Join sound'), 200, scrollable: find.byType(Scrollable).first);
    await tester.tap(find.text('Join sound'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('AFK sound'));
    await tester.pumpAndSettle();

    await tester.scrollUntilVisible(find.text('Apply sounds'), 200, scrollable: find.byType(Scrollable).first);
    await tester.tap(find.text('Apply sounds'));
    await tester.pumpAndSettle();

    expect(sentJoin, isTrue);
    expect(sentLeave, isFalse);
    expect(sentAfk, isTrue);
    expect(find.text('Sounds applied'), findsOneWidget);
  });

  testWidgets('a missing file error is shown, not swallowed', (tester) async {
    await open(
      tester,
      onSoundboard: ({required join, required leave, required afk}) async =>
          'sounds/leave.mp3 not found — put an MP3 named leave.mp3 in '
          '/opt/parley/sounds',
    );
    await tester.scrollUntilVisible(find.text('Leave sound'), 200, scrollable: find.byType(Scrollable).first);
    await tester.tap(find.text('Leave sound'));
    await tester.pumpAndSettle();
    await tester.scrollUntilVisible(find.text('Apply sounds'), 200, scrollable: find.byType(Scrollable).first);
    await tester.tap(find.text('Apply sounds'));
    await tester.pumpAndSettle();

    expect(
      find.textContaining('leave.mp3 not found'),
      findsOneWidget,
    );
    expect(find.text('Sounds applied'), findsNothing);
  });
}
