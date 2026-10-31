<!-- Parley release notes.

     This file travels inside every build and is what the app shows: on the
     first launch of a version (automatically) and behind Settings → What's
     new. Write it before you tag a release — the bundled copy is the one
     users read, since the GitHub release body is generated from
     CHANGELOG.md and stays on the web.

     Keep it short and user-facing: headings and bullet lists render as
     headings and bullets, everything else as a paragraph.
-->

- **Screen sharing now works on Windows.** *Share screen* opens the
  Windows picker for a monitor or a window, sends through the same H.264
  path as Linux, and shows your own live preview. Sharing the desktop
  normally costs nothing extra; a still screen keeps streaming too.
- **Share audio on Windows.** Pick the applications whose sound should
  travel with the stream (or share the whole default output). Parley's
  own voices are never captured, so nobody hears themselves back.
- **Fullscreen is real on Windows.** The fullscreen viewer takes the
  whole monitor, not just the app's window, and restores the window
  where it was when you leave it.
- **A still screen no longer looks like a slow viewer.** The sender now
  compares what a viewer decodes with what it actually sends, so a
  desktop with nothing changing keeps its resolution instead of being
  folded down to 480p.
