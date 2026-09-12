# 🎙️ Parley

> Self-hosted voice, video and screen-share for teams that already know each other.
> **One binary. One container. One UDP port.**

TeamSpeak's shape, not Discord's — one install serves one community.
Voice is the product; everything else grows later.

---

## 📈 Status

| Phase | Scope | |
|:---|:---|:---:|
| 0 | Workspace, crates, CI, Docker image | ✅ |
| 1a | QUIC transport, TLS identity, first stream | ✅ |
| 1b | Config file, persistent certificate, stable fingerprint | ✅ |
| 1c | Protobuf control protocol, framed messages | ✅ |
| 1d | Accounts — SQLite, Argon2id, login | ✅ |
| 1e | Presence, heartbeats, session tokens | ✅ |
| 2 | Text chat — channels, messages, history, broadcast | ✅ |
| 3a | Voice channels, state broadcast, snapshot | ✅ |
| 3b | Opus encode/decode (parley-core) | ✅ |
| 3c | Jitter buffer + PLC | ✅ |
| 3d | Audio device layer — cpal, feature-gated | ✅ |
| 3e | Audio pipelines, native 48 kHz I/O, test client | ✅ |
| 3f | Latebct measurement --verdict below | ✅ |
| 4 | Hardening, screen share, video, platforms | ⏳ |
| 4a | AEC / NS (WebRTC APM) on mic path | ✅ |
| 4b | Per-user volume in client mix | ✅ |
| 4+ | FEC, bitrate ladder, congestion, screen share | ⏳ |

---

## ✨ What it is

- 🎯 Group voice channels, screen share at source quality (1440p60+), camera video
- 🐧 Native clients: **Linux → Android → Windows → iOS** — no Electron, no webviews, ever
- 🔑 Join by IP/domain + account · 15–20 concurrent users target
- 🐳 Deploy with `docker compose up` · one TOML config · one UDP port

## 🚫 What it's not

- No Electron bulls**t
- No multi-server hosting, federation, or public API (internal for now)
- No E2EE at v1 — the server owner is trusted by design
- DMs, rich text, search, push — later, not never

---

## ⚙️ Tech stack

| Layer | Choice | Why |
|:---|:---|:---|
| Language | **Rust** | Server *and* shared core crate compiled into every client — protocol exists once |
| UI | **Flutter** + `flutter_rust_bridge` | UI only; all performance-critical code stays in Rust |
| Transport | **QUIC** (quinn) | TLS 1.3 built in · streams for control, datagrams for media · no WebRTC (no browser, no P2P = no ICE/STUN/TURN) |
| Voice | **SFU, no transcoding** | Server forwards packets; clients mix — per-user volume, near-zero server CPU |
| Audio codec | **Opus** | 64 kbps speech → 256 kbps stereo music, auto-downgrade under congestion |
| Video codec | **H.264** | Universal hardware encode/decode · codec IDs negotiated per stream |
| AEC / NS | **WebRTC APM** | Echo cancellation + noise suppression on the mic path; far-end reference from the render mix |
| Auth | **Argon2id** | Per-server username + password · TOFU cert pinning (share the fingerprint with the invite) |
| Storage | **SQLite** (WAL) | Zero-config persistence |
| Messages | **Protobuf** (prost) | Versioned wire format, forward-compatible |
| TLS | **rustls + ring** | Pure Rust · pinned to ring for clean musl cross-compilation |

Every message carries version + capability bits — later features are additive, never rewrites.

---

## 🗺️ Roadmap

### Phase 1 — Transport & Identity
- [x] QUIC listener, self-signed TLS certificate, SHA-256 fingerprint
- [x] Persistent identity — cert survives restarts, fingerprint is stable
- [x] TOML config file with defaults
- [x] Protobuf `Hello` / `HelloAck` over length-prefixed QUIC streams
- [x] Accounts: SQLite, Argon2id, login flow, admin bootstrap
- [x] Presence: online user list, heartbeats, session tokens

### Phase 2 — Text Chat (basic)
- [x] Channel model, raw text, SQLite persistence, history fetch

