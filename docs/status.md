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

Performance budgets (server RSS, 10 talkers, login time, binary size)
were verified in 6g and have not regressed since; see the README table.

## Limits and unverified things

- **Linux only.** Android / Windows / iOS are parked; macOS has never
  been built.
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

## Missing features

Product gaps, roughly in the order a small community would notice:

1. **User management** — no RPC and no UI: accounts are created with
   `parley-server adduser` inside the container. No roles, bans, kicks
   or per-channel permissions.
2. **Client robustness** — no automatic reconnect with backoff (a
   dropped connection waits for *Reconnect*), no device pickers
   (input/output), no deafen/push-to-talk, no notifications.
3. **Chat depth** — no DMs, mentions, rich text, search, attachments or
   file transfer.
4. **Share depth** — no remote control, no per-window "watch this one"
   pinning in the app, and no preview of someone else's screen before
   watching (your own stream is previewed while sharing).
5. **Operations** — no metrics endpoint, log rotation, or admin UI; one
   channel is created by default (`general`) and channels are created by
   editing SQLite.
6. **Distribution** — no AppImage/Flatpak/AUR package and no
   auto-update; the tarball is the only client artifact.
7. **Other platforms** — Android and Windows are the next clients;
   camera video is parked until a client can preview it.

## Next milestone suggestions

- **8a — management surface:** user CRUD + roles over the protocol,
  channel creation, and the admin UI that makes the container
  self-sufficient.
- **8b — client polish:** reconnect with backoff, remembered servers,
  device pickers, mute/deafen, and a bounded connect timeout (the CLI's
  `--check` now has one; the app still waits).
- **8c — packaging:** AppImage or Flatpak build + auto-update, so a new
  device needs one download and no terminal.
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
