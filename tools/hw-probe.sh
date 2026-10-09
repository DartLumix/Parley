#!/usr/bin/env bash
# Parley hardware codec probe — "what does THIS machine actually support?"
#
# Answers, for the machine that runs it:
#   * which VA-API encode/decode entrypoints the driver exposes (H.264, HEVC)
#   * which driver/stack is behind them (mesa, libva, kernel, firmware)
#   * whether ffmpeg — the candidate hardware-encode backend — has the
#     VAAPI encoders we would call
#   * whether this kernel is in the window where H.264 VCE encode wedges
#     the GPU (see the caveat below)
#
# Usage:
#   tools/hw-probe.sh                      # capability report only. Safe: no
#                                          # encode job is ever submitted.
#   tools/hw-probe.sh --encode hevc        # ALSO run a bounded VAAPI encode,
#                                          # then validate the bitstream
#   tools/hw-probe.sh --encode h264        # needs --force (see caveat)
#   tools/hw-probe.sh --device /dev/dri/renderD129
#
# CAVEAT — READ BEFORE --encode h264
#   Kernels 7.2.2 .. 7.3-rc1 have an amdgpu regression that hangs the VCE
#   encode engine on GCN/Polaris when an H.264 VAAPI job is submitted:
#     ring vce0 timeout, signaled seq=... emitted seq=...
#     GPU reset begin! ... (recovery can itself crash the kernel)
#   On the affected kernels that is a hard hang needing a power cycle.
#   HEVC encode is NOT affected by that regression (reports from RX 470/480/
#   580/590-class hardware), which is why --encode hevc is the useful test.
#   Fixed in 7.3-rc2 (gitlab.freedesktop.org/drm/amd issue 5790; see the
#   Arch BBS thread 314944 and rustdesk issue 16324 for the same signature).
#
# Exit status: 0 = everything asked for is present/worked; 1 = something
# asked for is missing or the encode test failed.
set -uo pipefail

CODEC=""
DEVICE=""
FORCE=0
KEEP=0
OUT=""
DURATION="${PROBE_SECONDS:-2}"
SIZE="${PROBE_SIZE:-1920x1080}"
BITRATE="${PROBE_BITRATE:-4M}"
GOP="${PROBE_GOP:-60}"
# nv12 = upload tightly-packed NV12 (canonical ffmpeg VAAPI pipeline).
# bgra = upload BGRA and let the GPU convert (the path the Parley app
# needs: PipeWire hands us strided BGRx, never NV12).
INPUT="${PROBE_INPUT:-nv12}"

usage() { sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --encode) CODEC="${2:-}"; shift 2 ;;
        --device) DEVICE="${2:-}"; shift 2 ;;
        --out)    OUT="${2:-}"; shift 2 ;;
        --force)  FORCE=1; shift ;;
        --keep)   KEEP=1; shift ;;
        -h|--help) usage 0 ;;
        *) echo "unknown argument: $1" >&2; usage 1 ;;
    esac
done

if [ -n "$CODEC" ] && [ "$CODEC" != "h264" ] && [ "$CODEC" != "hevc" ]; then
    echo "--encode takes h264 or hevc (got '$CODEC')" >&2
    exit 1
fi
if [ "$CODEC" = "h264" ] && [ "$FORCE" -ne 1 ]; then
    cat >&2 <<'EOF'
--encode h264 refused: on kernels 7.2.2..7.3-rc1 an H.264 VAAPI job wedges
VCE and the recovery is unreliable (needs a power cycle on the polaris box).
Re-run with --force if this machine is known-clear (7.3-rc2+ or a non-VCE GPU).
EOF
    exit 1
fi

say()  { printf '%s\n' "$*"; }
head1() { printf '\n== %s ==\n' "$*"; }
have() { command -v "$1" >/dev/null 2>&1; }

FAIL=0
VERDICT=()

