# Development

## Layout

```
proto/                  .proto files — protocol source of truth
crates/
  parley-proto/         wire types, framing, media + video fragmentation
  parley-core/          shared session I/O, audio pipeline, encoder trait, H.264
  parley-server/        QUIC listener, TLS, config, routing (+ parley-adduser)
  parley-client/        terminal client + reusable capture/audio modules
app/                    Flutter desktop client (Rust bridge in app/rust)
deploy/                 the released compose file and env template
packaging/              install.sh, icon, quickstart shipped in the tarball
tools/                  hw-probe.sh, e2e-container.sh, package-*.sh
docs/                   see docs/README.md
vendor/moq-vaapi/       patched VA-API crate (see vendor/README.md)
```

## Dependencies

Arch / CachyOS:

```sh
sudo pacman -S --needed base-devel clang cmake meson ninja nasm pkgconf \
    gtk3 libva libva-utils pipewire alsa-lib rustup ffmpeg mpv git
```

Ubuntu 22.04+:

```sh
sudo apt install clang cmake ninja-build meson nasm pkg-config libclang-dev \
    libgtk-3-dev liblzma-dev libpipewire-0.3-dev libasound2-dev libva-dev
```

The WebRTC AEC is built from source (`bundled`) — hence meson, ninja,
a C++ toolchain and libclang — so the binaries carry no system WebRTC
dependency. One caveat: the crate prefers a **system abseil >= 20240722**
when pkg-config finds one (Arch ships it; Ubuntu does not) and otherwise
fetches abseil via a meson wrap during the build. A local packaging run
on such a machine produces a binary that links system absl; the release
CI has no system abseil, which is why released tarballs are
self-contained. `tools/package-client.sh` warns when it sees that.

The crate configures with `meson setup --reconfigure`, which Ubuntu
22.04's meson 0.61 rejects on a fresh build dir ("Directory does not
contain a valid build tree"). Install meson from pip (>= 1.0) on 22.04;
the release workflow does exactly that.

Opus follows the same rule: on glibc Linux `audiopus_sys` links the
system `libopus.so` unless `LIBOPUS_STATIC=1` is set. The release job
sets that plus `LIBOPUS_NO_PKG=1` so the bundled Opus is built (cmake
< 4 — its CMakeLists predates policy 3.5) and linked statically, which
24.04's non-PIC `libopus.a` cannot be.

Building the shipping features also needs PipeWire >= 0.3.65 headers:
pipewire-rs uses `spa_video_info_raw.flags`, which 22.04's 0.3.48 does
not have. 24.04 (1.0.5) is therefore the build and runtime baseline for
the release client; 22.04 builds everything except `screen-capture`.

## Everyday commands

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# server (dev): listens on :4443, data/ holds cert+key+db
PARLEY_ADMIN_PASSWORD=secret cargo run -p parley-server

# terminal client
cargo run -p parley-client --features audio-device,video,screen-capture \
    -- --server 127.0.0.1:4443 --user alice --password 'secret'
```

Feature flags (all default-off so CI and the musl server build never
link device libraries):

| feature | what it adds |
|:--|:--|
| `audio-device` | cpal I/O + WebRTC AEC/NS |
| `video` | openh264 encode/decode + the video receive hub |
| `screen-capture` | portal capture (ashpd + PipeWire) |
| `vaapi` | hardware encode via the vendored crate (`--hw-encode`) |

## Flutter client

```sh
cargo build --release -p parley_flutter \
    --features audio-device,video,screen-capture,hw-encode
cd app
flutter pub get
flutter run --release -d linux        # or: flutter build linux --release
```

The Rust half must match the Flutter mode: the runner installs the
cargo `debug` cdylib for a debug Flutter build and `release` otherwise.
Mixing them bundles the wrong library — the failure mode is a stale or
broken app. A debug Rust build is also too slow for real-time 1080p
encoding (the pause/renegotiate loop documented in
[two-machine-demo.md](two-machine-demo.md)).

Codegen: after editing `app/rust/src/api/`, run
`flutter_rust_bridge_codegen generate` and commit the generated Dart.
Keep bridge signatures to plain types — opaque Rust types in a
signature break the generated code.

Checks: `flutter analyze`, `flutter test`, plus a launch (`flutter run`)
for anything touching the GTK window, the texture bridge or the audio
devices.

## Native / hardware

- `tools/hw-probe.sh` — driver and encoder probe, keyframe shape,
  per-profile rates; `--encode h264` prints a RESULT line.
- Measurements and transcripts: [hd-encode-runs.md](hd-encode-runs.md).
- VAAPI is opt-in everywhere; machines inside the known driver window
  must not pass `--hw-encode` (see the runbook).

## Container

```sh
docker build -t parley:local .
cargo build --release -p parley-client
tools/e2e-container.sh          # builds, boots, adduser, two logins, negative
```

The root `docker-compose.yml` builds and runs from a checkout;
`deploy/compose.yaml` runs the released image.

## Tests worth knowing

| test | what it protects |
|:--|:--|
| `parley-core` video tests | wire format, fragment assembly, loss recovery |
| `parley-core` `video_receive` tests | display hub: one stale frame after a stop (the ghost card) |
| `parley-server/tests/transport.rs` | SFU routing, scoping, presence, generations |
| `parley-client/tests/aec.rs` | synthetic echo cancellation, no hardware needed |
| `parley-core/tests/vaapi_*.rs` | hardware path (skipped without a capable GPU) |
| `app/test/` | window toolbar behaviour, login form |
