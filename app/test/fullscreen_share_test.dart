import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:parley/src/widgets/fullscreen_share.dart';

/// The fullscreen page is the one place a share is shown with no chrome
/// around it, so its contract is: the picture keeps the stream's shape,
/// the controls are exactly the ones that make sense for what is being
/// shown, and Esc always gets you out.
void main() {
  Future<void> pump(
    WidgetTester tester, {
    required double aspectRatio,
    VoidCallback? onStopWatching,
    int width = 1920,
    int height = 1080,
  }) async {
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          backgroundColor: Colors.black,
          body: FullscreenShare(
            title: 'someone',
            textureId: 7,
            width: width,
            height: height,
            aspectRatio: aspectRatio,
            onExit: () {},
            onStopWatching: onStopWatching,
          ),
        ),
      ),
    );
  }

  testWidgets('a received share is letterboxed to its own aspect',
      (tester) async {
    // A 21:9 stream must not be stretched to 16:9.
    await pump(tester, aspectRatio: 2560 / 1080, onStopWatching: () {});

    final aspect = tester.widget<AspectRatio>(
      find.ancestor(
        of: find.byType(Texture),
        matching: find.byType(AspectRatio),
      ),
    );
    expect(aspect.aspectRatio, closeTo(2560 / 1080, 1e-9));
  });

  testWidgets('our own preview is drawn on its 16:9 canvas', (tester) async {
    await pump(tester, aspectRatio: 16 / 9, width: 1280, height: 720);

    final aspect = tester.widget<AspectRatio>(
      find.ancestor(
        of: find.byType(Texture),
        matching: find.byType(AspectRatio),
      ),
    );
    expect(aspect.aspectRatio, closeTo(16 / 9, 1e-9));
    // There is no watch state to stop on your own share.
    expect(find.text('Stop watching'), findsNothing);
    expect(find.text('Exit fullscreen'), findsOneWidget);
    expect(find.text('someone · 1280×720 · Esc to exit'), findsOneWidget);
  });

  testWidgets('a share whose size is not known yet says only what it knows',
      (tester) async {
    await pump(tester, aspectRatio: 16 / 9, width: 0, height: 0);

    expect(find.text('someone · Esc to exit'), findsOneWidget);
  });

  testWidgets('Stop watching and Esc both reach the caller', (tester) async {
    var stopped = 0;
    var exited = 0;
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: FullscreenShare(
            title: 'someone',
            textureId: 7,
            width: 1920,
            height: 1080,
            aspectRatio: 16 / 9,
            onExit: () => exited++,
            onStopWatching: () => stopped++,
          ),
        ),
      ),
    );

    await tester.tap(find.text('Stop watching'));
    await tester.pump();
    expect(stopped, 1);

    await tester.sendKeyEvent(LogicalKeyboardKey.escape);
    await tester.pump();
    expect(exited, 1);
  });
}
