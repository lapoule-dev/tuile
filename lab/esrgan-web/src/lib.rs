// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The compact Real-ESRGAN network (SRVGGNetCompact) in Burn, run in the browser on WebGPU.
//!
//! The network is a plain stack of 3x3 convolutions with per-channel PReLU at the input's resolution, a last
//! convolution to `3 * scale^2` channels, a pixel shuffle, and the input upsampled by nearest neighbour added back.
//! It is written here from that description with Burn's functional ops; the weights come from `prepare.py`, which
//! dumps the published PyTorch checkpoint as raw f32 in forward order.

use burn::nn::PixelShuffleConfig;
use burn::tensor::activation::prelu;
use burn::tensor::module::conv2d;
use burn::tensor::ops::ConvOptions;
use burn::tensor::{Device, Tensor, TensorData};

/// One 3x3 convolution, and the PReLU after it (every layer but the last has one).
#[derive(Clone)]
struct Layer {
    weight: Tensor<4>,
    bias: Tensor<1>,
    alpha: Option<Tensor<1>>,
}

/// SRVGGNetCompact: `num_conv` body convolutions of `feat` channels, at `scale`.
#[derive(Clone)]
pub struct Compact {
    layers: Vec<Layer>,
    scale: usize,
}

/// What the published checkpoint `realesr-general-x4v3` uses.
pub const FEAT: usize = 64;
pub const NUM_CONV: usize = 32;
pub const SCALE: usize = 4;

impl Compact {
    /// Builds the network from its tensors as `prepare.py` writes them: for each layer its weight, its bias and
    /// (but the last) its PReLU slopes, flattened in PyTorch's order.
    pub fn from_flat(flat: &[f32], feat: usize, num_conv: usize, scale: usize, device: &Device) -> Result<Self, String> {
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
            let weight = Tensor::from_data(TensorData::new(take(cout * cin * 9)?, [cout, cin, 3, 3]), device);
            let bias = Tensor::from_data(TensorData::new(take(cout)?, [cout]), device);
            let alpha = if i <= num_conv { Some(Tensor::from_data(TensorData::new(take(cout)?, [cout]), device)) } else { None };
            layers.push(Layer { weight, bias, alpha });
        }
        if at != flat.len() {
            return Err(format!("{} weights left over after {at}", flat.len() - at));
        }
        Ok(Self { layers, scale })
    }

    /// `x`: `[batch, 3, h, w]` in [0, 1]. Returns `[batch, 3, h * scale, w * scale]`, unclamped.
    pub fn forward(&self, x: Tensor<4>) -> Tensor<4> {
        let options = ConvOptions::new([1, 1], [1, 1], [1, 1], 1);
        let mut y = x.clone();
        for layer in &self.layers {
            y = conv2d(y, layer.weight.clone(), Some(layer.bias.clone()), options.clone());
            if let Some(alpha) = &layer.alpha {
                y = prelu(y, alpha.clone());
            }
        }
        let y = PixelShuffleConfig::new(self.scale).init().forward(y);
        y + nearest(x, self.scale)
    }
}

/// Nearest-neighbour upsampling by an integer factor, by repetition: exact, whatever an interpolation op rounds.
fn nearest(x: Tensor<4>, s: usize) -> Tensor<4> {
    let [b, c, h, w] = x.dims();
    x.reshape([b, c, h, 1, w, 1]).expand([b, c, h, s, w, s]).reshape([b, c, h * s, w * s])
}

/// Interleaved RGB bytes, `h x w`, as the network's `[1, 3, h, w]` input in [0, 1].
pub fn input(rgb: &[u8], w: usize, h: usize, device: &Device) -> Tensor<4> {
    let floats: Vec<f32> = rgb.iter().map(|&v| f32::from(v) / 255.0).collect();
    Tensor::<3>::from_data(TensorData::new(floats, [h, w, 3]), device).permute([2, 0, 1]).unsqueeze()
}

#[cfg(target_family = "wasm")]
mod web {
    use super::*;
    use burn::tensor::wgpu::WgpuBackend;
    use wasm_bindgen::prelude::*;

    fn js(e: impl std::fmt::Display) -> JsValue {
        JsValue::from_str(&e.to_string())
    }

    /// The network on the browser's WebGPU device.
    #[wasm_bindgen]
    pub struct Upscaler {
        model: Compact,
        device: Device,
    }

    #[wasm_bindgen]
    impl Upscaler {
        /// Acquires the WebGPU device and uploads the weights (`weights.bin` as a `Float32Array`).
        pub async fn create(weights: Vec<f32>) -> Result<Upscaler, JsValue> {
            console_error_panic_hook::set_once();
            let device = Device::wgpu_options().graphics_api(WgpuBackend::WebGpu).init_async().await.map_err(js)?;
            let model = Compact::from_flat(&weights, FEAT, NUM_CONV, SCALE, &device).map_err(js)?;
            Ok(Upscaler { model, device })
        }

        pub fn scale(&self) -> usize {
            self.model.scale
        }

        /// Runs the network on interleaved RGB bytes; resolves to its output, CHW f32, unclamped.
        pub fn run(&self, rgb: Vec<u8>, w: usize, h: usize) -> js_sys::Promise {
            let (model, device) = (self.model.clone(), self.device.clone());
            wasm_bindgen_futures::future_to_promise(async move {
                let y = model.forward(input(&rgb, w, h, &device));
                let data = y.into_data_async().await.map_err(|e| js(format!("{e:?}")))?;
                let out: Vec<f32> = data.try_into_vec().map_err(|e| js(format!("{e:?}")))?;
                Ok(js_sys::Float32Array::from(out.as_slice()).into())
            })
        }
    }
}
