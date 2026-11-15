//! Windows H.264 encode/decode through Media Foundation.
//!
//! Encoding targets a hardware Media Foundation encoder MFT
//! (`MFT_ENUM_FLAG_HARDWARE`) and rejects asynchronous transforms, whose
//! event-driven sample flow is not compatible with Parley's one-call-per-
//! frame encoder contract. A machine with no synchronous hardware MFT falls
//! back to openh264 in `video.rs`; that is a capability fallback, never a
//! silent CPU encoder with a "hardware" label.
//!
//! Decoding prefers a hardware Media Foundation decoder but accepts a
//! software Media Foundation decoder as the next step before openh264: both
//! are native platform decoders and the fallback ordering keeps the stream
//! alive on machines with a codec pack but no decode engine.
//!
//! The MFTs take NV12 and emit H.264. The conversion and Annex-B
//! normalization are shared with the other hardware backends, so a frame
//! reaching the MoQ publisher has the same shape on every OS.

use std::cell::Cell;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::OnceLock;

use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonLowLatency, CODECAPI_AVEncCommonMaxBitRate,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, ICodecAPI, IMFActivate, IMFAttributes, IMFMediaBuffer,
    IMFMediaType, IMFTransform, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
    MF_VERSION, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video,
    MFSTARTUP_FULL, MFStartup, MFT_CATEGORY_VIDEO_DECODER, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG, MFT_ENUM_FLAG_ALL, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_INFO, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnumEx,
    MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    eAVEncCommonRateControlMode_PeakConstrainedVBR, eAVEncH264VProfile_Main,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::{VARIANT_0_0_0, VT_BOOL, VT_UI4, VariantInit};
use windows::core::Interface;

use crate::error::Error;
use crate::video::{
    CodecId, VideoEncoder, bgra_to_nv12_into, h264_has_nal, h264_to_annexb,
    keyframe_interval_frames, nv12_to_rgb,
};

/// MF uses 100 ns units for sample times.
const HNS_PER_SECOND: i64 = 10_000_000;

fn mf_err(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Codec(format!("Media Foundation {what}: {e}"))
}

/// Process-wide Media Foundation startup. A decoder exists per remote
/// sender and encoders can be recreated on a resolution tier change; calling
/// `MFShutdown` from any one object's destructor would tear the platform out
/// from under the others. Start once, keep it for the process lifetime.
static MF_START: OnceLock<Result<(), String>> = OnceLock::new();

thread_local! {
    /// COM initialization is per apartment/thread. The decoder can be
    /// called from a different receiver thread than the one that created it;
    /// initialize each thread once, not once per decoded frame (which leaks
    /// COM initialization references and is measurable in a long call).
    static COM_READY: Cell<bool> = const { Cell::new(false) };
}

fn ensure_mta() {
    COM_READY.with(|ready| {
        if !ready.get() {
            // RPC_E_CHANGED_MODE means an STA was already installed; MF still
            // works there. Do not call CoUninitialize: the thread may outlive
            // this decoder and COM is process-lifetime infrastructure here.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            ready.set(true);
        }
    });
}

pub(crate) unsafe fn init_mf() -> Result<(), Error> {
    ensure_mta();
    // Full startup is required for the H.264 MFTs; lite mode is for
    // codec-free media use. The call is process-wide and happens once, so
    // its cost is irrelevant compared with a missing decoder.
    match MF_START
        .get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| e.to_string()) })
    {
        Ok(()) => Ok(()),
        Err(e) => Err(Error::Codec(format!("Media Foundation startup: {e}"))),
    }
}

