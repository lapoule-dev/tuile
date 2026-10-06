// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The film as an mp4, H.264 by the machine's own encoder.
//!
//! macOS has a hardware H.264 encoder behind VideoToolbox, and it is two
//! orders of magnitude faster than encoding in software. It is the same
//! codec a browser's encoder gives the same film, written by the same
//! muxer. The system's frameworks are reached through the `objc2-*`
//! bindings.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::{null, null_mut, NonNull};
use std::sync::mpsc::{channel, Receiver, Sender};

use objc2_core_foundation::{
    kCFBooleanFalse, CFDictionary, CFNumber, CFNumberType, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    kCMSampleAttachmentKey_NotSync, kCMTimeInvalid, CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
};
use objc2_core_video::{
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVPixelFormatType_420YpCbCr8Planar, CVPixelBuffer,
    CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_TransferFunction,
    kVTCompressionPropertyKey_YCbCrMatrix, kVTProfileLevel_H264_High_AutoLevel,
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
};
use tuile_mp4::{Muxer, ParameterSets};

use crate::sink::Sink;
use crate::Error;

type Status = i32;

/// `'avc1'`.
const H264: u32 = u32::from_be_bytes(*b"avc1");

/// What the encoder hands back for one picture.
struct Unit {
    /// Length-prefixed NAL units, four bytes each.
    data: Vec<u8>,
    key: bool,
    sets: Option<ParameterSets>,
}

fn check(status: Status, what: &str) -> Result<(), Error> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("VideoToolbox: {what} failed ({status})").into())
    }
}

/// One parameter set of a format description, and the length of the NAL
/// length prefix its samples use.
fn parameter_set(description: &CMFormatDescription, index: usize) -> Option<(Vec<u8>, i32)> {
    let (mut set, mut size, mut prefix) = (null::<u8>(), 0usize, 0i32);
    // SAFETY: the out pointers are valid, and the set that comes back is
    // the description's own, copied before it is let go.
    unsafe {
        let status = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            description,
            index,
            &mut set,
            &mut size,
            null_mut(),
            &mut prefix,
        );
        (status == 0 && !set.is_null())
            .then(|| (std::slice::from_raw_parts(set, size).to_vec(), prefix))
    }
}

/// What a sample buffer holds: the picture's bytes, whether a decoder can
/// start from it, and on such a picture the parameter sets.
unsafe fn unit_of(sample: &CMSampleBuffer) -> Result<Unit, String> {
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
        let sets = if key {
            let description = sample
                .format_description()
                .ok_or("a picture without a format")?;
            match (
                parameter_set(&description, 0),
                parameter_set(&description, 1),
            ) {
                (Some((sps, 4)), Some((pps, _))) => Some(ParameterSets { sps, pps }),
                (Some(_), Some(_)) => return Err("NAL lengths are not four bytes".into()),
                _ => None,
            }
        } else {
            None
        };
        Ok(Unit { data, key, sets })
    }
}

/// Called by the encoder, on a thread of its own, once per picture and in
/// the order pictures went in (frame reordering is off).
unsafe extern "C-unwind" fn encoded(
    context: *mut c_void,
    _: *mut c_void,
    status: Status,
    _: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: `context` is the sender boxed in `open`, alive until the
    // session is invalidated in `end_session`, after which no call arrives.
    let out = unsafe { &*(context as *const Sender<Result<Unit, String>>) };
    let unit = if status != 0 || sample.is_null() {
        Err(format!("a picture was not encoded ({status})"))
    } else {
        // SAFETY: `sample` is valid for the length of this call, and
        // everything read from it is copied before returning.
        unsafe { unit_of(&*sample) }
    };
    let _ = out.send(unit.map_err(|why| format!("VideoToolbox: {why}")));
}

/// The film as an mp4, H.264 by VideoToolbox.
pub struct H264Film {
    path: PathBuf,
    bitrate: u32,
    session: Option<CFRetained<VTCompressionSession>>,
    context: *mut Sender<Result<Unit, String>>,
    units: Option<Receiver<Result<Unit, String>>>,
    muxer: Option<Muxer>,
    size: (usize, usize),
    fps: u32,
    sent: i64,
    written: u64,
    began: Option<std::time::Instant>,
    seconds: f64,
}

impl H264Film {
    pub fn at(path: impl Into<PathBuf>, bitrate: u32) -> Self {
        Self {
            path: path.into(),
            bitrate,
            session: None,
            context: null_mut(),
            units: None,
            muxer: None,
            size: (0, 0),
            fps: 30,
            sent: 0,
            written: 0,
            began: None,
            seconds: 0.0,
        }
    }

    /// Writes what the encoder has finished into the film.
    fn drain(&mut self) -> Result<(), Error> {
        let Some(units) = self.units.as_ref() else {
            return Ok(());
        };
        while let Ok(unit) = units.try_recv() {
            let unit = unit?;
            if self.muxer.is_none() {
                let sets = unit
                    .sets
                    .clone()
                    .ok_or("VideoToolbox: the first picture came without parameter sets")?;
                self.muxer = Some(Muxer::new(
                    u16::try_from(self.size.0)?,
                    u16::try_from(self.size.1)?,
                    self.fps,
                    sets,
                )?);
            }
            if let Some(muxer) = self.muxer.as_mut() {
                muxer.push(self.written, unit.data, unit.key)?;
                self.written += 1;
            }
        }
        Ok(())
    }

