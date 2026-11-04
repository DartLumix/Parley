//! JNI surface for the Android capture service.
//!
//! Kotlin does the platform work (MediaProjection, Camera2, YUV conversion)
//! and hands RGBA frames to these methods. The frames then enter the same
//! `parley_client::screen` encoder used by Linux and Windows.

use jni::objects::{JByteBuffer, JObject, JString};
use jni::sys::{jboolean, jint};
use jni::JNIEnv;

/// `ShareService.nativePushFrame(width, height, rowStride, buffer)`.
///
/// `buffer` is a direct `ByteBuffer` owned by Kotlin. The JNI call is on
/// the capture thread; the Rust side copies and swizzles into its own
/// tightly packed BGRA buffer before handing it to the encoder.
#[no_mangle]
pub extern "system" fn Java_dev_parley_app_ShareService_nativePushFrame(
    env: JNIEnv,
    _this: JObject,
    width: jint,
    height: jint,
    row_stride: jint,
    buffer: JByteBuffer,
) {
    let Ok(address) = env.get_direct_buffer_address(&buffer) else {
        return;
    };
    let Ok(capacity) = env.get_direct_buffer_capacity(&buffer) else {
        return;
    };
    if address.is_null() || capacity <= 0 {
        return;
    }
    // SAFETY: Kotlin owns the direct buffer for the duration of the call;
    // we only read its capacity bytes and copy them before returning.
    let rgba = unsafe { std::slice::from_raw_parts(address, capacity as usize) };
    parley_client::screen::android_push_frame(
        width as u32,
        height as u32,
        row_stride as usize,
        rgba,
    );
}

/// `ShareService.nativePushYuvFrame(...)`.
///
/// Camera2 gives YUV_420_888 planes, not RGBA. Converting them in Kotlin
/// through per-pixel absolute ByteBuffer accesses was the 5 fps camera
/// bottleneck; the three direct planes now cross JNI once and Rust does
/// the YUV conversion, rotation and BGRA packing in a single pass.
///
/// The plane slices are `slice()` views created by Kotlin, so their base
/// address is the beginning of the plane and their capacity covers the
/// rows and pixel strides passed alongside them.
#[no_mangle]
pub extern "system" fn Java_dev_parley_app_ShareService_nativePushYuvFrame(
    env: JNIEnv,
    _this: JObject,
    width: jint,
    height: jint,
    y_buffer: JByteBuffer,
    y_row_stride: jint,
    y_pixel_stride: jint,
    u_buffer: JByteBuffer,
    u_row_stride: jint,
    u_pixel_stride: jint,
    v_buffer: JByteBuffer,
    v_row_stride: jint,
    v_pixel_stride: jint,
    rotation: jint,
    mirror: jboolean,
) {
    let Ok(y_address) = env.get_direct_buffer_address(&y_buffer) else {
        return;
    };
    let Ok(u_address) = env.get_direct_buffer_address(&u_buffer) else {
        return;
    };
    let Ok(v_address) = env.get_direct_buffer_address(&v_buffer) else {
        return;
    };
    let Ok(y_capacity) = env.get_direct_buffer_capacity(&y_buffer) else {
        return;
    };
    let Ok(u_capacity) = env.get_direct_buffer_capacity(&u_buffer) else {
        return;
    };
    let Ok(v_capacity) = env.get_direct_buffer_capacity(&v_buffer) else {
        return;
    };
    if y_address.is_null()
        || u_address.is_null()
        || v_address.is_null()
        || y_capacity <= 0
        || u_capacity <= 0
        || v_capacity <= 0
    {
        return;
    }
    // SAFETY: Kotlin owns the three direct buffers for the duration of
    // this call. We only read their capacity bytes and copy the converted
    // output before returning.
    let y = unsafe { std::slice::from_raw_parts(y_address, y_capacity as usize) };
    let u = unsafe { std::slice::from_raw_parts(u_address, u_capacity as usize) };
    let v = unsafe { std::slice::from_raw_parts(v_address, v_capacity as usize) };
    parley_client::screen::android_push_yuv_frame(
        width as u32,
        height as u32,
        y,
        y_row_stride as usize,
        y_pixel_stride as usize,
        u,
        u_row_stride as usize,
        u_pixel_stride as usize,
        v,
        v_row_stride as usize,
        v_pixel_stride as usize,
        rotation as u32,
        mirror != 0,
    );
}

/// MediaProjection stopped, the camera disconnected, or the service is
/// being destroyed. The share supervisor ends the encoder and reports a
/// normal share end.
#[no_mangle]
pub extern "system" fn Java_dev_parley_app_ShareService_nativeCaptureEnded(
    _env: JNIEnv,
    _this: JObject,
) {
    parley_client::screen::android_capture_ended();
}

/// The capture service could not start or the platform source failed.
/// The reason travels to the session as `share_failed` instead of a
/// silent `share_ended`.
#[no_mangle]
pub extern "system" fn Java_dev_parley_app_ShareService_nativeCaptureFailed(
    mut env: JNIEnv,
    _this: JObject,
    reason: JString,
) {
    let reason = env
        .get_string(&reason)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "Android capture failed".to_string());
    parley_client::screen::android_capture_failed(reason);
}
