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

# 1b. conversion vs driver cost per profile (tells a slow GPU from a slow
#     conversion; useful on B, whose CPU is not known either)
PARLEY_VAAPI=1 cargo test --release -p parley-core --features vaapi \
    --test vaapi_encode_cost -- --nocapture --test-threads=1

# 1c. the vendored crate's own per-frame cost (bitrate/rate variations, to
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
hd 1280x720:  120 frames in 1.24 s → 96.5 fps
hd 1920x1080: 120 frames in 2.51 s → 47.8 fps
hd 2560x1440: 120 frames in 4.07 s → 29.5 fps
each profile: keyframe t=0 and t=60 stand alone; every keyframe carries SPS+PPS
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
