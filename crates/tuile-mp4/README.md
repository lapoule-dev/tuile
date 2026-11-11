<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-mp4

H.264 samples into an mp4, in memory.

The browser's encoder (WebCodecs) hands out an `avcC` description and
length-prefixed access units; a native encoder hands out Annex-B. Both end
up here as the samples of one video track, written to a `Vec<u8>` — no
file, so it runs the same in a wasm worker as in a binary.

Several encoders may feed one film, each owning a contiguous slice of it.
That only joins into something decodable if they all produced the same
parameter sets, so every slice's `avcC` is checked against the first and a
mismatch is an error, never a film that turns to garbage half-way.

Two more things a film needs from its container:

- **Phone clips** (`clip`): the video track of an mp4 or a QuickTime movie,
  with the turn its track header asks to show it upright and the colour it
  says (BT.709, or BT.2020 with HLG or PQ, from its `colr` box or its HEVC
  stream's VUI), for a decoder to read.
- **Sound** (`audio`): AAC frames laid beside a film's pictures, which are
  copied untouched — faststart, interleaved by the second, the encoder's
  priming hidden, cut at the pictures' end.

## Where it sits

No workspace dependency. Used by `tuile-farm` (joining segments) and
`tuile-film-native` (the films it encodes), and by the film pages in a
worker.

## Example

```rust
// Two NAL units behind Annex-B start codes.
let stream = [0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x68, 0xBB];
let units = tuile_mp4::nal_units(&stream);
assert_eq!(units, vec![&[0x67, 0xAA][..], &[0x68, 0xBB][..]]);
```

## Features

None. Wasm-clean.

## Documentation

- [`docs/22-film-jobs.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/22-film-jobs.md)
