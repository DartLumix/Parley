# Changelog

All notable changes are recorded here. Releases are tagged `vX.Y.Z`;
`vX.Y.Z-rcN` tags are prereleases and are never marked latest.

## [Unreleased]

### Added

- **A Windows client.** The same Rust core and Flutter UI, built on
  `windows-latest` and attached to releases as
  `parley-<v>-windows-x86_64.zip`: chat, voice through WASAPI with echo
  cancellation, and watching other people's shares through openh264.
  Screen sharing is not there yet — the portal/PipeWire capture path has
  no Windows equivalent — so the Share button is absent rather than
  failing when pressed, and software H.264 carries video.

### Changed

- **The AEC is its own cargo feature, one per backend.**
  `audio-device` is now device I/O alone; `aec` is the WebRTC APM (Unix)
  and `aec-rust` is the pure-Rust AEC3 port (Windows), with `aec.rs`
  exposing one API over both and refusing a build that enables them
  together. `aec-rust` is plain Rust, so the synthetic-echo test runs on
  every platform, not only where meson exists. Desktop builds enable one
  of them; on Linux, `screen-capture` without either is a compile error,
  so a release can never quietly lose echo cancellation.

## [0.3.5] — 2026-10-22

Smoother shares, a screens section that survives a small window, and
nothing playing until you ask it to.

### Added

- **A join/leave cue.** Somebody entering or leaving the voice channel
  you are in plays a short rising (join) or falling (leave) pair,
  mixed into this machine's render only — never sent to peers, never
  in the AEC reference, never in a screen share's audio. Silent when
  you are not in voice, which is the only time it would have
  somewhere to play.

### Changed

- **The sharer's own picture is the stream's own pixels.** The preview
  was a fixed 640×360 canvas with the capture letterboxed into it, so a
  3440×1440 screen became 640×268 inside black bars and was then scaled
  back up — soft in the tile, and unusable on the new fullscreen page,
  which blew 360p across the whole screen. The canvas now follows the
  encoder's output size (same aspect, up to 2560×1440), the tile and the
  fullscreen page use that shape instead of assuming 16:9, and the
  hand-off is a straight BGRA→RGBA swizzle when the encoder keeps the
  capture size: 3.3 ms/frame at 1080p and 4.3 ms at 2560×1070 on the
  preview thread (`preview_render_cost`), against a 16.7 ms slot. The
  preview also follows the share's own rate instead of a fixed 30 fps.
- **An unopened share shows a poster, not the stream.** A remote tile
  used to play the live picture behind its Watch button, so "watch"
  was a label rather than a choice. It is now a dark plate with the
  sharer's name, the stream's size and rate, and Watch; opening it is
  what starts the picture. The display bridge is told which senders
  are on screen, so a closed share costs no RGB conversion, no
  texture upload and no raster-thread hand-off per frame — on a 1080p
  stream that is 8 MB and a platform-thread hop, 30 times a second,
  per share nobody is watching. Decoding is deliberately not gated:
  opening a tile shows its next frame instead of waiting for the next
  IDR.

### Fixed

