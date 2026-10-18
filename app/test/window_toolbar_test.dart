import 'dart:io' show Platform;

import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:parley/src/window.dart';

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  final calls = <String>[];

  setUp(() {
    calls.clear();
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(windowChannel, (call) async {
      calls.add(call.method);
      return true;
    });
  });

  tearDown(() {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(windowChannel, null);
  });

  Future<void> pumpToolbar(WidgetTester tester, {bool enabled = true}) async {
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: Stack(
            children: [
              const SizedBox.expand(),
              WindowToolbar(enabled: enabled),
            ],
          ),
        ),
      ),
    );
  }

  testWidgets(
    'reveals from the top edge and hides again',
    (tester) async {
      await pumpToolbar(tester);

      // Hidden: the bar is translated a full height above the window.
      expect(tester.getTopLeft(find.byIcon(Icons.close)).dy, lessThan(0));

      final mouse = await tester.createGesture(kind: PointerDeviceKind.mouse);
      await mouse.addPointer(location: const Offset(100, 100));
      addTearDown(mouse.removePointer);
      await mouse.moveTo(const Offset(200, 3));
      await tester.pumpAndSettle();

      final revealed = tester.getTopLeft(find.byIcon(Icons.close)).dy;
      expect(revealed, greaterThanOrEqualTo(0));
      expect(revealed, lessThan(36));

      // Leaving schedules the hide (1.2 s) and the 160 ms slide.
      await mouse.moveTo(const Offset(200, 300));
      await tester.pump(const Duration(milliseconds: 1400));
      await tester.pumpAndSettle();
      expect(tester.getTopLeft(find.byIcon(Icons.close)).dy, lessThan(0));
    },
    skip: !Platform.isLinux,
  );

  testWidgets(
    'minimise, maximise and close reach the window channel',
    (tester) async {
      await pumpToolbar(tester);
      final mouse = await tester.createGesture(kind: PointerDeviceKind.mouse);
      await mouse.addPointer(location: const Offset(100, 100));
      addTearDown(mouse.removePointer);
      await mouse.moveTo(const Offset(200, 3));
      await tester.pumpAndSettle();

      await tester.tap(find.byTooltip('Minimize'));
      await tester.pump();
      expect(calls, contains('minimize'));

      await tester.tap(find.byTooltip('Maximize'));
      await tester.pump();
      expect(calls, contains('toggleMaximize'));

      await tester.tap(find.byTooltip('Close'));
      await tester.pump();
      expect(calls, contains('close'));
    },
    skip: !Platform.isLinux,
  );

  testWidgets(
    'is absent when disabled (fullscreen viewer)',
    (tester) async {
      await pumpToolbar(tester, enabled: false);
      expect(find.byTooltip('Close'), findsNothing);
    },
    skip: !Platform.isLinux,
  );
}
