// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The film as an mp4 by an NVIDIA card's encoder: H.264, AV1, the HEVC
//! Main10 mezzanine the final stage reads, or — for two eyes — MV-HEVC.
//!
//! On a machine with an NVIDIA card the render runs on Vulkan and the
//! pictures are encoded by the card's own encoder (NVENC, reached over
//! CUDA through `nvidia-video-codec-sdk`). Nothing here is specific to a
//! cloud or a card model; the card says what it encodes.
//!
//! One encode loop ([`encode`]) serves every single-view film, the render's
//! sinks and the final compression alike: a pool of buffers, so that an
//! encoder with B-frames and lookahead holds pictures before it hands any
//! back; its pictures come out in decoding order, each with the frame it
//! is shown as, and [`Package`] writes them into a film laid out for a
//! viewer (`tuile_mp4::film`: faststart, BT.709 said, composition offsets).
//! The colour is said in the stream too (VUI, AV1's colour config).
//!
//! The encoder runs on its own thread, a few pictures behind the render.

// The bindings' one way into an input buffer is an unchecked copy.
#![allow(unsafe_code)]

use std::path::PathBuf;
use std::sync::mpsc::{sync_channel, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::encode::{node_spec, NodeCodec};
pub use crate::encode::{Encoded, Frame, Input, Out, Package, Rate, Spec};
use crate::sink::Sink;
use crate::Error;
use cudarc::driver::CudaContext;
use nvidia_video_codec_sdk::sys::nvEncodeAPI::{
    GUID, HEVC_3D_REFERENCE_DISPLAY_INFO, NV_ENC_BFRAME_REF_MODE, NV_ENC_BIT_DEPTH,
    NV_ENC_BUFFER_FORMAT, NV_ENC_CAPS, NV_ENC_CODEC_AV1_GUID, NV_ENC_CODEC_H264_GUID,
    NV_ENC_CODEC_HEVC_GUID, NV_ENC_CONFIG, NV_ENC_CONFIG_H264_VUI_PARAMETERS,
    NV_ENC_H264_PROFILE_HIGH_GUID, NV_ENC_HEVC_PROFILE_MAIN10_GUID, NV_ENC_HEVC_PROFILE_MAIN_GUID,
    NV_ENC_MULTI_PASS, NV_ENC_PARAMS_RC_MODE, NV_ENC_PIC_PARAMS_HEVC, NV_ENC_PIC_TYPE,
    NV_ENC_PRESET_P1_GUID, NV_ENC_PRESET_P2_GUID, NV_ENC_PRESET_P3_GUID, NV_ENC_PRESET_P4_GUID,
    NV_ENC_PRESET_P5_GUID, NV_ENC_PRESET_P6_GUID, NV_ENC_PRESET_P7_GUID,
    NV_ENC_TEMPORAL_FILTER_LEVEL, NV_ENC_TUNING_INFO, NV_ENC_VUI_COLOR_PRIMARIES,
    NV_ENC_VUI_MATRIX_COEFFS, NV_ENC_VUI_TRANSFER_CHARACTERISTIC, NV_ENC_VUI_VIDEO_FORMAT,
};
use nvidia_video_codec_sdk::{
    CodecPictureParams, EncodeError, EncodePictureParams, Encoder, EncoderInitParams, EncoderInput,
    ErrorKind,
};

/// What the card is asked to encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvencCodec {
    Av1,
    H264,
    /// HEVC Main10, near-transparent, a closed GOP a second: the
    /// mezzanine a render node writes for the final stage
    /// (`tuile_farm::compress::Profile::mezzanine`).
    Mezzanine,
    /// The two eyes of a film as the two layers of one HEVC stream, with
    /// the stereo metadata a headset reads: a spatial film. Blackwell cards
    /// and later; a film of two eyes only.
    MvHevc,
}

/// Pictures waiting for the encoder before the render waits for it.
const WAITING: usize = 4;

/// The encoder's buffer format for pictures handed as `input`.
fn buffer_format(input: Input) -> NV_ENC_BUFFER_FORMAT {
    match input {
        Input::Iyuv => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_IYUV,
        Input::Nv12 => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
        Input::P010 => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV420_10BIT,
    }
}

