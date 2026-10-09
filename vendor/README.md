# vendor/

`moq-vaapi` 0.1.0, copied from crates.io and patched. `Cargo.toml` at the
repository root points `[patch.crates-io]` at this copy, so `cargo build`
with the `vaapi` feature (the CLI's `--hw-encode`, the Flutter app's
`hw-encode`) builds this one. The crate is excluded from the workspace: its
sources are upstream's (tabs, `#![allow(clippy::all)]`) and must not be
swept by `fmt`/`clippy`/`test --workspace`.

## Status: WORKING on Mesa/radeonsi (verified 2026-10-09)

Two independent defects broke hardware share on this project's AMD hardware
(RX 590 / polaris10, Mesa 26.2.4, radeonsi, VCE). Both are fixed and the
fixes are measured, not assumed:

| | symptom | cause | fix |
|:--|:--|:--|:--|
| 1 | no parameter sets anywhere in the stream (`dsNoParamSets`) | the driver accepts packed SPS/PPS and then does not write them into the coded buffer | every IDR is emitted with its SPS/PPS in-band |
| 2 | every slice header misparsed (`Native:4 dsFrameCorrupted`, ffmpeg `deblocking_filter_idc 4 out of range`) | our synthesized SPS/PPS declared fields Mesa's VCE slice writer never writes (11-bit `frame_num`, POC type 0 with a 12-bit LSB, deblocking present) | declare what the driver actually writes: 7-bit `frame_num`, `pic_order_cnt_type = 2`, no deblocking fields |

The two are in `src/encode.rs`; each carries a `PARLEY PATCH` comment with
the evidence. Fix 2 was found by diffing our stream against ffmpeg's own
VAAPI output on the same GPU (`h264dump.py` field-by-field: ffmpeg's
decodable stream uses exactly `log2_max_frame_num_minus4=3`,
`pic_order_cnt_type=2`, `deblocking=0`), then confirming with openh264 and
ffmpeg.

### Measured state after the fixes

Decoder view, openh264 on a FRESH decoder (a viewer joining at a keyframe),
negative control = the same bytes with SPS/PPS stripped, which must fail:

| profile | standalone-decodable keyframes | encode rate through our path |
|:--|:--|:--|
| 1280x720 @ 60 | yes (SPS+PPS in-band) | 96.5 fps |
| 1920x1080 @ 60 | yes | 47.8 fps |
| 2560x1440 @ 60 | yes | 29.5 fps |

For reference, `ffmpeg -c:v h264_vaapi` on the same GPU and kernel
(`7.3.0-rc4-1-cachyos-rc`) encodes 720p at ~114 fps, 1080p at ~55, 1440p at
~31 — so the driver's own ceiling at 1080p is ~55 fps, and the crate's path
is within ~15% of it after the fixes in `parley-core` (parallel BGRA→NV12,
reused repack/NV12 buffers).

Consequence for Parley, now that it holds: **the profile rate is measured,
not assumed.** `create_encoder_auto` times the real strided encode path at
creation and publishes that rate (`hardware_measured_fps`); the share path
paces, timestamps and rate-hints at `target_fps()`. On this machine that is
60 fps at 720p, ~48 at 1080p and ~29 at 1440p. 1080p60 is *not* reachable
here (the driver stops at ~55 fps while our path stops at ~48) and 1440p60
is not reachable at all; advertising either would mean pacing frames the
encoder cannot deliver, which the latency ladder would then blame on the
network.

## The patches

1. **In-band parameter sets** (`Encoder::encode_surface`, plus the
   `has_parameter_sets` helper): every IDR picture is emitted with its
   SPS/PPS ahead of the slices. A driver that accepts packed headers does
   not have to write them into the coded buffer, and radeonsi/VCE does not.
   The patch reuses the crate's own synthesis (`packed_sps`/`packed_pps`,
   the same structures the encoder was configured from, so the slices
   reference exactly these) and prepends them when the driver's output has
   no SPS. Repeats are legal for a compliant decoder, and the
   `has_parameter_sets` check means a driver that *does* write them never
   gets duplicates.

2. **Slice syntax the driver actually writes** (`build_sps_pps`,
   `FRAME_NUM_LIMIT`, the `FrameMeta` bookkeeping): Mesa's VCE slice writer
   emits a fixed layout regardless of what the packed headers declare.
   Declaring more than it writes makes every slice header misparse. The
   SPS/PPS now declare `MaxFrameNum = 128` (7-bit `frame_num`),
   `pic_order_cnt_type = 2` (no POC fields in the slice), and no deblocking
   filter control fields — and `frame_num` is counter-modulo-128 to match.
   Keep the two in lockstep: `FRAME_NUM_LIMIT` and the SPS builder are one
   setting spelled twice.

Upstream: both are worth reporting to moq-dev/vaapi (the packed-header
request is advertised by the driver and then not honoured; and the slice
syntax a driver writes is not discoverable from outside, which is why the
fix has to live inside `encode_surface`/`build_sps_pps`).

## Keeping it in sync

When bumping `moq-vaapi`, diff the new release against this copy, re-apply
both patches, and re-run:

```sh
# driver half: does this machine encode H.264, do its IRAPs stand alone?
tools/hw-probe.sh --encode h264

# parley half: our wrapper's keyframes at every profile, plus the
# negative control that proves the check bites (720p/1080p/1440p sweep,
# keyframes, throughput, adaptive rate)
PARLEY_VAAPI=1 cargo test --release -p parley-core --features vaapi \
    --test vaapi_keyframe_shape -- --nocapture --test-threads=1

# where the time goes, if a profile is slower than the driver allows
PARLEY_VAAPI=1 cargo test --release -p parley-core --features vaapi \
    --test vaapi_encode_cost -- --nocapture --test-threads=1
PARLEY_VAAPI=1 cargo test --release -p parley-core --features vaapi \
    --test vaapi_crate_cost -- --nocapture --test-threads=1

# dry run of the whole stack (no GPU): the app's self-test also refuses a
# hardware encoder whose keyframe does not decode by a fresh decoder
```

All of these are env-gated (`PARLEY_VAAPI=1`) and never run in CI: they
need a VAAPI node, and on a machine in the kernel window above an H.264
encode job can hang VCE (`tools/hw-probe.sh` prints the window check).