# ---------------------------------------------------------------- kernel ---
# Affected window: 7.2.2 <= k < 7.3-rc2. Handles both "7.2.7-1-cachyos" and
# "7.3.rc4-1" style versions.
KERNEL_CAVEAT="unknown"
kernel_window() {
    local kern base maj rest min pat rc
    kern="$(uname -r)"
    base="$(printf '%s' "$kern" | grep -oE '^[0-9]+\.[0-9]+(\.[0-9]+)?')"
    [ -n "$base" ] || { printf 'unknown'; return; }
    maj="${base%%.*}"
    rest="${base#*.}"
    min="${rest%%.*}"
    pat=0
    case "$rest" in *.*) pat="${rest#*.}" ;; esac
    rc="$(printf '%s' "$kern" | grep -oiE 'rc[0-9]+' | head -1 | grep -oE '[0-9]+')"
    if [ "$maj" -ne 7 ]; then printf 'clear'; return; fi
    if [ "$min" -lt 2 ]; then printf 'clear'; return; fi
    if [ "$min" -eq 2 ] && [ "$pat" -lt 2 ]; then printf 'clear'; return; fi
    if [ "$min" -gt 3 ]; then printf 'clear'; return; fi
    if [ "$min" -eq 3 ]; then
        if [ -z "$rc" ]; then printf 'clear'; else
            if [ "$rc" -ge 2 ]; then printf 'clear'; else printf 'affected'; fi
        fi
        return
    fi
    printf 'affected'
}
KERNEL_CAVEAT="$(kernel_window)"

head1 "system"
say "distro:  $(. /etc/os-release 2>/dev/null && printf '%s' "${PRETTY_NAME:-unknown}")"
say "kernel:  $(uname -r)  [H.264 VCE regression window: $KERNEL_CAVEAT]"
if have pacman; then
    say "mesa:    $(pacman -Q mesa 2>/dev/null | awk '{print $2}')"
    say "libva:   $(pacman -Q libva 2>/dev/null | awk '{print $2}')"
    say "ffmpeg:  $(pacman -Q ffmpeg 2>/dev/null | awk '{print $2}')"
    say "fw-amdgpu: $(pacman -Q linux-firmware-amdgpu 2>/dev/null | awk '{print $2}')"
fi
say "render nodes: $(ls /dev/dri/renderD* 2>/dev/null | tr '\n' ' ')"
say "video nodes:  $(ls /dev/dri/card* 2>/dev/null | tr '\n' ' ')"

# ---------------------------------------------------------------- va-api ---
head1 "va-api"
H264_ENC=no; HEVC_ENC=no; H264_DEC=no; HEVC_DEC=no
VA_DRIVER=""
if have vainfo; then
    VA_OUT="$(vainfo 2>&1)"
    VA_DRIVER="$(printf '%s' "$VA_OUT" | grep -m1 'Driver version' | sed 's/.*Driver version: //')"
    grep -qE 'VAProfileH264[A-Za-z]*[[:space:]]*:.*VAEntrypointEnc(Slice|SliceLP)' <<<"$VA_OUT" && H264_ENC=yes
    grep -qE 'VAProfileHEVCMain[[:space:]]*:.*VAEntrypointEnc(Slice|SliceLP)'   <<<"$VA_OUT" && HEVC_ENC=yes
    grep -qE 'VAProfileH264[A-Za-z]*[[:space:]]*:.*VAEntrypointVLD'            <<<"$VA_OUT" && H264_DEC=yes
    grep -qE 'VAProfileHEVCMain[[:space:]]*:.*VAEntrypointVLD'                 <<<"$VA_OUT" && HEVC_DEC=yes
    say "driver:  ${VA_DRIVER:-<no driver string>}"
    say "H.264 encode: $H264_ENC    H.264 decode: $H264_DEC"
    say "HEVC  encode: $HEVC_ENC    HEVC  decode: $HEVC_DEC"
else
    say "vainfo missing (install libva-utils to get entrypoint detail)"
    FAIL=1
fi

# ----------------------------------------------------------------- ffmpeg ---
head1 "ffmpeg (candidate hardware-encode backend)"
FF_H264_VAAPI=no; FF_HEVC_VAAPI=no
if have ffmpeg; then
    say "version: $(ffmpeg -version 2>/dev/null | head -1 | awk '{print $3}')"
    ffenc="$(ffmpeg -hide_banner -encoders 2>/dev/null)"
    grep -qE '^ *V[.A-Z]*D h264_vaapi' <<<"$ffenc" && FF_H264_VAAPI=yes
    grep -qE '^ *V[.A-Z]*D hevc_vaapi' <<<"$ffenc" && FF_HEVC_VAAPI=yes
    say "h264_vaapi: $FF_H264_VAAPI    hevc_vaapi: $FF_HEVC_VAAPI"
    grep -qE '^ *V[.A-Z]*D hevc ' <(ffmpeg -hide_banner -decoders 2>/dev/null) \
        && say "software hevc decoder: yes" || say "software hevc decoder: MISSING"
