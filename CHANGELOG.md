# Changelog

All notable changes are recorded here. Releases are tagged `vX.Y.Z`;
`vX.Y.Z-rcN` tags are prereleases and are never marked latest.

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
