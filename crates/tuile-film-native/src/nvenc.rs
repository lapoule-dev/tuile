// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The film as an mp4, AV1 or H.264 by an NVIDIA card's encoder.
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
    NV_ENC_BUFFER_FORMAT, NV_ENC_CODEC_AV1_GUID, NV_ENC_CODEC_H264_GUID, NV_ENC_PARAMS_RC_MODE,
    NV_ENC_PIC_TYPE, NV_ENC_PRESET_P4_GUID, NV_ENC_TUNING_INFO,
};
use nvidia_video_codec_sdk::{EncodeError, Encoder, EncoderInitParams, EncoderInput, ErrorKind};
use tuile_mp4::{annex_b_to_avcc, nal_units, Codec, Muxer, ParameterSets};

use crate::av1::{av1c_of, obus};
use crate::sink::Sink;
use crate::Error;

/// What the card is asked to encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvencCodec {
    Av1,
    H264,
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
        let key = matches!(
            lock.picture_type(),
            NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR | NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I
        );
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

impl Sink for NvencFilm {
    fn open(&mut self, width: u32, height: u32, fps: u32) -> Result<(), Error> {
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

    fn close(&mut self) -> Result<(), Error> {
        self.pictures = None;
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
