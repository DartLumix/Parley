import 'dart:math' as math;

import 'package:flutter/material.dart';

/// One share as the screens panel shows it: a name, the stream's
/// size/rate and — for our own share — the preview switch.
class ScreenTile {
  const ScreenTile({
    required this.userId,
    required this.name,
    required this.own,
    required this.width,
    required this.height,
    required this.fps,
    this.textureId,
    this.notice,
    this.previewEnabled = true,
    this.volume = 1.0,
  });

  final int userId;
  final String name;
  final bool own;

  /// The texture to paint, when there is one. Null means a notice is
  /// shown instead (preview off, first frame pending, no surface yet).
  final int? textureId;
  final int width;
  final int height;
  final int fps;

  /// Shown instead of a picture when [textureId] is null.
  final String? notice;

  /// Our own preview switch's value. Ignored for remote shares.
  final bool previewEnabled;

  /// This share's system-audio trim (1.0 = untouched).
  final double volume;

  String get info {
    if (width <= 0 || height <= 0) return own ? 'your screen' : 'connecting\u2026';
    final rate = fps > 0 ? ' \u00b7 $fps fps' : '';
    return '$width\u00d7$height$rate';
  }
}

/// The screens section: a 16:9 tile per share.
///
/// With nothing open the tiles split the section — one fills it, two sit
/// side by side, three split the top row and centre the third below,
/// four make a cross, and so on. Opening a share ("Watch") makes it big
/// and leaves the remaining shares as a horizontal strip of thumbnails,
/// so watching one stream never hides the fact that others are sharing.
class ScreensPanel extends StatelessWidget {
  const ScreensPanel({
    super.key,
    required this.tiles,
    required this.open,
    required this.onWatch,
    required this.onClose,
    required this.onFullscreen,
    required this.onPreviewEnabled,
    required this.onVolume,
  });

  final List<ScreenTile> tiles;

  /// Ids of the tiles that are open (a subset of [tiles], in the order
  /// they were opened).
  final List<int> open;

  final ValueChanged<int> onWatch;
  final ValueChanged<int> onClose;
  final ValueChanged<int> onFullscreen;
  final ValueChanged<bool> onPreviewEnabled;
  final void Function(int userId, double gain) onVolume;

  static const double _gap = 8;
  static const double _stripHeight = 84;

  /// What the chat keeps when a share is open: its header, a couple of
  /// lines of conversation and the input row.
  static const double _minChatHeight = 200;

  /// How tall the section wants to be at this width: the grid's natural
  /// 16:9 height, capped so the chat below keeps usable space.
  static double heightFor({
    required int tileCount,
    required int openCount,
    required double width,
    required double availableHeight,
  }) {
    if (tileCount == 0) return 0;
    final cols = (openCount > 0 ? openCount : tileCount) == 1
        ? 1
        : 2;
    final shown = openCount > 0 ? openCount : tileCount;
    final rows = (shown / cols).ceil();
    // Guards against the first frame, when the window may still be a few
    // pixels wide: a negative SizedBox is an exception, not a layout.
    final cellW = math.max(1.0, (width - _gap * (cols - 1)) / cols);
    final grid = rows * cellW * 9 / 16 + (rows - 1) * _gap;
    final strip = openCount > 0 && openCount < tileCount
        ? _stripHeight + _gap
        : 0.0;
    // Watching something is the point of the pane: an open share may
    // take 84% of it (the chat keeps its header and input); the preview
    // grid stays at 72% so the chat has room for a conversation.
    //
    // The percentage alone is not enough on a short window: at 500 px of
    // pane the 84% cap left the chat ~50 px — a header, no messages, and
    // the input under the floating bar. So the chat also gets a floor.
    final share = openCount > 0 ? 0.84 : 0.72;
    final forChat = availableHeight - _minChatHeight;
    final cap = math.max(140.0, math.min(availableHeight * share, forChat));
    return math.max(0, math.min(grid + strip, cap));
  }

  List<ScreenTile> get _openTiles {
    final byId = {for (final t in tiles) t.userId: t};
    return [
      for (final id in open)
        if (byId[id] != null) byId[id]!,
    ];
  }

