import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:parley/src/theme.dart';
import 'package:parley/src/widgets/share_dialog.dart';

/// The dialog is the only place a share's caps are chosen. These tests pin
/// the two behaviours that matter: the remembered defaults come back
/// unchanged on "Start sharing", and a changed choice is what the caller
/// receives (the Rust side then treats it as a cap).
void main() {
  /// Pump a host page, open the dialog, and return a reader for the value
  /// the dialog will hand back once it is dismissed.
  Future<ShareOptions? Function()> open(
    WidgetTester tester, {
    int height = 0,
    int fps = 0,
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
    expect(result(), (height: 720, fps: 30));
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
    expect(result(), (height: 480, fps: 15));
  });

  testWidgets('cancel means no share', (tester) async {
    final result = await open(tester);

    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();
    expect(result(), isNull);
  });
}
