// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A film for a headset: spatial video, MV-HEVC.
//!
//! The two eyes are two layers of one HEVC stream, and the file says what
//! they are — which layer is which eye, how far apart the eyes were, the
//! field they see — so that a player shows them in depth. Nothing of the
//! format is written here: AVFoundation encodes and writes it
//! (`AVAssetWriter` and its tagged pixel buffer group adaptor, macOS 14 and
//! later), as it writes the spatial videos the system's own cameras make.
//! What this module does is hand it each frame's two eyes, tagged, and the
//! stereo metadata once.

#![allow(unsafe_code)]

use std::path::PathBuf;
use std::ptr::{null_mut, NonNull};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterInputTaggedPixelBufferGroupAdaptor,
    AVAssetWriterStatus, AVFileTypeMPEG4, AVFileTypeQuickTimeMovie, AVMediaTypeVideo,
    AVVideoAverageBitRateKey, AVVideoCodecKey, AVVideoCodecTypeHEVC,
    AVVideoCompressionPropertiesKey, AVVideoExpectedSourceFrameRateKey, AVVideoHeightKey,
    AVVideoWidthKey,
};
use objc2_core_foundation::{CFArray, CFRetained, CFString};
use objc2_core_media::{
    kCMFormatDescriptionExtension_HorizontalFieldOfView, kCMTagStereoLeftEye, kCMTagStereoRightEye,
    CMTag, CMTagCategory, CMTagCollection, CMTaggedBufferGroup, CMTime,
};
use objc2_core_video::{
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer,
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSDictionary, NSMutableDictionary, NSNumber, NSString, NSURL};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_HasLeftStereoEyeView,
    kVTCompressionPropertyKey_HasRightStereoEyeView, kVTCompressionPropertyKey_HeroEye,
    kVTCompressionPropertyKey_HorizontalDisparityAdjustment,
    kVTCompressionPropertyKey_MVHEVCLeftAndRightViewIDs,
    kVTCompressionPropertyKey_MVHEVCVideoLayerIDs, kVTCompressionPropertyKey_MVHEVCViewIDs,
    kVTCompressionPropertyKey_StereoCameraBaseline, kVTHeroEye_Left,
};

use crate::sink::{Eyes, Sink};
use crate::Error;

/// The longest the writer may take to be ready for the next frame.
const READY_WITHIN: Duration = Duration::from_secs(60);

/// A CoreFoundation string as the Foundation one it is (toll-free bridged).
fn ns(cf: &CFString) -> &NSString {
    // SAFETY: CFString and NSString are the same object.
    unsafe { &*(cf as *const CFString).cast::<NSString>() }
}

/// What is said of the film once, from its first frame's eyes: the eyes'
/// distance in micrometres, and their horizontal field in thousandths of a
/// degree.
pub fn stereo_metadata(eyes: &Eyes<'_>) -> (u32, u32) {
    let info = crate::sink::stereo_info(eyes);
    (info.baseline_um, info.hfov_millidegrees)
}

/// I420 planes into an NV12 buffer: the luma as it is, the two chromas
/// interleaved.
///
/// # Safety
///
/// `buffer` is a locked bi-planar 4:2:0 buffer of `w × h`.
unsafe fn fill_nv12(buffer: &CVPixelBuffer, i420: &[u8], w: usize, h: usize) {
    let (luma, chroma) = (w * h, (w / 2) * (h / 2));
    let (y, u, v) = (
        &i420[..luma],
        &i420[luma..luma + chroma],
        &i420[luma + chroma..luma + 2 * chroma],
    );
    unsafe {
        let to = CVPixelBufferGetBaseAddressOfPlane(buffer, 0).cast::<u8>();
        let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0);
        for row in 0..h {
            std::ptr::copy_nonoverlapping(y.as_ptr().add(row * w), to.add(row * stride), w);
        }
        let to = CVPixelBufferGetBaseAddressOfPlane(buffer, 1).cast::<u8>();
        let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1);
        for row in 0..h / 2 {
            let line = std::slice::from_raw_parts_mut(to.add(row * stride), w);
            for x in 0..w / 2 {
                line[2 * x] = u[row * (w / 2) + x];
                line[2 * x + 1] = v[row * (w / 2) + x];
            }
        }
    }
}

/// What AVFoundation holds while the film is written.
struct Writing {
    writer: Retained<AVAssetWriter>,
    input: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputTaggedPixelBufferGroupAdaptor>,
}

/// A spatial film (MV-HEVC), by AVFoundation: `.mov` for a path that ends
/// so, an mp4 otherwise.
pub struct SpatialFilm {
    path: PathBuf,
    bitrate: u32,
    size: (usize, usize),
    fps: u32,
    writing: Option<Writing>,
    sent: i64,
    began: Option<Instant>,
    seconds: f64,
}

