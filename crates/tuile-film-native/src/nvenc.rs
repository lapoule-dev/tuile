// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The film as an mp4, AV1, H.264 or — for two eyes — MV-HEVC by an NVIDIA
//! card's encoder.
//!
//! On a machine with an NVIDIA card the render runs on Vulkan and the
//! pictures are encoded by the card's own encoder (NVENC, reached over
//! CUDA through `nvidia-video-codec-sdk`): AV1 on the cards that have it,
//! H.264 on all of them. Nothing here is specific to a cloud or a card
//! model; the card says what it encodes.
//!
//! The encoder runs on its own thread, a few pictures behind the render.

// The bindings' one way into an input buffer is an unchecked copy.
#![allow(unsafe_code)]

use std::path::PathBuf;
use std::sync::mpsc::{sync_channel, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cudarc::driver::CudaContext;
use nvidia_video_codec_sdk::sys::nvEncodeAPI::{
    HEVC_3D_REFERENCE_DISPLAY_INFO, NV_ENC_BIT_DEPTH, NV_ENC_BUFFER_FORMAT, NV_ENC_CAPS,
    NV_ENC_CODEC_AV1_GUID, NV_ENC_CODEC_H264_GUID, NV_ENC_CODEC_HEVC_GUID,
    NV_ENC_HEVC_PROFILE_MAIN_GUID, NV_ENC_MULTI_PASS, NV_ENC_PARAMS_RC_MODE,
    NV_ENC_PIC_PARAMS_HEVC, NV_ENC_PIC_TYPE, NV_ENC_PRESET_P4_GUID, NV_ENC_TEMPORAL_FILTER_LEVEL,
    NV_ENC_TUNING_INFO,
};
use nvidia_video_codec_sdk::{
    CodecPictureParams, EncodeError, EncodePictureParams, Encoder, EncoderInitParams, EncoderInput,
    ErrorKind,
};
use tuile_mp4::{annex_b_to_avcc, nal_units, Codec, Muxer, ParameterSets};

use crate::av1::{av1c_of, obus};
use crate::sink::Sink;
use crate::Error;

/// What the card is asked to encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvencCodec {
    Av1,
    H264,
    /// The two eyes of a film as the two layers of one HEVC stream, with
    /// the stereo metadata a headset reads: a spatial film. Blackwell cards
    /// and later; a film of two eyes only.
    MvHevc,
}

/// Pictures waiting for the encoder before the render waits for it.
const WAITING: usize = 4;

/// The film as an mp4, by NVENC.
pub struct NvencFilm {
    path: PathBuf,
    bitrate: u32,
    codec: NvencCodec,
    /// The CUDA device the encoder is opened on.
    device: usize,
    pictures: Option<SyncSender<Vec<u8>>>,
    /// For a spatial film: each frame's two eyes, and its frame rate until
    /// the first frame opens the encoder.
    eyes: Option<SyncSender<(Vec<u8>, Vec<u8>)>>,
    fps: u32,
    encoder: Option<JoinHandle<Result<(Vec<u8>, f64), String>>>,
    size: (usize, usize),
    seconds: f64,
}

impl NvencFilm {
    pub fn at(path: impl Into<PathBuf>, bitrate: u32, codec: NvencCodec) -> Self {
        Self {
            path: path.into(),
            bitrate,
            codec,
            device: 0,
            pictures: None,
            eyes: None,
            fps: 30,
            encoder: None,
            size: (0, 0),
            seconds: 0.0,
        }
    }

    /// Opens the encoder on this CUDA device rather than the first.
    #[must_use]
    pub fn on_device(mut self, device: usize) -> Self {
        self.device = device;
        self
    }
}

fn told(e: EncodeError) -> String {
    format!("NVENC: {e}")
}