unsafe fn activate_first(
    category: windows::core::GUID,
    input: &MFT_REGISTER_TYPE_INFO,
    output: &MFT_REGISTER_TYPE_INFO,
    flags: MFT_ENUM_FLAG,
) -> Result<Option<IMFTransform>, Error> {
    let mut activates: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            category,
            flags,
            Some(input),
            Some(output),
            &mut activates,
            &mut count,
        )
        .map_err(|e| mf_err("enumerate", e))?;
        if count == 0 || activates.is_null() {
            if !activates.is_null() {
                CoTaskMemFree(Some(activates as *const c_void));
            }
            return Ok(None);
        }
        let list = std::slice::from_raw_parts(activates, count as usize);
        let mut chosen = None;
        for activate in list.iter().flatten() {
            let Ok(transform) = activate.ActivateObject::<IMFTransform>() else {
                continue;
            };
            let attrs: IMFAttributes = match transform.cast() {
                Ok(attrs) => attrs,
                Err(_) => continue,
            };
            if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 {
                // Hardware MFTs are commonly asynchronous. Microsoft's
                // documented bridge back to the synchronous ProcessInput /
                // ProcessOutput contract is to unlock the transform before
                // types are set. If the MFT refuses, skip it: driving a
                // still-async MFT as sync would never produce output.
                if attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).is_err() {
                    continue;
                }
            }
            chosen = Some(transform);
            break;
        }
        // MFTEnumEx returns a CoTaskMem array of owned activation references.
        // A borrowed slice does not drop the elements, so release each COM
        // reference explicitly before freeing the array storage; otherwise
        // every enumeration leaks one reference per codec (and keeps its DLL
        // loaded for the process lifetime).
        for activate in list.iter().flatten() {
            drop(ptr::read(activate));
        }
        CoTaskMemFree(Some(activates as *const c_void));
        Ok(chosen)
    }
}

unsafe fn set_video_geometry(
    media_type: &IMFMediaType,
    width: usize,
    height: usize,
    fps: f32,
) -> Result<(), Error> {
    unsafe {
        media_type
            .SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)
            .map_err(|e| mf_err("frame size", e))?;
        let rate = (fps.max(1.0) * 1000.0).round() as u64;
        media_type
            .SetUINT64(&MF_MT_FRAME_RATE, (rate << 32) | 1000)
            .map_err(|e| mf_err("frame rate", e))?;
        media_type
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)
            .map_err(|e| mf_err("pixel aspect ratio", e))?;
        media_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(|e| mf_err("interlace mode", e))?;
    }
    Ok(())
}

unsafe fn make_encoder_output_type(
    width: usize,
    height: usize,
    fps: f32,
    bitrate: u32,
) -> Result<IMFMediaType, Error> {
    unsafe {
        let media_type = MFCreateMediaType().map_err(|e| mf_err("output type", e))?;
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| mf_err("output major type", e))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
            .map_err(|e| mf_err("output subtype", e))?;
        set_video_geometry(&media_type, width, height, fps)?;
        media_type
            .SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
            .map_err(|e| mf_err("average bitrate", e))?;
        media_type
            .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)
            .map_err(|e| mf_err("H.264 profile", e))?;
        // Do NOT pin level 4.2 or MF_MT_ALL_SAMPLES_INDEPENDENT here. A
        // 1440p60 stream needs level 5+, and a fixed level makes the setter
        // reject an otherwise capable hardware encoder. The MFT chooses a
        // conforming level from frame size/rate; zero B-frames is enforced
        // through ICodecAPI below.
        Ok(media_type)
    }
}

fn codec_u32(codec: &ICodecAPI, key: &windows::core::GUID, value: u32) -> Result<(), Error> {
    let mut variant = unsafe { VariantInit() };
    unsafe {
        let inner = &mut *variant.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous = VARIANT_0_0_0 { ulVal: value };
        codec
            .SetValue(key, &variant)
            .map_err(|e| mf_err("codec parameter", e))?;
    }
    Ok(())
}

fn codec_bool(codec: &ICodecAPI, key: &windows::core::GUID, value: bool) -> Result<(), Error> {
    let mut variant = unsafe { VariantInit() };
    unsafe {
        let inner = &mut *variant.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous = VARIANT_0_0_0 {
            boolVal: VARIANT_BOOL(if value { -1 } else { 0 }),
        };
        codec
            .SetValue(key, &variant)
            .map_err(|e| mf_err("codec parameter", e))?;
    }
    Ok(())
}

fn sequence_header(media_type: &IMFMediaType) -> Vec<u8> {
    unsafe {
        let Ok(size) = media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else {
            return Vec::new();
        };
        if size == 0 {
            return Vec::new();
        }
        let mut buffer = vec![0u8; size as usize];
        match media_type.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buffer, None) {
            Ok(()) => parameter_sets_to_annexb(&buffer),
            Err(_) => Vec::new(),
        }
    }
}