/// The codec `spec` encodes, as NVENC names it.
fn guid(spec: &Spec) -> GUID {
    match spec.out {
        Out::H264 => NV_ENC_CODEC_H264_GUID,
        Out::Hevc => NV_ENC_CODEC_HEVC_GUID,
        Out::Av1 => NV_ENC_CODEC_AV1_GUID,
    }
}

/// `spec`'s preset, P1 to P7.
fn preset_guid(spec: &Spec) -> GUID {
    match spec.preset {
        0 | 1 => NV_ENC_PRESET_P1_GUID,
        2 => NV_ENC_PRESET_P2_GUID,
        3 => NV_ENC_PRESET_P3_GUID,
        4 => NV_ENC_PRESET_P4_GUID,
        5 => NV_ENC_PRESET_P5_GUID,
        6 => NV_ENC_PRESET_P6_GUID,
        _ => NV_ENC_PRESET_P7_GUID,
    }
}

fn told(e: EncodeError) -> String {
    format!("NVENC: {e}")
}

/// The BT.709 limited-range description the stream carries.
fn bt709(vui: &mut NV_ENC_CONFIG_H264_VUI_PARAMETERS) {
    vui.videoSignalTypePresentFlag = 1;
    vui.videoFormat = NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
    vui.videoFullRangeFlag = 0;
    vui.colourDescriptionPresentFlag = 1;
    vui.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
    vui.transferCharacteristics =
        NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
    vui.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;
}

fn depth(bits: u8) -> NV_ENC_BIT_DEPTH {
    if bits > 8 {
        NV_ENC_BIT_DEPTH::NV_ENC_BIT_DEPTH_10
    } else {
        NV_ENC_BIT_DEPTH::NV_ENC_BIT_DEPTH_8
    }
}

/// The preset's own settings, with what `spec` asks changed.
fn configure(config: &mut NV_ENC_CONFIG, spec: &Spec) -> Result<(), String> {
    config.frameIntervalP = i32::from(spec.b_frames) + 1;
    config.gopLength = spec.gop;
    let rc = &mut config.rcParams;
    rc.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_VBR;
    match spec.rate {
        Rate::Quality { quality, max } => {
            // No mean: the quality decides, under the ceiling.
            rc.averageBitRate = 0;
            rc.maxBitRate = max;
            rc.targetQuality = quality.clamp(1, 51);
            rc.targetQualityLSB = 0;
        }
        Rate::Vbr { average, max } => {
            rc.averageBitRate = average;
            rc.maxBitRate = max.max(average);
            rc.targetQuality = 0;
        }
    }
    rc.set_enableLookahead(u32::from(spec.lookahead > 0));
    rc.lookaheadDepth = u16::from(spec.lookahead);
    rc.set_enableAQ(u32::from(spec.spatial_aq));
    // Temporal AQ reads the lookahead.
    rc.set_enableTemporalAQ(u32::from(spec.temporal_aq && spec.lookahead > 0));
    rc.multiPass = if spec.multipass {
        NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_FULL_RESOLUTION
    } else {
        NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED
    };
    let b_ref = if spec.b_frames >= 2 {
        NV_ENC_BFRAME_REF_MODE::NV_ENC_BFRAME_REF_MODE_MIDDLE
    } else {
        NV_ENC_BFRAME_REF_MODE::NV_ENC_BFRAME_REF_MODE_DISABLED
    };
    // SAFETY: the preset read was this codec's, so this member of the
    // codec config union is the one in use.
    unsafe {
        match spec.out {
            Out::H264 => {
                if spec.bit_depth > 8 || spec.input.bit_depth() > 8 {
                    return Err("NVENC: H.264 is encoded from 8 bits to 8 bits here".into());
                }
                config.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
                let h264 = &mut config.encodeCodecConfig.h264Config;
                // An IDR a GOP: left unsaid, the preset's period is
                // infinite, and the key frames are plain I pictures that
                // later ones may reach past — a seek there decodes garbage.
                h264.idrPeriod = spec.gop;
                h264.useBFramesAsRef = b_ref;
                bt709(&mut h264.h264VUIParameters);
            }
            Out::Hevc => {
                config.profileGUID = if spec.bit_depth > 8 {
                    NV_ENC_HEVC_PROFILE_MAIN10_GUID
                } else {
                    NV_ENC_HEVC_PROFILE_MAIN_GUID
                };
                let hevc = &mut config.encodeCodecConfig.hevcConfig;
                hevc.idrPeriod = spec.gop;
                hevc.useBFramesAsRef = b_ref;
                hevc.set_chromaFormatIDC(1);
                // Eight bits in and ten out is the encoder's own
                // conversion; ten in and eight out is not offered.
                hevc.inputBitDepth = depth(spec.input.bit_depth());
                hevc.outputBitDepth = depth(spec.bit_depth);
                bt709(&mut hevc.hevcVUIParameters);
            }
            Out::Av1 => {
                let av1 = &mut config.encodeCodecConfig.av1Config;
                av1.idrPeriod = spec.gop;
                av1.inputBitDepth = depth(spec.input.bit_depth());
                av1.outputBitDepth = depth(spec.bit_depth);
                av1.colorPrimaries = NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
                av1.transferCharacteristics =
                    NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
                av1.matrixCoefficients = NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;
                av1.colorRange = 0;
            }
        }
    }
    if spec.input.bit_depth() > spec.bit_depth {
        return Err(format!(
            "NVENC: a {}-bit stream from {}-bit pictures: dither them first",
            spec.bit_depth,
            spec.input.bit_depth()
        ));
    }
    Ok(())
}

