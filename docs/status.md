# Status

Snapshot: **0.6.2** (2026-11-15). Self-hosted voice, video and screen
sharing with Linux and Windows desktop clients, an Android client, a
single-container server and a deploy kit.

Protocol 2 was introduced in 0.6.1 as a breaking media-plane change from
0.6.0: that upgrade requires the server, every desktop client and the
Android APK together. 0.6.2 keeps protocol 2 and is wire-compatible with
0.6.1. The server needs both UDP ports reachable: control/voice on 4443
and MoQ media on 4445.

## What works, and what proves it

| area | state | evidence |
|:--|:--|:--|
| QUIC control transport, TLS identity, TOFU pinning | shipping | transport tests; `--check` against a real container |
| Accounts, sessions, tokens, presence, heartbeat | shipping | server tests; container e2e (bootstrap, adduser, login, negative) |
| Text chat, history, files, per-channel broadcast | shipping | server tests; app UI |
| Voice: Opus 48 kHz, jitter buffer + PLC, FEC, loss ladder | shipping | core/app tests; digital latency 49.8 ms median (budget 150 ms) |
| AEC/NS (WebRTC APM; AEC3 port on Windows), per-user volume, music channels | shipping | synthetic-echo test (>10 dB), by-ear A/B; mixer tests for account/device gain mapping |
| MoQ media plane: authenticated relay, room broadcast, screen/camera/share audio | shipping | protocol/transport tests; live two-machine LAN run; server logs show the audio and video tracks and subscriptions |
| Screen share: portal capture (Linux), WGC (Windows), MediaProjection (Android) | shipping | core video/MoQ tests; Linux live run; Windows cross-target build; Android release build |
| Share audio: default output or exactly the picked apps (Linux/Windows) | shipping | PipeWire and WASAPI capture tests; live Linux two-machine receiver run |
| Hardware encode: VA-API Linux, Media Foundation Windows, MediaCodec Android | shipping | VAAPI keyframe/rate tests; platform backend unit tests; Windows and Android release builds |
| Hardware decode: VA-API, Media Foundation, MediaCodec with software fallback | shipping | backend round-trip tests; Linux hardware stream run; cross-target builds |
| Adaptive publisher: capped VBR, 2 s IDR, FPS-first two-stage, snap-back | shipping | `parley-core` controller tests; live 1080p/30 screen share |
| Camera share (Linux V4L2, Android Camera2) | shipping | conversion/rotation unit tests; Android release build; a live Linux camera run is still the manual check |
| Poke notifications, wheel volume on roster rows | shipping | server routing test; widget tests |
| Flutter client: session, channels, chat, voice, share watch/send, fullscreen | shipping | widget tests, release bundle, live two-machine runs |
| Windows client: voice, chat, share send/watch, WGC + WASAPI audio, MF codecs | shipping, cross-built | CI cross-build; no live Windows desktop in this repository's CI |
| Android client: voice, chat, screen/camera share, MediaCodec, APK updater | shipping | release build; no live Android device in this repository's CI |
| Container + release automation | shipping | `tools/e2e-container.sh`; release workflow builds and tests before publishing |
| In-app updates, bundled release notes | shipping | unit/widget tests for the release decision, checksums, note popup and swap scripts' quoting; the swap itself is manual (see below) |

Performance budgets (server RSS, 10 talkers, login time, binary size)
were verified in 6g and have not regressed since; see the README table.
The server is a zero-transcode relay, so its CPU and codec surface no
longer scale with the number or resolution of shares.

## Limits and unverified things

- **Linux is the reference client.** The live two-machine media runs in
  this repository use Linux on both ends. Windows is verified by the
  cross-target build and unit tests; Android by the release build and
  unit tests. Neither has a live device run in CI.
- **Hardware encode is opt-in on Linux** (VA-API), and one known driver
  window (the polaris box) must not enable it — the probe enforces that.
  Windows Media Foundation and Android MediaCodec are the normal path and
  fall back to software at runtime. A machine with no synchronous
  hardware MFT or no compatible MediaCodec encoder stays on openh264.
- **Camera sharing is Linux and Android only.** Windows needs a Media
  Foundation source reader before the choice can appear there. Android
  screen and camera shares are video-only: system share audio is not
  implemented on that platform. The Linux camera path has conversion and
  rotation tests; a live camera run is still manual.
- **Fullscreen state readback** was proven on one compositor
  (Hyprland). Other desktops are unverified.
- **The window toolbar's visual result** is verified by widget tests and
  the startup log (`decorations off`), not by watching it on every
  desktop.
- **The connected VPS path** is verified with the identical image and
  compose file locally; the actual remote host has not been booted from
  here.
- **Two-machine media runs are manual** (runbook in
  [two-machine-demo.md](two-machine-demo.md)); CI covers protocol, auth,
  relay authorization and routing, not pixels or audio.
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
   dropped connection waits for *Reconnect*), no deafen/push-to-talk, no
   notifications.
3. **Chat depth** — no DMs, mentions, rich text or search. File transfer
   and personal sounds are implemented.
4. **Share depth** — no remote control, and no per-window "watch this
   one" pinning: *Watch* opens a share, the tile itself is a poster (the
   stream is decoded and measured behind it, but not painted) before
   that. Your own stream is one of the tiles while you share, and its
   preview is live because it is your own capture.
5. **Operations** — no metrics endpoint or log rotation; one channel is
   created by default (`general`), the rest are managed in the app.
6. **Distribution** — no AppImage or Flatpak package, and the AUR
   package is prepared but not published; the tarball, the Windows zip
   and the signed Android APK are the client artifacts.
7. **Other platforms** — Windows now has the full desktop feature set
   except camera capture; Android has voice, chat, screen and camera
   sharing (video-only) and hardware codecs. iOS and macOS have not been
   built.

## Next milestone suggestions

- **8a — moderation depth:** bans and temporary mutes, per-channel
  permissions, and an audit trail on top of the management page.
- **8b — client polish:** reconnect with backoff, device pickers,
  mute/deafen, and a bounded connect timeout (the CLI's `--check` now has
  one; the app still waits).
- **8c — packaging:** AppImage or Flatpak build and the first AUR push,
  so a new device needs one download and no terminal (in-app updates are
  in).
- **8d — iOS/macOS bring-up** and the Windows camera source reader.
- **8e — verification depth:** property/fuzz tests on the wire and MoQ
  decoders, a two-client CI test that exchanges media groups, and a perf
  budget check in CI.

## Where the details live

- Deployment, accounts, backup, upgrades: [deploy.md](deploy.md)
- Client install and troubleshooting: [client.md](client.md)
- Cutting and verifying a release: [release.md](release.md)
- Build, features, tests: [development.md](development.md)
- Encoder measurements and transcripts: [hd-encode-runs.md](hd-encode-runs.md)
- Decisions: [adr/](adr/)