else
    say "ffmpeg MISSING — the ffmpeg-backed hardware path cannot run here"
    FAIL=1
fi

# ---------------------------------------------------------------- verdict ---
head1 "verdict"
[ "$H264_ENC" = yes ] && [ "$FF_H264_VAAPI" = yes ] && VERDICT+=("H.264 hardware encode: usable")
[ "$HEVC_ENC" = yes ] && [ "$FF_HEVC_VAAPI" = yes ] && VERDICT+=("HEVC hardware encode: usable")
[ "$HEVC_DEC" = yes ] && VERDICT+=("HEVC hardware decode: usable")
[ "$H264_DEC" = yes ] && VERDICT+=("H.264 hardware decode: usable")
if [ "${#VERDICT[@]}" -eq 0 ]; then say "nothing hardware-accelerated found — software codecs only"; fi
for v in "${VERDICT[@]}"; do say "* $v"; done
if [ "$KERNEL_CAVEAT" = affected ]; then
    cat <<'EOF'

CAVEAT: this kernel (7.2.2 .. 7.3-rc1) is in the amdgpu regression window
where an H.264 VAAPI encode job hangs VCE (ring vce0 timeout) and the GPU
recovery can crash — i.e. fall back to software H.264 here, or use HEVC
hardware encode, or boot 7.3-rc2+ (cachyos: linux-cachyos-rc 7.3.rc4-1).
EOF
fi

# ------------------------------------------------------------ encode test ---
if [ -z "$CODEC" ]; then
    say ""
    say "Capability report only. Run with --encode hevc to submit a bounded"
    say "${DURATION}s ${SIZE} VAAPI job and validate the bitstream it produces."
    exit $FAIL
fi

head1 "encode test: $CODEC (bounded, ${DURATION}s, ${SIZE})"
if ! have ffmpeg; then say "ffmpeg missing — cannot run"; exit 1; fi
if [ "$CODEC" = hevc ] && [ "$HEVC_ENC" = no ]; then say "driver exposes no HEVC encode entrypoint"; exit 1; fi
if [ "$CODEC" = h264 ] && [ "$H264_ENC" = no ]; then say "driver exposes no H.264 encode entrypoint"; exit 1; fi

# Pick a device: the first render node whose driver accepts the encode profile.
if [ -z "$DEVICE" ]; then
    for n in /dev/dri/renderD*; do [ -e "$n" ] || continue; DEVICE="$n"; break; done
fi
[ -n "$DEVICE" ] && [ -e "$DEVICE" ] || { say "no usable /dev/dri/renderD* node"; exit 1; }
say "device:  $DEVICE"

TMP="$(mktemp -d)"
trap '[ "$KEEP" -eq 1 ] || rm -rf "$TMP"' EXIT
OUT="${OUT:-$TMP/probe-$CODEC.hevc}"
[ "$CODEC" = h264 ] && OUT="${OUT%.hevc}.h264"

if [ "$INPUT" = bgra ]; then
    VF='format=bgra,hwupload,scale_vaapi=format=nv12'
    say "input:   BGRA upload + GPU convert (scale_vaapi)"
else
    VF='format=nv12,hwupload'
    say "input:   NV12 upload"
fi
say "cmd: ffmpeg -vaapi_device $DEVICE -f lavfi -i testsrc2=s=$SIZE:r=30 -t $DURATION \\"
say "       -vf $VF -c:v ${CODEC}_vaapi -b:v $BITRATE -g $GOP -f $CODEC OUT"
# 180s ceiling: a wedged VCE ring will not be rescued by killing ffmpeg, but
# the kernel's own job timeout fires at ~10s and we want the error text.
timeout -k 5 180 ffmpeg -hide_banner -loglevel error -vaapi_device "$DEVICE" \
    -f lavfi -i "testsrc2=s=$SIZE:r=30" -t "$DURATION" \
    -vf "$VF" -c:v "${CODEC}_vaapi" -b:v "$BITRATE" -g "$GOP" \
    -f "$CODEC" "$OUT"
RC=$?
if [ $RC -ne 0 ]; then
    say "ENCODE FAILED (exit $RC) — if this was a VCE timeout, reboot and read the caveat"
    exit 1
fi
say "encoded: $(stat -c '%s bytes' "$OUT")"