  @override
  Widget build(BuildContext context) {
    final openTiles = _openTiles;
    final openIds = {for (final t in openTiles) t.userId};
    final rest = [
      for (final t in tiles)
        if (!openIds.contains(t.userId)) t,
    ];
    return Container(
      decoration: BoxDecoration(
        color: const Color(0xFF0B0B0D),
        borderRadius: BorderRadius.circular(10),
        border: Border.all(color: Colors.white24),
      ),
      padding: const EdgeInsets.all(_gap),
      child: LayoutBuilder(
        builder: (context, c) {
          // The strip shrinks with the window; the picture it surrounds
          // must never be the part that pays for a short pane.
          final stripHeight = math.max(
            1.0,
            math.min(_stripHeight, math.max(40.0, c.maxHeight * 0.22)),
          );
          return Column(
            children: [
              Expanded(
                child: _grid(
                  openTiles.isEmpty ? tiles : openTiles,
                  big: openTiles.isNotEmpty,
                ),
              ),
              if (openTiles.isNotEmpty && rest.isNotEmpty) ...[
                const SizedBox(height: _gap),
                SizedBox(height: stripHeight, child: _strip(rest, stripHeight)),
              ],
            ],
          );
        },
      ),
    );
  }

  /// Lay tiles out in 16:9 cells, centred in the section.
  Widget _grid(List<ScreenTile> shown, {required bool big}) {
    final cols = shown.length == 1 ? 1 : 2;
    final rows = (shown.length / cols).ceil();
    return LayoutBuilder(
      builder: (context, c) {
        final cellW = math.max(1.0, (c.maxWidth - _gap * (cols - 1)) / cols);
        final cellH = math.max(1.0, (c.maxHeight - _gap * (rows - 1)) / rows);
        final tileH = math.min(cellW * 9 / 16, cellH);
        final tileW = tileH * 16 / 9;
        return Column(
          mainAxisAlignment: MainAxisAlignment.center,
          children: [
            for (var row = 0; row < rows; row++) ...[
              if (row > 0) const SizedBox(height: _gap),
              Row(
                mainAxisAlignment: MainAxisAlignment.center,
                children: [
                  for (var col = 0; col < cols; col++)
                    if (row * cols + col < shown.length) ...[
                      if (col > 0) const SizedBox(width: _gap),
                      SizedBox(
                        width: tileW,
                        height: tileH,
                        child: big
                            ? _openTile(context, shown[row * cols + col])
                            : _inviteTile(shown[row * cols + col]),
                      ),
                    ],
                ],
              ),
            ],
          ],
        );
      },
    );
  }

