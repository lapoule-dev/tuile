// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The compact Real-ESRGAN network (SRVGGNetCompact) in Burn, run in the browser on WebGPU.
//!
//! The network is a plain stack of 3x3 convolutions with per-channel PReLU at the input's resolution, a last
//! convolution to `3 * scale^2` channels, a pixel shuffle, and the input upsampled by nearest neighbour added back.
//! It is written here from that description with Burn's functional ops; the weights come from `prepare.py`, which
//! dumps the published PyTorch checkpoint as raw f32 in forward order (`weights.rs` re-encodes them smaller).
//!
//! Two ways to compute a convolution, picked at load:
//! - [`Algo::Direct`]: Burn's `conv2d`, which on WebGPU without autotune is its direct kernel (one invocation per
//!   output vector, no shared-memory tiling), with an NCHW <-> NHWC copy on each side of every layer;
//! - [`Algo::Gemm`]: activations stay NHWC; each layer pads, gathers its nine shifted views into an
//!   `[h * w, 9 * c]` matrix (im2col) and multiplies it by the weights as a `[9 * c, c_out]` matrix with Burn's
//!   tiled matmul, which accumulates f16 inputs in f32.

pub mod weights;

use burn::nn::PixelShuffleConfig;
use burn::tensor::activation::prelu;
use burn::tensor::module::conv2d;
use burn::tensor::ops::{ConvOptions, PadMode};
use burn::tensor::quantization::{QuantScheme, QuantStore, QuantValue, ScaleDtype};
use burn::tensor::{Device, FloatDType, Tensor, TensorData};

/// How a convolution is computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algo {
    Direct,
    Gemm,
}

/// Burn's own int8 quantisation of the GEMM weights, kept quantised on the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quant {
    None,
    /// One symmetric scale for the whole weight matrix.
    PerTensor,
    /// One scale per block of 64 input values of an output channel (nine per output channel); a matrix whose
    /// rows are not a multiple of 64 (the first layer's 27) falls back to one scale.
    PerBlock,
}

const BLOCK: usize = 64;

#[derive(Clone)]
enum Kernel {
    /// `[c_out, c_in, 3, 3]`, PyTorch's layout.
    Conv(Tensor<4>),
    /// `[9 * c_in, c_out]`, rows ordered (dy, dx, c_in) like the im2col columns.
    Matrix(Tensor<2>),
}

/// One 3x3 convolution, and the PReLU after it (every layer but the last has one).
#[derive(Clone)]
struct Layer {
    kernel: Kernel,
    bias: Tensor<1>,
    alpha: Option<Tensor<1>>,
}

/// SRVGGNetCompact: `num_conv` body convolutions of `feat` channels, at `scale`.
#[derive(Clone)]
pub struct Compact {
    layers: Vec<Layer>,
    scale: usize,
    algo: Algo,
}

/// What the published checkpoint `realesr-general-x4v3` uses.
pub const FEAT: usize = 64;
pub const NUM_CONV: usize = 32;
pub const SCALE: usize = 4;

/// Burn's symmetric int8. WGSL has no 8-bit type, so CubeCL keeps four values in a `u32` (`PackedU32`); the CPU
/// backend the native tests run on quantises only to `Native` storage.
fn int8() -> QuantScheme {
    let store = if cfg!(test) { QuantStore::Native } else { QuantStore::PackedU32(0) };
    QuantScheme::default().with_value(QuantValue::Q8S).with_store(store)
}

/// The weight matrix `[k, n]`, quantised by Burn as `quant` asks.
fn quantise(matrix: Tensor<2>, quant: Quant) -> Tensor<2> {
    let k = matrix.dims()[0];
    match quant {
        Quant::None => matrix,
        Quant::PerBlock if k % BLOCK == 0 => {
            // blocks run along the input values of one output channel: quantise the transpose
            matrix.transpose().quantize_dynamic(&int8().per_block([1, BLOCK as u8], ScaleDtype::F32)).transpose()
        }
        _ => matrix.quantize_dynamic(&int8().per_tensor(ScaleDtype::F32)),
    }
}

