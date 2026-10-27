<!-- Parley release notes.

     This file travels inside every build and is what the app shows: on the
     first launch of a version (automatically) and behind Settings → What's
     new. Write it before you tag a release — the bundled copy is the one
     users read, since the GitHub release body is generated from
     CHANGELOG.md and stays on the web.

     Keep it short and user-facing: headings and bullet lists render as
     headings and bullets, everything else as a paragraph.
-->

- Parley now updates itself: **Settings → Updates → Check for updates**
  finds a new release, installs it after checking the download against
  the release checksums, and comes back on the new build. A call or a
  share in progress ends with the restart, and an install owned by a
  package manager is left alone and says so.
- What changed shows up by itself, here, the first time you launch a new
  version — and any time from **What's new**, next to the update button.