impl SpatialFilm {
    pub fn at(path: impl Into<PathBuf>, bitrate: u32) -> Self {
        Self {
            path: path.into(),
            bitrate,
            size: (0, 0),
            fps: 30,
            writing: None,
            sent: 0,
            began: None,
            seconds: 0.0,
        }
    }

    fn failed(&self, what: &str) -> Error {
        let why = self
            .writing
            .as_ref()
            .and_then(|w| unsafe { w.writer.error() })
            .map(|e| e.localizedDescription().to_string())
            .unwrap_or_else(|| "no reason given".into());
        format!("spatial film: {what}: {why}").into()
    }

    /// The writer, its input and its adaptor, set for these eyes.
    fn start(&mut self, eyes: &Eyes<'_>) -> Result<(), Error> {
        let (w, h) = self.size;
        let (baseline_um, hfov) = stereo_metadata(eyes);
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // The writer refuses a file that is there.
        let _ = std::fs::remove_file(&self.path);
        let url = NSURL::from_file_path(&self.path).ok_or("the film's path is not a file URL")?;
        let mov = self.path.extension().is_some_and(|e| e == "mov");
        // SAFETY: the system's constants, present on any macOS that has
        // AVFoundation; the calls are given objects of the types they take.
        unsafe {
            let file_type = if mov {
                AVFileTypeQuickTimeMovie
            } else {
                AVFileTypeMPEG4
            }
            .ok_or("AVFoundation names no such file type")?;
            let writer = AVAssetWriter::assetWriterWithURL_fileType_error(&url, file_type)
                .map_err(|e| format!("spatial film: {}", e.localizedDescription()))?;

            let ids =
                || NSArray::from_retained_slice(&[NSNumber::new_i32(0), NSNumber::new_i32(1)]);
            let yes = NSNumber::new_bool(true);
            let properties = NSMutableDictionary::<NSString, AnyObject>::new();
            let set = |key: &CFString, value: &AnyObject| properties.insert(ns(key), value);
            set(kVTCompressionPropertyKey_MVHEVCVideoLayerIDs, &ids());
            set(kVTCompressionPropertyKey_MVHEVCViewIDs, &ids());
            set(kVTCompressionPropertyKey_MVHEVCLeftAndRightViewIDs, &ids());
            set(kVTCompressionPropertyKey_HasLeftStereoEyeView, &yes);
            set(kVTCompressionPropertyKey_HasRightStereoEyeView, &yes);
            set(kVTCompressionPropertyKey_HeroEye, ns(kVTHeroEye_Left));
            set(
                kVTCompressionPropertyKey_StereoCameraBaseline,
                &NSNumber::new_u32(baseline_um),
            );
            // The eyes were drawn converged — what is on the screen has no
            // parallax already: nothing more to shift.
            set(
                kVTCompressionPropertyKey_HorizontalDisparityAdjustment,
                &NSNumber::new_i32(0),
            );
            set(
                kCMFormatDescriptionExtension_HorizontalFieldOfView,
                &NSNumber::new_u32(hfov),
            );
            let key = |k: Option<&'static NSString>| k.ok_or("AVFoundation names no such key");
            properties.insert(
                key(AVVideoAverageBitRateKey)?,
                // Both eyes: what a spatial film is asked for unless told.
                &NSNumber::new_u32(if self.bitrate == 0 {
                    30_000_000
                } else {
                    self.bitrate
                }),
            );
            properties.insert(
                key(AVVideoExpectedSourceFrameRateKey)?,
                &NSNumber::new_u32(self.fps),
            );

            let settings = NSMutableDictionary::<NSString, AnyObject>::new();
            settings.insert(
                key(AVVideoCodecKey)?,
                AVVideoCodecTypeHEVC.ok_or("AVFoundation has no HEVC")?,
            );
            settings.insert(key(AVVideoWidthKey)?, &NSNumber::new_usize(w));
            settings.insert(key(AVVideoHeightKey)?, &NSNumber::new_usize(h));
            settings.insert(key(AVVideoCompressionPropertiesKey)?, &properties);

            let input = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
                AVMediaTypeVideo.ok_or("AVFoundation has no video")?,
                Some(&settings),
            );
            input.setExpectsMediaDataInRealTime(false);

            let attributes = NSMutableDictionary::<NSString, AnyObject>::new();
            attributes.insert(
                ns(kCVPixelBufferPixelFormatTypeKey),
                &NSNumber::new_u32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange),
            );
            attributes.insert(ns(kCVPixelBufferWidthKey), &NSNumber::new_usize(w));
            attributes.insert(ns(kCVPixelBufferHeightKey), &NSNumber::new_usize(h));
            // IOSurface-backed, as the adaptor wants its buffers.
            attributes.insert(
                ns(kCVPixelBufferIOSurfacePropertiesKey),
                &NSDictionary::<NSString, AnyObject>::new(),
            );
            let adaptor = AVAssetWriterInputTaggedPixelBufferGroupAdaptor::assetWriterInputTaggedPixelBufferGroupAdaptorWithAssetWriterInput_sourcePixelBufferAttributes(
                &input,
                Some(&attributes),
            );
            if !writer.canAddInput(&input) {
                return Err("spatial film: the writer takes no such input".into());
            }
            writer.addInput(&input);
            self.writing = Some(Writing {
                writer,
                input,
                adaptor,
            });
            let writing = self.writing.as_ref().ok_or("no writer")?;
            if !writing.writer.startWriting() {
                return Err(self.failed("starting"));
            }
            writing
                .writer
                .startSessionAtSourceTime(CMTime::new(0, self.fps as i32));
        }
        self.began = Some(Instant::now());
        Ok(())
    }

    /// One eye's picture, in a buffer of the adaptor's pool.
    fn buffer(
        &self,
        pool: &CVPixelBufferPool,
        i420: &[u8],
    ) -> Result<CFRetained<CVPixelBuffer>, Error> {
        let (w, h) = self.size;
        if i420.len() != w * h + 2 * (w / 2) * (h / 2) {
            return Err(format!("a {w}×{h} eye is not {} bytes of I420", i420.len()).into());
        }
        // SAFETY: the buffer is the pool's, filled within its planes while
        // locked.
        unsafe {
            let mut out: *mut CVPixelBuffer = null_mut();
            if CVPixelBufferPool::create_pixel_buffer(None, pool, NonNull::from(&mut out)) != 0 {
                return Err("spatial film: no buffer from the pool".into());
            }
            let buffer = CFRetained::from_raw(NonNull::new(out).ok_or("no buffer")?);
            CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::empty());
            fill_nv12(&buffer, i420, w, h);
            CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::empty());
            Ok(buffer)
        }
    }
}

