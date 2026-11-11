// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A Mac's own encoder and decoder, through VideoToolbox: the backend of a
//! film's encodes on macOS, as NVENC and NVDEC are on an NVIDIA card.
//!
//! - [`encode`]: pictures in, one session, a film out laid out for a viewer
//!   (`tuile_mp4::film`: faststart, BT.709 said, composition offsets for
//!   reordered frames). H.264 High, HEVC Main or Main10.
//! - [`VtFilm`]: a render's [`Sink`] on [`encode`] — H.264, or the HEVC
//!   Main10 mezzanine the final stage reads.
//! - [`Decoder`]: a film's samples back to pictures (NV12, or P010 for a
//!   ten-bit stream), in the order they are shown.
//! - [`compress_profile`]: the final stage on a Mac, decoder to encoder.
//!
//! What a session is asked comes from a [`Spec`] — the profile's, shared
//! with every backend. What VideoToolbox does not offer is not faked: it
//! has no lookahead, no adaptive quantisation and no B-frame count to set
//! (reordering is on or off, the encoder places them), and no per-picture
//! multipass; those fields of a spec are read by NVENC only. Its constant
//! quality is a number from 0 to 1 ([`quality`]), not NVENC's scale, and
//! it holds no ceiling beside it.
//!
//! The system's frameworks are reached through the `objc2-*` bindings.

#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut, NonNull};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Instant;

use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    kCMSampleAttachmentKey_NotSync, kCMTimeInvalid, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
};
use objc2_core_video::{
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVPixelFormatType_420YpCbCr8Planar,
    CVImageBuffer, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AllowOpenGOP,
    kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_ColorPrimaries,
    kVTCompressionPropertyKey_DataRateLimits, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_Quality,
    kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_TransferFunction,
    kVTCompressionPropertyKey_YCbCrMatrix, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_High_AutoLevel, kVTProfileLevel_HEVC_Main10_AutoLevel,
    kVTProfileLevel_HEVC_Main_AutoLevel, VTCompressionSession, VTDecodeFrameFlags,
    VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord, VTDecompressionSession,
    VTEncodeInfoFlags, VTSessionSetProperty,
};
use tuile_mp4::film::{FilmWriter, Written};

use crate::compress::Stream;
use crate::encode::{node_spec, Frame, Input, NodeCodec, Out, Rate, Spec};
use crate::sink::Sink;
use crate::Error;

type Status = i32;

/// Pictures waiting for the encoder before the render waits for it.
const WAITING: usize = 4;

fn check(status: Status, what: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("VideoToolbox: {what} failed ({status})"))
    }
}

/// VideoToolbox's constant quality, 0 (smallest) to 1 (best), for a
/// profile's quality on NVENC's scale (`targetQuality`, 1 best to 51) and
/// the codec it encodes: the point where the Mac's encoder lands a film at
/// the size an L4's NVENC gives the same profile, measured on one
/// production run (720×1280, 4266 frames, the L4's own mezzanine decoded
/// on the Mac; `docs/explorations/compression-pipeline.md` §9.2). H.264 at
/// `targetQuality` 24 is 0.748; HEVC at 21 is 0.737 and at 17 (the
/// mezzanine) 0.809, which sets the slope for both.
pub fn quality(out: Out, nvenc: u8) -> f64 {
    let (anchor, at) = match out {
        Out::H264 => (24.0, 0.748),
        Out::Hevc | Out::Av1 => (21.0, 0.737),
    };
    let q = f64::from(nvenc.clamp(1, 51));
    (at - (q - anchor) * 0.018).clamp(0.0, 1.0)
}

/// The pixel format VideoToolbox takes for pictures handed as `input`.
fn pixel_format(input: Input) -> u32 {
    match input {
        Input::Iyuv => kCVPixelFormatType_420YpCbCr8Planar,
        Input::Nv12 => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        Input::P010 => kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
    }
}

/// Each plane of a picture handed as `input`: bytes a row, and rows.
fn planes(input: Input, (width, height): (usize, usize)) -> Vec<(usize, usize)> {
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    match input {
        Input::Iyuv => vec![(width, height), (cw, ch), (cw, ch)],
        Input::Nv12 => vec![(width, height), (2 * cw, ch)],
        Input::P010 => vec![(2 * width, height), (4 * cw, ch)],
    }
}

/// A picture buffer of `input`'s format holding `tight` (rows packed).
fn picture_buffer(
    input: Input,
    size: (usize, usize),
    tight: &[u8],
) -> Result<CFRetained<CVPixelBuffer>, String> {
    if tight.len() != input.bytes(size) {
        return Err(format!(
            "a {}×{} {input:?} picture is {} bytes, not {}",
            size.0,
            size.1,
            input.bytes(size),
            tight.len()
        ));
    }
    // SAFETY: the buffer is created, filled within its planes' bounds
    // while locked, and handed on retained.
    unsafe {
        let mut out: *mut CVPixelBuffer = null_mut();
        check(
            CVPixelBufferCreate(
                None,
                size.0,
                size.1,
                pixel_format(input),
                None,
                NonNull::from(&mut out),
            ),
            "a picture buffer",
        )?;
        let buffer = CFRetained::from_raw(NonNull::new(out).ok_or("no picture buffer")?);
        check(
            CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()),
            "locking a picture",
        )?;
        let mut from = 0usize;
        for (plane, (row, rows)) in planes(input, size).into_iter().enumerate() {
            let to = CVPixelBufferGetBaseAddressOfPlane(&buffer, plane).cast::<u8>();
            let stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer, plane);
            for r in 0..rows {
                std::ptr::copy_nonoverlapping(
                    tight.as_ptr().add(from + r * row),
                    to.add(r * stride),
                    row,
                );
            }
            from += row * rows;
        }
        CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::empty());
        Ok(buffer)
    }
}