/// The encoder's thread: pictures in, the finished mp4 out.
fn encode(
    waiting: std::sync::mpsc::Receiver<Vec<u8>>,
    device: usize,
    codec: NvencCodec,
    (width, height): (usize, usize),
    fps: u32,
    bitrate: u32,
) -> Result<(Vec<u8>, f64), String> {
    let cuda = CudaContext::new(device).map_err(|e| format!("CUDA device {device}: {e:?}"))?;
    let encoder = Encoder::initialize_with_cuda(cuda).map_err(told)?;
    let guid = match codec {
        NvencCodec::Av1 => NV_ENC_CODEC_AV1_GUID,
        NvencCodec::H264 => NV_ENC_CODEC_H264_GUID,
        NvencCodec::MvHevc => return Err("NVENC: MV-HEVC is a film of two eyes".into()),
    };
    if !encoder.get_encode_guids().map_err(told)?.contains(&guid) {
        return Err(format!("NVENC: this card does not encode {codec:?}"));
    }
    // The preset's own settings, with what a film needs changed: no frame
    // reordering (pictures out as they went in), a key frame every two
    // seconds, no looking ahead (one picture in, one picture out, which is
    // what the loop below relies on), and the bitrate asked for.
    let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_HIGH_QUALITY;
    let mut config = encoder
        .get_preset_config(guid, NV_ENC_PRESET_P4_GUID, tuning)
        .map_err(told)?
        .presetCfg;
    config.frameIntervalP = 1;
    config.gopLength = 2 * fps;
    if codec == NvencCodec::H264 {
        // An IDR a GOP: left unsaid, the preset's period is infinite, and
        // the key frames every two seconds are plain I pictures that later
        // ones may reach past — a player seeking to one decodes garbage.
        // SAFETY: the preset asked for is H.264's, so the codec config is.
        unsafe {
            config.encodeCodecConfig.h264Config.idrPeriod = config.gopLength;
        }
    }
    config.rcParams.set_enableLookahead(0);
    config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_VBR;
    config.rcParams.averageBitRate = bitrate;
    config.rcParams.maxBitRate = bitrate.saturating_mul(2);
    let mut init = EncoderInitParams::new(guid, width as u32, height as u32);
    init.preset_guid(NV_ENC_PRESET_P4_GUID)
        .tuning_info(tuning)
        .framerate(fps, 1)
        .enable_picture_type_decision()
        .encode_config(&mut config);
    let session = encoder
        .start_session(NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_IYUV, init)
        .map_err(told)?;
    let mut input = session.create_input_buffer().map_err(told)?;
    let mut output = session.create_output_bitstream().map_err(told)?;

    // A buffer says how wide its rows are when it is first locked, and they
    // may be wider than the picture's.
    drop(input.lock().map_err(told)?);
    let pitch = match input.pitch() as usize {
        0 => width,
        pitch => pitch,
    };

    let (luma, chroma) = (width * height, (width / 2) * (height / 2));
    let mut muxer: Option<Muxer> = None;
    let (mut index, mut busy) = (0u64, 0.0f64);
    let open = |codec: Codec| {
        Muxer::with(
            u16::try_from(width).map_err(|e| e.to_string())?,
            u16::try_from(height).map_err(|e| e.to_string())?,
            fps,
            codec,
        )
        .map_err(|e| e.to_string())
    };
    for i420 in waiting {
        let began = Instant::now();
        {
            let mut lock = input.lock().map_err(told)?;
            let planes = if pitch == width {
                i420
            } else {
                let mut pitched = vec![0u8; pitch * height + 2 * (pitch / 2) * (height / 2)];
                let mut to = 0;
                for (from, w, h, p) in [
                    (&i420[..luma], width, height, pitch),
                    (&i420[luma..luma + chroma], width / 2, height / 2, pitch / 2),
                    (&i420[luma + chroma..], width / 2, height / 2, pitch / 2),
                ] {
                    for row in 0..h {
                        pitched[to + row * p..to + row * p + w]
                            .copy_from_slice(&from[row * w..(row + 1) * w]);
                    }
                    to += p * h;
                }
                pitched
            };
            // SAFETY: `planes` is exactly a planar 4:2:0 picture of the
            // session's size at the buffer's pitch, which is what the
            // buffer was created to hold.
            unsafe { lock.write(&planes) };
        }
        loop {
            match session.encode_picture(&mut input, &mut output, Default::default()) {
                Ok(()) => break,
                Err(e) if e.kind() == ErrorKind::EncoderBusy => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(e) => return Err(told(e)),
            }
        }
        let lock = output.lock().map_err(told)?;
        let key = is_sync(codec, lock.picture_type());
        let data = lock.data();
        let sample = match codec {
            NvencCodec::Av1 => {
                if muxer.is_none() {
                    let record = obus(data)
                        .into_iter()
                        .find(|(kind, _)| *kind == 1)
                        .and_then(|(_, header)| av1c_of(header))
                        .ok_or("NVENC: the first picture carries no sequence header this reads")?;
                    muxer = Some(open(Codec::Av1(record))?);
                }
                data.to_vec()
            }
            NvencCodec::MvHevc => unreachable!("refused before the session"),
            NvencCodec::H264 => {
                if muxer.is_none() {
                    let units = nal_units(data);
                    let of = |kind: u8| {
                        units
                            .iter()
                            .find(|unit| unit.first().is_some_and(|b| b & 0x1f == kind))
                            .map(|unit| unit.to_vec())
                    };
                    let (sps, pps) = of(7)
                        .zip(of(8))
                        .ok_or("NVENC: the first picture carries no parameter sets")?;
                    muxer = Some(open(Codec::Avc(ParameterSets { sps, pps }))?);
                }
                annex_b_to_avcc(data)
            }
        };
        drop(lock);
        if let Some(muxer) = muxer.as_mut() {
            muxer.push(index, sample, key).map_err(|e| e.to_string())?;
        }
        index += 1;
        busy += began.elapsed().as_secs_f64();
    }
    session.end_of_stream().map_err(told)?;
    let film = muxer
        .ok_or("a film of no picture")?
        .finish()
        .map_err(|e| e.to_string())?;
    Ok((film, busy))
}