impl Compact {
    /// Builds the network from its tensors as `prepare.py` writes them: for each layer its weight, its bias and
    /// (but the last) its PReLU slopes, flattened in PyTorch's order. Tensors take the device's default float dtype.
    pub fn from_flat(
        flat: &[f32],
        feat: usize,
        num_conv: usize,
        scale: usize,
        device: &Device,
        algo: Algo,
        quant: Quant,
    ) -> Result<Self, String> {
        if quant != Quant::None && algo != Algo::Gemm {
            return Err("Burn's quantised weights are only wired into the GEMM path".into());
        }
        let mut at = 0;
        let mut take = |n: usize| -> Result<Vec<f32>, String> {
            let part = flat.get(at..at + n).ok_or_else(|| format!("weights end at {}, need {}", flat.len(), at + n))?;
            at += n;
            Ok(part.to_vec())
        };
        let mut layers = Vec::with_capacity(num_conv + 2);
        for i in 0..num_conv + 2 {
            let (cin, cout) = match i {
                0 => (3, feat),
                i if i == num_conv + 1 => (feat, 3 * scale * scale),
                _ => (feat, feat),
            };
            let weight = Tensor::<4>::from_data(TensorData::new(take(cout * cin * 9)?, [cout, cin, 3, 3]), device);
            let kernel = match algo {
                Algo::Direct => Kernel::Conv(weight),
                Algo::Gemm => Kernel::Matrix(quantise(weight.permute([2, 3, 1, 0]).reshape([9 * cin, cout]), quant)),
            };
            let bias = Tensor::from_data(TensorData::new(take(cout)?, [cout]), device);
            let alpha = if i <= num_conv { Some(Tensor::from_data(TensorData::new(take(cout)?, [cout]), device)) } else { None };
            layers.push(Layer { kernel, bias, alpha });
        }
        if at != flat.len() {
            return Err(format!("{} weights left over after {at}", flat.len() - at));
        }
        Ok(Self { layers, scale, algo })
    }

    /// `x`: `[batch, 3, h, w]` in [0, 1]. Returns `[batch, 3, h * scale, w * scale]`, unclamped.
    pub fn forward(&self, x: Tensor<4>) -> Tensor<4> {
        self.forward_probed(x, false).0
    }

    /// The forward pass; with `probe`, also the largest |activation| after each layer, as one tensor.
    pub fn forward_probed(&self, x: Tensor<4>, probe: bool) -> (Tensor<4>, Option<Tensor<1>>) {
        let mut peaks = Vec::new();
        let y = match self.algo {
            Algo::Direct => {
                let options = ConvOptions::new([1, 1], [1, 1], [1, 1], 1);
                let mut y = x.clone();
                for layer in &self.layers {
                    let Kernel::Conv(weight) = &layer.kernel else { unreachable!() };
                    y = conv2d(y, weight.clone(), Some(layer.bias.clone()), options.clone());
                    if let Some(alpha) = &layer.alpha {
                        y = prelu(y, alpha.clone());
                    }
                    if probe {
                        peaks.push(y.clone().abs().max().reshape([1]));
                    }
                }
                PixelShuffleConfig::new(self.scale).init().forward(y)
            }
            Algo::Gemm => {
                let [b, _, h, w] = x.dims();
                let mut y = x.clone().permute([0, 2, 3, 1]);
                for layer in &self.layers {
                    let Kernel::Matrix(weight) = &layer.kernel else { unreachable!() };
                    let (c, cout) = (y.dims()[3], weight.dims()[1]);
                    let p = y.pad([(1, 1), (1, 1), (0, 0)], PadMode::Constant(0.0));
                    let mut views = Vec::with_capacity(9);
                    for dy in 0..3 {
                        for dx in 0..3 {
                            views.push(p.clone().slice([0..b, dy..dy + h, dx..dx + w, 0..c]));
                        }
                    }
                    let cols = Tensor::cat(views, 3).reshape([b * h * w, 9 * c]);
                    let mut z = cols.matmul(weight.clone()) + layer.bias.clone().unsqueeze::<2>();
                    if let Some(alpha) = &layer.alpha {
                        z = prelu(z, alpha.clone());
                    }
                    if probe {
                        peaks.push(z.clone().abs().max().reshape([1]));
                    }
                    y = z.reshape([b, h, w, cout]);
                }
                // pixel shuffle from NHWC: channel (c, i, j) goes to row h * s + i, column w * s + j
                let s = self.scale;
                y.reshape([b, h, w, 3, s, s]).permute([0, 3, 1, 4, 2, 5]).reshape([b, 3, h * s, w * s])
            }
        };
        let peaks = probe.then(|| Tensor::cat(peaks, 0));
        (y + nearest(x, self.scale), peaks)
    }
}

