# HD encode — the runs to do, per machine

What this verifies, in one line: **hardware H.264 encode produces streams a
receiver can decode, at the highest profile the machine can actually hold.**
Everything below is copy-pasteable. Run steps 0–2 on **each** machine; step 3
is the two-machine live test.

Names used below:

- **Machine A** — the dev desktop: RX 590 (polaris10), Mesa 26.2.4,
  kernel `7.3.0-rc4-1-cachyos-rc`. Already verified here (see
  [vendor/README.md](../vendor/README.md)); re-run only to confirm nothing
  regressed.
- **Machine B** — the second device. Its GPU is unknown to the code, which
  is the point: the encoder reports what it measured, so B's numbers decide
  B's profile.

## Step −1 — get the code onto Machine B (once)

**Over git. Nothing has to travel any other way**: the checkout is the whole
artifact (`vendor/moq-vaapi` is committed, the generated protobuf is
compiled by a build script from a vendored `protoc`, and the generated
Flutter bindings are committed too).

```sh
# on B
git clone git@github.com:DartLumix/Parley.git ~/Documents/Parley
cd ~/Documents/Parley && git log --oneline -1    # the commit A pushed
```

With the repository already on disk, `git pull --ff-only` instead. Do NOT copy
`~/.config/parley/known_hosts` between machines: that is each client's own
TOFU store. If B has an entry for A from an OLDER Parley instance (a
regenerated cert), the connection will hard-fail on a fingerprint mismatch —
remove that line and let it pin afresh.

If you would rather not use git, the same checkout moves as files (measured
~6 MB; `target/` alone is 27 GB and must not travel — B rebuilds it):

```sh
# on A
rsync -aH --info=progress2 \
  --exclude 'target/' --exclude 'app/build/' --exclude 'app/.dart_tool/' \
  --exclude 'app/linux/flutter/ephemeral/' --exclude '.freebuff/' \
  --exclude 'data/' --exclude '*.annexb' \
  ~/Documents/Parley/ <user>@<laptop>:~/Documents/Parley/
```

`vendor/` must come along in that case (the whole `moq-vaapi` patch lives
there). `data/` does not: it is the cert + SQLite of whichever machine runs
the *server*, and B is a client here. Copying `data/` would give B a second
server identity, not the one A prints.

Build prerequisites on B (Arch/CachyOS package names; these are what A has):

```sh
sudo pacman -S --needed base-devel clang cmake meson ninja nasm pkgconf libva \
    pipewire alsa-lib rustup ffmpeg mpv git
```

`clang` is what the VAAPI feature needs at build time: the vendored crate runs
`bindgen` for the libva bindings and fails without `libclang.so` (Arch ships it
in the `clang` package). `cmake`/`meson`/`ninja` build openh264 and the WebRTC
AEC from source, `nasm` enables openh264's assembly, `libva` is the VAAPI
backend, `pipewire` is the capture API, `ffmpeg`+`mpv` are the probe and test
viewer. Rust comes from `rustup` (the repo pins `stable`). The Flutter app
additionally needs a Flutter SDK — on A it lives at `~/dev/flutter`, not in a
package.

## What "working" means (the three checks each machine must pass)

1. **The driver encodes H.264 and its keyframes stand alone** — otherwise
   the stream is unusable for a mid-stream (late-joining) receiver.
2. **Our wrapper's keyframes stand alone at every profile** (with a negative
   control that proves the check can fail).
3. **The advertised rate is one the encoder holds** — the profile rate is
   measured at encoder creation, so this is a check on the measurement
   path, not on a datasheet.

Expect at most ~55 fps at 1080p on VCE, ~30 at 1440p; **1080p60 and 1440p60
are not reachable on Machine A.** If B is Intel iHD or a newer AMD (VCN),
its measured numbers may be higher — that is exactly what the run reports.

---

## Step 0 — driver probe (safe, no encode job)

```sh
cd ~/Documents/Parley
tools/hw-probe.sh                       # capability report only
tools/hw-probe.sh --encode h264         # 2 s 1080p job + per-IDR validation
```

The H.264 gate now fires **only** inside the kernel window where an H.264
VAAPI job hangs VCE (7.2.2 .. 7.3-rc1). On a clear kernel it just runs; if it
refuses, the kernel is affected and the encode test must wait (or use
`--encode hevc`).

Look for: `H.264 encode: yes`, `whole-stream decode: OK`,
`per-IDR: 2/2 IRAP pictures decode standalone`, `RESULT: h264 hardware
encode works here`.

Overrides if needed: `PROBE_SIZE=2560x1440 PROBE_SECONDS=3 tools/hw-probe.sh
--encode h264`.

