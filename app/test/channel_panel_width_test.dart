import 'package:flutter_test/flutter_test.dart';
import 'package:parley/src/home_page.dart';

/// The left column (channels + the roster of who is in voice) used to be
/// measured down to the longest name and floored at 220 px — legible, but
/// the rows read like a squeezed strip, and the same column is the one a
/// user scans to see who is in the channel. The floor is now roughly
/// double that, so a window with ordinary handles gets a roomy column
/// rather than one just wide enough to fit them.
void main() {
  test('the default column is near double the old 220 px floor', () {
    final (lower, upper) = channelPanelBounds(1440);
    expect(lower, 420);
    expect(upper, 560);

    // A short roster measures ~200 px of names + chrome; the floor, not
    // the measurement, is what the column takes.
    expect(220.0.clamp(lower, upper), 420);
  });

  test('a very long handle still cannot eat the screens area', () {
    final (lower, upper) = channelPanelBounds(1440);
    expect(900.0.clamp(lower, upper), upper);
    expect(upper, lessThan(1441 * 0.47));
  });

  test('a narrow window gives the column its share, not a fixed 420', () {
    final (lower, upper) = channelPanelBounds(600);
    expect(lower, closeTo(228, 0.5));
    expect(upper, closeTo(276, 0.5));

    // 900 px of window: a 342 px column (38%), not 414 (46%) — the
    // screens area keeps room on the small windows where it matters.
    final (at900, cap900) = channelPanelBounds(900);
    expect(cap900, closeTo(414, 0.5));
    expect(220.0.clamp(at900, cap900), closeTo(342, 0.5));
  });

  test('the bounds stay ordered on a first frame a few pixels wide', () {
    final (lower, upper) = channelPanelBounds(10);
    expect(lower, lessThanOrEqualTo(upper));
    expect(lower, 160);
  });
}
