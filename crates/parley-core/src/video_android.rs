//! Android H.264 encode/decode through MediaCodec.
//!
//! The platform codecs are Java objects owned by `HardwareCodec.kt`; this
//! module is the JNI adapter that gives them the same one-frame-in /
//! one-frame-out contract as the desktop backends. The JavaVM is obtained
//! from `ndk-context`, which the Flutter activity initializes at startup.
//!
//! Color layout is explicit at the boundary: the encoder takes tightly
//! packed I420 (Rust converts BGRA once), and the decoder returns an 8-byte
//! little-endian width/height header followed by I420. MediaCodec is free to
//! use tiled/stride-padded surfaces internally; the Kotlin side copies each
//! plane through `Image.Plane.rowStride`/`pixelStride` before it crosses JNI.

use jni::objects::{GlobalRef, JByteArray, JClass, JObject, JValue};
use jni::{JNIEnv, JavaVM};

use crate::error::Error;
use crate::video::{CodecId, VideoEncoder, bgra_to_i420_into, h264_to_annexb, i420_to_rgb};

const CODEC_CLASS: &str = "dev.parley.app.HardwareCodec";

fn android_vm() -> Result<JavaVM, Error> {
    let ctx = ndk_context::android_context();
    if ctx.vm().is_null() {
        return Err(Error::Codec(
            "Android JavaVM is not initialized (MainActivity.initAndroidContext)".into(),
        ));
    }
    unsafe { JavaVM::from_raw(ctx.vm().cast()) }
        .map_err(|e| Error::Codec(format!("Android JavaVM: {e}")))
}

fn codec_class<'local>(
    env: &mut JNIEnv<'local>,
    ctx: *mut std::ffi::c_void,
) -> Result<JClass<'local>, Error> {
    if ctx.is_null() {
        return Err(Error::Codec(
            "Android context is not initialized (MainActivity.initAndroidContext)".into(),
        ));
    }
    let context = unsafe { JObject::from_raw(ctx.cast()) };
    let loader = env
        .call_method(&context, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])
        .and_then(|v| v.l())
        .map_err(|e| Error::Codec(format!("Android class loader: {e}")))?;
    let name = env
        .new_string(CODEC_CLASS)
        .map_err(|e| Error::Codec(format!("Android codec class name: {e}")))?;
    let class = env
        .call_method(
            &loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&name)],
        )
        .and_then(|v| v.l())
        .map_err(|e| Error::Codec(format!("load {CODEC_CLASS}: {e}")))?;
    // The class is looked up on a native thread with no cached loader. It is
    // a local reference, valid for this factory call; the GlobalRef that
    // survives is the codec object, not the class.
    Ok(JClass::from(class))
}

fn jni_error(env: &mut JNIEnv, what: &str, e: impl std::fmt::Display) -> Error {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    Error::Codec(format!("Android {what}: {e}"))
}

/// A hardware MediaCodec H.264 encoder.
pub struct MediaCodecEncoder {
    vm: JavaVM,
    codec: GlobalRef,
    width: usize,
    height: usize,
    fps: f32,
    timestamp: i64,
    i420: Vec<u8>,
    normalized: Vec<u8>,
    pending_idr: bool,
}