/// Whether a picture of `kind` the encoder gave is a sync sample: one a
/// player may start decoding at. For H.264 and HEVC only an IDR is — an I
/// picture (a scene cut) does not stop later pictures from referring past
/// it, and a seek that starts there decodes garbage. AV1's key frames are
/// reported as either.
fn is_sync(codec: NvencCodec, kind: NV_ENC_PIC_TYPE) -> bool {
    match codec {
        NvencCodec::Av1 => matches!(
            kind,
            NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR | NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I
        ),
        NvencCodec::H264 | NvencCodec::MvHevc => kind == NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR,
    }
}

/// An I420 picture as NV12 in a buffer whose rows are `pitch` wide: the
/// luma, then the two chromas interleaved, as the MV-HEVC session takes it.
fn nv12(i420: &[u8], (width, height): (usize, usize), pitch: usize) -> Vec<u8> {
    let (luma, chroma) = (width * height, (width / 2) * (height / 2));
    let (y, u, v) = (
        &i420[..luma],
        &i420[luma..luma + chroma],
        &i420[luma + chroma..],
    );
    let mut out = vec![0u8; pitch * height + pitch * (height / 2)];
    for row in 0..height {
        out[row * pitch..row * pitch + width].copy_from_slice(&y[row * width..(row + 1) * width]);
    }
    let base = pitch * height;
    for row in 0..height / 2 {
        let line = &mut out[base + row * pitch..base + row * pitch + width];
        for x in 0..width / 2 {
            line[2 * x] = u[row * (width / 2) + x];
            line[2 * x + 1] = v[row * (width / 2) + x];
        }
    }
    out
}