/// A picture's planes, rows packed: `input`'s layout.
///
/// # Safety
///
/// `buffer` is a 4:2:0 picture of `size` in `input`'s format.
unsafe fn tight_of(
    buffer: &CVPixelBuffer,
    input: Input,
    size: (usize, usize),
) -> Result<Vec<u8>, String> {
    unsafe {
        let read_only = CVPixelBufferLockFlags::ReadOnly;
        check(
            CVPixelBufferLockBaseAddress(buffer, read_only),
            "locking a decoded picture",
        )?;
        let mut out = Vec::with_capacity(input.bytes(size));
        for (plane, (row, rows)) in planes(input, size).into_iter().enumerate() {
            let from = CVPixelBufferGetBaseAddressOfPlane(buffer, plane).cast::<u8>();
            let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, plane);
            if from.is_null() || stride < row {
                CVPixelBufferUnlockBaseAddress(buffer, read_only);
                return Err("a decoded picture of another layout".into());
            }
            for r in 0..rows {
                out.extend_from_slice(std::slice::from_raw_parts(from.add(r * stride), row));
            }
        }
        CVPixelBufferUnlockBaseAddress(buffer, read_only);
        Ok(out)
    }
}

/// What the encoder hands back for one picture.
struct Unit {
    /// Length-prefixed NAL units, four bytes each: an mp4 sample.
    data: Vec<u8>,
    key: bool,
    shown: u64,
    /// On a key frame, the stream's parameter sets (VPS, SPS, PPS for
    /// HEVC; SPS, PPS for H.264).
    sets: Vec<Vec<u8>>,
}

/// The parameter sets of a format description, and the length of the NAL
/// length prefix its samples use.
fn parameter_sets(
    description: &CMFormatDescription,
    hevc: bool,
) -> Result<(Vec<Vec<u8>>, i32), String> {
    let (mut sets, mut prefix, mut count) = (Vec::new(), 0i32, 1usize);
    let mut index = 0usize;
    while index < count {
        let (mut set, mut size) = (null::<u8>(), 0usize);
        // SAFETY: the out pointers are valid, and the set that comes back is
        // the description's own, copied before it is let go.
        let status = unsafe {
            if hevc {
                CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                    description,
                    index,
                    &mut set,
                    &mut size,
                    &mut count,
                    &mut prefix,
                )
            } else {
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    description,
                    index,
                    &mut set,
                    &mut size,
                    &mut count,
                    &mut prefix,
                )
            }
        };
        if status != 0 || set.is_null() {
            return Err(format!(
                "VideoToolbox: parameter set {index} unread ({status})"
            ));
        }
        // SAFETY: `size` bytes at `set`, the description's.
        sets.push(unsafe { std::slice::from_raw_parts(set, size) }.to_vec());
        index += 1;
    }
    Ok((sets, prefix))
}

/// What a sample buffer holds: the picture's bytes, whether a decoder can
/// start from it, the frame it is shown as, and on such a picture the
/// parameter sets.
unsafe fn unit_of(sample: &CMSampleBuffer, hevc: bool) -> Result<Unit, String> {
    unsafe {
        let block = sample.data_buffer().ok_or("a picture without bytes")?;
        let mut data = vec![0u8; block.data_length()];
        let to = NonNull::new(data.as_mut_ptr().cast::<c_void>()).ok_or("an empty picture")?;
        if block.copy_data_bytes(0, data.len(), to) != 0 {
            return Err("a picture's bytes could not be read".into());
        }
        // A sync sample is one that does not say it is not.
        let key = match sample.sample_attachments_array(false) {
            Some(attachments) if attachments.count() > 0 => {
                let first = attachments.value_at_index(0).cast::<CFDictionary>();
                let not_sync: *const CFString = kCMSampleAttachmentKey_NotSync;
                !(*first).contains_ptr_key(not_sync.cast())
            }
            _ => true,
        };
        let pts = sample.presentation_time_stamp();
        let shown = u64::try_from(pts.value).map_err(|_| "a picture shown before the film")?;
        let sets = if key {
            let description = sample
                .format_description()
                .ok_or("a picture without a format")?;
            let (sets, prefix) = parameter_sets(&description, hevc)?;
            if prefix != 4 {
                return Err("NAL lengths are not four bytes".into());
            }
            sets
        } else {
            Vec::new()
        };
        Ok(Unit {
            data,
            key,
            shown,
            sets,
        })
    }
}

/// What the encoder's callback is handed.
struct Context {
    hevc: bool,
    units: Sender<Result<Unit, String>>,
}

/// Called by the encoder, on a thread of its own, once per picture, in the
/// order pictures are decoded.
unsafe extern "C-unwind" fn encoded(
    context: *mut c_void,
    _: *mut c_void,
    status: Status,
    _: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: `context` is the one boxed by `open`, alive until the
    // session is invalidated, after which no call arrives.
    let context = unsafe { &*(context as *const Context) };
    let unit = if status != 0 || sample.is_null() {
        Err(format!("a picture was not encoded ({status})"))
    } else {
        // SAFETY: `sample` is valid for the length of this call, and
        // everything read from it is copied before returning.
        unsafe { unit_of(&*sample, context.hevc) }
    };
    let _ = context
        .units
        .send(unit.map_err(|why| format!("VideoToolbox: {why}")));
}

/// One compression session and its callback's context.
struct Session {
    session: CFRetained<VTCompressionSession>,
    context: *mut Context,
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the session is ours; once invalidated the encoder calls
        // back no more, and only then is its context freed.
        unsafe { self.session.invalidate() };
        drop(unsafe { Box::from_raw(self.context) });
    }
}

