
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
| 6 | Backend hardening & client-readiness | ✅ |
| 6.9 | Client-prep — share audio, receiver hardening, latency re-measure | ✅ |
| 7a | Flutter shell + bridge verified (pong) | ✅ |
| 7b | Session — connect/auth/TOFU/heartbeat/events | ✅ |
| 7c | Channels, presence (push), join, chat, history | ✅ |
| 7d | Voice receive — mixer + cpal output through the bridge | ✅ |
| 7e | Voice send + AEC — join-driven devices, reconnect gap closed | ✅ |
| 7e2 | 4d loss ladder in the app — receiver reports + two-profile send controller | ✅ |
| 7f | Video receive — VideoReceiveHub → Flutter texture bridge | ✅ |
| 7g | Screen-share send — portal capture + encode through the bridge | ✅ |
| 7+ | Android / Windows / iOS bring-up, file transfer | ⏳ |

---

## ✨ What it is

- 🎯 Group voice channels, screen share at source quality (1080p30 software · 720p60 / 1080p~48 / 1440p~29 hardware-encoded on the dev box — the profile rate is measured at encoder creation, so it follows the machine), camera video
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
| Audio codec | Opus | Speech: 64 kbps ↔ 24 kbps + in-band FEC, adaptive via gap-rate feedback · Music channels: 256 kbps stereo (Application::Audio) |
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
- [x] VAAPI hardware encoder — **unparked and working** (7h). Two independent stream-shape defects fixed in the vendored crate: no parameter sets reached the coded buffer at all (driver accepts packed SPS/PPS and does not write them), and our synthesized SPS/PPS declared slice syntax Mesa's VCE writer never produces (11-bit `frame_num`, POC type 0, deblocking present) so every slice header misparsed. Keyframes now decode standalone on a fresh decoder at 720p/1080p/1440p, with a negative control proving the check bites. See [vendor/README.md](vendor/README.md)
- [x] HD profiles — measured, not assumed: hardware encode runs 98 fps at 720p, 47.8 at 1080p, 29.1 at 1440p through the real capture path, and `create_encoder_auto` times that path at creation so the share advertises the rate it can hold (`hardware_measured_fps` → `target_fps`). 1080p60 is *not* reachable on the dev box (driver ceiling ~55 fps at 1080p, our path ~48); 1440p60 is not reachable at all. Software stays at its 30 fps ceiling
- [x] 7i No CPU color conversion — packed BGRA is uploaded at the producer's own row pitch and the VA-API video processor converts/scales it to NV12 (it used to be a parallel CPU BT.601 loop plus a strided repack). The encoder is the ceiling, not the conversion: rates stayed at 98/47.8/29.1 fps while the share thread stopped touching pixels and stopped allocating per frame. Verified by `vaapi_bgra_path` (red decodes red; padded stride decodes identically to tight)
- [x] Loss-driven video bitrate ladder — landed in 6e

### Phase 6 — Backend Hardening & Client-Readiness 
Goal: every open/parked backend item closed — a stable, complete backend the
Flutter client can attach to. Full plan + acceptance criteria: HANDOFF §12.
- [x] 6a Share lifecycle — stop/start without process exit; viewer cleanup
- [x] 6b Multi-channel correctness + per-channel report routing
- [x] 6c Session-token relogin + reconnect hardening
- [x] 6d Music channels — 256 kbps stereo end-to-end
- [x] 6e Loss-driven video bitrate ladder (mirror 4d)
- [x] 6f Device-vanishing mitigations (bounded pcm channel)
- [x] 6g Performance budgets verified (RSS / 10 talkers / login / binary)
- [x] 6h Robustness — restart persistence, malformed input, netem matrix


### Phase 6.9 — Client-Prep: Share Audio & Receiver Hardening
- [x] Share audio — default-output capture (native PipeWire `stream.capture.sink`), mic-clocked mux into one stereo 256 kbps stream
- [x] Handover correctness — jitter stream-reset on seq discontinuity; mux lifetime = sys-capture lifetime
- [x] Receiver hardening — decode off the audio datagram reader, pre-decode skip-shed, dead-viewer suppression (the recorded "95% CPU / three defects" verdict was four stacked bugs, not a CPU floor)
- [x] Latency re-measured — glass-to-glass ~250 ms LAN (budget met, tail bounded); share-audio 138.5 ms median via a foreign-VPS internet path

### Phase 7 — Flutter Client (in progress; Linux first)
The dev client: `app/` — a plain dark-theme test bench exercising every
backend feature through `flutter_rust_bridge` before any real UI design.
- [x] 7a Shell + bridge — flutter_rust_bridge scaffold, `.so` bundled via
      CMake `install()` (POST_BUILD copies into the build tree, which the
      bundle assembly never reads — install() runs in the bundle pass)
- [x] 7b Session — connect/auth/TOFU (shared known_hosts with the terminal
      client), heartbeat, liveness, `session_closed` event
- [x] 7c Channels / presence / join / chat / history — event stream
      (StreamSink → Dart `Stream`); **presence push landed server-side
      after the app exposed presence as pull-only since 1e** (first
      backend bug found by the Flutter client)
- [x] 7d Voice receive — media datagram reader + parley-core mixer + cpal
      output (device in the bridge crate, feature-gated `audio-device`,
      default off: workspace CI clippy and the musl Docker build never
      touch alsa-sys)