## Step 1 — our path: keyframes at every profile, and where the time goes

```sh
cd ~/Documents/Parley

# 1a. keyframe shape, HD sweep, throughput, negative control
PARLEY_VAAPI=1 cargo test --release -p parley-core --features vaapi \
    --test vaapi_keyframe_shape -- --nocapture --test-threads=1

# 1b. the BGRA path's correctness — color and row pitch (7i: the conversion
#     is on the GPU now; this is the test that catches a stopped row walk or
#     a swapped channel order, which the keyframe checks cannot see)
PARLEY_VAAPI=1 cargo test --release -p parley-core --features vaapi \
    --test vaapi_bgra_path -- --nocapture --test-threads=1

# 1c. conversion vs driver cost per profile (tells a slow GPU from a slow
#     conversion; useful on B, whose CPU is not known either)
PARLEY_VAAPI=1 cargo test --release -p parley-core --features vaapi \
    --test vaapi_encode_cost -- --nocapture --test-threads=1

# 1d. the vendored crate's own per-frame cost (bitrate/rate variations, to
#     separate coded-buffer cost from encode cost)
PARLEY_VAAPI=1 cargo test --release -p parley-core --features vaapi \
    --test vaapi_crate_cost -- --nocapture --test-threads=1
```

Optional: `PARLEY_VAAPI_SWEEP=1920x1080,2560x1440` narrows 1a's sweep, and
`PARLEY_VAAPI_STREAM=/tmp/hw` writes the streams for an outside decoder:

```sh
for f in /tmp/hw.*.annexb; do
  echo "== $f"; ffprobe -v error -select_streams v -show_entries \
    stream=codec_name,profile,width,height,level -of default=nw=1 "$f"
  ffmpeg -v error -f h264 -i "$f" -f null - && echo "  decodes clean"
done
```

Machine A's expected output (2026-10-09):

```
hd 1280x720:  120 frames in 1.22 s → 98.0 fps
hd 1920x1080: 120 frames in 2.51 s → 47.8 fps
hd 2560x1440: 120 frames in 4.12 s → 29.1 fps
each profile: keyframe t=0 and t=60 stand alone; every keyframe carries SPS+PPS
red frame decodes to R=233.0 G=0.0 B=2.0
tight vs padded stride: mean |Δ| = 0.000 per channel byte
negative control: stripped keyframe correctly rejected (Native:16)
```

On Machine B, only the numbers should differ. **No picture at any profile**
is the failure this whole exercise exists to catch — report it with the
`ffprobe` line.

## Step 2 — live share between the two machines (CLI)

On **Machine A** (server, if it is not already running):

```sh
cargo run -p parley-server                 # writes data/, prints the fingerprint
```

On **Machine A** (viewer, terminal 2):

```sh
cargo run --release -p parley-client --features video -- \
    --server <A-ip>:4443 --user <viewer> --password <pass>
```

On **Machine B** (sender — the machine whose encoder is under test):

```sh
cargo run --release -p parley-client --features audio-device,video,screen-capture,vaapi -- \
    --server <A-ip>:4443 --user <sender> --password <pass> --hw-encode
# then, at the prompt:  share          (and  stop  to end it without exiting)
```

What to look for, in the sender's log:

```
video: hardware (VAAPI) encoder selected — <W>x<H>, measured <R> fps sustained
        (pacing at <P>), <bps> bps, keyframe self-test passed ...
share: encoder ready (<W>x<H>, source <f> fps → paced <P> fps, clean rung <bps> bps); hardware=true
```

`<P>` is the profile: 60 at 720p, ~48 at 1080p, ~29 at 1440p on Machine A.
**If the self-test FAILED line appears instead, the encoder fell back to
software — the run is not a hardware run**, and the line says why.

On the viewer (Machine A), mpv opens and shows the picture; the viewer prints
`video: user <id> sharing <W>x<H> @ <fps> fps` and, every 5 s, a stats line:

```
video: user 2: 120 shown (120 decoded, 0 no-picture, 0 decode errors),
       0 dropped, 0 skipped, 0 shed
```

`decoded` climbing with `no-picture`/`decode errors` at zero is the pass
condition. A stream that arrives but never becomes a picture prints a
warning naming exactly that (`no picture in the last 90 frames`).

Then re-verify the reverse direction (A sends to B) so both encoders are
exercised: swap the roles and repeat.

### Step 2b — zero-copy capture, if the compositor offers DMA-BUFs (7j)

