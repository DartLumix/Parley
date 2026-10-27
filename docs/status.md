# Status

Snapshot: **0.2.0** (2026-10-18). Linux desktop client + self-hosted
server, released as a client tarball, a single-container image and a
deploy kit.

## What works, and what proves it

| area | state | evidence |
|:--|:--|:--|
| QUIC transport, TLS identity, TOFU pinning | shipping | transport tests; `--check` against a real container |
| Accounts, sessions, tokens, presence, heartbeat | shipping | server tests; container e2e (bootstrap, adduser, login, negative) |
| Text chat, history, per-channel broadcast | shipping | server tests; app UI |
| Voice: Opus 48 kHz, jitter buffer + PLC, FEC, loss ladder | shipping | core/app tests; digital latency 49.8 ms median (budget 150 ms) |
| AEC/NS (WebRTC APM), per-user volume, music channels | shipping | synthetic-echo test (>10 dB), by-ear A/B |
| Screen share: portal capture, H.264, fragmentation, drop-until-IDR | shipping | core video tests; glass-to-glass ~250 ms LAN |
| Hardware encode (VAAPI), HD profiles, DMA-BUF capture | shipping, opt-in | keyframe-shape tests; measured 98/47.8/29.1 fps at 720p/1080p/1440p |
| Share audio (system output), share lifecycle notices | shipping | receiver e2e + latency measurement |
| Flutter client: session, channels, chat, voice, share watch/send, fullscreen | shipping | widget tests, release bundle, live two-machine runs |
| Container + release automation | shipping | `tools/e2e-container.sh`; release workflow builds and tests before publishing |
| In-app updates, bundled release notes | shipping | unit/widget tests for the release decision, checksums, note popup and the swap scripts' quoting; the swap itself is manual (see below) |

Performance budgets (server RSS, 10 talkers, login time, binary size)
were verified in 6g and have not regressed since; see the README table.

## Limits and unverified things

- **Linux is the reference client.** Windows ships voice with echo
  cancellation (the pure-Rust `aec-rust` backend; the C++ APM does not
  build there), chat and share *viewing*; starting a share needs a
  capture backend that does not exist yet. Android and iOS are parked;
  macOS has never been built.
- **Hardware encode is VAAPI only**, and one known driver window (the
  polaris box) must not pass `--hw-encode` — the probe enforces that.
- **Fullscreen state readback** was proven on one compositor
  (Hyprland). Other desktops are unverified.
- **The window toolbar's visual result** is verified by widget tests and
  the startup log (`decorations off`), not by watching it on every
  desktop.
- **The connected VPS path** is verified with the identical image and
  compose file locally; the actual remote host has not been booted from
  here.
- **Two-machine media runs are manual** (runbook in
  [two-machine-demo.md](two-machine-demo.md)); CI covers protocol, auth
  and routing, not pixels or audio.
- A local packaging build on a machine with system abseil >= 20240722
  links that abseil (the release CI does not; its tarball is
  self-contained).
- **The update swap is not covered by CI.** The release decision, the
  checksum check, the notes popup and the generated scripts are tested; the
  part that waits for the app to exit and moves directories runs on a real
  desktop. It is verified by hand on Linux and on Windows, and every step
  is logged to `update.log` beside the download.
- **The updater reads published releases only.** It installs the asset for
  its platform from `releases/latest` and verifies it against that release's
  `SHA256SUMS`; an `-rc` tag is never offered, and a release that lacks the
  asset or the checksum file is reported in the settings card instead of
  being half-installed.

## Missing features

Product gaps, roughly in the order a small community would notice:

1. **Moderation depth** — accounts, three role tiers, channel CRUD and
   voice kicks are in the management page, but there are no bans,
   temporary mutes, per-channel ACLs or audit trail.
2. **Client robustness** — no automatic reconnect with backoff (a
   dropped connection waits for *Reconnect*), no device pickers
   (input/output), no deafen/push-to-talk, no notifications.
3. **Chat depth** — no DMs, mentions, rich text, search, attachments or
   file transfer.
4. **Share depth** — no remote control, and no per-window "watch this
   one" pinning: *Watch* opens a share, the tile itself is a poster (the
   stream is decoded and measured behind it, but not painted) before
   that. Your own stream is one of the tiles while you share, and its
   preview is live because it is your own capture.
5. **Operations** — no metrics endpoint or log rotation; one channel is
   created by default (`general`), the rest are managed in the app.
6. **Distribution** — no AppImage or Flatpak package, and the AUR
   package is prepared but not published; the tarball and the Windows zip
   (with in-app updates straight from the release page) are the client
   artifacts.
7. **Other platforms** — Windows now builds (voice with echo
   cancellation, chat, viewing; see the limits above); what it lacks is
   capture (WGC/DXGI + WASAPI loopback), which is why the Share button is
   absent there. Android and iOS are after that; camera video is parked
   until a client can preview it.

## Next milestone suggestions

- **8a — moderation depth:** bans and temporary mutes, per-channel
  permissions, and an audit trail on top of the management page.
- **8b — client polish:** reconnect with backoff, remembered servers,
  device pickers, mute/deafen, and a bounded connect timeout (the CLI's
  `--check` now has one; the app still waits).
- **8c — packaging:** AppImage or Flatpak build and the first AUR push,
  so a new device needs one download and no terminal (in-app updates are
  in).
- **8d — Android bring-up** (the second client, reusing `parley-core`).
- **8e — verification depth:** property/fuzz tests on the wire
  decoders, a two-client CI test that exchanges media datagrams, and a
  perf budget check in CI.

## Where the details live

- Deployment, accounts, backup, upgrades: [deploy.md](deploy.md)
- Client install and troubleshooting: [client.md](client.md)
- Cutting and verifying a release: [release.md](release.md)
- Build, features, tests: [development.md](development.md)
- Encoder measurements and transcripts: [hd-encode-runs.md](hd-encode-runs.md)
- Decisions: [adr/](adr/)