- **The join/leave cue fired on news that was not a join.** The server
  sends a mute as the same `VoiceState` a join uses (`joined: true`,
  the member's current channel), and any re-announce — the 10 s roster
  poll, a reconnect, a second device — repeats a membership that never
  moved. The cue now follows the *transition*: a member who was not in
  your channel and now is (or the reverse), never a mute, never a
  member re-announced, never your own events. `isVoiceMembershipCue`
  is the whole rule, and it is unit-tested.
- **Fullscreen froze the share it was showing.** The viewer's
  fullscreen button lives on an open tile and entered through the same
  call the Watch button uses — which is a toggle. So opening fullscreen
  CLOSED the share on the way in: the display bridge was told nobody
  was looking, stopped converting and uploading frames, and the
  fullscreen page painted the one frame it already had until the viewer
  was  dismissed. Opening is now idempotent, so the picture follows the
  same texture into fullscreen and keeps running.
- **Expanding your own share closed it.** Your tile's expand button ran
  the same toggle, so on your own share it did the worst possible pair:
  it closed the tile (back to *Watch*) and put the window into
  fullscreen showing the session. Your own share now gets the same
  fullscreen page as anyone else's, drawing the capture preview on its
  16:9 canvas (the preview switch is turned on first if it was off),
  with *Exit fullscreen* and no *Stop watching* — there is no watch
  state to stop. When the share ends, the page closes and the window
  leaves fullscreen on its own.
- **A hardware share paced at the rate the encoder measured during
  its own cold start.** The self-test times the encoder's first
  frames, which pay for the video processor's context, the BGRX
  surface and the driver's shaders — so a machine whose encoder ran
  at ~60 fps paced its whole share at 50, and one at 1080p read as 30.
  Worse, the 60 fps choice in the share dialog could not exceed it: a
  60 fps request was silently a 30 fps stream. The measured rate is
  diagnostic now; pacing uses the profile ceiling (60 fps hardware,
  30 fps software) lowered only by the capture's own rate and the
  dialog's cap, and the sharer's tile reports the rate actually being
  sent rather than the one requested.
- **The VAAPI upload copied every frame out of the surface before
  overwriting it.** `vaGetImage` is a full-frame read-back, and the
  upload path overwrote all of it a moment later (the image format
  was also re-queried from the driver on every frame). 720p encodes
  went from 16.8 to 14.8 ms per frame and 1080p from 33.6 to 30.1 ms
  (`crates/parley-core/tests/vaapi_encode_cost.rs`).

- **1080p30 software encoding now fits its frame budget.** openh264
  encoded each frame as one slice on one core: measured against a
  screen-content workload at 1080p, mean 23.5 ms with a 48 ms p95 and
  117 of 240 frames over the 33 ms slot, which the sender's
  skip-to-newest pacing turned into visible stepping. Frames are now
  split into ~4 KB slices across four threads — 19 ms mean, 34.5 ms
  p95, 17 of 240 over — and 720p has comfortable headroom (mean 9 ms).
  `crates/parley-core/tests/software_encode_cost.rs` reproduces the
  measurement.
- **The software encoder no longer accepts more than 1080p.** A
  *Source* share on a 1440p/4K display used to hand openh264 the whole
  frame; at 1440p the same test measures 35 ms mean / 62 ms p95, so
  more than half the frames missed their slot. Software shares are
  downscaled to 1080p (aspect preserved, even dimensions); hardware
  encoding is not capped. The sharer's tile reports the stream's real
  size, so a capped share never claims a resolution viewers do not get.
- **The own-share preview runs at the stream's own 30 fps.** At 15 fps
  the only place a sharer can judge smoothness looked like a
  stuttering stream; the scaling still happens on its own thread with
  a latest-wins hand-off, so the encoder never waits for it.
- **A short window keeps a usable chat.** With a share open the screens
  section took 84% of the pane regardless of its height, leaving the
  chat a header and an input under the floating bar. The chat now has
  a floor (200 px); the screens section takes what is left.
- **Speakers are easier to read.** One row per person is a little
  taller, with 13 px names and 15 px icons; channel names and the
  "N in voice" line grew with it, and the column's measured width
  follows the new sizes. The column is wider too — more padding per
  row, more room after the mic icon, and a floor of 420 px (near double
  the old 220) with the cap at 560, so the roster is a comfortable
  list rather than a strip of names fitted to their own width.

## [0.3.0] — 2026-10-21

A screens section that holds every share, settings that stick, and a
preview that costs the stream nothing.

### Added

- **A screens section that actually contains the screens.** Every share
  — yours included — is a 16:9 tile with the sharer's name, the stream's
  real resolution and frame rate, a live thumbnail and a *Watch* button.
  One share fills the section; two split it left/right; three put two on
  top and centre the third below; four make a cross, and so on. Opening
  a share makes it big and leaves the rest as thumbnails in a strip
  along the bottom, so watching one stream never hides that others are
  still sharing. Several can be open at once; each one's system audio
  has its own slider, and closing one mutes only that one.
- **See your own stream while sharing.** Your tile is fed by the capture
  tap (fixed 640×360 canvas, letterboxed) before encoding, so it shows
  exactly what is being sent. The switch on the tile turns it off — off
  means no capture-side scaling work at all, mid-share included. The
  preview is rendered on its own thread and shares the captured buffer
  with the encoder (one `Arc`, no copy), so it can never delay a frame.
- **Settings page.** Video: hardware (VAAPI) encode, default share
  resolution and frame rate. Audio: echo cancellation (speech channels;
  music channels never used it and still do not). Account: the connected
  server and account, and *Sign out*. Saved immediately to
  `~/.config/parley/settings.toml`; the hardware-encode preference is
  re-applied at launch, and the file is read with defaults for missing
  keys so an older file keeps working.
- **Share options dialog.** Starting a share asks for a resolution cap
  (source/1440p/1080p/720p/480p) and frame-rate cap (source/60/30/15)
  before the portal opens. Both are caps: the capture is downscaled with
  nearest-neighbour sampling (aspect preserved, even dimensions), never
  enlarged, and pacing never exceeds the rate the encoder can hold.
- **A roster that reads downwards, in a column sized to its names.**
  Speakers are one per row inside their channel instead of inline chips,
  and the channel column is measured from the longest name (with a floor
  and a ceiling) so screens and chat get the rest of the window.

### Changed

- The hardware-encode switch moved from the floating bar to Settings →
  Video. One persisted source of truth; it still applies on the next
  share.
- The own-preview switch lives on your screen tile, not in Settings, and
  the corner overlay is gone: your share is one of the tiles, big or
  small like everyone else's.
- Remote tiles keep up with the stream: a one-second arrival window
  measures the real frame rate (so a limping encoder shows what it is
  actually delivering), and a shared window resized mid-share updates
  the tile's resolution instead of showing the old one.

## [0.2.3] — 2026-10-20

Staying signed in, and a roster that shows what people are actually doing.

### Added

- **Closing the window no longer signs you out.** A login remembers the
  server-issued session token — never the password — in
  `~/.config/parley/session.toml` (owner-only). The next launch reconnects
  to the last server on its own, and *Reconnect as …* does the same after a
  dropped connection. *Disconnect* is the explicit sign-out and deletes the
  token; a token the server refuses falls back to the form with the server
  and account prefilled.
- **Per-user volume.** Click a name in a channel roster for a 0–200% voice
  slider, applied in the receive mix.
- **Per-stream volume.** A sharer's system audio has its own bar, both in
  the roster dialog and in the share pane, trimmed independently of their
  voice — two streams, two levels.
- **Muted badge.** Mute is announced (`SetMute`) and kept by the server per
  session, so every roster shows a muted mic, a late joiner's snapshot
  includes it, and a channel rejoin carries it.
- **Speaking indicator.** Roster chips light up while that person's decoded
  audio is non-silent, from per-sender peaks the mixer already computes
  (polled at 10 Hz and decayed so the glow fades), and the mic button
  lights with your own capture level. Peaks are measured before volume, so
  a turned-down person still lights up.

### Changed

- `VoiceState` gains `muted`; `ClientMessage` gains `SetMute`. The change is
  additive — an older client simply never announces a mute.

## [0.2.2] — 2026-10-18

A reconnection fix: a server whose certificate changed is now a question the
client asks, not a wall it hits.

### Fixed

- **A changed server certificate no longer dead-ends the login.** The app
  shows the fingerprint saved on the machine beside the one the server
  presents now and offers *Trust new certificate*; nothing is written until
  you accept, and a refused attempt never re-pins. The CLI names both
  digests and the pin file instead of burying the refusal in a transport
  error. This is the expected case after deploying onto a fresh volume,
  which generates a new identity.
- **One server, one pin.** Pins are keyed by `host:port`, so `your-server`
  and `your-server:4443` (and a differently-cased name) are the same trust
  decision. Retyping the default port used to bypass the existing pin.

### Added

- **Certificate probe across the bridge** — a TLS-only, credential-free
  handshake the app uses to fill the trust prompt, with an end-to-end test
  that refuses a stale pin, leaves it untouched, then trusts and connects.

## [0.2.1] — 2026-10-18

A small follow-up to 0.2.0: a presence bug fix, a simpler deploy download, and
layout/UX polish.

### Added

- **Microphone mute.** A *Mute* button next to *Share screen* (only while
  in voice) stops the mic at the capture callback — muted users send no
  packets at all, and the mute survives channel switches.
- **One-file deploy kit.** `parley-deploy-<version>.tar.gz` now carries
  the server image, so deploying is one download, one extract and one
  `docker load`. There is no separate image asset to fetch.

### Changed

- **Client layout.** Channels fill the left pane; chat moved to the right
  pane (the one shares used before). A screen share now appears above the
  chat and pushes it down instead of taking over half the page.
- **Brighter app bar**, a lifted blue-black instead of near-black.
- **Port 4443 is the deploy default** end to end (compose, env template,
  runbooks): `your-server` with no port just works. 443 still works by
  setting `PARLEY_PORT`.

### Fixed

- **A disconnected user no longer stays online.** Disconnecting closes
  the QUIC connection explicitly (and the viewer's report task exits
  with its session), so the server ends the session immediately instead
  of keeping the account in presence for the next login.

## [0.2.0] — 2026-10-18

First downloadable release: a Linux client tarball, a single-container
server, and the tooling to verify the pair.

### Added

- **Release pipeline.** Tagging `v*` runs the full test suite, builds the
  Linux client tarball, the terminal client, the static server binary and
  the OCI image, then boots the image and runs a real client handshake
  before publishing the GitHub release.
- **Deploy kit.** `deploy/compose.yaml` — one container, one UDP port,
  one named volume — plus `.env.example` and the VPS runbook. The image
  now carries `parley-adduser`, and `parley-server adduser <user> <pass>`
  creates accounts inside the container.
- **Functional test.** `tools/e2e-container.sh` boots the image, adds a
  second account, logs in as both users, and proves a wrong password is
  refused. The client's new `--check` does connect → TLS/TOFU → auth →
  channel list → heartbeat and exits 0, without opening any device;
  an unreachable UDP port fails within 10 s with a sentence instead of
  hanging.
- **Client toolbar.** The GTK header bar is gone (it was the white strip
  above the page on Wayland). A thin toolbar slides down when the pointer
  reaches the top of the window with minimise / maximise / close, and the
  window is undecorated. The GTK app id is now
  `io.github.dartlumix.Parley`.
- **DNS.** `host[:port]` is accepted everywhere (app and terminal client),
  port defaulting to 4443; bracketed IPv6 works. Previously only
  `IP:port` parsed.
- **Version display.** The login page shows the build version, injected
  with `--dart-define=PARLEY_VERSION=…` by the release build.
- **UI tests.** Flutter widget tests for the hover toolbar and the login
  form, and CI now compiles and tests the Flutter half.

### Changed

- WebRTC audio processing is linked statically (`bundled`). The old build
  depended on the system's `libwebrtc-audio-processing-2`, which most
  distributions do not ship — the prebuilt client would not start there.
- The Flutter app is split into `main.dart` (bootstrap), `app.dart`,
  `home_page.dart`, `theme.dart`, `window.dart` and `widgets/`; the
  `ping`/`pong` scaffold and its generated file are removed.
- `docker-compose.yml` builds from a checkout; `deploy/compose.yaml` runs
  the released image with a named volume.
- MIT license, changelog, and a `docs/` index; dead internal-document
  references in the ADRs were replaced with the measurement document.

### Fixed

- **Build portability.** See the bundled AEC above.
- **Container account creation.** The image previously had no way to add
  a second account; the only path was the first-boot admin.

[0.2.0]: https://github.com/DartLumix/Parley/releases/tag/v0.2.0
