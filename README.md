
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
| 3f | Latency measurement — verdict below | ✅ |
| 4a | AEC / NS (WebRTC APM) on mic path | ✅ |
| 4b | Per-user volume in client mix | ✅ |
| 4c | Opus in-band FEC | ✅ |
| 4d | Loss-driven bitrate ladder + congestion response | ✅ |
| 5a | Video wire format + loss-hardened assembler | ✅ |
| 5b | H.264 codec layer (openh264, feature-gated) | ✅ |
| 5c | Synthetic 720p30 sender + mpv display | ✅ |
| 5d | Portal screen capture (ashpd + PipeWire) | ✅ |
| 5e | Encoder trait, rate pacing, glass-to-glass measured | ✅ |
| 6 | Backend hardening & client-readiness | ⏳ |
| 7+ | Flutter client, camera, platforms | ⏳ |

---

## ✨ What it is

- 🎯 Group voice channels, screen share at source quality (1080p30 software today · 1440p60 with hardware encode), camera video
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
| Audio codec | **Opus** | 64 kbps speech ↔ 24 kbps + in-band FEC under loss · adaptive via receiver gap-rate feedback |
| Video codec | **H.264** | openh264 (software, universal fallback) behind an encoder trait — hardware backends (VAAPI/MediaCodec/VideoToolbox) slot in per-platform · fragmented datagrams + drop-until-IDR loss recovery |
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
- [x] **Mouth-to-ear ≤ 150 ms (80 ms LAN)** — measured on dev machine (loopback),
      re-measured after the render period settled at 1024 frames (21.3 ms):
      digital callback→render median 49.8 ms / max 50.6 ms over 105 samples,
      spread 1.4 ms (zero rebuffers). Mouth-to-ear ≈ 70–100 ms adding the
      acoustic paths. **150 ms budget: met with margin. 80 ms LAN: met on the
      digital path; acoustic estimate straddles it — effectively both green.**

### Phase 4 — Voice Hardening
- [x] AEC / noise suppression — WebRTC APM (AEC3 + NS + high-pass) on the mic path;
      played mix is the far-end reference. Verified by synthetic-echo test (>10 dB
      reduction) + by-ear A/B (noise floor drop with AEC on). `--no-aec` disables.
- [x] Per-user volume — client-side gains in the mix (`--volume <user_id>:<gain>`)
- [x] FEC — Opus in-band FEC: encoder embeds redundancy (SILK mode only, ≤~32 kbps);
      receiver reconstructs a single lost frame from its successor packet, falling back
      to PLC on double loss or when no FEC is embedded
- [x] Loss-driven bitrate ladder + congestion response — Clean (64 kbps, FEC inert) ↔
      Robust (24 kbps + aggressive FEC); receiver gap-rate reports (`ReceiverReport`)
      routed back over the control stream; EMA + hysteresis; netem 10% loss / 40 ms
      verified in both directions

### Phase 5 — Screen Share
- [x] Wire format: `FLAG_VIDEO` + fragment sub-header; drop-until-IDR assembler
      (loss/reorder hardened; unit + SFU integration tests)
- [x] H.264 codec layer — openh264 (vendored source build, `video` feature-gated,
      CI-clean); encode/decode roundtrip + forced-IDR + Annex-B tests
- [x] Synthetic 720p30 sender (`--share`) + mpv viewer — 30 fps sustained end-to-end
- [x] Real capture: PipeWire portal (ashpd) → BGRA frames → encoder — 1080p BGRx,
      ~30 fps sustained software; viewer sizes itself from the H.264 SPS
- [x] Keyframe-on-join — forced IDR the moment a viewer joins (atomic flag; late
      joiner waits ~RTT, not the 2 s intra period)
- [x] Encoder abstraction — `trait VideoEncoder` + software fallback + `create_encoder()`
      selection; hardware backends slot in per-platform with runtime fallback
- [x] Rate discipline — sleep-pacing + skip-to-newest in the capture encoder;
      drift-free 30 fps (the 3-second-drift lesson lives in the handoff ledger)
- [x] Runtime bitrate mechanism — `set_bitrate` via encoder recreation (next frame
      is a standalone IDR with in-band SPS/PPS; tested)
- [x] Glass-to-glass measured — **~250 ms avg / 400 ms tail** (1080p30 software, mpv
      test viewer). Budget straddled: the remaining latency is viewer-side queuing —
      the real client owns this budget from here
- [ ] VAAPI hardware encoder (parked: polaris VCE kernel bug — handoff §8)
- [ ] 1440p60 / 1080p60 (parked: software ceiling ~32 fps @ 1080p; needs hardware)
- [ ] Loss-driven video bitrate ladder → Phase 6

### Phase 6 — Backend Hardening & Client-Readiness (current)
Goal: every open/parked backend item closed — a stable, complete backend the
Flutter client can attach to. Full plan + acceptance criteria: HANDOFF §12.
- [ ] 6a Share lifecycle — stop/start without process exit; viewer cleanup
- [ ] 6b Multi-channel correctness + per-channel report routing
- [ ] 6c Session-token relogin + reconnect hardening
- [ ] 6d Music channels — 256 kbps stereo end-to-end
- [ ] 6e Loss-driven video bitrate ladder (mirror 4d)
- [ ] 6f Device-vanishing mitigations
- [ ] 6g Performance budgets verified (RSS / 10 talkers / login / binary)
- [ ] 6h Robustness — restart persistence, malformed input, netem matrix

### Phase 7+ — Client & Beyond
- [ ] Flutter client bring-up (app/, flutter_rust_bridge over parley-core)
- [ ] Camera video (deferred from old Phase 6 — needs client UI for preview;
      same pipeline, A/V sync ≤ 40 ms)
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

# screen share (needs mpv; --release is REQUIRED for real-time fps)
cargo run --release -p parley-client --features audio-device,screen-capture -- \
    --server 127.0.0.1:4443 --user <user> --password <pass> --share   # sender: portal picker pops up
cargo run --release -p parley-client --features video -- \
    --server 127.0.0.1:4443 --user <user2> --password <pass2>         # viewer (mpv opens; video-only build)
# synthetic test pattern (no portal): build the sender with --features audio-device,video
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
| Voice mouth-to-ear | ≤ 150 ms / 80 ms LAN — **current: digital 49.8 ms median / 50.6 max (105 samples, 21.3 ms render period); mouth-to-ear ≈ 70–100 ms — budgets met** |
| Screen share glass-to-glass | ≤ 250 ms — **current: ~250 ms avg / 400 ms tail (1080p30 software, mpv test viewer); remaining latency is viewer-side queuing — the real client owns this budget** |
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
  parley-proto/         # wire types, framing, media + video fragmentation, generated protobuf code
  parley-core/          # shared session I/O + audio pipeline + encoder trait & H.264 codec (server + clients)
  parley-server/        # binary: QUIC listener, TLS, config, routing (+ parley-adduser bin)
  parley-client/        # test client: TOFU pinning, 48k audio, AEC/NS, mix, portal screen capture + share/view
app/                    # Flutter client (future)
docs/adr/               # one file per architectural decision
```