/// Nearest-neighbour upsampling by an integer factor, by repetition: exact, whatever an interpolation op rounds.
fn nearest(x: Tensor<4>, s: usize) -> Tensor<4> {
    let [b, c, h, w] = x.dims();
    x.reshape([b, c, h, 1, w, 1]).expand([b, c, h, s, w, s]).reshape([b, c, h * s, w * s])
}

/// `n` tiles of interleaved RGB bytes, `h x w` each, back to back, as the network's `[n, 3, h, w]` input in [0, 1].
pub fn input(rgb: &[u8], n: usize, w: usize, h: usize, device: &Device) -> Tensor<4> {
    let floats: Vec<f32> = rgb.iter().map(|&v| f32::from(v) / 255.0).collect();
    Tensor::<4>::from_data(TensorData::new(floats, [n, h, w, 3]), device).permute([0, 3, 1, 2])
}

/// The output as f32, whatever the network computed in.
pub fn as_f32(y: Tensor<4>) -> Tensor<4> {
    y.cast(FloatDType::F32)
}

#[cfg(test)]
mod tests {
    //! The GEMM path against Burn's own `conv2d`, and Burn's int8 weights against f32, on the CPU backend.
    use super::*;

    // feat 64 so that the per-block path (blocks of 64) is exercised on the body and last layers
    const SHAPE: (usize, usize, usize) = (64, 1, 2);

    fn flat() -> Vec<f32> {
        let (f, n, s) = SHAPE;
        let len = 3 * f * 9 + 2 * f + n * (f * f * 9 + 2 * f) + f * 3 * s * s * 9 + 3 * s * s;
        (0..len).map(|i| (i as f32 * 0.7071).sin() * 0.05).collect()
    }

    fn run(algo: Algo, quant: Quant) -> Vec<f32> {
        let device = Device::ndarray();
        let (f, n, s) = SHAPE;
        let net = Compact::from_flat(&flat(), f, n, s, &device, algo, quant).unwrap();
        let rgb: Vec<u8> = (0..2 * 6 * 5 * 3).map(|i| ((i * 37) % 256) as u8).collect();
        let y = net.forward(input(&rgb, 2, 5, 6, &device));
        assert_eq!(y.dims(), [2, 3, 12, 10]);
        y.into_data().to_vec().unwrap()
    }

    fn max_diff(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max)
    }

    #[test]
    fn gemm_matches_direct_convolution() {
        let d = max_diff(&run(Algo::Gemm, Quant::None), &run(Algo::Direct, Quant::None));
        assert!(d < 1e-4, "GEMM vs conv2d: max |Δ| {d}");
    }

    #[test]
    fn quantised_weights_stay_close() {
        let reference = run(Algo::Gemm, Quant::None);
        for quant in [Quant::PerTensor, Quant::PerBlock] {
            let d = max_diff(&run(Algo::Gemm, quant), &reference);
            // int8 weights move the output, a little: far more than f32 rounding (~1e-6), under a few hundredths
            assert!(d < 0.05 && d > 1e-4, "{quant:?}: max |Δ| {d}");
            eprintln!("{quant:?}: max |Δ| {d}");
        }
    }
}

#[cfg(target_family = "wasm")]
mod web {
    use super::*;
    use burn::tensor::ProfileOptions;
    use burn::tensor::wgpu::WgpuBackend;
    use wasm_bindgen::prelude::*;

    fn js(e: impl std::fmt::Display) -> JsValue {
        JsValue::from_str(&e.to_string())
    }

    fn set(o: &js_sys::Object, k: &str, v: impl Into<JsValue>) {
        let _ = js_sys::Reflect::set(o, &JsValue::from_str(k), &v.into());
    }