# 1. Does a decoder accept the whole stream?
if timeout 60 ffmpeg -v error -f "$CODEC" -i "$OUT" -f null - 2>"$TMP/dec.log"; then
    say "whole-stream decode: OK"
else
    say "whole-stream decode: FAILED"; sed -n '1,10p' "$TMP/dec.log"; exit 1
fi

# 2. The property the app actually depends on: every IRAP picture must be
#    decodable by a FRESH decoder, i.e. carry VPS/SPS/PPS in-band. Our
#    receivers construct decoders mid-stream (late joiner, forced IDR), so
#    an encoder that only sends parameter sets once is unusable for us.
have python3 || { say "python3 missing — skipping per-IDR validation"; exit 0; }
python3 - "$OUT" "$TMP" "$CODEC" <<'PY'
import os, subprocess, sys
path, tmp, codec = sys.argv[1], sys.argv[2], sys.argv[3]
data = open(path, 'rb').read()
# Annex-B split (3- or 4-byte start codes): (start of NAL, length of its code).
starts, i = [], 0
while i < len(data) - 3:
    if data[i:i+4] == b'\0\0\0\1': starts.append((i+4, 4)); i += 4
    elif data[i:i+3] == b'\0\0\1': starts.append((i+3, 3)); i += 3
    else: i += 1
units, cur = [], []
for idx, (start, _) in enumerate(starts):
    end = (starts[idx+1][0] - starts[idx+1][1]) if idx+1 < len(starts) else len(data)
    nal = data[start:end]
    if codec == 'hevc':
        t = (nal[0] >> 1) & 0x3F
        vcl = t <= 31
        new_pic = vcl and len(nal) > 2 and (nal[2] >> 7) & 1
        ir = t in (16, 17, 18, 19, 20, 21)   # BLA/IDR/CRA = IRAP
    else:
        t = nal[0] & 0x1F
        vcl, new_pic = 1 <= t <= 5, t in (1, 5)
        ir = t == 5
    if new_pic and any(n['vcl'] for n in cur):
        # A new picture begins. Per H.265/H.264 AU rules the non-VCL NALs
        # queued since the last slice (VPS/SPS/PPS, SEI) belong to THIS
        # picture, not to the one they happen to sit behind.
        k = len(cur)
        while k > 0 and not cur[k-1]['vcl']:
            k -= 1
        units.append(cur[:k]); cur = cur[k:]
    cur.append({'t': t, 'vcl': vcl, 'ir': ir, 'data': nal})
if cur: units.append(cur)
ir_units = [u for u in units if any(n['ir'] for n in u)]
print(f"per-IDR: {len(units)} access units, {len(ir_units)} of them IRAP")
# Sanity: an AU with no VCL NAL at all means the splitter or the stream is
# broken — report it rather than draw a conclusion from it.
if any(not any(n['vcl'] for n in u) for u in units):
    print("per-IDR: WARNING: access unit with no slice found (splitter/stream shape)")
if not ir_units:
    print(f"per-IDR: no IRAP picture found in {len(units)} access units — FAIL"); sys.exit(1)
bad = 0
for n, u in enumerate(ir_units):
    pss = {x['t'] for x in u if x['t'] in (32, 33, 34) or (codec == 'h264' and x['t'] in (7, 8))}
    f = os.path.join(tmp, f"ir{n}.{codec}")
    # Re-emit Annex-B: the splitter stored NAL bytes with the start code stripped.
    open(f, 'wb').write(b''.join(b'\x00\x00\x00\x01' + x['data'] for x in u))
    r = subprocess.run(['ffmpeg', '-v', 'error', '-f', codec, '-i', f, '-f', 'null', '-'],
                       capture_output=True)
    ok = r.returncode == 0
    bad += 0 if ok else 1
    print(f"per-IDR: IRAP {n} ({len(u)} NALs, param sets after previous picture: {sorted(pss) or 'NONE'}) "
          f"stoodalone-decode={'OK' if ok else 'FAIL'} {r.stderr.decode()[:120].strip()}")
print(f"per-IDR: {len(ir_units) - bad}/{len(ir_units)} IRAP pictures decode standalone")
sys.exit(1 if bad else 0)
PY
PY_RC=$?
if [ $PY_RC -ne 0 ]; then
    say "per-IDR validation FAILED — an encoder whose keyframes lack in-band"
    say "parameter sets cannot serve our mid-stream receivers"
    exit 1
fi
say ""
say "RESULT: $CODEC hardware encode works here and its IRAPs stand alone."
exit 0