impl MediaCodecEncoder {
    pub fn new(width: usize, height: usize, fps: f32, bitrate: u32) -> Result<Self, Error> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(Error::Codec(format!(
                "MediaCodec encoder needs even dimensions, got {width}x{height}"
            )));
        }
        let vm = android_vm()?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|e| Error::Codec(format!("attach Android encoder: {e}")))?;
        let class = codec_class(&mut env, ndk_context::android_context().context())?;
        let result = env
            .call_static_method(
                &class,
                "createEncoder",
                "(IIII)Ldev/parley/app/HardwareEncoder;",
                &[
                    JValue::Int(width as i32),
                    JValue::Int(height as i32),
                    JValue::Int(bitrate as i32),
                    JValue::Int(fps.round().max(1.0) as i32),
                ],
            )
            .map_err(|e| jni_error(&mut env, "create encoder", e))?;
        let object = result
            .l()
            .map_err(|e| jni_error(&mut env, "create encoder result", e))?;
        if object.is_null() {
            return Err(Error::Codec(
                "no hardware MediaCodec H.264 encoder accepted this format".into(),
            ));
        }
        let codec = env
            .new_global_ref(&object)
            .map_err(|e| jni_error(&mut env, "retain encoder", e))?;
        drop(env);
        Ok(Self {
            vm,
            codec,
            width,
            height,
            fps,
            timestamp: 0,
            i420: Vec::new(),
            normalized: Vec::new(),
            pending_idr: true,
        })
    }

    fn frame_duration_us(&self) -> i64 {
        (1_000_000.0 / f64::from(self.fps.max(1.0))).round() as i64
    }

    fn encode_frame(&mut self, keyframe: bool) -> Result<bool, Error> {
        // A null result means the codec has not produced an access unit yet.
        // Clear the previous frame first so the caller cannot mistake stale
        // bytes for output from this attempt.
        self.normalized.clear();
        let mut env = self
            .vm
            .attach_current_thread()
            .map_err(|e| Error::Codec(format!("attach Android encoder: {e}")))?;
        let size = self.i420.len() as i32;
        let buffer = unsafe {
            env.new_direct_byte_buffer(self.i420.as_mut_ptr(), self.i420.len())
                .map_err(|e| jni_error(&mut env, "I420 buffer", e))?
        };
        let buffer_obj: &JObject = buffer.as_ref();
        let result = env
            .call_method(
                self.codec.as_obj(),
                "encode",
                "(Ljava/nio/ByteBuffer;IJZ)[B",
                &[
                    JValue::Object(buffer_obj),
                    JValue::Int(size),
                    JValue::Long(self.timestamp),
                    JValue::Bool(keyframe as u8),
                ],
            )
            .map_err(|e| jni_error(&mut env, "encode", e))?;
        self.timestamp = self.timestamp.wrapping_add(self.frame_duration_us());
        let array = result
            .l()
            .map_err(|e| jni_error(&mut env, "encode result", e))?;
        if array.is_null() {
            return Ok(false);
        }
        let array = JByteArray::from(array);
        let bytes = env
            .convert_byte_array(&array)
            .map_err(|e| jni_error(&mut env, "encode bytes", e))?;
        let keyframe = h264_to_annexb(&bytes, &mut self.normalized);
        Ok(if self.normalized.is_empty() {
            false
        } else {
            keyframe
        })
    }
}

impl VideoEncoder for MediaCodecEncoder {
    fn codec(&self) -> CodecId {
        CodecId::H264
    }

    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn encode_bgra(&mut self, bgra: &[u8]) -> Result<(Vec<u8>, bool), Error> {
        let mut out = Vec::new();
        let keyframe =
            self.encode_bgra_strided(bgra, self.width, self.height, self.width * 4, &mut out)?;
        Ok((out, keyframe))
    }

    fn encode_bgra_strided(
        &mut self,
        bgra: &[u8],
        width: usize,
        height: usize,
        stride: usize,
        out: &mut Vec<u8>,
    ) -> Result<bool, Error> {
        if width != self.width || height != self.height {
            return Err(Error::Codec(format!(
                "MediaCodec encoder is {}x{}, got {width}x{height}",
                self.width, self.height
            )));
        }
        if stride < width * 4 || bgra.len() < stride * height.saturating_sub(1) + width * 4 {
            return Err(Error::Codec(
                "MediaCodec encoder got a truncated frame".into(),
            ));
        }
        let tight;
        let source = if stride == width * 4 && bgra.len() >= width * height * 4 {
            &bgra[..width * height * 4]
        } else {
            let mut copied = Vec::with_capacity(width * height * 4);
            for y in 0..height {
                let start = y * stride;
                copied.extend_from_slice(&bgra[start..start + width * 4]);
            }
            tight = copied;
            &tight
        };
        bgra_to_i420_into(source, width, height, &mut self.i420)?;
        let keyframe = self.encode_frame(self.pending_idr)?;
        if keyframe {
            self.pending_idr = false;
        }
        out.clear();
        out.extend_from_slice(&self.normalized);
        Ok(keyframe && !out.is_empty())
    }

    fn force_keyframe(&mut self) {
        self.pending_idr = true;
    }

    fn set_bitrate(&mut self, bps: u32) {
        let Ok(mut env) = self.vm.attach_current_thread() else {
            return;
        };
        let _ = env.call_method(
            self.codec.as_obj(),
            "setBitrate",
            "(I)V",
            &[JValue::Int(bps as i32)],
        );
    }
}