/// Normalize `MF_MT_MPEG_SEQUENCE_HEADER` to Annex-B SPS/PPS NAL units.
///
/// Some encoders expose the AVCDecoderConfigurationRecord (the `avcC` box
/// payload, starting with version 1), others already expose start-code
/// delimited parameter sets. Prepending an `avcC` record to an Annex-B IDR
/// would make the group undecodable, so parse it here.
fn parameter_sets_to_annexb(blob: &[u8]) -> Vec<u8> {
    if blob.starts_with(&[0, 0, 1]) || blob.starts_with(&[0, 0, 0, 1]) {
        return blob.to_vec();
    }
    if blob.first() != Some(&1) || blob.len() < 7 {
        return Vec::new();
    }
    let read_len = |at: usize| -> Option<usize> {
        let hi = *blob.get(at)? as usize;
        let lo = *blob.get(at + 1)? as usize;
        Some((hi << 8) | lo)
    };
    let mut out = Vec::new();
    let mut at = 5usize;
    let sps_count = usize::from(blob[at] & 0x1f);
    at += 1;
    for _ in 0..sps_count {
        let Some(len) = read_len(at) else {
            return Vec::new();
        };
        at += 2;
        let Some(end) = at.checked_add(len).filter(|end| *end <= blob.len()) else {
            return Vec::new();
        };
        if len == 0 {
            return Vec::new();
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&blob[at..end]);
        at = end;
    }
    let Some(&pps_count) = blob.get(at) else {
        return Vec::new();
    };
    at += 1;
    for _ in 0..pps_count {
        let Some(len) = read_len(at) else {
            return Vec::new();
        };
        at += 2;
        let Some(end) = at.checked_add(len).filter(|end| *end <= blob.len()) else {
            return Vec::new();
        };
        if len == 0 {
            return Vec::new();
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&blob[at..end]);
        at = end;
    }
    out
}

/// A synchronous hardware Media Foundation H.264 encoder.
pub struct MfEncoder {
    // Manually dropped so the MFT and ICodecAPI references are released
    // deterministically before the wrapper's remaining fields go away.
    // Media Foundation startup is process-wide (see `init_mf`).
    transform: ManuallyDrop<IMFTransform>,
    codec: ManuallyDrop<ICodecAPI>,
    width: usize,
    height: usize,
    bitrate: u32,
    pending_idr: bool,
    parameter_sets: Vec<u8>,
    out_info: MFT_OUTPUT_STREAM_INFO,
    nv12: Vec<u8>,
    normalized: Vec<u8>,
    timestamp: i64,
    frame_duration: i64,
    created_thread: std::thread::ThreadId,
}