/// The tags of one eye: which eye, which layer of the stream.
fn eye_tags(left: bool) -> Result<CFRetained<CMTagCollection>, Error> {
    // SAFETY: tags of the system's own, and a collection made of them.
    unsafe {
        let tags = [
            if left {
                kCMTagStereoLeftEye
            } else {
                kCMTagStereoRightEye
            },
            CMTag::with_s_int64_value(CMTagCategory::VideoLayerID, if left { 0 } else { 1 }),
        ];
        let mut out: *const CMTagCollection = std::ptr::null();
        if CMTagCollection::create(
            None,
            tags.as_ptr(),
            tags.len() as _,
            NonNull::from(&mut out),
        ) != 0
        {
            return Err("spatial film: an eye's tags".into());
        }
        Ok(CFRetained::from_raw(
            NonNull::new(out.cast_mut()).ok_or("no tags")?,
        ))
    }
}

impl Sink for SpatialFilm {
    fn open(&mut self, _: u32, _: u32, _: u32) -> Result<(), Error> {
        Err("a spatial film has two eyes: render it in stereo".into())
    }

    fn open_stereo(
        &mut self,
        _picture: (u32, u32),
        eye: (u32, u32),
        fps: u32,
    ) -> Result<(), Error> {
        self.size = (eye.0 as usize, eye.1 as usize);
        self.fps = fps.max(1);
        Ok(())
    }

    fn wants_i420(&self) -> bool {
        true
    }
    fn wants_rgba(&self) -> bool {
        false
    }

    fn picture(&mut self, _: u32, _: u32, _: &[u8], _: &[u8]) -> Result<(), Error> {
        Err("a spatial film has two eyes: render it in stereo".into())
    }

