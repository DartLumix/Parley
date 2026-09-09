# Parley

Self-hosted voice, video and screen-share server for a team of people who
already know each other. One binary, one container, one UDP port. TeamSpeak's
shape, not Discord's: one install = one community. Voice is the product;
everything else grows later.

## What it is
- Group voice channels, screen share at source quality (1440p60+), camera video.
- Native clients: **Linux → Android → Windows → iOS**, in that order.
- Join by IP/domain + account. 15–20 concurrent users target, no hard limits.
- Deploy: `docker compose up`. Config in one TOML file. Static IP assumed.

## What it's not
- No browser client, no macOS, no Electron/webview, ever.
- No multi-server hosting, federation, bots/public API, threads/forums.
- No E2EE at v1 — the server owner is trusted by design. DMs, rich text,
  search, push: later, not never.

## Tech decisions
- **Rust** — server *and* a shared core crate linked into every client.
  Protocol and media logic exist once, compiled per platform.
- **Flutter** for UI only (via flutter_rust_bridge); everything
  performance-critical stays in Rust.
- **QUIC** (quinn) as the only transport. TLS 1.3 built in, one connection:
  reliable streams for control/chat, unreliable datagrams for media.
  No WebRTC — no browser and no P2P means ICE/STUN/TURN are dead weight.
- **SFU, no transcoding** — server forwards encoded packets; clients decode
  and mix themselves, which gives per-user volume. Audio never touches the
  server's CPU beyond a copy.
- **Opus** audio: 64 kbps speech default, up to 256 kbps stereo for music
  channels, auto-downgrade under congestion.
- **H.264** video: hardware encode/decode on every target platform. Codec
  IDs are negotiated per stream, so VP9/AV1 are additions, not rewrites.
- **Echo cancellation / noise suppression**: WebRTC's audio processing
  library (same one PipeWire uses). Never hand-rolled.
- **Accounts**: per-server username + password (Argon2id), simple
  admin/user roles. Self-signed cert, fingerprint pinned on first connect
  (share the fingerprint with the invite — TeamSpeak model).
- **SQLite (WAL)** for persistence. **Protobuf** for control messages.
- Wire protocol carries version + capability bits everywhere, so later
  features are additive, never rewrites.

## Roadmap
| Phase | Scope | Done when |
|---|---|---|
| 0 | Repo, crates, CI matrix, Docker image, protocol skeleton | Builds on all targets, server runs in compose |
| 1 | QUIC connect, cert pinning, login, presence | Two clients online, see each other, reconnect works |
| 2 | Basic text chat + SQLite | Messages survive restart, 20 clients stable |
| 3 | Voice MVP: Opus, SFU routing, jitter buffer, mixing | Mouth-to-ear ≤ 150 ms (80 ms LAN), 1% loss tolerated |
| 4 | Voice hardening: AEC/NS, FEC, per-user volume, bitrate ladder | Music channel at 256 kbps, 5% loss survivable |
| 5 | Screen share: capture, H.264, 1440p60, multi-sharer | Glass-to-glass ≤ 250 ms, stable under bandwidth cap |
| 6 | Camera video + A/V sync | Sync skew ≤ 40 ms |
| 7+ | Android/Windows/iOS bring-up, file transfer, roles, rich text, search, DMs, push, auto-update | Ordered by need |

Platform bring-up (Android, Windows, iOS) starts after Phase 4; Linux is the
development platform throughout.

## Performance budgets
Mouth-to-ear ≤ 150 ms · screen share ≤ 250 ms · server idle ≤ 64 MB RSS ·
10 talkers ≤ 5% of one core · login→connected ≤ 400 ms · binary ≤ 15 MB.
A regression is a bug, tracked as one.

## Deployment
```yaml
services:
  parley:
    image: parley:latest
    ports: ["443:443/udp"]
    volumes: ["./data:/data"]
    restart: unless-stopped
```
First boot generates the certificate and admin account. All state (SQLite,
certs, uploads) lives under `data/` — backup by copying. No telemetry, no
outbound connections.

## Layout
`crates/parley-{proto,core,server}` · `app/` (Flutter client) ·
`proto/` (protocol source of truth) · `docs/adr/` (one file per decision).
