# Two-machine demo — the setup runbook

The goal: a working screen share between the desktop (**A**) and the laptop
(**B**), first through the CLI (which prints the diagnostics), then through the
Flutter app (the path the demo actually shows).

Deeper encoder verification — driver probes, keyframe shape, per-profile
throughput, the DMA-BUF path — lives in
[hd-encode-runs.md](hd-encode-runs.md). This file only gets a demo running.

Fill these in once, from **A**:

| value | where it comes from |
|:--|:--|
| `<A-ip>` | `ip -4 addr show scope global \| grep inet` — the LAN address, not loopback |
| `<fingerprint>` | the 64 hex in A's `parley listening … fingerprint=…` line |
| `<commit>` | `git rev-parse --short HEAD` on A |
| `<sender>` / `<sender-pass>` | account created in step A1 |
| `<viewer>` / `<viewer-pass>` | account created in step A1 |

Use the same address on both machines (`<A-ip>`, never `127.0.0.1` on A) — each
address is its own TOFU pin, so mixing them leaves two entries in
`known_hosts`. The port may be omitted or spelled out (`host`, `host:4443`);
both are the same pin.

---

## A1 — create the accounts (on A, server stopped)

The server's SQLite is `<repo>/data/parley.db`, relative to the working
directory, so run from the repo root.

```sh
cd ~/Documents/Parley
cargo run -p parley-server --bin parley-adduser -- data sender s3cret
cargo run -p parley-server --bin parley-adduser -- data viewer v3cret
```

**Pass** — two lines:

```
user "sender" created (data/parley.db)
user "viewer" created (data/parley.db)
```

If the user already exists, keep its password or choose a new name; a BUSY
error means the server is running — stop it and retry.

## A2 — start the server (on A, keep this terminal open)

```sh
cd ~/Documents/Parley
cargo run -p parley-server
```

**Pass** — one line naming the bind address and the identity:

```
INFO parley listening addr=0.0.0.0:4443 fingerprint=<fingerprint>
```

A fingerprint that differs from an earlier run means A regenerated its
identity (the cert/key in `data/` was replaced). B will pin whatever it sees
first, so settle this before B connects.

## A3 — check the path (on A)

```sh
ip -4 addr show scope global | grep inet    # <A-ip> is the 192.168.x.y one
ss -lun | grep 4443                          # one UDP listener
sudo ufw status                              # only if ufw exists; allow 4443/udp when active
```

**Pass**: a UDP listener on 4443 and an address B can route to.

---

## B1 — packages (on B)

Arch/CachyOS names, matching what A has:

```sh
sudo pacman -S --needed base-devel clang cmake meson ninja nasm pkgconf gtk3 libva \
    libva-utils pipewire alsa-lib rustup ffmpeg mpv git
rustup default stable
```