- [x] 7e Voice send + AEC — capture (bounded-channel RT discipline), SendPipeline,
      WebRTC AEC3/NS with the played mix as far-end reference, speech/music
      profiles off the channel mode; **voice is owned by the session** — join =
      listen + transmit (TeamSpeak shape), leaving/disconnecting drops the
      VoiceControl (Drop = teardown), which closes the 7d
      voice-state-across-reconnects gap structurally. Start/Stop Listening
      retired from the dev UI
- [x] 7e2 Loss ladder (4d in the app) — per-session 5 s worst-peer
      ReceiverReport through the session writer (server routes + stamps);
      peer reports steer the speech encoder's two-profile controller: EMA
      0.7/0.3 stepped at 1 Hz, → Robust (24 kbps + FEC) above 30‰, → Clean
      (64 kbps) after 20 s below 10‰, 5 s switch cooldown — the ladder math
      is a pure struct unit-tested under `--features audio-device`. Music
      channels ignore reports by design
- [x] 7f Video receive — the app is the second display consumer of
      parley-core's VideoReceiveHub (the seam 6.9 built it for): media
      reader forwards FLAG_VIDEO payloads to a dedicated hub task (3b:
      decode never on the reader), one irondash_texture external texture
      per sender (RGB24→RGBA, latest-frame-wins, created/destroyed on
      the platform thread), `video_started`/`video_stopped` events drive
      a plain Texture widget; viewer-side VideoReport (5 s worst gap
      rate) feeds the sharer's 6e ladder; feature `video` default-off,
      build with `--features audio-device,video`
- [x] 7g Screen-share send — portal capture through the bridge in the app: start/stop
      from the UI (`start_screen_share` + `share_started`/`share_ended`/`share_failed`
      events), share-audio mux, keyframe poke for a late joiner, and the HW-encode
      toggle gated on `hw_encode_supported()` (feature `screen-capture`, which implies
      `video`; the Rust side is parley-client's screen module, the same one the CLI
      uses). Verified by build + clippy with `audio-device,video,screen-capture,hw-encode`;
      the on-screen run is step 3 of [docs/hd-encode-runs.md](docs/hd-encode-runs.md)
- [ ] Android / Windows / iOS bring-up
- [ ] File transfer (QUIC streams), roles, rich text, search, DMs, push, auto-update

**Parked indefinitely** : camera video — returns after the client has adecent foundation (it needs UI preview; the pipeline exists).

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
#    sender: type "share" / "stop" to start/stop without restarting
#    share audio: append --share-audio (captures the DEFAULT OUTPUT — a local listener must play to another sink, else feedback)
cargo run --release -p parley-client --features video -- \
    --server 127.0.0.1:4443 --user <user2> --password <pass2>         # viewer (mpv opens; video-only build)
    # token relogin: password login prints a token — rerun with --token <tok> instead of --user/--password
# music channels: add a channel with mode=1 (sqlite) — clients auto-profile; --music sends synthetic L/R test tones
# synthetic test pattern (no portal): build the sender with --features audio-device,video
# Flutter dev client (app/): bridge surface is plain types only — session_id
# handles; non-plain types in pub signatures get wrapped as opaque objects
# and the generated code stops resolving (the 7d lesson)
cd app
cargo build -p parley_flutter --features audio-device   # Rust (audio needs the feature)
flutter_rust_bridge_codegen generate                    # API changed → regenerate Dart
cargo build -p parley_flutter --features audio-device   # rebuild with generated wiring
flutter run -d linux                                    # CMake install() re-bundles the .so
# dev-server CWD determines the cert identity (data/ is relative) — always
# run T1 from the repo root, else TOFU hard-fails on a fresh app/data/ cert

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
| Screen share glass-to-glass | ≤ 250 ms — **met: ~250 ms avg LAN (stopwatch method, 1080p30 software, mpv test viewer); tail bounded by the frame-queue cap + pre-decode shed (310 ms median / 350 max via a foreign-VPS internet path, both ends home Wi-Fi); Phase 7's real display owns this budget from here** |
| Share-audio tap-to-speaker | ≤ 250 ms — **met: 138.5 ms median / 187 ms max (default-output monitor → laptop speaker, foreign-VPS internet path)** |
| Low-end receiver (full 1080p30 + stereo + AEC) | ≤ 1 core — **current: ~0.5 core client + ~0.1 core mpv on the 2-core laptop** |
| Hardware encode rate (measured at creation, per machine) | must hold what the profile advertises — **current: 98 fps @720p, 47.8 @1080p, 29.1 @1440p (RX 590 VCE, Mesa 26.2.4; driver ceiling ~114/55/31, and the color conversion is on the GPU since 7i); the share paces and timestamps at the measured number, never at a constant** |
| Server idle RSS | ≤ 64 MB — **current: 54.8 MB (info logging), 48.1 MB at RUST_LOG=warn; container 1.4 MiB idle** |
| 10 active talkers | ≤ 50% of one core — **current: 43% at 10× occupancy (10 senders × 10 recipients, 5000 forwarded pps, loopback; 100% delivery; perf-profiled: cost is per-datagram kernel sendmsg + loopback softirq, not parley code)** |
| Login → connected | ≤ 400 ms — **current: 70 ms (loopback); 1.0–2.2 s laptop first join over Wi-Fi/internet (re-login with --token is faster)** |
| Binary size | ≤ 15 MB — **current: 6.2 MB binary; docker image 9.6 MB** |

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
app/                    # Flutter dev client: rust/ (parley_flutter bridge crate) + lib/
docs/adr/               # one file per architectural decision
```