/// The encoder's thread for a spatial film: each frame's two eyes in, the
/// finished mp4 out. Each eye is one picture of the session, the left as
/// view 0 (the base layer), the right as view 1; the access unit is both.
fn encode_eyes(
    waiting: std::sync::mpsc::Receiver<(Vec<u8>, Vec<u8>)>,
    device: usize,
    (width, height): (usize, usize),
    fps: u32,
    bitrate: u32,
    stereo: tuile_mp4::spatial::StereoInfo,
) -> Result<(Vec<u8>, f64), String> {
    // Each step said on stderr with TUILE_NVENC_TRACE set: the driver may
    // end the process without a word.
    let trace = std::env::var_os("TUILE_NVENC_TRACE").is_some();
    let step = |what: &str| {
        if trace {
            eprintln!("nvenc mv-hevc: {what}");
        }
    };
    let cuda = CudaContext::new(device).map_err(|e| format!("CUDA device {device}: {e:?}"))?;
    // The context current on this thread, as FFmpeg pushes it around every
    // call: MV-HEVC's encode wants it where a single view does not.
    cuda.bind_to_thread()
        .map_err(|e| format!("CUDA device {device}: {e:?}"))?;
    let encoder = Encoder::initialize_with_cuda(cuda).map_err(told)?;
    step("encoder open");
    let guid = NV_ENC_CODEC_HEVC_GUID;
    if !encoder.get_encode_guids().map_err(told)?.contains(&guid) {
        return Err("NVENC: this card does not encode HEVC".into());
    }
    let supported = encoder
        .get_encode_caps(guid, NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_MVHEVC_ENCODE)
        .map_err(told)?;
    step(&format!("MV-HEVC supported: {supported}"));
    if supported == 0 {
        return Err("NVENC: this card does not encode MV-HEVC (Blackwell and later do)".into());
    }
    // MV-HEVC wants the high-quality tuning, and nothing that looks ahead
    // or reorders: pictures out as they went in, a key frame every two
    // seconds, the bitrate for both eyes.
    let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_HIGH_QUALITY;
    let mut config = encoder
        .get_preset_config(guid, NV_ENC_PRESET_P4_GUID, tuning)
        .map_err(told)?
        .presetCfg;
    step("preset read");
    config.frameIntervalP = 1;
    config.gopLength = 2 * fps;
    config.rcParams.set_enableLookahead(0);
    config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_VBR;
    config.rcParams.multiPass = NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED;
    config.rcParams.averageBitRate = bitrate;
    config.rcParams.maxBitRate = bitrate.saturating_mul(2);
    // As FFmpeg's encoder asks for it (libavcodec/nvenc.c): the main
    // profile, MV-HEVC on, and the 3D reference displays SEI, whose info
    // goes with the first picture.
    config.profileGUID = NV_ENC_HEVC_PROFILE_MAIN_GUID;
    // SAFETY: the preset asked for is HEVC's, so the codec config is.
    unsafe {
        let hevc = &mut config.encodeCodecConfig.hevcConfig;
        hevc.set_enableMVHEVC(1);
        hevc.set_outputHevc3DReferenceDisplayInfo(1);
        // Nothing MV-HEVC refuses: no temporal filter.
        hevc.tfLevel = NV_ENC_TEMPORAL_FILTER_LEVEL::NV_ENC_TEMPORAL_FILTER_LEVEL_0;
        // What FFmpeg says outright where a preset leaves it unsaid: 4:2:0,
        // eight bits in and out, one slice a picture, an IDR a GOP.
        hevc.set_chromaFormatIDC(1);
        hevc.inputBitDepth = NV_ENC_BIT_DEPTH::NV_ENC_BIT_DEPTH_8;
        hevc.outputBitDepth = NV_ENC_BIT_DEPTH::NV_ENC_BIT_DEPTH_8;
        hevc.sliceMode = 3;
        hevc.sliceModeData = 1;
        hevc.idrPeriod = config.gopLength;
    }
    let mut init = EncoderInitParams::new(guid, width as u32, height as u32);
    init.preset_guid(NV_ENC_PRESET_P4_GUID)
        .tuning_info(tuning)
        .framerate(fps, 1)
        .enable_picture_type_decision()
        .disable_split_encode()
        .encode_config(&mut config);
    let session = encoder
        .start_session(NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12, init)
        .map_err(told)?;
    step("session started");
    let mut inputs = [
        session.create_input_buffer().map_err(told)?,
        session.create_input_buffer().map_err(told)?,
    ];
    let mut outputs = [
        session.create_output_bitstream().map_err(told)?,
        session.create_output_bitstream().map_err(told)?,
    ];
    drop(inputs[0].lock().map_err(told)?);
    let pitch = match inputs[0].pitch() as usize {
        0 => width,
        pitch => pitch,
    };

    let mut muxer: Option<tuile_mp4::spatial::SpatialMuxer> = None;
    let mut busy = 0.0f64;
    // One reference display: the left eye view 0, the right view 1.
    let mut displays = HEVC_3D_REFERENCE_DISPLAY_INFO {
        precRefDisplayWidth: 31,
        ..Default::default()
    };
    displays.leftViewId[0] = 0;
    displays.rightViewId[0] = 1;
    let mut first = true;
    for (left, right) in waiting {
        let began = Instant::now();
        for (view, i420) in [left, right].into_iter().enumerate() {
            {
                let mut lock = inputs[view].lock().map_err(told)?;
                // SAFETY: a planar 4:2:0 picture of the session's size at
                // the buffer's pitch, which the buffer was made to hold.
                unsafe { lock.write(&nv12(&i420, (width, height), pitch)) };
            }
            step(&format!("view {view} written"));
            loop {
                let mut hevc = NV_ENC_PIC_PARAMS_HEVC {
                    viewId: view as u32,
                    ..Default::default()
                };
                if first {
                    hevc.p3DReferenceDisplayInfo = &raw mut displays;
                }
                let params = EncodePictureParams {
                    codec_params: Some(CodecPictureParams::Hevc(hevc)),
                    ..Default::default()
                };
                match session.encode_picture(&mut inputs[view], &mut outputs[view], params) {
                    Ok(()) => break,
                    // The base layer waits for its second view: both come
                    // out once the right eye is in.
                    Err(e) if view == 0 && e.kind() == ErrorKind::NeedMoreInput => break,
                    Err(e) if e.kind() == ErrorKind::EncoderBusy => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    // Said before anything else: dropping a session whose
                    // encode failed may end the process in the driver.
                    Err(e) => {
                        eprintln!("nvenc mv-hevc: view {view}: {e} ({:?})", e.kind());
                        return Err(told(e));
                    }
                }
            }
        }
        first = false;
        // The base layer, then the second: one access unit.
        let mut sample = Vec::new();
        let mut sets: Vec<Vec<u8>> = Vec::new();
        let mut key = false;
        step("both views submitted");
        for (view, output) in outputs.iter_mut().enumerate() {
            let lock = output.lock().map_err(told)?;
            step(&format!("view {view}: {} bytes out", lock.data().len()));
            if view == 0 {
                key = is_sync(NvencCodec::MvHevc, lock.picture_type());
            }
            let (parameters, units) = tuile_mp4::spatial::entry_units(lock.data());
            if muxer.is_none() {
                sets.extend(parameters.iter().map(|u| u.to_vec()));
            }
            for unit in units {
                sample.extend((unit.len() as u32).to_be_bytes());
                sample.extend_from_slice(unit);
            }
        }
        if muxer.is_none() {
            step(&format!("{} parameter sets", sets.len()));
            let units: Vec<&[u8]> = sets.iter().map(Vec::as_slice).collect();
            let (hvcc, lhvc) =
                tuile_mp4::spatial::hevc_records(&units).map_err(|e| format!("NVENC: {e}"))?;
            muxer = Some(
                tuile_mp4::spatial::SpatialMuxer::new(
                    u16::try_from(width).map_err(|e| e.to_string())?,
                    u16::try_from(height).map_err(|e| e.to_string())?,
                    fps,
                    &hvcc,
                    &lhvc,
                    stereo,
                )
                .map_err(|e| e.to_string())?,
            );
        }
        if let Some(muxer) = muxer.as_mut() {
            muxer.push(&sample, key).map_err(|e| e.to_string())?;
        }
        busy += began.elapsed().as_secs_f64();
    }
    session.end_of_stream().map_err(told)?;
    let film = muxer
        .ok_or("a film of no picture")?
        .finish()
        .map_err(|e| e.to_string())?;
    Ok((film, busy))
}