| package | why |
|:--|:--|
| `clang` | `libclang.so` for the `vaapi` feature's bindgen; the build fails without it |
| `gtk3` | the Flutter Linux target links `PkgConfig::GTK` |
| `cmake`, `meson`, `ninja` | openh264 and the WebRTC AEC build from source |
| `nasm` | openh264's assembly |
| `libva` | the VAAPI backend |
| `libva-utils` | `vainfo` — without it the probe cannot see the driver's encode entrypoints and reports "software codecs only" even on a working GPU |
| `pipewire` | screen capture (and the portal's stream) |
| `alsa-lib` | audio device I/O |
| `mpv` | the CLI viewer's window |
| `ffmpeg` | the probe and stream inspection |
| `rustup` | the repo pins `stable` |

### B1a — the ScreenCast portal (this is what `share` talks to)

`share` does not capture the screen itself: it asks
`org.freedesktop.portal.ScreenCast` for a stream. The portal **frontend** ships
no capture backend, so the backend must match the desktop, and a missing or
mismatched one fails before anything is captured.

```sh
echo "$XDG_CURRENT_DESKTOP  $XDG_SESSION_TYPE"
pacman -Q | grep -iE 'xdg-desktop-portal|^(pipewire|wireplumber) '
```

Then install the frontend plus the backend for that desktop:

| desktop | backend package |
|:--|:--|
| Hyprland | `xdg-desktop-portal-hyprland` |
| sway / other wlroots | `xdg-desktop-portal-wlr` |
| KDE Plasma | `xdg-desktop-portal-kde` |
| GNOME | `xdg-desktop-portal-gnome` |

```sh
# the second package is the one from the table for your desktop
sudo pacman -S --needed xdg-desktop-portal xdg-desktop-portal-hyprland pipewire wireplumber
systemctl --user restart xdg-desktop-portal xdg-desktop-portal-hyprland
```

`xdg-desktop-portal-gtk` is **not** an answer here: it implements neither
ScreenCast nor RemoteDesktop, so a GTK-only portal answers
`No such interface "org.freedesktop.portal.ScreenCast"`.

**Pass** — no dialog yet, just proof the interface is routed:

```sh
gdbus call --session --dest org.freedesktop.portal.Desktop \
    --object-path /org/freedesktop/portal/desktop \
    --method org.freedesktop.DBus.Properties.Get \
    org.freedesktop.portal.ScreenCast version
```

`(<uint32 6>,)` (or any `(<uint32 N>,)`, N ≥ 4) means the portal is ready.
An `InvalidArgs`/`UnknownInterface` error naming ScreenCast means no installed
backend matches `$XDG_CURRENT_DESKTOP` — fix that before Round 1, because
`share` will fail with exactly that text.

If the backend package *is* installed and running but the probe still fails,
the frontend process itself is the suspect: it routes by the
`XDG_CURRENT_DESKTOP` in **its own** environment, and a frontend started before
your session imported that variable has none.

```sh
pid=$(systemctl --user show xdg-desktop-portal -p MainPID --value)
tr '\0' '\n' < /proc/$pid/environ | grep XDG_CURRENT_DESKTOP || echo "frontend has no desktop var"
systemctl --user restart xdg-desktop-portal     # re-reads the env, re-routes
```

Empty output from the `grep` is the whole bug: restart, then re-probe. That is
exactly what the laptop hit — `xdg-desktop-portal-hyprland` installed and
healthy, but the frontend had started without the variable and answered
`No such interface "org.freedesktop.portal.ScreenCast"` until restarted.

---

## B2 — Flutter SDK (on B; needed for step B5)

The app's `pubspec.yaml` requires Dart `^3.13.5`, i.e. Flutter 3.47 or newer.

```sh
git clone https://github.com/flutter/flutter.git -b stable ~/dev/flutter
echo 'export PATH="$HOME/dev/flutter/bin:$PATH"' >> ~/.bashrc
exec bash
flutter --version
flutter doctor -v
```

**Pass**: `flutter --version` shows Dart 3.13.5 or newer, `flutter doctor`
reports the Linux toolchain satisfied, and `pkg-config --modversion gtk+-3.0`
prints a version.

## B3 — get the code (on B)

```sh
git clone git@github.com:DartLumix/Parley.git ~/Documents/Parley
cd ~/Documents/Parley
git rev-parse --short HEAD    # must equal <commit>
git status --porcelain        # must print nothing
```

Nothing needs to move outside git: `vendor/moq-vaapi` is committed,
`protoc` comes from `protoc-bin-vendored`, and the Flutter bridge glue is
committed, so B runs no codegen.

If B already has a clone from before the history rewrite, a pull will not
fast-forward:

```sh
cd ~/Documents/Parley
git fetch origin --force
git reset --hard origin/main
```

Do **not** copy `data/` from A (that is the server's identity, and B is a
client) or `~/.config/parley/known_hosts` (each client's own pins).

## B4 — build the CLI and gate the encoder (on B)

```sh
cd ~/Documents/Parley
cargo build --release -p parley-client --features audio-device,aec,video,screen-capture,vaapi
tools/hw-probe.sh
tools/hw-probe.sh --encode h264
```

**Pass**: the build exits 0 and the probe ends with
`RESULT: h264 hardware encode works here`, after
`per-IDR: 2/2 IRAP pictures decode standalone`.

A refusal that names the kernel window is the gate doing its job: run B's
shares without `--hw-encode` (software) and say so in the report. The client
has no such guard of its own — `--hw-encode` submits a VAAPI H.264 job on any
kernel — so inside the window the flag must simply be omitted.

If the verdict says `nothing hardware-accelerated found` while `ffmpeg` above
it lists `h264_vaapi: yes`, `vainfo` was missing: install `libva-utils` and
re-run. The probe reads encode entrypoints from `vainfo`, not from ffmpeg, so
the verdict is a false negative until it is installed.

---

## Round 1 — B shares, A watches

**On A**:

```sh
cd ~/Documents/Parley
cargo run --release -p parley-client --features video -- \
    --server <A-ip>:4443 --user <viewer> --password <viewer-pass>
```

**On B**:

```sh
cd ~/Documents/Parley
cargo run --release -p parley-client --features audio-device,aec,video,screen-capture,vaapi -- \
    --server <A-ip>:4443 --user <sender> --password <sender-pass>

# at the prompt:  share
```

Add `--hw-encode` **only** if B4 printed
`RESULT: h264 hardware encode works here`. On a kernel inside the VCE window
(the polaris box's 7.2.7) that line cannot print, and the flag would submit
the very job the window breaks — so B shares in software here.

**Pass on B** — both lines agreeing on the backend. `hardware=true` only when
the flag was given:

```
video: hardware (VAAPI) encoder selected — <W>x<H>, self-test <R> fps, pacing up to <P> fps at <bps> bps, keyframe self-test passed ...
share: encoder ready (<W>x<H>, source <f> fps → paced <P> fps, clean rung <bps> bps); hardware=true
```

Software is the expected result on B and prints `hardware=false`; the run is
still valid, it just does not exercise VAAPI. Its selected line is:

```
video: software (openh264) encoder selected — <W>x<H> @ <fps> fps, <bps> bps
```

If the self-test FAILED line appears instead, the encoder fell back to
software — the run is not a hardware run — and that line says why.

**Pass on A** — an mpv window showing B's screen, `video: user N sharing WxH @
fps`, and every 10 s a stats line where `decoded` climbs while the rest stay 0:

```
video: user 2: 120 shown (120 decoded, 0 no-picture, 0 decode errors), 0 dropped, 0 skipped, 0 shed
```

The prompt accepts `share` to start a share and `stop` to end one without
leaving the session.

**Choosing a rate.** `pacing up to P` is the profile ceiling (60 fps
hardware, 30 fps software) lowered by the capture's own rate and the share
dialog's cap — not a promise. What the encoder holds is what the 10 s
`share: encoded N frames (R fps)` line and the sharer's own tile report
(the tile replaces the negotiated rate with the measured one after the
first second). On this project's RX 590 the VAAPI path measured ~14.8 ms
per 720p frame and ~30 ms per 1080p frame, so 720p is the size that
reaches 60 and 1080p tops out around 30. The software encoder does 1080p
in ~19 ms, which is why 1080p30 in software is smoother than 1080p in
hardware on that GPU — hardware is worth it for the CPU it frees, not for
the rate it adds there.

---

## Round 2 — A shares, B watches

Swap the roles, so both encoders are exercised:

**On B**:

```sh
cargo run --release -p parley-client --features video -- \
    --server <A-ip>:4443 --user <sender> --password <sender-pass>
```

**On A**:

```sh
cargo run --release -p parley-client --features audio-device,aec,video,screen-capture,vaapi -- \
    --server <A-ip>:4443 --user <viewer> --password <viewer-pass> --hw-encode
# at the prompt:  share
```

Same acceptance, mirrored.

---

## B5 — the app, on both machines

After that machine's CLI round passes:

```sh
cd ~/Documents/Parley
cargo build --release -p parley_flutter --features audio-device,aec,video,screen-capture,hw-encode
cd app
flutter pub get
flutter run --release -d linux
```

**Both halves must say release.** The runner's CMake picks which Rust cdylib
to install from `CMAKE_BUILD_TYPE`: `Debug` installs
`target/debug/libparley_flutter.so`, anything else installs
`target/release/libparley_flutter.so`. Flutter passes that flag itself
(`build_linux.dart` → `-DCMAKE_BUILD_TYPE=Release` for `--release`), so
`flutter run -d linux` with a release cargo build — or the reverse — bundles a
mismatched library and either fails to start or silently runs the old one.

Do not use a debug Rust build for the demo. Unoptimized openh264 at 1080p is
slow enough that the PipeWire `process()` callback holds the producer's buffer
too long; the source then runs out of buffers and the stream cycles
`Streaming -> Paused` → renegotiate → `Paused -> Streaming`, the encoder is
recreated each cycle, throughput collapses to a few fps, and the loss ladder
drops to the robust rung. `voice: encoder behind — N frames dropped` in the
same log is the same CPU starvation. A release build of the identical code
holds 28–30 fps with no pause cycling.

In the app: connect to `<A-ip>:4443`, log in, join a voice channel, then use the
screen-share control. The **hardware-encoding switch (Settings → Video) only
exists** in a build with the `hw-encode` feature, and it applies at the next
encoder creation — stop and re-share to switch it. Share from B to A and from
A to B.

**Pass**: the receiving window shows the shared screen as a texture. A black
window is diagnosed from the sender's `encoder ready` line plus the receiver's
stats (frames arrived but did not decode vs frames never arrived).

---

## When it fails

| symptom | where | meaning |
|:--|:--|:--|
| `fingerprint mismatch for <addr>: the certificate changed` | client | a stale pin for that address; the app offers to trust the new certificate, or delete that line from `~/.config/parley/known_hosts` and reconnect |
| `hardware (VAAPI) encoder selected` never appears | sender | fell back to software; the preceding line names the reason |
| `no picture in the last 90 frames` | viewer | frames arrive, nothing decodes — stream shape, not the network |
| `Native:4 dsFrameCorrupted` / `Native:16 dsNoParamSets` | viewer | keyframes do not stand alone: encoder side |
| `share: portal: A portal frontend implementing org.freedesktop.portal.ScreenCast was not found` | sender | no ScreenCast-capable backend, or none matching the desktop: see B1a (gtk alone does not count). If the backend is installed and running, the frontend was started without `XDG_CURRENT_DESKTOP` — restart it (B1a) |
| `share: send_datagram failed: TooLarge` | sender | path MTU collapsed; one frame is dropped by design and the share continues |
| session connects, no mpv window | viewer | `mpv` is not on PATH (step B1) |
| connect times out from B | B | not the server's `0.0.0.0:4443`, wrong `<A-ip>`, or a firewall on A |

## What to report per machine

- the `RESULT:` line from `tools/hw-probe.sh --encode h264`;
- the `hd …` fps lines if the encode tests in
  [hd-encode-runs.md](hd-encode-runs.md) were run;
- the sender's `hardware (VAAPI) encoder selected` line (the measured rate);
- the newest viewer stats line.

That separates a driver limit from a wrapper defect from a transport one.