  /// A share nobody has opened: a poster (never the picture), name,
  /// Watch button and the stream's size/rate.
  ///
  /// A remote share is not rendered until it is opened — watching is the
  /// choice the Watch button asks for, and the point of the button is
  /// lost if the picture is already playing behind it. Our own tile is
  /// the exception: that picture is our own preview, this machine's
  /// capture, which the sharer asked to see.
  Widget _inviteTile(ScreenTile tile) {
    return _frame(
      key: ValueKey('screen-${tile.userId}'),
      child: Stack(
        fit: StackFit.expand,
        children: [
          if (tile.own) _picture(tile) else const _Poster(),
          Container(
            color: Colors.black.withValues(alpha: tile.own ? 0.35 : 0.18),
          ),
          Padding(
            padding: const EdgeInsets.fromLTRB(10, 6, 6, 6),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(
                  children: [
                    Expanded(
                      child: Text(
                        tile.name,
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: const TextStyle(
                          fontSize: 12,
                          fontWeight: FontWeight.w600,
                        ),
                      ),
                    ),
                    if (tile.own) _previewSwitch(tile),
                  ],
                ),
                Expanded(
                  child: Center(
                    child: FilledButton.tonalIcon(
                      onPressed: () => onWatch(tile.userId),
                      icon: const Icon(Icons.play_arrow, size: 18),
                      label: const Text('Watch'),
                      style: FilledButton.styleFrom(
                        visualDensity: VisualDensity.compact,
                        padding: const EdgeInsets.symmetric(horizontal: 14),
                      ),
                    ),
                  ),
                ),
                Row(
                  children: [
                    const Icon(
                      Icons.screen_share,
                      size: 12,
                      color: Colors.white54,
                    ),
                    const SizedBox(width: 5),
                    Expanded(
                      child: Text(
                        tile.info,
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: const TextStyle(
                          fontSize: 11,
                          color: Colors.white60,
                        ),
                      ),
                    ),
                  ],
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }

  /// An open share: the picture owns the tile, controls over a gradient.
  ///
  /// The controls adapt to the tile: a wide tile gets the per-share
  /// volume slider, a narrow one drops it (the roster's volume dialog
  /// still reaches the same trim) so the name and stream info are never
  /// squeezed to an ellipsis by chrome.
  Widget _openTile(BuildContext context, ScreenTile tile) {
    return LayoutBuilder(
      builder: (context, c) {
        final compact = c.maxWidth < 420;
        return _frame(
          key: ValueKey('screen-${tile.userId}'),
          child: Stack(
            fit: StackFit.expand,
            children: [
              Container(color: Colors.black),
              _picture(tile),
              Positioned(
                left: 0,
                right: 0,
                bottom: 0,
                child: Container(
                  padding: const EdgeInsets.fromLTRB(10, 14, 4, 4),
                  decoration: const BoxDecoration(
                    gradient: LinearGradient(
                      begin: Alignment.topCenter,
                      end: Alignment.bottomCenter,
                      colors: [Colors.transparent, Color(0xCC000000)],
                    ),
                  ),
                  child: Column(
                    mainAxisSize: MainAxisSize.min,
                    crossAxisAlignment: CrossAxisAlignment.stretch,
                    children: [
                      Row(
                        children: [
                          Expanded(
                            child: Text(
                              tile.name,
                              maxLines: 1,
                              overflow: TextOverflow.ellipsis,
                              style: const TextStyle(
                                fontSize: 12,
                                fontWeight: FontWeight.w600,
                              ),
                            ),
                          ),
                          if (tile.own) _previewSwitch(tile),
                        ],
                      ),
                      Row(
                        children: [
                          Expanded(
                            child: Text(
                              tile.info,
                              maxLines: 1,
                              overflow: TextOverflow.ellipsis,
                              style: const TextStyle(
                                fontSize: 10,
                                color: Colors.white60,
                              ),
                            ),
                          ),
                          if (!tile.own && !compact)
                            // Per-stream volume: this share's system audio
                            // only. The sharer's VOICE keeps its own
                            // roster level.
                            SizedBox(
                              width: 88,
                              child: SliderTheme(
                                data: SliderTheme.of(context).copyWith(
                                  trackHeight: 2,
                                  thumbShape: const RoundSliderThumbShape(
                                    enabledThumbRadius: 5,
                                  ),
                                  overlayShape: const RoundSliderOverlayShape(
                                    overlayRadius: 8,
                                  ),
                                ),
                                child: Slider(
                                  value: tile.volume,
                                  min: 0,
                                  max: 2,
                                  onChanged: (v) => onVolume(tile.userId, v),
                                ),
                              ),
                            ),
                          // Every open tile can go fullscreen: our own
                          // shows the capture preview, a remote one its
                          // received texture.
                          IconButton(
                            tooltip: 'Fullscreen',
                            icon: const Icon(Icons.fullscreen, size: 18),
                            visualDensity: VisualDensity.compact,
                            onPressed: () => onFullscreen(tile.userId),
                          ),
                          IconButton(
                            tooltip: 'Close this screen',
                            icon: const Icon(Icons.close, size: 16),
                            visualDensity: VisualDensity.compact,
                            onPressed: () => onClose(tile.userId),
                          ),
                        ],
                      ),
                    ],
                  ),
                ),
              ),
            ],
          ),
        );
      },
    );
  }

  /// The remaining shares while something is open: small thumbnails in a
  /// horizontal strip, each click-to-open.
  Widget _strip(List<ScreenTile> rest, double height) {
    return ListView.separated(
      scrollDirection: Axis.horizontal,
      itemCount: rest.length,
      separatorBuilder: (_, _) => const SizedBox(width: _gap),
      itemBuilder: (context, i) {
        final tile = rest[i];
        return SizedBox(
          key: ValueKey('screen-strip-${tile.userId}'),
          width: height * 16 / 9,
          child: _frame(
            child: InkWell(
              onTap: () => onWatch(tile.userId),
              child: Stack(
                fit: StackFit.expand,
                children: [
                  if (tile.own) _picture(tile) else const _Poster(iconSize: 18),
                  Container(color: Colors.black.withValues(alpha: 0.30)),
                  const Center(
                    child: Icon(
                      Icons.play_circle_outline,
                      size: 22,
                      color: Colors.white70,
                    ),
                  ),
                  Positioned(
                    left: 5,
                    right: 5,
                    bottom: 4,
                    child: Text(
                      tile.name,
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
                      style: const TextStyle(
                        fontSize: 10,
                        color: Colors.white70,
                      ),
                    ),
                  ),
                ],
              ),
            ),
          ),
        );
      },
    );
  }

  Widget _frame({Key? key, required Widget child}) {
    return Container(
      key: key,
      decoration: BoxDecoration(
        borderRadius: BorderRadius.circular(8),
        border: Border.all(color: Colors.white.withValues(alpha: 0.10)),
      ),
      clipBehavior: Clip.antiAlias,
      // The tile's own dark plate: a share with no picture yet (or a
      // letterboxed 21:9 stream) must still read as a card.
      child: Material(color: const Color(0xFF141417), child: child),
    );
  }

  /// The live picture, or the tile's notice when there is none.
  Widget _picture(ScreenTile tile) {
    if (tile.textureId != null) {
      return Center(
        child: AspectRatio(
          // Both kinds of picture keep the stream's shape: our own
          // preview canvas follows the encoder's size (so a 21:9 screen
          // is not squeezed into 16:9), and a remote capture arrives at
          // it.
          aspectRatio: tile.width > 0 && tile.height > 0
              ? tile.width / tile.height
              : 16 / 9,
          child: Texture(textureId: tile.textureId!),
        ),
      );
    }
    return _NoticeTile(
      text: tile.notice ?? (tile.own ? 'your screen' : 'waiting\u2026'),
    );
  }

  Widget _previewSwitch(ScreenTile tile) {
    return Tooltip(
      message: tile.previewEnabled
          ? 'Hide your preview (stops its work)'
          : 'Show your preview',
      child: SizedBox(
        height: 26,
        width: 40,
        child: FittedBox(
          fit: BoxFit.contain,
          child: Switch(
            value: tile.previewEnabled,
            onChanged: onPreviewEnabled,
          ),
        ),
      ),
    );
  }
}

/// What an unopened share shows instead of its picture: a dark plate
/// with a faint screen glyph. Deliberately not a frame of the stream —
/// the stream starts when Watch is pressed.
class _Poster extends StatelessWidget {
  const _Poster({this.iconSize = 34});

  final double iconSize;

  @override
  Widget build(BuildContext context) {
    return DecoratedBox(
      decoration: const BoxDecoration(
        gradient: LinearGradient(
          begin: Alignment.topLeft,
          end: Alignment.bottomRight,
          colors: [Color(0xFF1C1C22), Color(0xFF0E0E11)],
        ),
      ),
      child: Center(
        child: Icon(
          Icons.desktop_windows_outlined,
          size: iconSize,
          color: Colors.white.withValues(alpha: 0.16),
        ),
      ),
    );
  }
}

/// A tile with no picture yet: a small icon and one short line.
class _NoticeTile extends StatelessWidget {
  const _NoticeTile({required this.text});

  final String text;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.all(8),
      child: Center(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Icon(
              Icons.hourglass_empty,
              size: 18,
              color: Colors.white.withValues(alpha: 0.38),
            ),
            const SizedBox(height: 6),
            Text(
              text,
              textAlign: TextAlign.center,
              maxLines: 2,
              overflow: TextOverflow.ellipsis,
              style: const TextStyle(fontSize: 10, color: Colors.white38),
            ),
          ],
        ),
      ),
    );
  }
}