/// A session opened for `spec`, and where its pictures come back.
fn open(spec: &Spec) -> Result<(Session, Receiver<Result<Unit, String>>), String> {
    let hevc = match spec.out {
        Out::H264 => false,
        Out::Hevc => true,
        Out::Av1 => return Err("VideoToolbox encodes no AV1".into()),
    };
    if !hevc && spec.bit_depth > 8 {
        return Err("VideoToolbox: H.264 is encoded in 8 bits".into());
    }
    if spec.input.bit_depth() != spec.bit_depth {
        return Err(format!(
            "VideoToolbox: a {}-bit stream from {}-bit pictures: convert them first",
            spec.bit_depth,
            spec.input.bit_depth()
        ));
    }
    let (units, received) = channel();
    let context = Box::into_raw(Box::new(Context { hevc, units }));
    let (width, height) = spec.size;
    let codec = if hevc {
        kCMVideoCodecType_HEVC
    } else {
        kCMVideoCodecType_H264
    };
    // SAFETY: calls into the system's API with valid arguments; the
    // session is released when its `CFRetained` is dropped.
    let session = unsafe {
        let mut out: *mut VTCompressionSession = null_mut();
        let status = VTCompressionSession::create(
            None,
            width as i32,
            height as i32,
            codec,
            None,
            None,
            None,
            Some(encoded),
            context.cast(),
            NonNull::from(&mut out),
        );
        if status != 0 || out.is_null() {
            drop(Box::from_raw(context));
            return Err(format!(
                "VideoToolbox: opening a {:?} encoder failed ({status})",
                spec.out
            ));
        }
        Session {
            session: CFRetained::from_raw(NonNull::new_unchecked(out)),
            context,
        }
    };
    let fps = spec.fps.max(1);
    let yes: &CFType = CFBoolean::new(true);
    let no: &CFType = CFBoolean::new(false);
    // SAFETY: the system's constants.
    let profile: &CFType = unsafe {
        match (hevc, spec.bit_depth > 8) {
            (false, _) => kVTProfileLevel_H264_High_AutoLevel,
            (true, true) => kVTProfileLevel_HEVC_Main10_AutoLevel,
            (true, false) => kVTProfileLevel_HEVC_Main_AutoLevel,
        }
    };
    // A key frame is forced at every GOP's first frame (`encode`); the
    // encoder's own interval is left wider than that, or with frames
    // reordered it places its key a B-group early.
    let interval = CFNumber::new_i32(spec.gop.saturating_mul(2).min(i32::MAX as u32) as i32);
    let interval_seconds = CFNumber::new_f64(f64::from(2 * spec.gop) / f64::from(fps));
    let per_second = CFNumber::new_i32(fps as i32);
    // A constant quality, or a mean with its ceiling: so many bytes in any
    // one second. Not both — measured on an M2, a session given a ceiling
    // ignores the quality it is asked and holds a mean of its own — so a
    // constant-quality film has no ceiling here, and its whole size is
    // what the profile's cap holds (`crate::compress::deliver`).
    // SAFETY: the system's constants.
    let (rate_key, rate_value, ceiling): (&CFString, CFRetained<CFNumber>, Option<u32>) = unsafe {
        match spec.rate {
            Rate::Quality { quality: q, .. } => (
                kVTCompressionPropertyKey_Quality,
                CFNumber::new_f64(quality(spec.out, q)),
                None,
            ),
            Rate::Vbr { average, max } => (
                kVTCompressionPropertyKey_AverageBitRate,
                CFNumber::new_i32(average.min(i32::MAX as u32) as i32),
                Some(max.max(average)),
            ),
        }
    };
    let limits = ceiling.map(|ceiling| {
        CFArray::from_retained_objects(&[
            CFNumber::new_f64(f64::from(ceiling) / 8.0),
            CFNumber::new_f64(1.0),
        ])
    });
    let reorder = if spec.b_frames > 0 { yes } else { no };
    // SAFETY: the system's constants, present on any macOS that has
    // VideoToolbox; values of the types the keys take.
    unsafe {
        let required: [(&CFString, &CFType); 10] = [
            // Not a live stream: quality before latency.
            (kVTCompressionPropertyKey_RealTime, no),
            (kVTCompressionPropertyKey_AllowFrameReordering, reorder),
            (kVTCompressionPropertyKey_ProfileLevel, profile),
            (rate_key, &rate_value),
            (kVTCompressionPropertyKey_MaxKeyFrameInterval, &interval),
            (
                kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                &interval_seconds,
            ),
            (kVTCompressionPropertyKey_ExpectedFrameRate, &per_second),
            // What the planes are: BT.709, as the render converts them.
            (
                kVTCompressionPropertyKey_ColorPrimaries,
                kCVImageBufferColorPrimaries_ITU_R_709_2,
            ),
            (
                kVTCompressionPropertyKey_TransferFunction,
                kCVImageBufferTransferFunction_ITU_R_709_2,
            ),
            (
                kVTCompressionPropertyKey_YCbCrMatrix,
                kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            ),
        ];
        for (key, value) in required {
            let status = VTSessionSetProperty(&session.session, key, Some(value));
            if status != 0 {
                return Err(format!("VideoToolbox: setting {key} failed ({status})"));
            }
        }
        if let Some(limits) = &limits {
            check(
                VTSessionSetProperty(
                    &session.session,
                    kVTCompressionPropertyKey_DataRateLimits,
                    Some(limits),
                ),
                "the ceiling",
            )?;
        }
        if hevc {
            // Each key frame an IDR: a closed GOP, where a join or a seek
            // can start. H.264's key frames always are.
            check(
                VTSessionSetProperty(
                    &session.session,
                    kVTCompressionPropertyKey_AllowOpenGOP,
                    Some(no),
                ),
                "closing the GOPs",
            )?;
        }
        // Quality before speed, where the encoder has the choice.
        let _ = VTSessionSetProperty(
            &session.session,
            kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
            Some(no),
        );
        check(session.session.prepare_to_encode_frames(), "preparing")?;
    }
    Ok((session, received))
}