    fn stereo(
        &mut self,
        _index: u32,
        _frame: u32,
        _rgba: &[u8],
        _i420: &[u8],
        eyes: &Eyes<'_>,
    ) -> Result<(), Error> {
        if self.writing.is_none() {
            self.start(eyes)?;
        }
        let writing = self.writing.as_ref().ok_or("no writer")?;
        let waiting = Instant::now();
        // SAFETY: the writer and its input are ours and alive.
        while !unsafe { writing.input.isReadyForMoreMediaData() } {
            if unsafe { writing.writer.status() } != AVAssetWriterStatus::Writing {
                return Err(self.failed("writing"));
            }
            if waiting.elapsed() > READY_WITHIN {
                return Err("spatial film: the writer was not ready within a minute".into());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        // SAFETY: the pool exists once writing has started.
        let pool =
            unsafe { writing.adaptor.pixelBufferPool() }.ok_or("spatial film: no buffer pool")?;
        let buffers = [
            self.buffer(&pool, eyes.left)?,
            self.buffer(&pool, eyes.right)?,
        ];
        let tags = [eye_tags(true)?, eye_tags(false)?];
        let (tags, buffers) = (
            CFArray::from_retained_objects(&tags),
            CFArray::from_retained_objects(&buffers),
        );
        // SAFETY: two tag collections and two pixel buffers, in order.
        let appended = unsafe {
            let mut out: *mut CMTaggedBufferGroup = null_mut();
            if CMTaggedBufferGroup::create(
                None,
                tags.as_opaque(),
                buffers.as_opaque(),
                NonNull::from(&mut out),
            ) != 0
            {
                return Err("spatial film: the eyes could not be grouped".into());
            }
            let group = CFRetained::from_raw(NonNull::new(out).ok_or("no group")?);
            writing
                .adaptor
                .appendTaggedPixelBufferGroup_withPresentationTime(
                    &group,
                    CMTime::new(self.sent, self.fps as i32),
                )
        };
        if !appended {
            return Err(self.failed("a frame"));
        }
        self.sent += 1;
        Ok(())
    }

    fn close(&mut self) -> Result<(), Error> {
        let writing = self.writing.as_ref().ok_or("a spatial film of no frame")?;
        let (done, finished) = std::sync::mpsc::channel();
        let handler = block2::RcBlock::new(move || {
            let _ = done.send(());
        });
        // SAFETY: every append has returned; the handler only signals.
        unsafe {
            writing.input.markAsFinished();
            writing.writer.finishWritingWithCompletionHandler(&handler);
        }
        finished
            .recv_timeout(Duration::from_secs(600))
            .map_err(|_| "spatial film: the writer did not finish")?;
        if unsafe { writing.writer.status() } != AVAssetWriterStatus::Completed {
            return Err(self.failed("finishing"));
        }
        self.seconds = self.began.map_or(0.0, |b| b.elapsed().as_secs_f64());
        Ok(())
    }

    fn spent(&self) -> Vec<(String, f64)> {
        vec![(
            "spatial film by AVFoundation, open for".into(),
            self.seconds,
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera() -> tuile_film::FrameCamera {
        let view = tuile_film::BakedView {
            position: [tuile_core::geo::WGS84_A + 1500.0, 0.0, 0.0],
            direction: [-0.6, 0.0, 0.8],
            up: [0.8, 0.0, 0.6],
            viewport_px: [256.0, 144.0],
            fovy_rad: 0.8,
        };
        tuile_film::FrameCamera::of(&view, 16.0 / 9.0)
    }

    /// Four frames of two flat eyes, through AVFoundation: the file is one
    /// HEVC track whose sample entry carries the second layer's record
    /// (`lhvC`) and the stereo boxes (`vexu`, with the eyes, and `hfov`).
    #[test]
    fn two_eyes_become_a_spatial_film_with_its_stereo_metadata() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("eyes.mov");
        let centre = camera();
        let stereo = tuile_film::Stereo {
            baseline_m: Some(10.0),
            convergence: tuile_film::Convergence::Distance(2000.0),
            ..tuile_film::Stereo::default()
        };
        let pair = stereo.eyes(&centre);
        let left_camera = centre.eye(&pair, tuile_film::Eye::Left);
        let eyes = Eyes {
            left: &[],
            right: &[],
            pair: &pair,
            left_camera: &left_camera,
        };
        let (baseline_um, hfov) = stereo_metadata(&eyes);
        assert_eq!(baseline_um, 10_000_000);
        assert!((60_000..90_000).contains(&hfov), "{hfov}");

        let (w, h) = (256usize, 144usize);
        let plane = |y: u8| [vec![y; w * h], vec![128u8; w * h / 2]].concat();
        let (left, right) = (plane(60), plane(180));
        let mut film = SpatialFilm::at(&path, 4_000_000);
        assert!(
            film.open(512, 144, 30).is_err(),
            "one eye is no spatial film"
        );
        film.open_stereo((512, 144), (w as u32, h as u32), 30)
            .expect("open");
        for i in 0..4 {
            let eyes = Eyes {
                left: &left,
                right: &right,
                ..eyes
            };
            film.stereo(i, i + 1, &[], &[], &eyes).expect("a frame");
        }
        film.close().expect("closed");
        let bytes = std::fs::read(&path).expect("the film");
        let has = |kind: &[u8]| bytes.windows(4).any(|w| w == kind);
        for kind in [b"hvc1", b"hvcC", b"lhvC", b"vexu", b"hfov"] {
            assert!(has(kind), "no {} box", String::from_utf8_lossy(kind));
        }
    }
}
