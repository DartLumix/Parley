# vendor/

`moq-vaapi` 0.1.0, copied from crates.io and patched. `Cargo.toml` at the
repository root points `[patch.crates-io]` at this copy, so `cargo build`
with the `vaapi` feature (the CLI's `--hw-encode`, the Flutter app's
`hw-encode`) builds this one. The crate is excluded from the workspace: its
sources are upstream's (tabs, `#![allow(clippy::all)]`) and must not be
swept by `fmt`/`clippy`/`test --workspace`.

## Status: necessary, NOT sufficient on Mesa/radeonsi

Read this before trusting the patch. It fixes one of two problems, and on
this project's AMD hardware (RX 590, Mesa 26.2.4, radeonsi, VCE) the second
one still leaves hardware-encoded H.264 undecodable. Measured state:

| decoder view of the hardware stream | before patch | after patch |
|:--|:--|:--|
| parameter sets anywhere in the stream | none | SPS+PPS in every IDR |
| ffprobe stream header | `non-existing PPS` | Profile Main, 1280x720, Level 4.0 |
| openh264 on a fresh decoder | `Native:16 dsNoParamSets` | `Native:4 dsFrameCorrupted` |
| ffmpeg on the whole stream | no frames | no frames (`deblocking_filter_idc 4 out of range`, `idr_pic_id is invalid`) |

So the parameter sets now exist and parse, but the driver's slice payloads do
not parse against them: Mesa's VCE encoder writes slice headers whose syntax
does not match the PPS this crate synthesizes and hands it (`deblocking_filter_idc`
and `idr_pic_id` are the fields ffmpeg trips over — i.e. the slice header is
shorter/longer than the PPS implies). That mismatch is inside the driver's
interpretation of the VA-API picture/slice parameters, not something this
crate's bitstream synthesis can paper over without reverse-engineering
Mesa's slice syntax for this hardware.

Consequence for Parley: `--hw-encode` / the app's HW-encode toggle must NOT be
presented as working on radeonsi yet. Contradicting stream shapes, this should
be re-checked on Intel (iHD/i965), where this crate's packed-header path is
the one it was written against.

## The patch

One change, in `src/encode.rs` (`Encoder::encode_surface`, plus the
`has_parameter_sets` helper next to it): **every IDR picture is emitted with
its SPS/PPS in-band, ahead of the slices.**

Why: the encoder synthesizes SPS and PPS and hands them to the driver as
*packed headers*, then reads back the coded buffer. A driver that accepts
packed headers does not have to write them into the coded buffer, and
radeonsi/VCE does not — the buffer holds the slice NALs and a filler. The
stream then has no parameter sets anywhere, so *no* receiver can decode it:
a viewer that joins at a keyframe fails with `dsNoParamSets` (Native:16), and
so does a decoder that has watched from frame 0, because there is nothing to
decode against.

The patch reuses the crate's own synthesis (`packed_sps`/`packed_pps`, the
same structures the encoder was configured from, so the slices reference
exactly these) and prepends them when the driver's output has no SPS.
Repeats are legal for a compliant decoder, and the `has_parameter_sets` check
means a driver that *does* write them never gets duplicates.

Upstream: worth reporting to moq-dev/vaapi (the packed-header request is
advertised by the driver and then not honoured; the coded buffer's shape is
not reachable from outside, which is why the fix has to live inside
`encode_surface`).

## Keeping it in sync

When bumping `moq-vaapi`, diff the new release against this copy, re-apply the
patch, and re-run:

```sh
PARLEY_VAAPI=1 cargo test -p parley-core --features vaapi --test vaapi_keyframe_shape -- --nocapture
```

That test currently FAILS at the last step (frame-corrupted, not
missing-params) — it is the reproduction of the remaining driver mismatch,
kept so a fixed backend or driver has something to prove itself against.