> Verified once end to end on Machine A (Hyprland, xdg-desktop-portal-hyprland,
> self-view on the same box): negotiation, import, encode, transport and
> decode all ran, with the sender at ~28 fps of a 30 fps source and the viewer
> at `1020 decoded, 0 no-picture, 0 decode errors, 0 dropped, 0 skipped`. The
> transcript is at the end of this section — re-run it after a compositor
> upgrade, since the offer has to match the producer's format list exactly.

Same run with two extra flags on the sender (build it with `vaapi` too):

```sh
cargo run --release -p parley-client --features audio-device,video,screen-capture,vaapi -- \
    --server <A-ip>:4443 --user <sender> --password <pass> --hw-encode --dmabuf
```

What changes, and how to tell it apart from the memfd path:

- The sender's log says `negotiated <W>x<H> BGRx @ <fps> fps (dma-buf, modifier <M>)`
  instead of `(source-paced)`. `<M>` is the tiling the compositor chose — `0x0`
  is linear. An import failure is loud (`vaapi encode (dmabuf): ...`) and the
  frame is dropped, not silently copied.
- If the portal or the compositor refuses the DMA-BUF offer, or this build has
  no `vaapi` feature, the share fails at start with the reason rather than
  falling back — a CPU copy measured as zero-copy is worse than an error.
- Everything downstream is unchanged: the same `encoder ready` line, the same
  viewer stats. What to check is that `decoded` still climbs and the encode
  rate is within a frame of the memfd run — the point is where the work runs,
  not how fast it is (the driver is the ceiling either way).

What a passing run looks like (Hyprland / xdg-desktop-portal-hyprland,
1920x1080 monitor, hardware encode on an RX 590):

```
sender:  share: pw stream Connecting -> Paused
sender:  share: negotiated 1920x1080 BGRA @ 30 fps (dma-buf, modifier 0xffffffffffffff)
sender:  video: hardware (VAAPI) encoder selected — 1920x1080, measured 50.6 fps sustained
             (pacing at 51), 9486187 bps, keyframe self-test passed ...
sender:  share: encoder ready (1920x1080, source 30 fps → paced 30 fps, clean rung 5625000 bps); hardware=true
sender:  share: pw stream Paused -> Streaming
sender:  share: encoded 280 frames (28.0 fps), 0 stale skipped
viewer:  video: user 14 sharing 1920x1080 @ 30 fps
viewer:  video: user 14: 240 shown (1020 decoded, 0 no-picture, 0 decode errors), 0 dropped, 0 skipped, 0 shed
```

The first version of this path paced by DROPPING an early frame and measured
`24.7 fps` with the viewer dropping 14 — the frames were early, not stale.
Waiting for the slot while still holding the producer's buffer (which is the
backpressure PipeWire expects) recovered them: `28 fps`, nothing dropped.

The failure this step was written against, and why the modifier list is what
it is: offering `DRM_FORMAT_MOD_LINEAR` alone ended the negotiation with

```
share: pw stream Paused -> Error("no more input formats")
```

against a portal whose own format list is `BGRA` under
`DRM_FORMAT_MOD_INVALID` (`0x00ffffffffffffff`, which `pw-dump` prints for the
screencast node) plus `BGRA`/`BGRx` with no modifier at all. The offer now
carries INVALID and LINEAR, and BGRA is the default format, so both the
portal's DMA-BUF entry and a GBM-style linear producer can match.

## Step 3 — the Flutter app (the path users actually run)

Both machines:

```sh
cd ~/Documents/Parley/app
cargo build -p parley_flutter --features audio-device,video,screen-capture,hw-encode
flutter run -d linux          # CMake install() re-bundles the .so
```

In the app: connect + log in, join a voice channel, then use the screen-share
control. The HW-encode toggle is only visible in a build that has the
`hw-encode` feature (that is what `hw_encode_supported()` reports), and it
takes effect at the next encoder creation — stop and re-share to switch.

On the receiving machine the shared screen appears in the app window
(texture); the same three-way accounting applies, printed on the sender's
side as before. Share from B to A and from A to B.

If a share starts and stays black, check the sender's `encoder ready` line
first: `hardware=true` with a measured rate means the encoder is live, and
the receiver's stats line then says whether frames arrived and stopped at
the decoder (`decoded=0`) or never arrived (`skipped`/`dropped` climbing).

## Step 4 — report back

For each machine, paste:

- `tools/hw-probe.sh` header block + the `RESULT` line of `--encode h264`;
- the `hd ...` lines and any failure from step 1a;
- the sender's `hardware (VAAPI) encoder selected` line (measured rate);
- the viewer's most recent stats line.

That is enough to tell a driver limit from a wrapper defect from a transport
one, and it is the same evidence Machine A's fixes were built from.
