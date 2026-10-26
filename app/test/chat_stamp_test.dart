import 'package:flutter_test/flutter_test.dart';
import 'package:parley/src/home_page.dart';

void main() {
  test('chatStamp shows local date and time, year only when it differs', () {
    final now = DateTime(2026, 3, 4, 12);
    // A same-year message: dd/mm HH:MM.
    expect(
      chatStamp(DateTime(2026, 10, 26, 9, 5), now: now),
      '26/10 09:05',
    );
    // Another year stays unambiguous.
    expect(
      chatStamp(DateTime(2025, 12, 31, 23, 59), now: now),
      '2025-31/12 23:59',
    );
  });
}