    fn end_session(&mut self) {
        if let Some(session) = self.session.take() {
            // SAFETY: the session is ours; once invalidated the encoder
            // calls back no more, and only then is its context freed.
            unsafe { session.invalidate() };
        }
        if !self.context.is_null() {
            // SAFETY: boxed in `open`, and no callback can still hold it.
            drop(unsafe { Box::from_raw(self.context) });
            self.context = null_mut();
        }
    }
}

impl Drop for H264Film {
    fn drop(&mut self) {
        self.end_session();
    }
}

impl Sink for H264Film {
    fn open(&mut self, width: u32, height: u32, fps: u32) -> Result<(), Error> {
        self.size = (width as usize, height as usize);
        self.fps = fps.max(1);
        let (sender, units) = channel();
        self.units = Some(units);
        self.context = Box::into_raw(Box::new(sender));
        let number = |value: i32| {
            // SAFETY: a pointer to an `i32`, read as one.
            unsafe { CFNumber::new(None, CFNumberType::SInt32Type, (&raw const value).cast()) }
                .ok_or("a number could not be made")
        };
        let (rate, interval, per_second) = (
            number(self.bitrate.min(i32::MAX as u32) as i32)?,
            number((2 * self.fps) as i32)?,
            number(self.fps as i32)?,
        );
        let no: &CFType = unsafe { kCFBooleanFalse }.ok_or("no CFBoolean")?;
        // SAFETY: calls into the system's API with valid arguments; the
        // session is released when its `CFRetained` is dropped.
        unsafe {
            let mut out: *mut VTCompressionSession = null_mut();
            check(
                VTCompressionSession::create(
                    None,
                    width as i32,
                    height as i32,
                    H264,
                    None,
                    None,
                    None,
                    Some(encoded),
                    self.context.cast(),
                    NonNull::from(&mut out),
                ),
                "opening an H.264 encoder",
            )?;
            let session = CFRetained::from_raw(NonNull::new(out).ok_or("no encoder session")?);
            let settings: [(&CFString, &CFType); 9] = [
                // Not a live stream: quality before latency.
                (kVTCompressionPropertyKey_RealTime, no),
                // Pictures out in the order they went in.
                (kVTCompressionPropertyKey_AllowFrameReordering, no),
                (
                    kVTCompressionPropertyKey_ProfileLevel,
                    kVTProfileLevel_H264_High_AutoLevel,
                ),
                (kVTCompressionPropertyKey_AverageBitRate, &rate),
                (kVTCompressionPropertyKey_MaxKeyFrameInterval, &interval),
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
            for (key, value) in settings {
                check(
                    VTSessionSetProperty(&session, key, Some(value)),
                    "a setting",
                )?;
            }
            check(session.prepare_to_encode_frames(), "preparing")?;
            self.session = Some(session);
        }
        self.began = Some(std::time::Instant::now());
        Ok(())
    }

    fn wants_i420(&self) -> bool {
        true
    }

    fn picture(&mut self, _index: u32, _frame: u32, _: &[u8], i420: &[u8]) -> Result<(), Error> {
        let (w, h) = self.size;
        let (luma, chroma) = (w * h, (w / 2) * (h / 2));
        if i420.len() != luma + 2 * chroma {
            return Err(format!("a {w}×{h} I420 picture is not {} bytes", i420.len()).into());
        }
        let session = self.session.as_ref().ok_or("the film was not opened")?;
        // SAFETY: the buffer is created, filled within its planes' bounds
        // while locked, and handed to the encoder, which retains it.
        unsafe {
            let mut out: *mut CVPixelBuffer = null_mut();
            check(
                CVPixelBufferCreate(
                    None,
                    w,
                    h,
                    kCVPixelFormatType_420YpCbCr8Planar,
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
            let planes = [
                (&i420[..luma], w, h),
                (&i420[luma..luma + chroma], w / 2, h / 2),
                (&i420[luma + chroma..], w / 2, h / 2),
            ];
            for (plane, (from, width, height)) in planes.into_iter().enumerate() {
                let to = CVPixelBufferGetBaseAddressOfPlane(&buffer, plane).cast::<u8>();
                let stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer, plane);
                for row in 0..height {
                    std::ptr::copy_nonoverlapping(
                        from.as_ptr().add(row * width),
                        to.add(row * stride),
                        width,
                    );
                }
            }
            CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::empty());
            check(
                session.encode_frame(
                    &buffer,
                    CMTime::new(self.sent, self.fps as i32),
                    CMTime::new(1, self.fps as i32),
                    None,
                    null_mut(),
                    null_mut(),
                ),
                "encoding a picture",
            )?;
        }
        self.sent += 1;
        self.drain()
    }

    fn close(&mut self) -> Result<(), Error> {
        let session = self.session.as_ref().ok_or("the film was not opened")?;
        // SAFETY: the session is ours and valid; this returns once every
        // picture given has been called back.
        check(
            unsafe { session.complete_frames(kCMTimeInvalid) },
            "finishing",
        )?;
        self.drain()?;
        self.end_session();
        self.seconds = self.began.map_or(0.0, |b| b.elapsed().as_secs_f64());
        if self.written != self.sent as u64 {
            return Err(format!(
                "VideoToolbox gave back {} of {} pictures",
                self.written, self.sent
            )
            .into());
        }
        let muxer = self.muxer.take().ok_or("a film of no picture")?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.path, muxer.finish()?)?;
        Ok(())
    }

    fn spent(&self) -> Vec<(String, f64)> {
        vec![(
            "H.264 by the machine's encoder, open for".into(),
            self.seconds,
        )]
    }
}