/// Whether a picture of `kind` the encoder gave is a sync sample: one a
/// player may start decoding at. For H.264 and HEVC only an IDR is — an I
/// picture (a scene cut) does not stop later pictures from referring past
/// it, and a seek that starts there decodes garbage. AV1's key frames are
/// reported as either.
fn is_sync(out: Out, kind: NV_ENC_PIC_TYPE) -> bool {
    match out {
        Out::Av1 => matches!(
            kind,
            NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR | NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I
        ),
        Out::H264 | Out::Hevc => kind == NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR,
    }
}

/// Encodes `frames` on CUDA device `device` as `spec` says, handing each
/// encoded picture to `take` in decoding order. Returns the seconds spent
/// in the encoder's calls.
///
/// A picture handed in is not always encoded at once: with lookahead or
/// B-frames the encoder holds pictures. Output buffers are read in the
/// order they were handed — which is the order pictures are decoded in —
/// each once the pool's depth of pictures has followed it, as NVIDIA's own
/// encoder samples read them; the rest after the end of the stream.
pub fn encode(
    device: usize,
    spec: &Spec,
    frames: impl IntoIterator<Item = Result<Frame, String>>,
    mut take: impl FnMut(Encoded) -> Result<(), String>,
) -> Result<f64, String> {
    // Each step said on stderr with TUILE_NVENC_TRACE set: the driver may
    // end the process without a word.
    let trace = std::env::var_os("TUILE_NVENC_TRACE").is_some();
    let step = |what: &str| {
        if trace {
            eprintln!("nvenc: {what}");
        }
    };
    step(&format!("{spec:?}"));
    let cuda = CudaContext::new(device).map_err(|e| format!("CUDA device {device}: {e:?}"))?;
    let encoder = Encoder::initialize_with_cuda(cuda).map_err(told)?;
    step("encoder open");
    let guid = guid(spec);
    if !encoder.get_encode_guids().map_err(told)?.contains(&guid) {
        return Err(format!("NVENC: this card does not encode {:?}", spec.out));
    }
    if spec.b_frames > 0 {
        let most = encoder
            .get_encode_caps(guid, NV_ENC_CAPS::NV_ENC_CAPS_NUM_MAX_BFRAMES)
            .map_err(told)?;
        if most < i32::from(spec.b_frames) {
            return Err(format!(
                "NVENC: {} B-frames asked of {:?}, the card does {most}",
                spec.b_frames, spec.out
            ));
        }
    }
    let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_HIGH_QUALITY;
    let preset = preset_guid(spec);
    let mut config = encoder
        .get_preset_config(guid, preset, tuning)
        .map_err(told)?
        .presetCfg;
    configure(&mut config, spec)?;
    step("configured");
    let (width, height) = spec.size;
    let mut init = EncoderInitParams::new(guid, width as u32, height as u32);
    init.preset_guid(preset)
        .tuning_info(tuning)
        .framerate(spec.fps, 1)
        .enable_picture_type_decision()
        .encode_config(&mut config);
    let session = encoder
        .start_session(buffer_format(spec.input), init)
        .map_err(told)?;
    step("session started");
    let pool = spec.buffers();
    let mut inputs = Vec::with_capacity(pool);
    let mut outputs = Vec::with_capacity(pool);
    for _ in 0..pool {
        inputs.push(session.create_input_buffer().map_err(told)?);
        outputs.push(session.create_output_bitstream().map_err(told)?);
    }
    // A buffer says how wide its rows are when it is first locked, and they
    // may be wider than the picture's.
    drop(inputs[0].lock().map_err(told)?);
    let pitch = match inputs[0].pitch() as usize {
        0 => width * if spec.input == Input::P010 { 2 } else { 1 },
        pitch => pitch,
    };
    step(&format!("{pool} buffers, pitch {pitch}"));
    let bytes = spec.input.bytes(spec.size);
    // A picture's output is read once the encoder can no longer be holding
    // it: `delay` pictures after it was handed in. Reading one sooner —
    // when an encode call answers success with lookahead on, which
    // means one picture is out, not every one handed in — dies in the
    // driver (measured on an L4, driver 580: a segmentation fault).
    let delay = pool - 1;
    let (mut sent, mut got) = (0usize, 0usize);
    let mut busy = 0.0f64;
    let out = spec.out;
    for frame in frames {
        let frame = frame?;
        if frame.data.len() != bytes {
            return Err(format!(
                "a {width}×{height} {:?} picture is {bytes} bytes, not {}",
                spec.input,
                frame.data.len()
            ));
        }
        let began = Instant::now();
        let slot = sent % pool;
        {
            let mut lock = inputs[slot].lock().map_err(told)?;
            let planes = spec.input.pitched(&frame.data, spec.size, pitch);
            // SAFETY: `planes` is exactly a picture of the session's size
            // and format at the buffer's pitch, which the buffer was made
            // to hold.
            unsafe { lock.write(&planes) };
        }
        loop {
            let params = EncodePictureParams {
                input_timestamp: frame.shown,
                ..Default::default()
            };
            let result = session.encode_picture(&mut inputs[slot], &mut outputs[slot], params);
            if frame.shown < 3 {
                step(&format!(
                    "picture {}: {:?}",
                    frame.shown,
                    result.as_ref().err().map(|e| e.kind())
                ));
            }
            match result {
                // Asked for more input, or not: either way the picture is
                // in, and outputs are read by their delay.
                Ok(()) => break,
                Err(e) if e.kind() == ErrorKind::NeedMoreInput => break,
                Err(e) if e.kind() == ErrorKind::EncoderBusy => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(e) => return Err(told(e)),
            }
        }
        sent += 1;
        while got + delay < sent {
            take(read(out, &mut outputs[got % pool])?)?;
            got += 1;
        }
        busy += began.elapsed().as_secs_f64();
    }
    let began = Instant::now();
    loop {
        match session.end_of_stream() {
            Ok(()) => break,
            Err(e) if e.kind() == ErrorKind::EncoderBusy => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => return Err(told(e)),
        }
    }
    while got < sent {
        take(read(out, &mut outputs[got % pool])?)?;
        got += 1;
    }
    Ok(busy + began.elapsed().as_secs_f64())
}