/// Encodes `frames` as `spec` says into a film at `path`. Returns what was
/// written and the seconds spent.
pub fn encode(
    spec: &Spec,
    frames: impl IntoIterator<Item = Result<Frame, String>>,
    path: &Path,
) -> Result<(Written, f64), String> {
    let began = Instant::now();
    let (session, units) = open(spec)?;
    let fps = spec.fps.max(1) as i32;
    let (width, height) = (
        u16::try_from(spec.size.0).map_err(|e| e.to_string())?,
        u16::try_from(spec.size.1).map_err(|e| e.to_string())?,
    );
    let mut writer: Option<FilmWriter> = None;
    let mut written = 0u64;
    let mut drain = |writer: &mut Option<FilmWriter>| -> Result<(), String> {
        while let Ok(unit) = units.try_recv() {
            let unit = unit?;
            if writer.is_none() {
                let entry = match spec.out {
                    Out::H264 => {
                        let [sps, pps, ..] = unit.sets.as_slice() else {
                            return Err(
                                "VideoToolbox: the first picture came without parameter sets"
                                    .into(),
                            );
                        };
                        tuile_mp4::film::avc1(width, height, sps, pps)
                    }
                    _ => {
                        let sets: Vec<&[u8]> = unit.sets.iter().map(Vec::as_slice).collect();
                        let record =
                            tuile_mp4::spatial::hevc_record(&sets).map_err(|e| e.to_string())?;
                        tuile_mp4::film::hvc1(width, height, &record)
                    }
                }
                .map_err(|e| e.to_string())?;
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                }
                *writer =
                    Some(FilmWriter::create(path, entry, fps as u32).map_err(|e| e.to_string())?);
            }
            if let Some(w) = writer.as_mut() {
                w.push(&unit.data, unit.key, unit.shown)
                    .map_err(|e| e.to_string())?;
                written += 1;
            }
        }
        Ok(())
    };
    // SAFETY: the system's constant.
    let force = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
    let key_frame =
        CFDictionary::<CFString, CFType>::from_slices(&[force], &[CFBoolean::new(true)]);
    let gop = u64::from(spec.gop.max(1));
    let mut sent = 0u64;
    for frame in frames {
        let frame = frame?;
        let buffer = picture_buffer(spec.input, spec.size, &frame.data)?;
        let options = (frame.shown % gop == 0).then(|| key_frame.as_opaque());
        // SAFETY: the session is ours and valid; the buffer is retained by
        // the encoder for as long as it needs it.
        check(
            unsafe {
                session.session.encode_frame(
                    &buffer,
                    CMTime::new(frame.shown as i64, fps),
                    CMTime::new(1, fps),
                    options,
                    null_mut(),
                    null_mut(),
                )
            },
            "encoding a picture",
        )?;
        sent += 1;
        drain(&mut writer)?;
    }
    // SAFETY: the session is ours; this returns once every picture given
    // has been called back.
    check(
        unsafe { session.session.complete_frames(kCMTimeInvalid) },
        "finishing",
    )?;
    drain(&mut writer)?;
    drop(drain);
    drop(session);
    if written != sent {
        return Err(format!(
            "VideoToolbox gave back {written} of {sent} pictures"
        ));
    }
    let film = writer
        .ok_or("a film of no picture")?
        .finish()
        .map_err(|e| e.to_string())?;
    Ok((film, began.elapsed().as_secs_f64()))
}

/// What a render node asks of VideoToolbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VtCodec {
    /// H.264, as a film delivered from the render node itself.
    H264,
    /// HEVC Main10, near-transparent: the mezzanine the final stage reads
    /// (`tuile_farm::compress::Profile::mezzanine`).
    Mezzanine,
}

/// A render's film by the Mac's encoder.
///
/// The encoder runs on its own thread, a few pictures behind the render.
pub struct VtFilm {
    path: PathBuf,
    /// Bits a second; 0 for the profile's by the picture's size.
    bitrate: u32,
    codec: VtCodec,
    pictures: Option<SyncSender<Vec<u8>>>,
    encoder: Option<JoinHandle<Result<f64, String>>>,
    size: (usize, usize),
    seconds: f64,
}

impl VtFilm {
    pub fn at(path: impl Into<PathBuf>, bitrate: u32, codec: VtCodec) -> Self {
        Self {
            path: path.into(),
            bitrate,
            codec,
            pictures: None,
            encoder: None,
            size: (0, 0),
            seconds: 0.0,
        }
    }

    /// The session a film of `size` at `fps` is encoded by: the mezzanine
    /// handed ten bits (the render's eight, made ten exactly), H.264 its
    /// eight.
    fn spec(&self, size: (usize, usize), fps: u32) -> Spec {
        match self.codec {
            VtCodec::Mezzanine => {
                node_spec(NodeCodec::Mezzanine, self.bitrate, Input::P010, size, fps)
            }
            VtCodec::H264 => node_spec(NodeCodec::H264, self.bitrate, Input::Iyuv, size, fps),
        }
    }
}

impl Sink for VtFilm {
    fn open(&mut self, width: u32, height: u32, fps: u32) -> Result<(), Error> {
        let size = (width as usize, height as usize);
        self.size = size;
        let spec = self.spec(size, fps.max(1));
        let (pictures, waiting) = sync_channel::<Vec<u8>>(WAITING);
        let path = self.path.clone();
        self.pictures = Some(pictures);
        let span = crate::sink::encoder_span();
        self.encoder = Some(std::thread::spawn(move || {
            let _in = span.enter();
            let input = spec.input;
            let frames = waiting.into_iter().enumerate().map(|(i, i420)| {
                Ok(Frame {
                    data: match input {
                        Input::P010 => crate::encode::i420_to_p010(&i420, size),
                        _ => i420,
                    },
                    shown: i as u64,
                })
            });
            let (_, seconds) = encode(&spec, frames, &path)?;
            Ok(seconds)
        }));
        Ok(())
    }

    fn wants_i420(&self) -> bool {
        true
    }
    fn wants_rgba(&self) -> bool {
        false
    }

    fn picture(&mut self, _index: u32, _frame: u32, _: &[u8], i420: &[u8]) -> Result<(), Error> {
        let (w, h) = self.size;
        if i420.len() != Input::Iyuv.bytes((w, h)) {
            return Err(format!("a {w}×{h} I420 picture is not {} bytes", i420.len()).into());
        }
        let pictures = self.pictures.as_ref().ok_or("the film was not opened")?;
        if pictures.send(i420.to_vec()).is_err() {
            // The encoder stopped: say why, not that a channel closed.
            self.pictures = None;
            return match self.encoder.take().map(JoinHandle::join) {
                Some(Ok(Err(why))) => Err(why.into()),
                _ => Err("the encoder stopped".into()),
            };
        }
        Ok(())
    }