impl MfEncoder {
    pub fn new(width: usize, height: usize, fps: f32, bitrate: u32) -> Result<Self, Error> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(Error::Codec(format!(
                "Media Foundation encoder needs even dimensions, got {width}x{height}"
            )));
        }
        unsafe {
            init_mf()?;
            let input = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_NV12,
            };
            let output = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            };
            let transform = activate_first(
                MFT_CATEGORY_VIDEO_ENCODER,
                &input,
                &output,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            )?
            .ok_or_else(|| {
                Error::Codec(
                    "no synchronous hardware H.264 encoder MFT (using software encode)".into(),
                )
            })?;
            let attrs: IMFAttributes = transform.cast().map_err(|e| mf_err("attributes", e))?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            let codec: ICodecAPI = transform.cast().map_err(|e| mf_err("codec API", e))?;
            codec
                .IsSupported(&CODECAPI_AVEncVideoForceKeyFrame)
                .map_err(|e| mf_err("keyframe control", e))?;

            let input_type = MFCreateMediaType().map_err(|e| mf_err("input type", e))?;
            input_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| mf_err("input major type", e))?;
            input_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .map_err(|e| mf_err("input subtype", e))?;
            set_video_geometry(&input_type, width, height, fps)?;
            transform
                .SetInputType(0, &input_type, 0)
                .map_err(|e| mf_err("input type", e))?;

            let output_type = make_encoder_output_type(width, height, fps, bitrate)?;
            transform
                .SetOutputType(0, &output_type, 0)
                .map_err(|e| mf_err("output type", e))?;

            codec_u32(
                &codec,
                &CODECAPI_AVEncCommonRateControlMode,
                eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32,
            )?;
            codec_u32(&codec, &CODECAPI_AVEncCommonMeanBitRate, bitrate)?;
            codec_u32(
                &codec,
                &CODECAPI_AVEncCommonMaxBitRate,
                bitrate.saturating_mul(5) / 3,
            )?;
            codec_u32(
                &codec,
                &CODECAPI_AVEncMPVGOPSize,
                keyframe_interval_frames(fps),
            )?;
            // Parley's H.264 profile is zero-B-frame: a B-frame reorders
            // the stream and adds latency. Hardware encoders commonly
            // default to 1+, so this is not optional.
            codec_u32(&codec, &CODECAPI_AVEncMPVDefaultBPictureCount, 0)?;
            let _ = codec_bool(&codec, &CODECAPI_AVEncCommonLowLatency, true);

            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| mf_err("begin streaming", e))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| mf_err("start of stream", e))?;
            let out_info = transform
                .GetOutputStreamInfo(0)
                .map_err(|e| mf_err("output stream info", e))?;
            // `output_type` is the type WE requested. The encoder may fill
            // in the negotiated type (notably MF_MT_MPEG_SEQUENCE_HEADER)
            // only after SetOutputType, so read the current type back.
            let parameter_sets = transform
                .GetOutputCurrentType(0)
                .ok()
                .map(|current| sequence_header(&current))
                .unwrap_or_default();
            Ok(Self {
                transform: ManuallyDrop::new(transform),
                codec: ManuallyDrop::new(codec),
                width,
                height,
                bitrate,
                pending_idr: true,
                parameter_sets,
                out_info,
                nv12: Vec::new(),
                normalized: Vec::new(),
                timestamp: 0,
                frame_duration: (HNS_PER_SECOND as f64 / fps.max(1.0) as f64).round() as i64,
                created_thread: std::thread::current().id(),
            })
        }
    }

    fn update_output_type(&mut self) -> Result<(), Error> {
        unsafe {
            // A stream change on an encoder normally means the driver picked
            // a different profile/level; take its first output type and keep
            // the new sequence header so the next IDR can carry SPS/PPS.
            for index in 0..8 {
                let Ok(candidate) = self.transform.GetOutputAvailableType(0, index) else {
                    break;
                };
                if self.transform.SetOutputType(0, &candidate, 0).is_ok() {
                    self.parameter_sets = sequence_header(&candidate);
                    self.out_info = self
                        .transform
                        .GetOutputStreamInfo(0)
                        .unwrap_or(self.out_info);
                    return Ok(());
                }
            }
        }
        Err(Error::Codec(
            "Media Foundation encoder offered no usable output type".into(),
        ))
    }

    fn convert_frame(
        &mut self,
        bgra: &[u8],
        width: usize,
        height: usize,
        stride: usize,
    ) -> Result<(), Error> {
        if width == self.width && height == self.height && stride == width * 4 {
            bgra_to_nv12_into(bgra, width, height, &mut self.nv12)
        } else if width == self.width && height == self.height {
            let row = width * 4;
            let mut packed = Vec::with_capacity(row * height);
            for y in 0..height {
                let start = y
                    .checked_mul(stride)
                    .ok_or_else(|| Error::Codec("strided frame overflow".into()))?;
                let Some(src) = bgra.get(start..start + row) else {
                    return Err(Error::Codec("strided frame truncated".into()));
                };
                packed.extend_from_slice(src);
            }
            bgra_to_nv12_into(&packed, width, height, &mut self.nv12)
        } else {
            Err(Error::Codec(format!(
                "Media Foundation encoder is {}x{}, got {width}x{height}",
                self.width, self.height
            )))
        }
    }

    fn submit(&mut self) -> Result<(), Error> {
        unsafe {
            let force_idr = self.pending_idr;
            if force_idr {
                let _ = codec_bool(&self.codec, &CODECAPI_AVEncVideoForceKeyFrame, true);
            }
            let sample = MFCreateSample().map_err(|e| mf_err("sample", e))?;
            let buffer = MFCreateMemoryBuffer(self.nv12.len() as u32)
                .map_err(|e| mf_err("input buffer", e))?;
            let mut dst: *mut u8 = ptr::null_mut();
            buffer
                .Lock(&mut dst, None, None)
                .map_err(|e| mf_err("input buffer lock", e))?;
            ptr::copy_nonoverlapping(self.nv12.as_ptr(), dst, self.nv12.len());
            buffer
                .SetCurrentLength(self.nv12.len() as u32)
                .map_err(|e| mf_err("input buffer length", e))?;
            let _ = buffer.Unlock();
            sample
                .AddBuffer(&buffer)
                .map_err(|e| mf_err("sample buffer", e))?;
            sample
                .SetSampleTime(self.timestamp)
                .map_err(|e| mf_err("sample time", e))?;
            sample
                .SetSampleDuration(self.frame_duration)
                .map_err(|e| mf_err("sample duration", e))?;
            self.timestamp = self.timestamp.wrapping_add(self.frame_duration);
            self.transform
                .ProcessInput(0, &sample, 0)
                .map_err(|e| mf_err("process input", e))?;
            // Only consume the request after the frame was accepted: a
            // rejected input must not silently turn the next frame into a
            // P-frame when the receiver still has no IDR.
            if force_idr {
                self.pending_idr = false;
            }
            Ok(())
        }
    }

    fn take_output(&mut self, out: &mut Vec<u8>) -> Result<bool, Error> {
        out.clear();
        let provides_samples =
            self.out_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        // A compressed keyframe can exceed one byte per pixel (especially
        // at 1440p/4K); a buffer-too-small here loses the IDR. Two bytes
        // per pixel leaves ample headroom without over-allocating 4K.
        let capacity = self
            .out_info
            .cbSize
            .max((self.width * self.height * 2) as u32);
        let sample = if provides_samples {
            None
        } else {
            let sample = unsafe { MFCreateSample().map_err(|e| mf_err("output sample", e))? };
            let buffer =
                unsafe { MFCreateMemoryBuffer(capacity).map_err(|e| mf_err("output buffer", e))? };
            unsafe {
                sample
                    .AddBuffer(&buffer)
                    .map_err(|e| mf_err("output sample buffer", e))?;
            }
            Some(sample)
        };
        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
        };
        match result {
            Ok(()) => {
                let produced = unsafe { ManuallyDrop::take(&mut output.pSample) };
                let _events = unsafe { ManuallyDrop::take(&mut output.pEvents) };
                let Some(produced) = produced else {
                    return Ok(false);
                };
                let buffer: IMFMediaBuffer = unsafe {
                    produced
                        .ConvertToContiguousBuffer()
                        .map_err(|e| mf_err("output buffer", e))?
                };
                let mut data: *mut u8 = ptr::null_mut();
                let mut len = 0u32;
                let keyframe = unsafe {
                    buffer
                        .Lock(&mut data, None, Some(&mut len))
                        .map_err(|e| mf_err("output lock", e))?;
                    let bytes = std::slice::from_raw_parts(data, len as usize);
                    let keyframe = h264_to_annexb(bytes, &mut self.normalized);
                    let _ = buffer.Unlock();
                    if keyframe {
                        // Some encoders publish the sequence header only
                        // once streaming starts. Pick it up on the first
                        // IDR rather than sending a group without SPS/PPS.
                        if self.parameter_sets.is_empty()
                            && let Ok(current) = self.transform.GetOutputCurrentType(0)
                        {
                            self.parameter_sets = sequence_header(&current);
                        }
                        // A keyframe may carry SPS but omit PPS (or vice
                        // versa). Prepend the full set unless both are
                        // already present.
                        if !self.parameter_sets.is_empty()
                            && !(h264_has_nal(&self.normalized, 7)
                                && h264_has_nal(&self.normalized, 8))
                        {
                            out.extend_from_slice(&self.parameter_sets);
                        }
                    }
                    out.extend_from_slice(&self.normalized);
                    keyframe
                };
                if out.is_empty() {
                    return Ok(false);
                }
                Ok(keyframe)
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                let _ = unsafe { ManuallyDrop::take(&mut output.pSample) };
                let _ = unsafe { ManuallyDrop::take(&mut output.pEvents) };
                Ok(false)
            }
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                let _ = unsafe { ManuallyDrop::take(&mut output.pSample) };
                let _ = unsafe { ManuallyDrop::take(&mut output.pEvents) };
                self.update_output_type()?;
                Ok(false)
            }
            Err(e) => {
                let _ = unsafe { ManuallyDrop::take(&mut output.pSample) };
                let _ = unsafe { ManuallyDrop::take(&mut output.pEvents) };
                Err(mf_err("process output", e))
            }
        }
    }
}

