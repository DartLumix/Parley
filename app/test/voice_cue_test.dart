import 'package:flutter_test/flutter_test.dart';
import 'package:parley/src/home_page.dart';

/// The join/leave cue is a sound, so a false positive is not a cosmetic
/// bug: muting yourself must not chirp, and a re-announce of a roster
/// that never moved must stay silent. The event carries both, so the
/// cue is decided by the membership TRANSITION.
void main() {
  final general = BigInt.from(1);
  final music = BigInt.from(2);
  const me = 1;
  const other = 2;

  bool cue({
    int userId = other,
    BigInt? myChannel,
    BigInt? channelBefore,
    bool joined = true,
    BigInt? eventChannel,
    int? self = me,
  }) {
    return isVoiceMembershipCue(
      userId: userId,
      me: self,
      myChannel: myChannel ?? general,
      channelBefore: channelBefore,
      joined: joined,
      eventChannel: eventChannel ?? general,
    );
  }

  test('somebody arriving in my channel is a cue', () {
    expect(cue(channelBefore: null), isTrue);
    expect(cue(channelBefore: music), isTrue, reason: 'moved in from another');
  });

  test('a member re-announced is not, mute included', () {
    // SetMute is broadcast as VoiceState{joined: true, channel_id: mine}.
    expect(cue(channelBefore: general), isFalse);
    expect(cue(channelBefore: general, joined: true), isFalse);
  });

  test('leaving my channel is a cue; leaving another is not', () {
    expect(cue(channelBefore: general, joined: false, eventChannel: general), isTrue);
    expect(
      cue(channelBefore: music, joined: false, eventChannel: music),
      isFalse,
    );
  });

  test('my own events are never a cue', () {
    expect(cue(userId: me, channelBefore: general), isFalse);
    expect(cue(userId: me, channelBefore: general, joined: false), isFalse);
  });

  test('nothing is a cue while I am not in voice', () {
    expect(
      isVoiceMembershipCue(
        userId: other,
        me: me,
        myChannel: null,
        channelBefore: null,
        joined: true,
        eventChannel: general,
      ),
      isFalse,
    );
    expect(cue(self: null), isFalse, reason: 'no session yet');
  });
}