    fn close(&mut self) -> Result<(), Error> {
        self.pictures = None;
        let encoder = self.encoder.take().ok_or("the film was not opened")?;
        self.seconds = encoder.join().map_err(|_| "the encoder panicked")??;
        Ok(())
    }

    fn spent(&self) -> Vec<(String, f64)> {
        vec![(
            format!(
                "{:?} by the machine's encoder, on its own thread",
                self.codec
            ),
            self.seconds,
        )]
    }
}

/// Pictures the decoder's callback has handed back: each one's
/// presentation time, in the track's ticks, and the picture.
type Decoded = Mutex<Vec<Result<(i64, CFRetained<CVImageBuffer>), String>>>;

/// Called by the decoder once per picture.
unsafe extern "C-unwind" fn decoded(
    context: *mut c_void,
    _: *mut c_void,
    status: Status,
    _: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    pts: CMTime,
    _: CMTime,
) {
    // SAFETY: `context` is the decoder's list, alive as long as its session.
    let out = unsafe { &*(context as *const Decoded) };
    let picture = match NonNull::new(image) {
        // SAFETY: a valid image for this call, retained to be read after.
        Some(image) if status == 0 => Ok((pts.value, unsafe { CFRetained::retain(image) })),
        _ => Err(format!(
            "VideoToolbox: a picture was not decoded ({status})"
        )),
    };
    if let Ok(mut list) = out.lock() {
        list.push(picture);
    }
}

/// A film's video track decoded by the Mac's decoder, its pictures handed
/// in the order they are shown: NV12 for an eight-bit stream, P010 for a
/// ten-bit one.
pub struct Decoder {
    session: CFRetained<VTDecompressionSession>,
    description: CFRetained<CMFormatDescription>,
    pictures: *mut Decoded,
    out: Input,
    size: (usize, usize),
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: once invalidated, the session calls back no more.
        unsafe { self.session.invalidate() };
        drop(unsafe { Box::from_raw(self.pictures) });
    }
}

impl Decoder {
    pub fn of(stream: &Stream) -> Result<Self, String> {
        let size = stream.size().ok_or("a track of no size")?;
        let out = if stream.bit_depth > 8 {
            Input::P010
        } else {
            Input::Nv12
        };
        // Parameter sets only: VPS, SPS and PPS of HEVC (types 32 to 34),
        // SPS and PPS of H.264 (7 and 8).
        let sets: Vec<&[u8]> = stream
            .parameter_sets()
            .iter()
            .map(Vec::as_slice)
            .filter(|unit| match (stream.hevc, unit.first()) {
                (true, Some(b)) => (32..=34).contains(&((b >> 1) & 0x3f)),
                (false, Some(b)) => matches!(b & 0x1f, 7 | 8),
                _ => false,
            })
            .collect();
        let mut pointers: Vec<NonNull<u8>> = sets
            .iter()
            .map(|s| NonNull::new(s.as_ptr().cast_mut()).ok_or("an empty parameter set"))
            .collect::<Result<_, _>>()?;
        let mut sizes: Vec<usize> = sets.iter().map(|s| s.len()).collect();
        if pointers.is_empty() {
            return Err("a stream with no parameter sets".into());
        }
        let length = stream.length_size() as i32;
        // SAFETY: the sets outlive the call, which copies them.
        let description = unsafe {
            let mut desc: *const CMFormatDescription = null();
            let status = if stream.hevc {
                CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    None,
                    pointers.len(),
                    NonNull::new_unchecked(pointers.as_mut_ptr()),
                    NonNull::new_unchecked(sizes.as_mut_ptr()),
                    length,
                    None,
                    NonNull::from(&mut desc),
                )
            } else {
                CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    None,
                    pointers.len(),
                    NonNull::new_unchecked(pointers.as_mut_ptr()),
                    NonNull::new_unchecked(sizes.as_mut_ptr()),
                    length,
                    NonNull::from(&mut desc),
                )
            };
            check(status, "describing the stream")?;
            CFRetained::from_raw(NonNull::new(desc.cast_mut()).ok_or("no stream description")?)
        };
        let pictures: *mut Decoded = Box::into_raw(Box::new(Mutex::new(Vec::new())));
        let format = CFNumber::new_i32(pixel_format(out) as i32);
        // SAFETY: the system's constant.
        let key = unsafe { kCVPixelBufferPixelFormatTypeKey };
        let attributes =
            CFDictionary::<CFString, CFType>::from_slices(&[key], &[&*format as &CFType]);
        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(decoded),
            decompressionOutputRefCon: pictures.cast(),
        };
        // SAFETY: valid arguments; the record is copied by the call.
        let session = unsafe {
            let mut session: *mut VTDecompressionSession = null_mut();
            let status = VTDecompressionSession::create(
                None,
                &description,
                None,
                Some(attributes.as_opaque()),
                &record,
                NonNull::from(&mut session),
            );
            if status != 0 || session.is_null() {
                drop(Box::from_raw(pictures));
                return Err(format!("VideoToolbox: opening a decoder failed ({status})"));
            }
            CFRetained::from_raw(NonNull::new_unchecked(session))
        };
        Ok(Self {
            session,
            description,
            pictures,
            out,
            size,
        })
    }

    /// The layout pictures are handed in.
    pub fn out(&self) -> Input {
        self.out
    }

    /// Every sample of `stream` from `file`, decoded: each picture handed
    /// to `take` in the order it is shown, as frame `n` of the film.
    /// Returns the pictures handed.
    pub fn decode_all(
        &self,
        stream: &Stream,
        file: &mut (impl std::io::Read + std::io::Seek),
        mut take: impl FnMut(Frame) -> Result<(), String>,
    ) -> Result<u64, String> {
        let timescale = stream.timescale() as i32;
        let mut waiting: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        let mut next = 0u64;
        for n in 0..stream.frames() {
            let mut sample = stream.sample(file, n).map_err(|e| e.to_string())?;
            let (dts, pts, duration) = stream.timing(n);
            // SAFETY: the block buffer copies the sample; the sample buffer
            // and the decode are given valid objects, and the decode is
            // synchronous: its pictures are called back before it returns.
            unsafe {
                let mut block: *mut CMBlockBuffer = null_mut();
                check(
                    CMBlockBuffer::create_with_memory_block(
                        None,
                        null_mut(),
                        sample.len(),
                        None,
                        null(),
                        0,
                        sample.len(),
                        objc2_core_media::kCMBlockBufferAssureMemoryNowFlag,
                        NonNull::from(&mut block),
                    ),
                    "a sample's block",
                )?;
                let block = CFRetained::from_raw(NonNull::new(block).ok_or("no block")?);
                check(
                    CMBlockBuffer::replace_data_bytes(
                        NonNull::new_unchecked(sample.as_mut_ptr().cast()),
                        &block,
                        0,
                        sample.len(),
                    ),
                    "a sample's bytes",
                )?;
                let timing = CMSampleTimingInfo {
                    duration: CMTime::new(i64::from(duration), timescale),
                    presentationTimeStamp: CMTime::new(pts, timescale),
                    decodeTimeStamp: CMTime::new(dts, timescale),
                };
                let size = sample.len();
                let mut buffer: *mut CMSampleBuffer = null_mut();
                check(
                    CMSampleBuffer::create_ready(
                        None,
                        Some(&block),
                        Some(&self.description),
                        1,
                        1,
                        &timing,
                        1,
                        &size,
                        NonNull::from(&mut buffer),
                    ),
                    "a sample buffer",
                )?;
                let buffer = CFRetained::from_raw(NonNull::new(buffer).ok_or("no sample buffer")?);
                check(
                    self.session.decode_frame(
                        &buffer,
                        VTDecodeFrameFlags::empty(),
                        null_mut(),
                        null_mut(),
                    ),
                    "decoding a picture",
                )?;
            }
            self.collect(stream, &mut waiting)?;
            while let Some(data) = waiting.remove(&next) {
                take(Frame { data, shown: next })?;
                next += 1;
            }
        }
        // SAFETY: the session is ours.
        check(
            unsafe { self.session.wait_for_asynchronous_frames() },
            "the last pictures",
        )?;
        self.collect(stream, &mut waiting)?;
        while let Some(data) = waiting.remove(&next) {
            take(Frame { data, shown: next })?;
            next += 1;
        }
        if !waiting.is_empty() || next != stream.frames() as u64 {
            return Err(format!(
                "{next} pictures decoded in order of {}, {} out of place",
                stream.frames(),
                waiting.len()
            ));
        }
        Ok(next)
    }

    /// The pictures called back since the last time, by the frame each is
    /// shown as.
    fn collect(&self, stream: &Stream, waiting: &mut BTreeMap<u64, Vec<u8>>) -> Result<(), String> {
        // SAFETY: the list lives as long as the decoder.
        let list = std::mem::take(
            &mut *unsafe { &*self.pictures }
                .lock()
                .map_err(|_| "a poisoned list")?,
        );
        for picture in list {
            let (pts, image) = picture?;
            let shown = stream.shown_at(pts).ok_or("a picture shown at no frame")?;
            // SAFETY: a picture of the format the session was asked for.
            let data = unsafe { tight_of(&image, self.out, self.size)? };
            waiting.insert(shown, data);
        }
        Ok(())
    }
}

