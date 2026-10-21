import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:parley/src/theme.dart';
import 'package:parley/src/widgets/screens_panel.dart';

/// The screens section is the part of the window people actually look at,
/// so its contract is geometric: one share fills it, N shares split it
/// without ever losing their 16:9 shape, and opening one must not hide
/// that the others are still sharing.
void main() {
  const panelWidth = 700.0;
  const panelHeight = 500.0;

  final open = <int>[];
  final watchedCbs = <int>[];
  final closed = <int>[];
  final fullscreened = <int>[];
  final volumes = <(int, double)>[];
  final previewToggles = <bool>[];

  setUp(() {
    open.clear();
    watchedCbs.clear();
    closed.clear();
    fullscreened.clear();
    volumes.clear();
    previewToggles.clear();
  });

  ScreenTile tile(
    int id, {
    bool own = false,
    int w = 1920,
    int h = 1080,
    int fps = 60,
    int? textureId = 7,
    String? notice,
    bool previewEnabled = true,
  }) {
    return ScreenTile(
      userId: id,
      name: own ? 'me (you)' : 'user $id',
      own: own,
      textureId: textureId,
      width: w,
      height: h,
      fps: fps,
      notice: notice,
      previewEnabled: previewEnabled,
    );
  }

  Future<void> pump(
    WidgetTester tester,
    List<ScreenTile> tiles, {
    List<int> initialOpen = const [],
  }) async {
    open
      ..clear()
      ..addAll(initialOpen);
    await tester.pumpWidget(
      MaterialApp(
        theme: buildParleyTheme(),
        home: Scaffold(
          body: Center(
            child: SizedBox(
              width: panelWidth,
              height: panelHeight,
              child: StatefulBuilder(
                builder: (context, setState) => ScreensPanel(
                  tiles: tiles,
                  open: open,
                  onWatch: (id) {
                    watchedCbs.add(id);
                    setState(() {
                      if (!open.contains(id)) open.add(id);
                    });
                  },
                  onClose: (id) {
                    closed.add(id);
                    setState(() => open.remove(id));
                  },
                  onFullscreen: fullscreened.add,
                  onPreviewEnabled: previewToggles.add,
                  onVolume: (id, v) => volumes.add((id, v)),
                ),
              ),
            ),
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();
  }

  testWidgets('one share fills the section at 16:9', (tester) async {
    await pump(tester, [tile(1)]);

    final rect = tester.getRect(find.byKey(const ValueKey('screen-1')));
    // 700 - 2*8 padding - 2*1 border = 682 wide, 682 * 9/16 high.
    expect(rect.width, closeTo(682, 0.5));
    expect(rect.height, closeTo(682 * 9 / 16, 0.5));
    expect(find.text('Watch'), findsOneWidget);
    expect(find.text('1920\u00d71080 \u00b7 60 fps'), findsOneWidget);
  });

  testWidgets('two shares split the section left/right', (tester) async {
    await pump(tester, [tile(1), tile(2)]);

    final a = tester.getRect(find.byKey(const ValueKey('screen-1')));
    final b = tester.getRect(find.byKey(const ValueKey('screen-2')));
    expect(a.size, b.size);
    expect(a.top, b.top);
    expect(a.right, lessThanOrEqualTo(b.left), reason: 'no overlap');
    expect(a.width, closeTo((682 - 8) / 2, 0.5));
    expect(a.width / a.height, closeTo(16 / 9, 0.01));
  });

  testWidgets('three shares: two up, the third centred under them',
      (tester) async {
    await pump(tester, [tile(1), tile(2), tile(3)]);

    final a = tester.getRect(find.byKey(const ValueKey('screen-1')));
    final b = tester.getRect(find.byKey(const ValueKey('screen-2')));
    final c = tester.getRect(find.byKey(const ValueKey('screen-3')));
    expect(a.top, b.top);
    expect(c.top, greaterThan(a.bottom));
    expect(a.size, c.size);
    final centre = (a.center.dx + b.center.dx) / 2;
    expect(c.center.dx, closeTo(centre, 1), reason: 'third tile centred');
  });

  testWidgets('four shares make a cross', (tester) async {
    await pump(tester, [tile(1), tile(2), tile(3), tile(4)]);

    final rects = [
      for (final id in [1, 2, 3, 4])
        tester.getRect(find.byKey(ValueKey('screen-$id'))),
    ];
    expect(rects[0].top, rects[1].top);
    expect(rects[2].top, rects[3].top);
    expect(rects[2].top, greaterThan(rects[0].bottom));
    expect(rects[0].left, rects[2].left);
    expect(rects[1].left, rects[3].left);
  });

  testWidgets('opening one share makes it big and keeps the rest visible',
      (tester) async {
    await pump(tester, [tile(1), tile(2)], initialOpen: [1]);

    final big = tester.getRect(find.byKey(const ValueKey('screen-1')));
    expect(big.width, closeTo(682, 0.5));
    // The other share is still on screen, in the strip.
    expect(find.byKey(const ValueKey('screen-strip-2')), findsOneWidget);
    final strip = tester.getRect(
      find.byKey(const ValueKey('screen-strip-2')),
    );
    expect(strip.height, closeTo(84, 1));
    // The strip sits under the big picture, not over it.
    expect(strip.top, greaterThan(big.bottom));
  });

  testWidgets('Watch opens, Close closes, the strip opens too',
      (tester) async {
    await pump(tester, [tile(1), tile(2)]);

    await tester.tap(find.text('Watch').first);
    await tester.pumpAndSettle();
    expect(watchedCbs, [1]);

    // Open tile shows its controls; closing it calls back.
    await tester.tap(find.byTooltip('Close this screen'));
    await tester.pumpAndSettle();
    expect(closed, [1]);

    // Reopen through the tile, then use the strip thumbnail.
    await tester.tap(find.text('Watch').first);
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const ValueKey('screen-strip-2')));
    await tester.pumpAndSettle();
    expect(watchedCbs.last, 2);
  });

  testWidgets('a remote share shows no picture until Watch is pressed',
      (tester) async {
    await pump(tester, [tile(1)]);

    // The texture exists (the stream is running) but nothing paints it:
    // watching is what the Watch button asks for.
    expect(find.byType(Texture), findsNothing);

    await tester.tap(find.text('Watch'));
    await tester.pumpAndSettle();
    expect(find.byType(Texture), findsOneWidget);
  });

  testWidgets('our own tile shows its preview without being opened',
      (tester) async {
    await pump(tester, [tile(1, own: true)]);

    expect(find.byType(Texture), findsOneWidget);
  });

  testWidgets('our own ultrawide preview keeps the stream\u2019s shape',
      (tester) async {
    // The preview used to be a fixed 16:9 canvas, so a 3440×1440 screen
    // was squeezed into it and looked squashed inside bars. The tile now
    // takes the stream's aspect like a remote share does.
    await pump(tester, [tile(1, own: true, w: 3440, h: 1440)]);

    final aspect = tester.widget<AspectRatio>(
      find.ancestor(
        of: find.byType(Texture),
        matching: find.byType(AspectRatio),
      ),
    );
    expect(aspect.aspectRatio, closeTo(3440 / 1440, 1e-9));
  });

  testWidgets('the own-share switch flips the preview preference',
      (tester) async {
    await pump(tester, [tile(1, own: true)]);

    await tester.tap(find.byType(Switch));
    await tester.pumpAndSettle();
    expect(previewToggles, [false]);
  });

  testWidgets('our own tile goes fullscreen too, without a stop-watching',
      (tester) async {
    await pump(tester, [tile(1, own: true)], initialOpen: [1]);

    expect(find.byTooltip('Close this screen'), findsOneWidget);
    await tester.tap(find.byTooltip('Fullscreen'));
    await tester.pumpAndSettle();
    expect(fullscreened, [1]);
    // The preview switch stays with the sharer on the tile.
    expect(find.byType(Switch), findsOneWidget);
  });

  testWidgets('a tile with no picture says why', (tester) async {
    await pump(tester, [
      tile(1, own: true, textureId: null, notice: 'preview off'),
    ]);

    expect(find.text('preview off'), findsOneWidget);
    expect(find.text('your screen'), findsNothing);
  });

  testWidgets('fullscreen and per-share volume reach the caller',
      (tester) async {
    await pump(tester, [tile(1), tile(2)], initialOpen: [1]);

    await tester.tap(find.byTooltip('Fullscreen'));
    await tester.pumpAndSettle();
    expect(fullscreened, [1]);

    // Drag the open share's slider; the callback sees its user id.
    await tester.drag(find.byType(Slider), const Offset(-20, 0));
    await tester.pumpAndSettle();
    expect(volumes, isNotEmpty);
    expect(volumes.last.$1, 1);
  });

  testWidgets('the section height follows the tile count', (tester) async {
    // One 16:9 tile at 900 px wide wants 506 px; the cap keeps 28% for
    // the chat, and the chat's minimum never inverts.
    expect(
      ScreensPanel.heightFor(
        tileCount: 1,
        openCount: 0,
        width: 900,
        availableHeight: 900,
      ),
      closeTo(900 * 9 / 16, 0.5),
    );
    expect(
      ScreensPanel.heightFor(
        tileCount: 4,
        openCount: 0,
        width: 900,
        availableHeight: 500,
      ),
      closeTo(300, 0.5),
      reason: 'the chat floor wins over the 72% share on a short pane',
    );
    // A short window while watching: the open share still may not eat
    // the chat's floor (this was a 40 px chat with the input under the
    // floating bar).
    expect(
      ScreensPanel.heightFor(
        tileCount: 3,
        openCount: 1,
        width: 900,
        availableHeight: 430,
      ),
      closeTo(230, 0.5),
      reason: 'the chat keeps 200 px even at the 84% cap',
    );
    expect(
      ScreensPanel.heightFor(
        tileCount: 0,
        openCount: 0,
        width: 900,
        availableHeight: 500,
      ),
      0,
    );
  });
}
