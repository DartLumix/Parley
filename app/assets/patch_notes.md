<!-- Parley release notes.

     This file travels inside every build and is what the app shows: on the
     first launch of a version (automatically) and behind Settings → What's
     new. Write it before you tag a release — the bundled copy is the one
     users read, since the GitHub release body is generated from
     CHANGELOG.md and stays on the web.

     Keep it short and user-facing. The app renders it line by line, with no
     markdown engine: `#`/`##` become headings, `-`/`*` become bullets, every
     other line is a paragraph, and **bold** markers and backticks are
     stripped. Emoji and plain Unicode (·, →, —) are fine.
-->

# Parley 0.6.3

## Features
- Per l'incredibile felicità di FladioGelo, nelle impostazioni c'è il toggle per avere le notifiche di sistema per il poke (se le attivi e non funzionano te la vedi tu, puoi emulare un altro DE se ti va)
- Issues report: sezione nelle impostazioni per reportare bug, crash, problemi vari e robe. Vi allega i log della sessione corrente oppure potete allegare i log dell'ultima se era crushata. Se serve allegate screenshot. Vi allega in automatico anche tutte le informazioni di sistema di driver, versioni robe (così FladioGelo viene automaticamente flaggato come se lo fixa da solo). Dovrebbe penso servire l'account di Github per caricare l'Issue almeno che non sia cambiato qualcosa

## Fixes

- Windows di mmerda. L'updater dovrebbe funzionare. (chiaramente si vedrà con la 0.6.4)
- Audio della stream da chiusa
- Unmute da Deaf adesso funziona sensatamente
- Forse fixato il problema dell'audio che svanisce quando ti tippi sulla stoneblock (LOL)



# Parley 0.6.2

A small reliability patch for the 0.6.1 hardware media paths. There is no
protocol change; this build is wire-compatible with 0.6.1.

## Fixes

- Windows builds pass the release lint gate again, so the Windows client
  can be rebuilt and published from this tag.
- Recreating a Media Foundation encoder or decoder no longer leaks codec
  activation references.
- Android hardware decoding handles unusual vendor AVCC buffers without
  risking an integer overflow or a misclassified frame.
- Android hardware encoding no longer repeats the previous frame while
  the codec is still buffering its first output.

# Parley 0.6.1

A new media plane, smoother sharing, and volume controls that finally do
what they say. Server and every client must be upgraded together.

## A dedicated media path

- Screen video, camera video and share audio now travel over an
  authenticated MoQ relay on UDP 4445. The server only fans out the
  broadcast — it never decodes or re-encodes your picture.
- The publisher adapts itself: bitrate first, then resolution, then frame
  rate, with a two-second keyframe interval and a quick snap back when the
  network recovers.
- Hardware encoding and decoding use VA-API on Linux, Media Foundation on
  Windows and MediaCodec on Android, with openh264 as the fallback.

## Fixes

- Share audio no longer stops after its first moment. Its Opus timestamps
  now come from the encoder's sample clock, so a burst of packets can no
  longer rewind the track and close it.
- Per-user voice volume now changes what you hear, from both the slider and
  the mouse wheel. The mixer was treating the account id in the roster as a
  per-device stream id; it now maps between them correctly.
- Switching a user's share audio off clears every device they are sharing
  from, without touching their voice.
- Hardware keyframe checks accept encoders that buffer their first frames,
  and the VA-API path no longer schedules more frames than the encoder can
  sustain.

## Upgrade

This is protocol 2. Open UDP 4445 on the server, upgrade the server first,
then every desktop and Android client. A 0.6.0 client cannot join a 0.6.1
server.

# Parley 0.6.0

High-motion screen and camera shares stay sharp, and voice keeps its
share of the connection.

## Sharper sharing

- Each video frame now travels on its own reliable stream. One lost
  packet no longer wipes out a keyframe, so fast-moving screens and
  cameras hold their resolution instead of turning into a slideshow.
- Camera shares use the same transport, pacing and recovery as screen
  shares.
- A slow viewer gets a server-encoded lower tier while the sharer keeps
  sending full quality.

## Smoother calls

- Keyframes no longer crowd out voice audio on a busy link.
- A stalled viewer no longer delays everyone else, and keyframe requests
  are coalesced so the encoder is not forced into a keyframe loop.

# Parley 0.5.3

Streams stay on screen, show real names, and Android gets its camera flip
and a back button that no longer ends the call.

## Streams

- A share no longer disappears and comes back every few seconds. The
  client was checking a per-device stream id against account ids, so every
  roster refresh removed the live cards.
- Share cards show the sharer's username instead of `user 1` or `user 5`.
- A stopped share closes immediately instead of waiting for the watchdog.

## Android

- Camera shares have a front/back switch on the share bar and in the
  notification.
- Back leaves fullscreen first; anywhere else it minimizes the app, so a
  call or screen share keeps running. Your own fullscreen share has a
  Stop sharing button.
- The updater now hands the downloaded APK to Android's installer
  correctly. The download directory was outside the installer's shared
  roots, which caused `Failed to find configured root`.

# Parley 0.5.2

A bug-fix release for the 0.5 line. The wire protocol is unchanged.

## Android

- Receiving a screen share no longer crashes. Release builds were
  renaming a Flutter type used by the video plugin's native interface;
  that interface is now kept stable.

## Voice and streams

- A brief encoder stall no longer permanently silences your microphone
  until you leave and rejoin the channel.
- The stream tile no longer disappears and returns every five seconds;
  the periodic refresh updates the existing tile instead of announcing a
  new share.

## Server

- Kicking a device or deleting its channel no longer tears down the same
  account's other device.
- Chat length is counted in characters, so long non-Latin messages are no
  longer silently dropped.

# Parley 0.5.1

This release fixes the release build itself. The Android APK is signed by
the release key, and the terminal client builds with the 0.5.0
multi-device protocol. The 0.5.0 features below are included.

# Parley, now in your pocket.

0.5.0 brings the Android app, and lets a phone and a desktop stay
signed in together. Same rooms, same chat, same voice.

## Android

- The release page carries an APK for arm64 phones and tablets. Open it
  on the device and allow installs from this source the first time.
- Voice channels with echo cancellation, chat with files, presence and
  personal join/leave cues.
- Watch a friend's screen share from the phone.
- Settings → Updates downloads the next APK and hands it to Android's
  installer. The release key stays the same, so updates install over the
  current copy.
- The Parley mark is the launcher icon, the adaptive icon and the
  splash, instead of Flutter's default.

## One account, several devices

- Signing in on the phone no longer signs the desktop out. Both show up
  as badges on one presence row.
- Joining voice, muting or leaving happens on the device you did it on.
  The phone joining a channel does not move the desktop's microphone,
  and the phone leaving does not tear it down.
- Settings → Log out other devices disconnects the rest without making
  you sign in again later.

## Before you update

- This is protocol 1. Upgrade the server first, then every client;
  an older client is refused at the handshake instead of showing the
  wrong name on a voice bar.
- Android and desktop can talk to each other as soon as both are on
  0.5.0 or newer.
