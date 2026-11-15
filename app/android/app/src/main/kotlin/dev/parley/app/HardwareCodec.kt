package dev.parley.app

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.util.Log
import java.io.ByteArrayOutputStream
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.ArrayDeque

/**
 * Hardware H.264 MediaCodec bridge for the Rust media plane.
 *
 * Rust owns the capture/encode loop and the MoQ publisher; this class only
 * owns MediaCodec. Both factories deliberately return null when no hardware
 * codec accepts the requested format, so the Rust side can fall back to
 * OpenH264 rather than silently running a software MediaCodec under a
 * hardware label.
 */
object HardwareCodec {
    private const val TAG = "ParleyCodec"
    private const val MIME = MediaFormat.MIMETYPE_VIDEO_AVC

    @JvmStatic
    fun createEncoder(width: Int, height: Int, bitrate: Int, fps: Int): HardwareEncoder? {
        if (width <= 0 || height <= 0 || bitrate <= 0 || fps <= 0) return null
        for (info in MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos) {
            if (!info.isEncoder) continue
            if (!info.supportedTypes.any { it.equals(MIME, ignoreCase = true) }) continue
            if (!isHardware(info)) continue
            val caps = try {
                info.getCapabilitiesForType(MIME)
            } catch (_: Exception) {
                continue
            }
            val video = caps.videoCapabilities ?: continue
            // Rust feeds an Image-backed I420 plane layout. A codec that
            // does not advertise flexible YUV cannot expose the input Image
            // this bridge writes, even when configure() would accept it.
            if (!caps.colorFormats.contains(MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420Flexible)) {
                continue
            }
            try {
                if (!video.isSizeSupported(width, height)) continue
            } catch (_: Exception) {
                continue
            }
            // A codec's advertised bitrate ceiling is a capability, not a
            // reason to reject the whole encoder: clamp the target into the
            // supported range and let setBitrate adapt from there.
            val configuredBitrate =
                try {
                    bitrate.coerceIn(video.bitrateRange.lower, video.bitrateRange.upper)
                } catch (_: Exception) {
                    continue
                }
            val codec = try {
                MediaCodec.createByCodecName(info.name)
            } catch (_: Exception) {
                continue
            }
            try {
                val format = MediaFormat.createVideoFormat(MIME, width, height).apply {
                    setInteger(
                        MediaFormat.KEY_COLOR_FORMAT,
                        MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420Flexible,
                    )
                    setInteger(MediaFormat.KEY_BIT_RATE, configuredBitrate)
                    setInteger(MediaFormat.KEY_BITRATE_MODE, MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_VBR)
                    setInteger(MediaFormat.KEY_FRAME_RATE, fps)
                    setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 2)
                    // Profile matters for the zero-B-frame contract. API 29
                    // has KEY_MAX_B_FRAMES; before that a Baseline profile is
                    // the only way to guarantee no reordering on every
                    // vendor encoder (Main can legally use B frames). An
                    // optional key some vendors reject must not cost us the
                    // whole hardware encoder: fall back to Baseline if the
                    // zero-B constraint cannot be expressed.
                    var baseline = false
                    if (Build.VERSION.SDK_INT >= 29) {
                        try {
                            setInteger(MediaFormat.KEY_PROFILE, MediaCodecInfo.CodecProfileLevel.AVCProfileMain)
                            setInteger(MediaFormat.KEY_MAX_B_FRAMES, 0)
                        } catch (_: Exception) {
                            // If the codec cannot express the zero-B
                            // constraint, Baseline is the only remaining
                            // guarantee. Let a Baseline rejection propagate
                            // to the enclosing per-codec catch: silently
                            // keeping Main here would permit B-frame
                            // reordering and break the latency contract.
                            setInteger(
                                MediaFormat.KEY_PROFILE,
                                MediaCodecInfo.CodecProfileLevel.AVCProfileBaseline,
                            )
                            baseline = true
                        }
                    } else {
                        setInteger(
                            MediaFormat.KEY_PROFILE,
                            MediaCodecInfo.CodecProfileLevel.AVCProfileBaseline,
                        )
                        baseline = true
                    }
                    // No KEY_LEVEL: a fixed 4.2 rejects 1440p60 at configure
                    // time. The codec derives a conforming level from size and
                    // frame rate; the SPS carries it to the decoder.
                    if (Build.VERSION.SDK_INT >= 30) {
                        try {
                            setInteger(MediaFormat.KEY_LATENCY, 0)
                        } catch (_: Exception) {
                            // Latency is a hint; its absence is not a reason
                            // to reject an otherwise usable encoder.
                        }
                    }
                    if (baseline) {
                        Log.i(TAG, "encoder ${info.name} using Baseline (zero B frames)")
                    }
                }
                codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
                codec.start()
                Log.i(TAG, "encoder ${info.name} ${width}x${height} @ $fps, $configuredBitrate bps")
                return HardwareEncoder(codec, width, height)
            } catch (e: Exception) {
                Log.w(TAG, "encoder ${info.name} rejected ${width}x${height}: ${e.message}")
                try {
                    codec.stop()
                } catch (_: Exception) {
                }
                codec.release()
            }
        }
        return null
    }

    @JvmStatic
    fun createDecoder(width: Int, height: Int): HardwareDecoder? {
        val safeWidth = width.coerceAtLeast(2)
        val safeHeight = height.coerceAtLeast(2)
        for (info in MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos) {
            if (info.isEncoder) continue
            if (!info.supportedTypes.any { it.equals(MIME, ignoreCase = true) }) continue
            if (!isHardware(info)) continue
            // The decoder output is copied through `getOutputImage`, which
            // requires a flexible-YUV format. Rejecting it here makes the
            // next hardware candidate (or software fallback) win instead of
            // failing at the first decoded frame.
            val caps = try {
                info.getCapabilitiesForType(MIME)
            } catch (_: Exception) {
                continue
            }
            if (!caps.colorFormats.contains(MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420Flexible)) {
                continue
            }
            val codec = try {
                MediaCodec.createByCodecName(info.name)
            } catch (_: Exception) {
                continue
            }
            try {
                // Decoders negotiate their output format from the SPS; a
                // KEY_COLOR_FORMAT input hint is an encoder setting and some
                // vendor decoders reject it at configure time.
                val format = MediaFormat.createVideoFormat(MIME, safeWidth, safeHeight)
                codec.configure(format, null, null, 0)
                codec.start()
                Log.i(TAG, "decoder ${info.name} ${safeWidth}x${safeHeight}")
                return HardwareDecoder(codec, safeWidth, safeHeight)
            } catch (e: Exception) {
                Log.w(TAG, "decoder ${info.name} rejected: ${e.message}")
                try {
                    codec.stop()
                } catch (_: Exception) {
                }
                codec.release()
            }
        }
        return null
    }

    private fun isHardware(info: MediaCodecInfo): Boolean {
        if (Build.VERSION.SDK_INT >= 29) {
            return try {
                info.isHardwareAccelerated
            } catch (_: Exception) {
                false
            }
        }
        // Before API 29 there is no reliable hardware flag. Names that
        // advertise software (c2.android.*, OMX.google.*) are excluded; the
        // remaining vendor codecs are hardware on every device Parley
        // supports at minSdk 26.
        val name = info.name.lowercase()
        return !(name.startsWith("c2.android.") || name.startsWith("omx.google."))
    }
}