// Media Foundation COM interfaces are MTA objects and the encoder is
// created, used and dropped on the share's encoder thread (asserted below).
// The trait requires Send so `Box<dyn VideoEncoder>` can be stored in the
// share state; moving this encoder to another thread is a bug the debug
// assertion catches rather than a supported operation.
unsafe impl Send for MfEncoder {}

impl VideoEncoder for MfEncoder {
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
        debug_assert_eq!(
            std::thread::current().id(),
            self.created_thread,
            "Media Foundation encoder moved across threads"
        );
        if width == 0 || height == 0 || stride < width * 4 {
            return Err(Error::Codec(format!(
                "Media Foundation encoder got invalid {width}x{height} stride {stride}"
            )));
        }
        let needed = stride
            .saturating_mul(height.saturating_sub(1))
            .saturating_add(width.saturating_mul(4));
        if bgra.len() < needed {
            return Err(Error::Codec(format!(
                "Media Foundation encoder got {} bytes, needs {needed}",
                bgra.len()
            )));
        }
        self.convert_frame(bgra, width, height, stride)?;
        self.submit()?;
        self.take_output(out)
    }

    fn force_keyframe(&mut self) {
        self.pending_idr = true;
    }

    fn set_bitrate(&mut self, bps: u32) {
        if bps == 0 || bps == self.bitrate {
            return;
        }
        let max = bps.saturating_mul(5) / 3;
        match codec_u32(&self.codec, &CODECAPI_AVEncCommonMeanBitRate, bps)
            .and_then(|()| codec_u32(&self.codec, &CODECAPI_AVEncCommonMaxBitRate, max))
        {
            Ok(()) => self.bitrate = bps,
            Err(e) => eprintln!("video: Media Foundation bitrate change failed: {e}"),
        }
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(
                windows::Win32::Media::MediaFoundation::MFT_MESSAGE_NOTIFY_END_STREAMING,
                0,
            );
            // `codec` holds a reference to the same transform; release both
            // explicitly rather than relying on field-drop order.
            ManuallyDrop::drop(&mut self.codec);
            ManuallyDrop::drop(&mut self.transform);
            // MF_START is process-wide; see `init_mf`.
        }
    }
}