impl Sink for NvencFilm {
    fn open(&mut self, width: u32, height: u32, fps: u32) -> Result<(), Error> {
        if self.codec == NvencCodec::MvHevc {
            return Err("a spatial film has two eyes: render it in stereo".into());
        }
        let size = (width as usize, height as usize);
        self.size = size;
        let (pictures, waiting) = sync_channel::<Vec<u8>>(WAITING);
        let (device, codec, bitrate) = (self.device, self.codec, self.bitrate);
        self.pictures = Some(pictures);
        self.encoder = Some(std::thread::spawn(move || {
            encode(waiting, device, codec, size, fps.max(1), bitrate)
        }));
        Ok(())
    }

    fn wants_i420(&self) -> bool {
        true
    }

    fn picture(&mut self, _index: u32, _frame: u32, _: &[u8], i420: &[u8]) -> Result<(), Error> {
        let (w, h) = self.size;
        if i420.len() != w * h + 2 * (w / 2) * (h / 2) {
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

    fn open_stereo(&mut self, picture: (u32, u32), eye: (u32, u32), fps: u32) -> Result<(), Error> {
        if self.codec != NvencCodec::MvHevc {
            return self.open(picture.0, picture.1, fps);
        }
        // The encoder opens with the first frame, which says the eyes.
        self.size = (eye.0 as usize, eye.1 as usize);
        self.fps = fps.max(1);
        Ok(())
    }

    fn stereo(
        &mut self,
        index: u32,
        frame: u32,
        rgba: &[u8],
        i420: &[u8],
        eyes: &crate::sink::Eyes<'_>,
    ) -> Result<(), Error> {
        if self.codec != NvencCodec::MvHevc {
            return self.picture(index, frame, rgba, i420);
        }
        let (w, h) = self.size;
        let bytes = w * h + 2 * (w / 2) * (h / 2);
        if eyes.left.len() != bytes || eyes.right.len() != bytes {
            return Err(format!("a {w}×{h} eye is not {bytes} bytes of I420").into());
        }
        if self.eyes.is_none() {
            let stereo = crate::sink::stereo_info(eyes);
            let (sender, waiting) = sync_channel::<(Vec<u8>, Vec<u8>)>(WAITING);
            let (device, bitrate, size, fps) = (self.device, self.bitrate, self.size, self.fps);
            self.eyes = Some(sender);
            self.encoder = Some(std::thread::spawn(move || {
                encode_eyes(waiting, device, size, fps, bitrate, stereo)
            }));
        }
        let sender = self.eyes.as_ref().ok_or("the film was not opened")?;
        if sender
            .send((eyes.left.to_vec(), eyes.right.to_vec()))
            .is_err()
        {
            self.eyes = None;
            return match self.encoder.take().map(JoinHandle::join) {
                Some(Ok(Err(why))) => Err(why.into()),
                _ => Err("the encoder stopped".into()),
            };
        }
        Ok(())
    }

    fn close(&mut self) -> Result<(), Error> {
        self.pictures = None;
        self.eyes = None;
        let encoder = self.encoder.take().ok_or("the film was not opened")?;
        let (film, seconds) = encoder.join().map_err(|_| "the encoder panicked")??;
        self.seconds = seconds;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.path, film)?;
        Ok(())
    }

    fn spent(&self) -> Vec<(String, f64)> {
        vec![(
            format!("encoding {:?} by NVENC, on its own thread", self.codec),
            self.seconds,
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_idr_is_a_place_to_start_decoding_h264_or_hevc() {
        use NV_ENC_PIC_TYPE::*;
        for codec in [NvencCodec::H264, NvencCodec::MvHevc] {
            assert!(is_sync(codec, NV_ENC_PIC_TYPE_IDR));
            assert!(!is_sync(codec, NV_ENC_PIC_TYPE_I));
            assert!(!is_sync(codec, NV_ENC_PIC_TYPE_P));
        }
        assert!(is_sync(NvencCodec::Av1, NV_ENC_PIC_TYPE_I));
    }
}