impl Drop for MediaCodecEncoder {
    fn drop(&mut self) {
        if let Ok(mut env) = self.vm.attach_current_thread() {
            let _ = env.call_method(self.codec.as_obj(), "close", "()V", &[]);
        }
    }
}

/// A hardware MediaCodec H.264 decoder.
pub struct MediaCodecDecoder {
    vm: JavaVM,
    codec: GlobalRef,
    width: usize,
    height: usize,
    timestamp: i64,
}

impl MediaCodecDecoder {
    pub fn new() -> Result<Self, Error> {
        let vm = android_vm()?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|e| Error::Codec(format!("attach Android decoder: {e}")))?;
        let class = codec_class(&mut env, ndk_context::android_context().context())?;
        let result = env
            .call_static_method(
                &class,
                "createDecoder",
                "(II)Ldev/parley/app/HardwareDecoder;",
                &[JValue::Int(1920), JValue::Int(1080)],
            )
            .map_err(|e| jni_error(&mut env, "create decoder", e))?;
        let object = result
            .l()
            .map_err(|e| jni_error(&mut env, "create decoder result", e))?;
        if object.is_null() {
            return Err(Error::Codec(
                "no hardware MediaCodec H.264 decoder accepted this format".into(),
            ));
        }
        let codec = env
            .new_global_ref(&object)
            .map_err(|e| jni_error(&mut env, "retain decoder", e))?;
        drop(env);
        Ok(Self {
            vm,
            codec,
            width: 1920,
            height: 1080,
            timestamp: 0,
        })
    }

    pub fn decode(
        &mut self,
        frame: &[u8],
        rgba: bool,
    ) -> Result<Option<(Vec<u8>, usize, usize)>, Error> {
        let mut env = self
            .vm
            .attach_current_thread()
            .map_err(|e| Error::Codec(format!("attach Android decoder: {e}")))?;
        let mut frame_copy = frame.to_vec();
        let buffer = unsafe {
            env.new_direct_byte_buffer(frame_copy.as_mut_ptr(), frame_copy.len())
                .map_err(|e| jni_error(&mut env, "decoder input buffer", e))?
        };
        let buffer_obj: &JObject = buffer.as_ref();
        let result = env
            .call_method(
                self.codec.as_obj(),
                "decode",
                "(Ljava/nio/ByteBuffer;J)[B",
                &[JValue::Object(buffer_obj), JValue::Long(self.timestamp)],
            )
            .map_err(|e| jni_error(&mut env, "decode", e))?;
        self.timestamp = self.timestamp.wrapping_add(1_000_000 / 60);
        let array = result
            .l()
            .map_err(|e| jni_error(&mut env, "decode result", e))?;
        if array.is_null() {
            return Ok(None);
        }
        let array = JByteArray::from(array);
        let bytes = env
            .convert_byte_array(&array)
            .map_err(|e| jni_error(&mut env, "decoded bytes", e))?;
        if bytes.len() < 8 {
            return Ok(None);
        }
        let width = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]).max(0) as usize;
        let height = i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]).max(0) as usize;
        if width == 0 || height == 0 {
            return Ok(None);
        }
        let y_len = width * height;
        // MediaCodec crops to the display rectangle, which is allowed to be
        // odd; `Image` exposes ceil(w/2) x ceil(h/2) chroma planes.
        let chroma = width.div_ceil(2) * height.div_ceil(2);
        if bytes.len() < 8 + y_len + chroma * 2 {
            return Err(Error::Codec(format!(
                "MediaCodec decoder returned {} bytes for {width}x{height}",
                bytes.len()
            )));
        }
        self.width = width;
        self.height = height;
        let y = &bytes[8..8 + y_len];
        let u = &bytes[8 + y_len..8 + y_len + chroma];
        let v = &bytes[8 + y_len + chroma..8 + y_len + chroma * 2];
        Ok(Some((
            i420_to_rgb(y, u, v, width, height, rgba),
            width,
            height,
        )))
    }
}

impl Drop for MediaCodecDecoder {
    fn drop(&mut self) {
        if let Ok(mut env) = self.vm.attach_current_thread() {
            let _ = env.call_method(self.codec.as_obj(), "close", "()V", &[]);
        }
    }
}

// Both wrappers are used from one media thread at a time. The JavaVM and the
// global reference are Send; MediaCodec itself is created and used by that
// same thread.
unsafe impl Send for MediaCodecEncoder {}
unsafe impl Send for MediaCodecDecoder {}
