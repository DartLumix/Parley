<!-- Parley release notes.

     This file travels inside every build and is what the app shows: on the
     first launch of a version (automatically) and behind Settings → What's
     new. Write it before you tag a release — the bundled copy is the one
     users read, since the GitHub release body is generated from
     CHANGELOG.md and stays on the web.

     Keep it short and user-facing: headings and bullet lists render as
     headings and bullets, everything else as a paragraph. Asterisks and
     backticks are stripped, so use plain words instead of emphasis.
-->

# More than a voice.

## 01 · Streams hold the line

Frames leave at the stream's bitrate instead of in a burst. Fast motion no longer collapses the picture to 5 fps, and the bitrate ladder steps down gently instead of dropping off a cliff.

## 02 · Camera, everywhere

Share a webcam on Windows too (beta), or switch on Include camera and stay in the bottom-right corner of any screen share. One stream, one tile, both views.

## 03 · Make it yours

A new Profile page sits next to Settings. Change your username or password — the current one is required — and add a picture straight from your files. Your initials fill in until you do, and your picture follows you onto the roster and beside your chat messages.

## 04 · A poke that gets through

Pokes now ring even with your headphones muted, while AFK or deafened. That is the whole point of a poke.

Under the hood: six bitrate rungs, byte-level pacing, avatar bytes fetched once per revision, and a server that stays a pure relay unless you opt into transcoding.