/// A Media Foundation H.264 decoder, hardware first.
///
/// MTA COM objects are free-threaded; `decode` re-enters the MTA on whatever
/// receiver thread calls it, so moving the wrapper between the hub's reader
/// and display threads is safe. The transform itself is only called under the
/// hub's stream mutex.
pub struct MfDecoder {
    // Released deterministically; Media Foundation startup is process-wide.
    transform: ManuallyDrop<IMFTransform>,
    width: usize,
    height: usize,
    stride: usize,
    /// The decoder advertises a negative default stride for bottom-up NV12.
    bottom_up: bool,
    timestamp: i64,
    frame_duration: i64,
    out_info: MFT_OUTPUT_STREAM_INFO,
    nv12: Vec<u8>,
    rgb: Vec<u8>,
}

impl MfDecoder {
    pub fn new() -> Result<Self, Error> {
        unsafe {
            init_mf()?;
            let input = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            };
            let output = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_NV12,
            };
            let hardware = activate_first(
                MFT_CATEGORY_VIDEO_DECODER,
                &input,
                &output,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            )?;
            let transform = match hardware {
                Some(transform) => transform,
                None => activate_first(
                    MFT_CATEGORY_VIDEO_DECODER,
                    &input,
                    &output,
                    MFT_ENUM_FLAG_ALL,
                )?
                .ok_or_else(|| Error::Codec("no Media Foundation H.264 decoder MFT".into()))?,
            };
            // Media Foundation only honors low-latency mode when it is set
            // before streaming begins.
            if let Ok(attrs) = transform.cast::<IMFAttributes>() {
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }

            let input_type = MFCreateMediaType().map_err(|e| mf_err("decoder input type", e))?;
            input_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| mf_err("decoder input major type", e))?;
            input_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(|e| mf_err("decoder input subtype", e))?;
            // A compressed input type must not carry a guessed frame size:
            // the SPS decides it, and some decoders reject a placeholder
            // that disagrees. Only the major type and H.264 subtype belong
            // in the input type.
            transform
                .SetInputType(0, &input_type, 0)
                .map_err(|e| mf_err("decoder input type", e))?;
            // Try a placeholder output type for decoders that require one
            // before accepting input. A decoder that prefers to negotiate
            // after parsing SPS will reject this; that is not fatal, because
            // the first ProcessOutput then reports a stream change and
            // `update_output_type` takes the real NV12 type.
            if let Ok(output_type) = make_decoder_output_type(1920, 1080, 60.0) {
                let _ = transform.SetOutputType(0, &output_type, 0);
            }
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| mf_err("decoder begin streaming", e))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| mf_err("decoder start of stream", e))?;
            let out_info = transform
                .GetOutputStreamInfo(0)
                .map_err(|e| mf_err("decoder output stream info", e))?;
            Ok(Self {
                transform: ManuallyDrop::new(transform),
                width: 1920,
                height: 1080,
                stride: 1920,
                bottom_up: false,
                timestamp: 0,
                frame_duration: HNS_PER_SECOND / 60,
                out_info,
                nv12: Vec::new(),
                rgb: Vec::new(),
            })
        }
    }

    fn update_output_type(&mut self) -> Result<(), Error> {
        unsafe {
            for index in 0..8 {
                let Ok(candidate) = self.transform.GetOutputAvailableType(0, index) else {
                    break;
                };
                let subtype = candidate.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default();
                if subtype != MFVideoFormat_NV12 {
                    continue;
                }
                if self.transform.SetOutputType(0, &candidate, 0).is_ok() {
                    if let Ok(size) = candidate.GetUINT64(&MF_MT_FRAME_SIZE) {
                        self.width = (size >> 32) as usize;
                        self.height = (size & 0xffff_ffff) as usize;
                    }
                    // MF_MT_DEFAULT_STRIDE is a signed UINT32. A positive
                    // value larger than the visible width means the decoder
                    // kept row padding in its contiguous buffer; a negative
                    // value means the rows are stored bottom-up. Handle both:
                    // copying a bottom-up surface top-down would silently flip
                    // the picture.
                    match candidate
                        .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                        .ok()
                        .map(|s| s as i32)
                    {
                        Some(stride) if stride < 0 => {
                            self.bottom_up = true;
                            self.stride = stride.unsigned_abs() as usize;
                        }
                        Some(stride) if stride > 0 => {
                            self.bottom_up = false;
                            self.stride = (stride as usize).max(self.width);
                        }
                        _ => {
                            self.bottom_up = false;
                            self.stride = self.width;
                        }
                    }
                    if let Ok(rate) = candidate.GetUINT64(&MF_MT_FRAME_RATE) {
                        let fps = (rate >> 32) as f32 * 1000.0 / (rate & 0xffff_ffff).max(1) as f32;
                        if fps >= 1.0 {
                            self.frame_duration =
                                (HNS_PER_SECOND as f64 / fps as f64).round() as i64;
                        }
                    }
                    self.out_info = self
                        .transform
                        .GetOutputStreamInfo(0)
                        .unwrap_or(self.out_info);
                    return Ok(());
                }
            }
        }
        Err(Error::Codec(
            "Media Foundation decoder offered no NV12 output type".into(),
        ))
    }

    pub fn decode(
        &mut self,
        frame: &[u8],
        rgba: bool,
    ) -> Result<Option<(Vec<u8>, usize, usize)>, Error> {
        unsafe {
            // The hub may hand the decoder to a different receiver thread
            // than the one that created it. MTA interfaces are free-threaded,
            // but the calling thread still has to be in the MTA.
            ensure_mta();
            let sample = MFCreateSample().map_err(|e| mf_err("decoder sample", e))?;
            let buffer = MFCreateMemoryBuffer(frame.len() as u32)
                .map_err(|e| mf_err("decoder input buffer", e))?;
            let mut dst: *mut u8 = ptr::null_mut();
            buffer
                .Lock(&mut dst, None, None)
                .map_err(|e| mf_err("decoder input lock", e))?;
            ptr::copy_nonoverlapping(frame.as_ptr(), dst, frame.len());
            buffer
                .SetCurrentLength(frame.len() as u32)
                .map_err(|e| mf_err("decoder input length", e))?;
            let _ = buffer.Unlock();
            sample
                .AddBuffer(&buffer)
                .map_err(|e| mf_err("decoder sample buffer", e))?;
            sample
                .SetSampleTime(self.timestamp)
                .map_err(|e| mf_err("decoder sample time", e))?;
            sample
                .SetSampleDuration(self.frame_duration)
                .map_err(|e| mf_err("decoder sample duration", e))?;
            self.timestamp = self.timestamp.wrapping_add(self.frame_duration);
            self.transform
                .ProcessInput(0, &sample, 0)
                .map_err(|e| mf_err("decoder process input", e))?;

            let provides_samples =
                self.out_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            let capacity = self
                .out_info
                .cbSize
                .max((self.width * self.height * 2) as u32);
            let output_sample = if provides_samples {
                None
            } else {
                let s = MFCreateSample().map_err(|e| mf_err("decoder output sample", e))?;
                let b = MFCreateMemoryBuffer(capacity)
                    .map_err(|e| mf_err("decoder output buffer", e))?;
                s.AddBuffer(&b)
                    .map_err(|e| mf_err("decoder output sample buffer", e))?;
                Some(s)
            };
            let mut output = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(output_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            let result =
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status);
            match result {
                Ok(()) => {
                    let produced = ManuallyDrop::take(&mut output.pSample);
                    let _events = ManuallyDrop::take(&mut output.pEvents);
                    let Some(produced) = produced else {
                        return Ok(None);
                    };
                    let buffer: IMFMediaBuffer = produced
                        .ConvertToContiguousBuffer()
                        .map_err(|e| mf_err("decoder output buffer", e))?;
                    let mut data: *mut u8 = ptr::null_mut();
                    let mut len = 0u32;
                    buffer
                        .Lock(&mut data, None, Some(&mut len))
                        .map_err(|e| mf_err("decoder output lock", e))?;
                    let bytes = std::slice::from_raw_parts(data, len as usize);
                    let decoded = copy_nv12(
                        bytes,
                        &mut self.nv12,
                        self.width,
                        self.height,
                        self.stride,
                        self.bottom_up,
                    )
                    .map(|()| {
                        self.rgb = nv12_to_rgb(&self.nv12, self.width, self.height, rgba);
                        Some((std::mem::take(&mut self.rgb), self.width, self.height))
                    });
                    let _ = buffer.Unlock();
                    decoded
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    let _ = ManuallyDrop::take(&mut output.pSample);
                    let _ = ManuallyDrop::take(&mut output.pEvents);
                    Ok(None)
                }
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let _ = ManuallyDrop::take(&mut output.pSample);
                    let _ = ManuallyDrop::take(&mut output.pEvents);
                    self.update_output_type()?;
                    Ok(None)
                }
                Err(e) => {
                    let _ = ManuallyDrop::take(&mut output.pSample);
                    let _ = ManuallyDrop::take(&mut output.pEvents);
                    Err(mf_err("decoder process output", e))
                }
            }
        }
    }
}

