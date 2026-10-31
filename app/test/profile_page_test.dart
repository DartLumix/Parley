import 'dart:typed_data';
import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:parley/src/theme.dart';
import 'package:parley/src/widgets/profile_page.dart';

void main() {
  Widget host(SaveProfile onSave, {Uint8List? avatar}) {
    return MaterialApp(
      theme: buildParleyTheme(),
      home: Scaffold(
        body: Builder(
          builder: (context) => Center(
            child: ElevatedButton(
              onPressed: () => Navigator.of(context).push(
                MaterialPageRoute<bool>(
                  builder: (_) => ProfilePage(
                    username: 'alice',
                    role: 'user',
                    avatar: avatar,
                    onSave: onSave,
                  ),
                ),
              ),
              child: const Text('open'),
            ),
          ),
        ),
      ),
    );
  }

  Future<void> open(WidgetTester tester) async {
    // The form is taller than the default 800x600 test surface; give it
    // room so the Save button is laid out, not lazily skipped by the
    // ListView.
    await tester.binding.setSurfaceSize(const Size(1000, 1400));
    addTearDown(() => tester.binding.setSurfaceSize(null));
    await tester.tap(find.text('open'));
    await tester.pumpAndSettle();
  }

  testWidgets('a rename sends the new name and the current password',
      (tester) async {
    String? current;
    String? newName;
    Uint8List? sentAvatar;
    await tester.pumpWidget(
      host(({
        required currentPassword,
        required newUsername,
        required newPassword,
        required avatar,
        required clearAvatar,
      }) async {
        current = currentPassword;
        newName = newUsername;
        sentAvatar = avatar;
        return null;
      }),
    );
    await open(tester);

    await tester.enterText(find.byType(TextField).at(0), 'alice2');
    await tester.enterText(find.byType(TextField).at(1), 'secret');
    await tester.tap(find.text('Save changes'));
    await tester.pumpAndSettle();

    expect(current, 'secret');
    expect(newName, 'alice2');
    expect(sentAvatar, isNull);
    // A successful save closes the page.
    expect(find.text('open'), findsOneWidget);
  });

  testWidgets('mismatched new passwords never reach the server',
      (tester) async {
    var saves = 0;
    await tester.pumpWidget(
      host(({
        required currentPassword,
        required newUsername,
        required newPassword,
        required avatar,
        required clearAvatar,
      }) async {
        saves += 1;
        return null;
      }),
    );
    await open(tester);

    await tester.enterText(find.byType(TextField).at(1), 'secret');
    await tester.enterText(find.byType(TextField).at(2), 'one');
    await tester.enterText(find.byType(TextField).at(3), 'two');
    await tester.tap(find.text('Save changes'));
    await tester.pumpAndSettle();

    expect(saves, 0);
    expect(find.text('The new passwords do not match.'), findsOneWidget);
  });

  testWidgets('a server refusal stays on the page with its reason',
      (tester) async {
    await tester.pumpWidget(
      host(({
        required currentPassword,
        required newUsername,
        required newPassword,
        required avatar,
        required clearAvatar,
      }) async {
        return 'current password is incorrect';
      }),
    );
    await open(tester);

    await tester.enterText(find.byType(TextField).at(0), 'alice2');
    await tester.enterText(find.byType(TextField).at(1), 'wrong');
    await tester.tap(find.text('Save changes'));
    await tester.pumpAndSettle();

    expect(find.text('current password is incorrect'), findsOneWidget);
    expect(find.text('Save changes'), findsOneWidget);
  });

  testWidgets('picked images are resized to the avatar edge', (tester) async {
    // Image decode/encode is real (engine) async work: inside the fake
    // clock of a widget test it would never complete.
    await tester.runAsync(() async {
      final recorder = ui.PictureRecorder();
      final canvas = Canvas(recorder);
      canvas.drawRect(
        const Rect.fromLTWH(0, 0, 8, 8),
        Paint()..color = Colors.orange,
      );
      final source = await recorder.endRecording().toImage(8, 8);
      final png = await source.toByteData(format: ui.ImageByteFormat.png);
      expect(png, isNotNull);

      final prepared = await prepareAvatar(png!.buffer.asUint8List());
      expect(prepared.length, lessThan(maxAvatarBytes));
      expect(
        prepared.sublist(0, 8),
        <int>[137, 80, 78, 71, 13, 10, 26, 10],
        reason: 'the upload is always a PNG',
      );

      final codec = await ui.instantiateImageCodec(prepared);
      final frame = await codec.getNextFrame();
      expect(frame.image.width, avatarEdgePx);
      frame.image.dispose();
      codec.dispose();
      source.dispose();
    });
  });
}