    /// The network on the browser's WebGPU device.
    #[wasm_bindgen]
    pub struct Upscaler {
        model: Compact,
        device: Device,
        dtype: String,
    }

    #[wasm_bindgen]
    impl Upscaler {
        /// Acquires the WebGPU device and uploads the weights (an ESRW file, see `weights.rs`).
        ///
        /// `dtype` is `f32` or `f16`; f16 falls back to f32 when the device lacks `shader-f16` (see [`Self::dtype`]).
        /// `algo` is `direct` or `gemm`; `quant` is `none`, `tensor` or `block` (Burn's int8, GEMM only).
        pub async fn create(weights: Vec<u8>, dtype: String, algo: String, quant: String) -> Result<Upscaler, JsValue> {
            console_error_panic_hook::set_once();
            let mut device = Device::wgpu_options().graphics_api(WgpuBackend::WebGpu).init_async().await.map_err(js)?;
            let dtype = match dtype.as_str() {
                "f16" => match device.configure(FloatDType::F16) {
                    Ok(()) => "f16",
                    Err(_) => "f32 (no shader-f16)",
                },
                _ => "f32",
            };
            let algo = match algo.as_str() {
                "direct" => Algo::Direct,
                "gemm" => Algo::Gemm,
                a => return Err(js(format!("unknown algo {a}"))),
            };
            let quant = match quant.as_str() {
                "none" => Quant::None,
                "tensor" => Quant::PerTensor,
                "block" => Quant::PerBlock,
                q => return Err(js(format!("unknown quant {q}"))),
            };
            let flat = weights::decode(&weights, FEAT, NUM_CONV, SCALE).map_err(js)?;
            let model = Compact::from_flat(&flat, FEAT, NUM_CONV, SCALE, &device, algo, quant).map_err(js)?;
            Ok(Upscaler { model, device, dtype: dtype.into() })
        }

        pub fn scale(&self) -> usize {
            self.model.scale
        }

        /// The float type the network computes in.
        pub fn dtype(&self) -> String {
            self.dtype.clone()
        }

        /// Whether Burn's fusion is compiled in.
        pub fn fusion() -> bool {
            cfg!(feature = "fusion")
        }

        /// Runs the network on `n` tiles of interleaved RGB bytes, back to back. Resolves to
        /// `{ out: Float32Array (NCHW, unclamped), gpuMs }`: `gpuMs` is the forward pass in device time
        /// (timestamp queries when the device has them), upload and readback excluded.
        pub fn run(&self, rgb: Vec<u8>, n: usize, w: usize, h: usize) -> js_sys::Promise {
            let (model, device) = (self.model.clone(), self.device.clone());
            wasm_bindgen_futures::future_to_promise(async move {
                let x = input(&rgb, n, w, h, &device);
                device.flush().map_err(js)?;
                let (y, window) = device
                    .profile_with(ProfileOptions::default().flush(), move || as_f32(model.forward(x)))
                    .map_err(js)?;
                let data = y.into_data_async().await.map_err(|e| js(format!("{e:?}")))?;
                let out: Vec<f32> = data.to_vec().map_err(|e| js(format!("{e:?}")))?;
                let gpu = window.resolve().await.map(|t| t.duration().as_secs_f64() * 1e3);
                let o = js_sys::Object::new();
                set(&o, "out", js_sys::Float32Array::from(out.as_slice()));
                set(&o, "gpuMs", gpu.map(JsValue::from_f64).unwrap_or(JsValue::NULL));
                Ok(o.into())
            })
        }

        /// The largest |activation| after each of the network's layers, in the dtype it computes in.
        pub fn probe(&self, rgb: Vec<u8>, w: usize, h: usize) -> js_sys::Promise {
            let (model, device) = (self.model.clone(), self.device.clone());
            wasm_bindgen_futures::future_to_promise(async move {
                let (_, peaks) = model.forward_probed(input(&rgb, 1, w, h, &device), true);
                let data = peaks.unwrap().cast(FloatDType::F32).into_data_async().await.map_err(|e| js(format!("{e:?}")))?;
                let v: Vec<f32> = data.to_vec().map_err(|e| js(format!("{e:?}")))?;
                Ok(js_sys::Float32Array::from(v.as_slice()).into())
            })
        }
    }
}