class HardwareEncoder internal constructor(
    private val codec: MediaCodec,
    private val width: Int,
    private val height: Int,
) {
    private var csd = ByteArray(0)
    // MediaCodec may emit more than one access unit per drain. Returning
    // only the newest silently drops reference frames, so keep the backlog
    // and hand the Rust caller one access unit per input frame.
    private val outputs = ArrayDeque<ByteArray>()
    // dequeueInputBuffer(0) can legitimately have no free slot. Dropping
    // that picture would break the reference chain until the next IDR, so
    // hold a small bounded copy and feed it when a slot frees up.
    private val pendingInputs = ArrayDeque<PendingInput>()

    private data class PendingInput(
        val data: ByteArray,
        val ptsUs: Long,
        val keyframe: Boolean,
    )

    /**
     * Queue one tightly packed I420 frame and return the next encoded
     * access unit, if one is ready. A null return is normal: MediaCodec is
     * asynchronous and the hardware pipeline can buffer the first frames.
     */
    fun encode(i420: ByteBuffer, size: Int, ptsUs: Long, keyframe: Boolean): ByteArray? {
        if (pendingInputs.isEmpty()) {
            val input = codec.dequeueInputBuffer(0)
            if (input >= 0) {
                feed(input, i420, size, ptsUs, keyframe)
            } else {
                enqueue(i420, size, ptsUs, keyframe)
            }
        } else {
            // Preserve order: the current frame waits behind the backlog.
            enqueue(i420, size, ptsUs, keyframe)
        }
        while (pendingInputs.isNotEmpty()) {
            val input = codec.dequeueInputBuffer(0)
            if (input < 0) break
            val pending = pendingInputs.removeFirst()
            feed(input, ByteBuffer.wrap(pending.data), pending.data.size, pending.ptsUs, pending.keyframe)
        }
        drain()
        return outputs.pollFirst()
    }

    private fun enqueue(i420: ByteBuffer, size: Int, ptsUs: Long, keyframe: Boolean) {
        if (size <= 0) return
        val copy = ByteArray(size)
        i420.get(copy, 0, size)
        // Bounded like the decoder backlog. If a vendor withholds input
        // slots for this long, the next IDR is the only correct recovery;
        // keeping the newest frames is better than growing without bound.
        if (pendingInputs.size >= 8) pendingInputs.removeFirst()
        pendingInputs.addLast(PendingInput(copy, ptsUs, keyframe))
    }

    private fun feed(
        input: Int,
        i420: ByteBuffer,
        size: Int,
        ptsUs: Long,
        keyframe: Boolean,
    ) {
        val image = codec.getInputImage(input)
        if (image == null) {
            // A codec that advertises flexible YUV but cannot expose an
            // input Image is unusable for this path; give the empty
            // buffer back rather than feeding it bytes in the wrong
            // layout.
            codec.queueInputBuffer(input, 0, 0, ptsUs, 0)
            return
        }
        copyPlane(i420, 0, image.planes[0], width, height, width)
        val cw = width / 2
        val ch = height / 2
        val ySize = width * height
        if (image.planes.size >= 3) {
            copyPlane(i420, ySize, image.planes[1], cw, ch, cw)
            copyPlane(i420, ySize + cw * ch, image.planes[2], cw, ch, cw)
        } else if (image.planes.size == 2) {
            // Some flexible-YUV encoders expose NV12 as one interleaved
            // UV plane instead of separate U and V.
            copyInterleavedUv(i420, ySize, image.planes[1], cw, ch)
        }
        val flags = if (keyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
        codec.queueInputBuffer(input, 0, size, ptsUs, flags)
    }

    fun setBitrate(bps: Int) {
        if (bps <= 0) return
        try {
            val params = android.os.Bundle()
            params.putInt(MediaCodec.PARAMETER_KEY_VIDEO_BITRATE, bps)
            codec.setParameters(params)
        } catch (e: Exception) {
            Log.w("ParleyCodec", "encoder bitrate change failed: ${e.message}")
        }
    }

    fun close() {
        try {
            codec.stop()
        } catch (_: Exception) {
        }
        codec.release()
    }

    private fun drain() {
        while (true) {
            val info = MediaCodec.BufferInfo()
            val index = try {
                codec.dequeueOutputBuffer(info, 0)
            } catch (e: Exception) {
                Log.w("ParleyCodec", "encoder drain failed: ${e.message}")
                return
            }
            when {
                index == MediaCodec.INFO_TRY_AGAIN_LATER -> return
                index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                    val format = codec.outputFormat
                    val parts = ArrayList<ByteArray>(2)
                    for (key in arrayOf("csd-0", "csd-1")) {
                        val buf = format.getByteBuffer(key) ?: continue
                        buf.rewind()
                        val bytes = ByteArray(buf.remaining())
                        buf.get(bytes)
                        parts.add(bytes)
                    }
                    if (parts.isNotEmpty()) {
                        csd = toAnnexB(parts.reduce { a, b -> a + b })
                    }
                }
                index >= 0 -> {
                    val buffer = codec.getOutputBuffer(index)
                    if (buffer != null && info.size > 0) {
                        buffer.position(info.offset)
                        buffer.limit(info.offset + info.size)
                        val bytes = ByteArray(info.size)
                        buffer.get(bytes)
                        if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
                            csd = toAnnexB(bytes)
                        } else {
                            // The flag is normally set for an IDR, but a
                            // vendor encoder can omit it while the bitstream
                            // still starts a new GOP. Scan the NALs too, so
                            // the SPS/PPS always ride the first slice a
                            // receiver can use as an anchor.
                            // Normalize here as well as in Rust: if a vendor
                            // emits AVCC access units, the CSD (Annex-B) and
                            // the picture must not be concatenated as two
                            // different bitstream formats.
                            val frame = toAnnexB(bytes)
                            val key = (info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0) || hasIdr(frame)
                            outputs.add(if (key && csd.isNotEmpty()) csd + frame else frame)
                        }
                        buffer.clear()
                    }
                    codec.releaseOutputBuffer(index, false)
                }
            }
        }
    }

    private fun copyPlane(
        src: ByteBuffer,
        offset: Int,
        plane: android.media.Image.Plane,
        w: Int,
        h: Int,
        srcRow: Int,
    ) {
        val dst = plane.buffer
        val rowStride = plane.rowStride
        val pixelStride = plane.pixelStride
        val srcBase = src.position()
        val dstBase = dst.position()
        for (y in 0 until h) {
            for (x in 0 until w) {
                val value = src.get(srcBase + offset + y * srcRow + x)
                dst.put(dstBase + y * rowStride + x * pixelStride, value)
            }
        }
    }

    /** Write separate I420 U and V planes into an interleaved NV12 plane. */
    private fun copyInterleavedUv(
        src: ByteBuffer,
        offset: Int,
        plane: android.media.Image.Plane,
        w: Int,
        h: Int,
    ) {
        val dst = plane.buffer
        val rowStride = plane.rowStride
        val pixelStride = plane.pixelStride.coerceAtLeast(1)
        val srcBase = src.position()
        val dstBase = dst.position()
        for (y in 0 until h) {
            for (x in 0 until w) {
                val out = dstBase + y * rowStride + x * pixelStride
                dst.put(out, src.get(srcBase + offset + y * w + x))
                if (pixelStride > 1) {
                    dst.put(out + 1, src.get(srcBase + offset + w * h + y * w + x))
                }
            }
        }
    }

    /** Does this access unit contain an IDR slice (Annex-B or AVCC)? */
    private fun hasIdr(data: ByteArray): Boolean {
        var i = 0
        while (i + 3 < data.size) {
            if (data[i] == 0.toByte() && data[i + 1] == 0.toByte() && data[i + 2] == 1.toByte()) {
                if (i + 3 < data.size && (data[i + 3].toInt() and 0x1f) == 5) return true
                i += 3
            } else if (i + 4 < data.size && data[i] == 0.toByte() && data[i + 1] == 0.toByte() &&
                data[i + 2] == 0.toByte() && data[i + 3] == 1.toByte()
            ) {
                if (i + 4 < data.size && (data[i + 4].toInt() and 0x1f) == 5) return true
                i += 4
            } else {
                i++
            }
        }
        return false
    }

    /**
     * Codec-specific data is usually Annex-B, but some vendors expose it in
     * AVCC length-prefixed form. Rust's normalizer would then see a valid
     * Annex-B IDR after the CSD and leave the AVCC bytes in the group, so
     * convert the CSD here while its provenance is known.
     */
    private fun toAnnexB(data: ByteArray): ByteArray {
        if (data.isEmpty()) return data
        // A four-byte start code is unambiguous; never let an AVCC length
        // misclassify an Annex-B access unit. A three-byte start code is
        // ambiguous with an AVCC length such as 00 00 01 2c, so parse AVCC
        // first and validate every length and NAL header below.
        if (data.size >= 4 && data[0] == 0.toByte() && data[1] == 0.toByte() &&
            data[2] == 0.toByte() && data[3] == 1.toByte()
        ) return data
        if (data.size >= 4) {
            val out = ByteArrayOutputStream(data.size + 8)
            var i = 0
            var avcc = true
            while (i < data.size) {
                if (data.size - i < 4) {
                    avcc = false
                    break
                }
                val len = ((data[i].toInt() and 0xff) shl 24) or
                    ((data[i + 1].toInt() and 0xff) shl 16) or
                    ((data[i + 2].toInt() and 0xff) shl 8) or
                    (data[i + 3].toInt() and 0xff)
                i += 4
                // Compare against the remaining bytes rather than computing
                // i + len: a hostile or corrupt length can overflow Int.
                if (len <= 0 || len > data.size - i) {
                    avcc = false
                    break
                }
                val nal = data[i].toInt() and 0xff
                if (nal and 0x80 != 0 || (nal and 0x1f) !in 1..23) {
                    avcc = false
                    break
                }
                out.write(0)
                out.write(0)
                out.write(0)
                out.write(1)
                out.write(data, i, len)
                i += len
            }
            if (avcc && i == data.size && out.size() > 0) return out.toByteArray()
        }
        return data
    }
}