/// The picture in `output`, waited for: its bytes, whether it is a sync
/// sample, and the frame it is shown as.
fn read(out: Out, output: &mut nvidia_video_codec_sdk::Bitstream<'_>) -> Result<Encoded, String> {
    let lock = output.lock().map_err(told)?;
    Ok(Encoded {
        key: is_sync(out, lock.picture_type()),
        shown: lock.timestamp(),
        data: lock.data().to_vec(),
    })
}

/// The film as an mp4, by NVENC.
pub struct NvencFilm {
    path: PathBuf,
    /// Bits a second; 0 for the codec's own by the picture's size.
    bitrate: u32,
    codec: NvencCodec,
    /// The CUDA device the encoder is opened on.
    device: usize,
    pictures: Option<SyncSender<Vec<u8>>>,
    /// For a spatial film: each frame's two eyes, and its frame rate until
    /// the first frame opens the encoder.
    eyes: Option<SyncSender<(Vec<u8>, Vec<u8>)>>,
    fps: u32,
    encoder: Option<JoinHandle<Result<f64, String>>>,
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

    /// The encode a single-view film of `size` at `fps` gets: the
    /// mezzanine's profile, or H.264 and AV1 as a render node writes them —
    /// the bitrate asked for, else the H.264 profile's by the picture's
    /// size.
    fn spec(&self, size: (usize, usize), fps: u32) -> Result<Spec, String> {
        let node = match self.codec {
            NvencCodec::Mezzanine => NodeCodec::Mezzanine,
            NvencCodec::H264 => NodeCodec::H264,
            NvencCodec::Av1 => NodeCodec::Av1,
            NvencCodec::MvHevc => return Err("NVENC: MV-HEVC is a film of two eyes".into()),
        };
        // Eight bits handed in; ten out of the mezzanine is NVENC's own
        // conversion.
        Ok(node_spec(node, self.bitrate, Input::Iyuv, size, fps))
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
    path: PathBuf,
) -> Result<f64, String> {
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
                key = is_sync(Out::Hevc, lock.picture_type());
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
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, film).map_err(|e| e.to_string())?;
    Ok(busy)
}

