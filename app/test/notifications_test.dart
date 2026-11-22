import 'package:flutter_test/flutter_test.dart';
import 'package:parley/src/notifications.dart';

/// The text is the part of a system notification that has no native
/// dependency, and it is what the user reads if the banner is all they see.
void main() {
  test('a poke names the sender and the channel', () {
    expect(pokeNotificationTitle('Alice'), 'Alice poked you');
    expect(
      pokeNotificationBody(channel: 'Lounge'),
      'In “Lounge” · Open Parley to poke back',
    );
  });

  test('a poke without a channel still says how to answer', () {
    expect(pokeNotificationBody(), 'Open Parley to poke back');
    expect(pokeNotificationBody(channel: ''), 'Open Parley to poke back');
  });
}