class HardwareDecoder internal constructor(
    private val codec: MediaCodec,
    @Volatile private var width: Int,
    @Volatile private var height: Int,
) {
    // A zero-timeout dequeue can legitimately find no free input buffer. A
    // P-frame dropped at that moment would corrupt every frame until the next
    // IDR, so hold undelivered access units and feed them as slots free up.
    private val pending = ArrayDeque<Pair<ByteArray, Long>>()

    fun decode(annexb: ByteBuffer, ptsUs: Long): ByteArray? {
        val size = annexb.remaining()
        val bytes = ByteArray(size)
        annexb.get(bytes)
        if (pending.size >= 32) pending.removeFirst()
        pending.addLast(bytes to ptsUs)
        while (pending.isNotEmpty()) {
            val input = codec.dequeueInputBuffer(0)
            if (input < 0) break
            val buffer = codec.getInputBuffer(input)
            if (buffer == null) {
                codec.queueInputBuffer(input, 0, 0, ptsUs, 0)
                break
            }
            val (frame, ts) = pending.first()
            if (buffer.capacity() < frame.size) {
                // This access unit can never fit this codec's input buffer;
                // dropping it lets later frames through instead of wedging
                // the queue forever.
                codec.queueInputBuffer(input, 0, 0, ts, 0)
                pending.removeFirst()
                continue
            }
            buffer.clear()
            buffer.put(frame)
            codec.queueInputBuffer(input, 0, frame.size, ts, 0)
            pending.removeFirst()
        }
        return drain()
    }

    fun close() {
        try {
            codec.stop()
        } catch (_: Exception) {
        }
        codec.release()
    }

    private fun drain(): ByteArray? {
        var latest: ByteArray? = null
        while (true) {
            val info = MediaCodec.BufferInfo()
            val index = try {
                codec.dequeueOutputBuffer(info, 0)
            } catch (e: Exception) {
                Log.w("ParleyCodec", "decoder drain failed: ${e.message}")
                return latest
            }
            when {
                index == MediaCodec.INFO_TRY_AGAIN_LATER -> return latest
                index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                    val format = codec.outputFormat
                    // MediaFormat.getInteger(name, default) is API 29; this
                    // app supports API 26. Use the keyed getter and keep the
                    // previous value when a codec omits the key.
                    try {
                        width = format.getInteger(MediaFormat.KEY_WIDTH)
                    } catch (_: Exception) {
                    }
                    try {
                        height = format.getInteger(MediaFormat.KEY_HEIGHT)
                    } catch (_: Exception) {
                    }
                }
                index >= 0 -> {
                    val image = codec.getOutputImage(index)
                    if (image != null) {
                        latest = toI420(image)
                        image.close()
                    }
                    codec.releaseOutputBuffer(index, false)
                }
            }
        }
    }

    private fun toI420(image: android.media.Image): ByteArray {
        val w = image.width
        val h = image.height
        val cw = (w + 1) / 2
        val ch = (h + 1) / 2
        val out = ByteBuffer.allocate(8 + w * h + 2 * cw * ch).order(ByteOrder.LITTLE_ENDIAN)
        out.putInt(w)
        out.putInt(h)
        copyPlane(image.planes[0], w, h, cw, out)
        if (image.planes.size >= 3) {
            copyPlane(image.planes[1], cw, ch, cw, out)
            copyPlane(image.planes[2], cw, ch, cw, out)
        } else if (image.planes.size == 2) {
            copyInterleavedUv(image.planes[1], cw, ch, out)
        }
        return out.array()
    }

    private fun copyPlane(
        plane: android.media.Image.Plane,
        w: Int,
        h: Int,
        @Suppress("UNUSED_PARAMETER") cw: Int,
        out: ByteBuffer,
    ) {
        val src = plane.buffer
        val rowStride = plane.rowStride
        val pixelStride = plane.pixelStride
        val base = src.position()
        for (y in 0 until h) {
            for (x in 0 until w) {
                out.put(src.get(base + y * rowStride + x * pixelStride))
            }
        }
    }

    /** Split an interleaved NV12 UV plane into the I420 U then V layout. */
    private fun copyInterleavedUv(
        plane: android.media.Image.Plane,
        w: Int,
        h: Int,
        out: ByteBuffer,
    ) {
        val src = plane.buffer
        val rowStride = plane.rowStride
        val pixelStride = plane.pixelStride.coerceAtLeast(1)
        val base = src.position()
        // U plane, then V plane: the Rust decoder expects I420, not NV12.
        for (y in 0 until h) {
            for (x in 0 until w) {
                out.put(src.get(base + y * rowStride + x * pixelStride))
            }
        }
        for (y in 0 until h) {
            for (x in 0 until w) {
                if (pixelStride > 1) {
                    out.put(src.get(base + y * rowStride + x * pixelStride + 1))
                } else {
                    out.put(128.toByte())
                }
            }
        }
    }
}