/// The film at `joined` delivered by `profile` at `out`: decoded by the
/// Mac's decoder on a thread of its own, dithered to eight bits for an
/// eight-bit profile, encoded in one session; again at the capped rate when
/// a cap is passed (`crate::compress::deliver`). Returns what was written
/// and the encodes it took.
pub fn compress_profile(
    joined: &Path,
    out: &Path,
    profile: &tuile_farm::compress::Profile,
    fps: u32,
) -> Result<(Written, u32), Error> {
    let mut file = std::fs::File::open(joined)?;
    let track =
        tuile_mp4::film::read_track(&mut file)?.ok_or("no video track in the joined film")?;
    let stream = Stream::of(track)?;
    let size = stream.size().ok_or("a track of no size")?;
    let fps = fps.max(1);
    let input = crate::compress::input_for(stream.bit_depth, profile);
    crate::compress::deliver(profile, input, size, fps, stream.frames(), |spec| {
        pass(joined, &stream, out, spec)
    })
}

/// One encode of the whole film from its decode.
fn pass(joined: &Path, stream: &Stream, out: &Path, spec: &Spec) -> Result<Written, Error> {
    let (send, pictures) = sync_channel::<Result<Frame, String>>(8);
    let input = spec.input;
    let size = spec.size;
    std::thread::scope(|scope| -> Result<Written, Error> {
        let decoding = scope.spawn(move || -> Result<(), String> {
            let decoded = (|| -> Result<(), String> {
                let decoder = Decoder::of(stream)?;
                let mut file = std::fs::File::open(joined).map_err(|e| e.to_string())?;
                let from = decoder.out();
                decoder.decode_all(stream, &mut file, |frame| {
                    let data = match (from, input) {
                        (Input::P010, Input::Nv12) => {
                            crate::compress::p016_to_nv12(&frame.data, size)
                        }
                        (a, b) if a == b => frame.data,
                        (a, b) => return Err(format!("{a:?} pictures for a {b:?} encoder")),
                    };
                    send.send(Ok(Frame {
                        data,
                        shown: frame.shown,
                    }))
                    .map_err(|_| "the encoder stopped".to_string())
                })?;
                Ok(())
            })();
            if let Err(e) = &decoded {
                let _ = send.send(Err(e.clone()));
            }
            decoded
        });
        let encoded = encode(spec, pictures, out);
        let decoded = decoding.join().map_err(|_| "the decoder panicked")?;
        // The encoder's failure first: a decoder told the encoder stopped
        // says nothing of why.
        let (written, _) = encoded?;
        decoded?;
        Ok(written)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_farm::compress::Profile;

    /// `frames` pictures of a gradient that moves, I420, `w`×`h`.
    fn moving(w: usize, h: usize, frames: usize) -> Vec<Vec<u8>> {
        (0..frames)
            .map(|f| {
                let mut i420 = Vec::with_capacity(Input::Iyuv.bytes((w, h)));
                for y in 0..h {
                    for x in 0..w {
                        i420.push((32 + (x + 2 * f) % 160 + y / 8) as u8);
                    }
                }
                i420.extend(std::iter::repeat_n(128u8, 2 * (w / 2) * (h / 2)));
                i420
            })
            .collect()
    }

    fn render(film: &mut dyn Sink, w: usize, h: usize, frames: usize) {
        film.open(w as u32, h as u32, 30).expect("open");
        for (i, picture) in moving(w, h, frames).into_iter().enumerate() {
            film.picture(i as u32, i as u32, &[], &picture)
                .expect("picture");
        }
        film.close().expect("close");
    }

    fn track(path: &Path) -> tuile_mp4::film::Track {
        let mut file = std::fs::File::open(path).expect("film");
        tuile_mp4::film::read_track(&mut file)
            .expect("read")
            .expect("track")
    }

    fn faststart(path: &Path) -> bool {
        let bytes = std::fs::read(path).expect("film");
        let at = |kind: &[u8]| bytes.windows(4).position(|w| w == kind);
        matches!((at(b"moov"), at(b"mdat")), (Some(m), Some(d)) if m < d)
    }

    /// The frames a film's key frames are shown as.
    fn keys(t: &tuile_mp4::film::Track) -> Vec<u64> {
        let stream = Stream::of(t.clone()).expect("stream");
        let mut keys: Vec<u64> = t
            .samples
            .iter()
            .enumerate()
            .filter(|(_, s)| s.key)
            .map(|(n, _)| stream.shown_at(stream.timing(n).1).expect("shown"))
            .collect();
        keys.sort_unstable();
        keys
    }

    /// `frames` pictures of noise that drifts, I420: hard to compress, as
    /// a film's ground is.
    fn noisy(w: usize, h: usize, frames: usize) -> Vec<Vec<u8>> {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let field: Vec<u8> = (0..(w + frames) * h)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (16 + state % 220) as u8
            })
            .collect();
        (0..frames)
            .map(|f| {
                let mut i420: Vec<u8> = (0..h)
                    .flat_map(|y| field[y * (w + frames) + f..][..w].to_vec())
                    .collect();
                i420.extend(std::iter::repeat_n(128u8, 2 * (w / 2) * (h / 2)));
                i420
            })
            .collect()
    }

    /// A render on the Mac writes the mezzanine as an L4 does: HEVC Main10,
    /// a key frame a second, every frame, BT.709 said, the index first.
    #[test]
    fn a_render_writes_the_mezzanine_in_ten_bits_by_the_mac_s_encoder() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("m.mp4");
        let mut film = VtFilm::at(&path, 0, VtCodec::Mezzanine);
        render(&mut film, 256, 144, 75);
        let t = track(&path);
        let tuile_mp4::film::Entry::Hvc1(e) = &t.entry else {
            panic!("not HEVC: {:?}", t.entry)
        };
        assert_eq!(e.hvcc.bit_depth_luma_minus8 + 8, 10, "Main10");
        assert_eq!(e.colr, Some(tuile_mp4::film::bt709()));
        assert_eq!(t.samples.len(), 75);
        assert_eq!(keys(&t), [0, 30, 60], "an IDR a second");
        assert!(faststart(&path));
    }

    /// The final stage on the Mac: the mezzanine decoded, its ten bits
    /// dithered to eight, one H.264 session with B-frames — reordered,
    /// every frame, a key frame every two seconds — and back again: the
    /// decoded film's pictures are the rendered ones, within the encoder's
    /// error.
    #[test]
    fn a_mezzanine_is_compressed_to_h264_with_reordered_frames_and_decodes_back() {
        let dir = tempfile::tempdir().expect("dir");
        let mezzanine = dir.path().join("m.mp4");
        let (w, h, frames) = (256, 144, 130);
        render(
            &mut VtFilm::at(&mezzanine, 0, VtCodec::Mezzanine),
            w,
            h,
            frames,
        );
        let out = dir.path().join("web.mp4");
        let (written, passes) =
            compress_profile(&mezzanine, &out, &Profile::h264(), 30).expect("web");
        assert_eq!((written.frames, passes), (frames as u64, 1));
        let t = track(&out);
        assert!(matches!(t.entry, tuile_mp4::film::Entry::Avc1(_)));
        assert_eq!(keys(&t), [0, 60, 120], "an IDR every two seconds");
        assert!(
            t.samples
                .iter()
                .any(|s| s.composition != t.samples[0].composition),
            "B-frames: pictures shown in another order than decoded"
        );
        assert!(faststart(&out));

        // Back to pictures: each luma close to the one rendered.
        let stream = Stream::of(t).expect("stream");
        let decoder = Decoder::of(&stream).expect("decoder");
        let mut file = std::fs::File::open(&out).expect("film");
        let truth = moving(w, h, frames);
        let mut worst = 0.0f64;
        let n = decoder
            .decode_all(&stream, &mut file, |frame| {
                let rendered = &truth[frame.shown as usize][..w * h];
                let err: f64 = frame.data[..w * h]
                    .iter()
                    .zip(rendered)
                    .map(|(&a, &b)| (f64::from(a) - f64::from(b)).abs())
                    .sum::<f64>()
                    / (w * h) as f64;
                worst = worst.max(err);
                Ok(())
            })
            .expect("decoded");
        assert_eq!(n, frames as u64);
        assert!(worst < 3.0, "mean luma error {worst} on the worst frame");
    }

    /// `high` keeps the mezzanine's ten bits: HEVC Main10, a key frame a
    /// second.
    #[test]
    fn a_mezzanine_is_compressed_to_hevc_main10() {
        let dir = tempfile::tempdir().expect("dir");
        let mezzanine = dir.path().join("m.mp4");
        render(
            &mut VtFilm::at(&mezzanine, 0, VtCodec::Mezzanine),
            256,
            144,
            64,
        );
        let out = dir.path().join("high.mp4");
        let (written, _) =
            compress_profile(&mezzanine, &out, &Profile::hevc_main10(), 30).expect("high");
        assert_eq!(written.frames, 64);
        let t = track(&out);
        let tuile_mp4::film::Entry::Hvc1(e) = &t.entry else {
            panic!("not HEVC")
        };
        assert_eq!(e.hvcc.bit_depth_luma_minus8 + 8, 10);
        assert_eq!(keys(&t), [0, 30, 60]);
        assert!(faststart(&out));
    }

    /// The calibration's points: each profile lands where it was measured
    /// to match the L4's size.
    #[test]
    fn nvenc_s_quality_maps_onto_the_points_measured() {
        let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(near(quality(Out::H264, Profile::h264().quality), 0.748));
        assert!(near(
            quality(Out::Hevc, Profile::hevc_main10().quality),
            0.737
        ));
        assert!(near(
            quality(Out::Hevc, Profile::mezzanine().quality),
            0.809
        ));
        assert!(quality(Out::Hevc, 1) <= 1.0 && quality(Out::Hevc, 51) >= 0.0);
    }

    /// A profile's constant quality is what decides the size: the same
    /// film at a finer quality is larger, at a coarser one smaller — and
    /// the profile's own lands between.
    #[test]
    fn the_constant_quality_decides_the_size() {
        let dir = tempfile::tempdir().expect("dir");
        let (w, h) = (320, 240);
        let size_at = |quality: u8| {
            let mut spec = Spec::of(&Profile::h264(), Input::Nv12, (w, h), 30, None);
            if let Rate::Quality { max, .. } = spec.rate {
                spec.rate = Rate::Quality { quality, max };
            }
            let frames = noisy(w, h, 60).into_iter().enumerate().map(|(i, i420)| {
                Ok(Frame {
                    data: crate::encode::i420_to_nv12(&i420, (w, h)),
                    shown: i as u64,
                })
            });
            let path = dir.path().join(format!("q{quality}.mp4"));
            encode(&spec, frames, &path).expect("encoded").0.bytes
        };
        let (fine, own, coarse) = (size_at(12), size_at(Profile::h264().quality), size_at(36));
        assert!(fine > own && own > coarse, "{fine} > {own} > {coarse}");
    }

    /// A film over its profile's cap is encoded again, at the rate that
    /// fits — and fits.
    #[test]
    fn a_film_over_its_cap_is_encoded_again_and_fits() {
        let dir = tempfile::tempdir().expect("dir");
        let mezzanine = dir.path().join("m.mp4");
        let (w, h) = (320, 240);
        let mut film = VtFilm::at(&mezzanine, 0, VtCodec::Mezzanine);
        film.open(w as u32, h as u32, 30).expect("open");
        for (i, picture) in noisy(w, h, 90).into_iter().enumerate() {
            film.picture(i as u32, i as u32, &[], &picture)
                .expect("picture");
        }
        film.close().expect("close");
        let out = dir.path().join("web.mp4");
        let (first, _) = compress_profile(&mezzanine, &out, &Profile::h264(), 30).expect("web");
        let mut capped = Profile::h264();
        capped.size_cap_bytes = Some(first.bytes / 2);
        let (written, passes) = compress_profile(&mezzanine, &out, &capped, 30).expect("capped");
        // VideoToolbox misses a mean by a few per cent on a film of three
        // seconds: asked again, lower, it fits.
        assert!((2..=crate::compress::PASSES).contains(&passes), "{passes}");
        assert!(
            written.bytes <= first.bytes / 2,
            "{} bytes for a cap of {}",
            written.bytes,
            first.bytes / 2
        );
    }

    /// Two eyes rendered by the Mac (MV-HEVC by AVFoundation), joined and
    /// delivered as they are: still a spatial film, now laid out for a
    /// viewer.
    #[test]
    fn a_spatial_film_is_joined_and_delivered_as_it_is() {
        use crate::sink::Eyes;
        let dir = tempfile::tempdir().expect("dir");
        let view = tuile_film::BakedView {
            position: [tuile_core::geo::WGS84_A + 1500.0, 0.0, 0.0],
            direction: [-0.6, 0.0, 0.8],
            up: [0.8, 0.0, 0.6],
            viewport_px: [256.0, 144.0],
            fovy_rad: 0.8,
        };
        let centre = tuile_film::FrameCamera::of(&view, 16.0 / 9.0);
        let stereo = tuile_film::Stereo {
            baseline_m: Some(10.0),
            convergence: tuile_film::Convergence::Distance(2000.0),
            ..tuile_film::Stereo::default()
        };
        let pair = stereo.eyes(&centre);
        let left_camera = centre.eye(&pair, tuile_film::Eye::Left);
        let (w, h) = (256usize, 144usize);
        let mut picture = moving(w, h, 1).remove(0);
        // As the spatial sink is handed its eyes: NV12-sized I420.
        picture.truncate(Input::Iyuv.bytes((w, h)));
        let mut segments = Vec::new();
        for s in 0..2 {
            let path = dir.path().join(format!("seg{s}.mov"));
            let mut film = crate::SpatialFilm::at(&path, 4_000_000);
            film.open_stereo((2 * w as u32, h as u32), (w as u32, h as u32), 30)
                .expect("open");
            for i in 0..30 {
                let eyes = Eyes {
                    left: &picture,
                    right: &picture,
                    pair: &pair,
                    left_camera: &left_camera,
                };
                film.stereo(i, i, &[], &[], &eyes).expect("frame");
            }
            film.close().expect("closed");
            segments.push(path);
        }
        let joined = dir.path().join("joined.mp4");
        assert_eq!(
            tuile_mp4::film::join(&segments, &joined).expect("joined"),
            60
        );
        let out = dir.path().join("spatial.mp4");
        let done =
            crate::compress::compress(&joined, &out, &tuile_farm::compress::Delivery::Copy, 30, 0)
                .expect("delivered");
        assert_eq!((done.frames, done.passes), (60, 0));
        let t = track(&out);
        let tuile_mp4::film::Entry::Hvc1(e) = &t.entry else {
            panic!("not HEVC")
        };
        assert!(e.lhvc.is_some(), "the second eye's layer");
        assert!(faststart(&out));
    }
}
