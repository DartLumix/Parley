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

# Looks sharp.

## 01 · Streams hold the line

Frames leave at the stream's bitrate instead of in a burst. Fast motion no longer collapses the picture to 5 fps, and the bitrate ladder steps down gently instead of dropping off a cliff.

## 02 · Camera, everywhere

Share a webcam on Windows too (beta), or switch on Include camera and stay in the bottom-right corner of any screen share. One stream, one tile, both views.

## 03 · If it fails, keep going

A camera that cannot open never takes a screen share down. Windows shows the error; the stream keeps running.

Under the hood: six bitrate rungs, byte-level pacing, and a server that stays a pure relay unless you opt into transcoding.
