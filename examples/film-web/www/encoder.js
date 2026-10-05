// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// Which H.264 encoder this browser has for a picture size. Shared by the page,
// which asks before offering a size, and the workers, which encode.

// High profile from level 4.0 up to 6.2, then Main and Baseline: the first
// the browser accepts wins. Level 5.1 stops at 4096×2304; 6.x reaches 8K, on
// the browsers and machines that have such an encoder.
const CODECS = [
  "avc1.640028", "avc1.640032", "avc1.640033", "avc1.640034",
  "avc1.64003c", "avc1.64003d", "avc1.64003e",
  "avc1.4d0033", "avc1.42e033",
];

/// The encoder configuration for this size, or null when there is none.
export async function findEncoder(width, height, fps, bitrate) {
  if (typeof VideoEncoder === "undefined") return null;
  for (const codec of CODECS) {
    const config = {
      codec, width, height, bitrate, framerate: fps,
      avc: { format: "avc" },
      latencyMode: "quality",
    };
    try {
      if ((await VideoEncoder.isConfigSupported(config)).supported) return config;
    } catch (e) { /* a size the browser refuses to even consider */ }
  }
  return null;
}

export async function encoderConfig(width, height, fps, bitrate) {
  const config = await findEncoder(width, height, fps, bitrate);
  if (!config) throw new Error(`no H.264 encoder for ${width}×${height}`);
  return config;
}