/// Media Foundation's contiguous output can still be longer than the
/// visible NV12 picture. Copy exactly the tight Y+UV payload that the
/// shared converter understands.
fn copy_nv12(
    bytes: &[u8],
    out: &mut Vec<u8>,
    width: usize,
    height: usize,
    stride: usize,
    bottom_up: bool,
) -> Result<(), Error> {
    let stride = stride.max(width);
    let tight = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .map(|bytes| bytes / 2)
        .ok_or_else(|| Error::Codec("Media Foundation decoder size overflow".into()))?;
    let padded = stride
        .checked_mul(height)
        .and_then(|y| y.checked_add(y / 2))
        .ok_or_else(|| Error::Codec("Media Foundation decoder stride overflow".into()))?;
    // A padded buffer has `stride` bytes per Y row and per interleaved UV
    // row. If the driver handed us a tight buffer despite advertising a
    // larger stride, copy it tightly rather than rejecting a usable frame.
    let (row, rows_y) = if bytes.len() >= padded {
        (stride, height)
    } else {
        (width, height)
    };
    let need = if row == width {
        tight
    } else {
        row.checked_mul(rows_y)
            .and_then(|y| y.checked_add(row * (rows_y / 2)))
            .ok_or_else(|| Error::Codec("Media Foundation decoder size overflow".into()))?
    };
    if bytes.len() < need {
        return Err(Error::Codec(format!(
            "Media Foundation decoder produced {} bytes, needs {need}",
            bytes.len()
        )));
    }
    out.clear();
    out.reserve(tight);
    for y in 0..height {
        let src = if bottom_up { height - 1 - y } else { y };
        out.extend_from_slice(&bytes[src * row..src * row + width]);
    }
    let uv = row * height;
    for y in 0..height / 2 {
        let src = if bottom_up { height / 2 - 1 - y } else { y };
        out.extend_from_slice(&bytes[uv + src * row..uv + src * row + width]);
    }
    Ok(())
}

unsafe fn make_decoder_output_type(
    width: usize,
    height: usize,
    fps: f32,
) -> Result<IMFMediaType, Error> {
    unsafe {
        let media_type = MFCreateMediaType().map_err(|e| mf_err("decoder output type", e))?;
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| mf_err("decoder output major type", e))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(|e| mf_err("decoder output subtype", e))?;
        set_video_geometry(&media_type, width, height, fps)?;
        Ok(media_type)
    }
}

unsafe impl Send for MfDecoder {}

impl Drop for MfDecoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(
                windows::Win32::Media::MediaFoundation::MFT_MESSAGE_NOTIFY_END_STREAMING,
                0,
            );
            ManuallyDrop::drop(&mut self.transform);
            // MF_START is process-wide; see `init_mf`.
        }
    }
}
