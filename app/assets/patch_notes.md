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