### Phase 3 — Voice MVP
- [x] Voice channels: join/leave, state broadcast, snapshot
- [x] Media datagrams over QUIC + SFU routing
- [x] Opus encode/decode + 20 ms framing pipelines (parley-core)
- [x] Jitter buffer: wraparound-safe ordering + PLC on gap
- [x] Linux audio device layer — cpal, native 48 kHz (PipeWire graph runs 48k-only → no client-side resampling; rubato dropped)
- [x] Test client: TOFU fingerprint store, capture → Opus → send, recv → decode → mix → render
- [x] **Mouth-to-ear ≤ 150 ms (80 ms LAN)** — measured on dev machine (loopback):
      digital callback→render median 82 ms / max 114 ms; mouth-to-ear ≈ 100–135 ms.
      **150 ms budget: met. 80 ms LAN: missed** — dominant term is the render device
      period (106.7 ms, 5× PipeWire quantum; pipewire-alsa rejects quantum-aligned
      requests). Lever documented for Phase 4: PipeWire-native client or ALSA
      period workaround.

### Phase 4 — Voice Hardening
- [x] AEC / noise suppression — WebRTC APM (AEC3 + NS + high-pass) on the mic path;
      played mix is the far-end reference. Verified by synthetic-echo test (>10 dB
      reduction) + by-ear A/B (noise floor drop with AEC on). `--no-aec` disables.
- [x] Per-user volume — client-side gains in the mix (`--volume <user_id>:<gain>`)
- [x] FEC / RED — Opus in-band FEC: encoder embeds redundancy (SILK mode≤~32 kbps), 
      receiver reconstructs single lost frames from the successor packet before falling
      back to PLC
- [ ] Bitrate ladder (64→256 kbps), music channels
- [ ] Render-period latency fix (80 ms LAN target) — see Phase 3 verdict

### Phase 5 — Screen Share
- [ ] PipeWire capture → H.264 hardware encode → multi-sharer / multi-viewer
- [ ] **1440p60, glass-to-glass ≤ 250 ms**

### Phase 6 — Camera Video
- [ ] Same pipeline, A/V sync ≤ 40 ms

### Phase 7+ — Later
- [ ] Android / Windows / iOS bring-up
- [ ] File transfer (QUIC streams), roles, rich text, search, DMs, push, auto-update

---

## 🚀 Development

```bash
git clone <repo> && cd parley

cargo test --workspace                              # run tests
cargo run -p parley-server                          # dev mode (listens :4443)
docker build -t parley . && docker run --rm parley  # container build

cargo fmt --all                                     # format (CI enforces)
cargo clippy --workspace --all-targets -- -D warnings  # lint (CI enforces)

# hardware audio test (needs mic + output; never runs in CI)
PARLEY_AUDIO=1 cargo test -p parley-core --features audio-device --test audio_loopback -- --nocapture

# voice test client (needs mic + output; never runs in CI)
cargo run -p parley-server --bin parley-adduser -- data <user> <pass>
cargo run -p parley-client --features audio-device -- --server 127.0.0.1:4443 \
    --user <user> --password <pass>            # AEC/NS on by default; --no-aec to A/B
# two clients = two users; speak and listen on the other terminal
# per-user volume: append --volume <user_id>:<gain> (0.0 mutes)
# latency measurement: one client --measure, one --report (add --no-aec; loud clicks!)
# AEC verification (synthetic echo, no hardware): cargo test -p parley-client --features audio-device
```

Config (`config.toml`, auto-created with defaults):
```toml
listen = "0.0.0.0:4443"
data_dir = "data"
```

First boot: `PARLEY_ADMIN_PASSWORD=secret cargo run -p parley-server` creates the admin account.

First run generates `data/cert.der` + `data/key.pk8`. The fingerprint printed
on startup is **permanent** — that's the value you share with your team.

---

## 📊 Performance budgets

| Metric | Budget |
|:---|:---|
| Voice mouth-to-ear | ≤ 150 ms / 80 ms LAN — **current: ~100–135 ms typical (budget met, LAN miss)** |
| Screen share glass-to-glass | ≤ 250 ms |
| Server idle RSS | ≤ 64 MB |
| 10 active talkers | ≤ 5% of one core |
| Login → connected | ≤ 400 ms |
| Binary size | ≤ 15 MB |

A regression is a bug, tracked as one.

---

## 🐳 Deployment

```yaml
services:
  parley:
    image: parley:latest
    ports: ["443:4443/udp"]    # host 443 → container 4443
    volumes: ["./data:/data"]
    restart: unless-stopped
```

All state lives under `data/` — backup by copying. No telemetry, no outbound connections.

---

## 📁 Layout

```
proto/                  # .proto files — protocol source of truth
crates/
  parley-client/        # test client: TOFU pinning, 48k audio, AEC/NS, mix
  parley-proto/         # wire types, framing, generated protobuf code
  parley-core/          # shared session I/O + audio pipeline (server + future clients)
  parley-server/        # binary: QUIC listener, TLS, config, routing
app/                    # Flutter client (future)
docs/adr/               # one file per architectural decision
```