impl Sink for NvencFilm {
    fn open(&mut self, width: u32, height: u32, fps: u32) -> Result<(), Error> {
        if self.codec == NvencCodec::MvHevc {
            return Err("a spatial film has two eyes: render it in stereo".into());
        }
        let size = (width as usize, height as usize);
        self.size = size;
        let fps = fps.max(1);
        let spec = self.spec(size, fps)?;
        let (pictures, waiting) = sync_channel::<Vec<u8>>(WAITING);
        let (device, path) = (self.device, self.path.clone());
        self.pictures = Some(pictures);
        let span = crate::sink::encoder_span();
        self.encoder = Some(std::thread::spawn(move || {
            let _in = span.enter();
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            let mut film = Package::at(&path, spec.out, size, fps)?;
            let frames = waiting.into_iter().enumerate().map(|(i, i420)| {
                Ok(Frame {
                    data: i420,
                    shown: i as u64,
                })
            });
            let busy = encode(device, &spec, frames, |e| film.take(e))?;
            film.finish()?;
            Ok(busy)
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
            // Spatial films keep the bitrate they are asked for: both eyes.
            let bitrate = if bitrate == 0 { 30_000_000 } else { bitrate };
            let path = self.path.clone();
            self.eyes = Some(sender);
            let span = crate::sink::encoder_span();
            self.encoder = Some(std::thread::spawn(move || {
                let _in = span.enter();
                encode_eyes(waiting, device, size, fps, bitrate, stereo, path)
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
        self.seconds = encoder.join().map_err(|_| "the encoder panicked")??;
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
        for out in [Out::H264, Out::Hevc] {
            assert!(is_sync(out, NV_ENC_PIC_TYPE_IDR));
            assert!(!is_sync(out, NV_ENC_PIC_TYPE_I));
            assert!(!is_sync(out, NV_ENC_PIC_TYPE_P));
        }
        assert!(is_sync(Out::Av1, NV_ENC_PIC_TYPE_I));
    }

    /// A render node's codec gives the shared spec, handed eight bits; an
    /// H.264 session from ten bits is refused.
    #[test]
    fn a_render_node_s_encode_is_the_shared_spec_and_h264_takes_eight_bits() {
        let mezzanine = NvencFilm::at("m.mp4", 0, NvencCodec::Mezzanine)
            .spec((1920, 1080), 30)
            .expect("spec");
        assert_eq!(
            mezzanine,
            node_spec(NodeCodec::Mezzanine, 0, Input::Iyuv, (1920, 1080), 30)
        );
        let mut h264 = NvencFilm::at("w.mp4", 45_000_000, NvencCodec::H264)
            .spec((3840, 2160), 30)
            .expect("spec");
        h264.input = Input::P010;
        // SAFETY: a config of plain values; all zero is a valid one.
        let mut config: NV_ENC_CONFIG = unsafe { std::mem::zeroed() };
        assert!(configure(&mut config, &h264).is_err(), "no 10-bit H.264");
    }
}
