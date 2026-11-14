# Compositing at the join

*Exploration and work plan, 2026-11-14.*

## Why

A film with photos or phone clips is delivered today in two encodes. The
render service joins the mezzanine segments and compresses the whole film
(`tuile_farm::compress`). Then the host's own post-delivery pass downloads
the delivered file, has ffmpeg decode all of it, lays each picture over the
film's pauses (full screen, fade in, slow zoom, fade out; clips silent and
cut to the film's shape), and encodes all of it again. On the host's worker
that pass takes 8 cores for 4–5 minutes, adds a second lossy generation, and
is the last ffmpeg in a chain that is otherwise all Rust.

The decision (Laurent, 2026-11-14):

- the pictures are laid over the **uncompressed** film, where the segments
  are joined, and never in the render or in the compression;
- the compression stays purely technical and knows nothing of pictures;
- the mezzanine gets **key frames** where the pictures start and end, so
  that the join can cut and splice there without decoding.

## The pipeline, before and after

```text
before
  render segments (mezzanine) ─► join (sample copy) ─► compress ─► delivered
                                                                    └─► host: download, ffmpeg decode ALL,
                                                                        overlay, ffmpeg encode ALL, upload

after
  render segments (mezzanine, IDR forced at each range's first frame and the frame after its last)
     │
     ├─► composite job, one per range, in parallel, each started once its segments are in:
     │     decode ONLY the range's frames ─► lay the still or the clip ─► encode ONLY those frames
     │     with the render's own encoder ─► an "inserted" segment, IDR to IDR
     │
     └─► join: render segments cut at those IDRs + inserted segments, sample copy ─► compress (unchanged)
```

The compression decodes the joined film either way, so there is one lossy
generation for the whole film plus one, near-transparent, for the frames
under a picture. Before, every frame of the film paid two.

## What tuile knows, and what it does not

tuile knows one generic thing: **frames laid over ranges of a mezzanine.** A
range of film frames, a layer (a still picture or a clip, by its key in the
run store), an opacity ramp at either end, a slow zoom, and how many views
the picture packs side by side. Nothing in tuile is called a photo, a pause
or a media. The host decides what goes where. It prepares its photos: EXIF
orientation, the crop with room for the zoom, upload to the run store. It
gives the workflow the list of ranges.

## Facts this rests on

- Mezzanine (`Profile::mezzanine`): HEVC Main10, **no B-frames, no
  lookahead**, an IDR a second (`gop_seconds: 1.0`). Every IDR is a clean
  cut: no picture after it references a picture before it.
- Render segments are cut by frames. `SegmentOrder.frames` holds the film's
  1-based, inclusive frames. The sink numbers its pictures from 0 within the
  segment. Receipts (`tuile_farm::assemble::Receipt`) say each task's
  `first..=last` and its segments; a segment is `<run>/seg<g>.mp4`.
- The join (`tuile_mp4::film::join_into`) copies samples and refuses
  segments whose stream descriptions differ (`same_stream`). An inserted
  segment must therefore come out of the **same encoder session settings**
  as the render's segments. That is guaranteed by encoding it through the
  render's own sink (`setup::film_sink`) with the same codec and bitrate.
  The `Spec`, the input layout and the conversions are then identical by
  construction.
- Forcing an IDR:
  - VideoToolbox: `kVTEncodeFrameOptionKey_ForceKeyFrame`, already used
    for the GOP.
  - NVENC: `NV_ENC_PIC_FLAG_FORCEIDR` in `encodePicFlags`. The safe
    binding of our fork did not expose that field. A fork commit adds
    `EncodePictureParams::flags` (lapoule-dev/nvidia-video-codec-sdk,
    branch `feat/force-idr`, on top of `feat/decoder`), pinned by rev.
- The host's pictures today (stl-farm `media.rs`):
  - fade = `min(0.5 s, seconds / 4)` in and out, on alpha;
  - photo: linear zoom 1.00 → 1.08 over the range, centred, on a photo
    prepared at cover size × 2 (≤ 1920 px) or × 1;
  - clip: scaled to cover, centre-cropped, resampled to the film's rate,
    its last frame held if it is too short, cut to the range, no audio;
  - two views side by side: the same picture in each eye, at the same place
    (screen depth).

  The composite reproduces those numbers, which the host passes in.

## Step A — key frames and the splice (tuile PR 1)

Branch `feat/mezzanine-keys-splice`, from `main`.

1. **Fork**: `EncodePictureParams::flags` (done, `b6f9433`), and the
   `nvidia-video-codec-sdk` rev in `crates/tuile-film-native/Cargo.toml`
   moved onto it.
2. **`encode::Frame::key`**: an IDR forced on this picture.
   - `nvenc::encode` passes `NV_ENC_PIC_FLAG_FORCEIDR`.
   - `videotoolbox::encode` passes `ForceKeyFrame` (as it does each GOP).
   - Every existing caller sets `key: false`.
3. **Key frames in a render**:
   - `SegmentOrder.key_frames: Vec<u32>` holds film frames, defaulting to
     empty.
   - `setup::film_sink(path, bitrate, codec, keys)` takes the picture
     indices *within the segment* where an IDR is forced (`NvencFilm` /
     `VtFilm::with_keys`).
   - A codec that cannot honour them (AV1 by rav1e, MV-HEVC) refuses a
     non-empty list rather than ignoring it.
   - `Service::segment` turns the film's frames into the segment's indices.
4. **`tuile_mp4::film::join_pieces`**: pieces = (file, sample range). Each
   piece must start on a sync sample, or the join is refused.
   `join_into` becomes `join_pieces` over whole files, byte for byte the
   same output.
5. **`tuile_farm::concat::splice`** and
   **`tuile_farm::assemble::gather_with(…, inserts)`**:
   - `Insert { first, last, key }`, with film frames as the receipts number
     them and `key` in the segments' store.
   - The gather fetches the inserted segments beside the render's. It
     reads each segment's frame count from its index, maps every segment
     onto film frames, and builds the pieces: render segments cut at
     `first` and `last + 1`, inserted segments between.
   - It checks that:
     - inserts are sorted and do not overlap, and lie inside the film;
     - each insert holds exactly `last − first + 1` frames;
     - the render has a sync sample at `first` and at `last + 1` (or a
       segment boundary there).

     If the render put no key frame where asked, the join says so and
     names the frame.
   - The film's frame count is checked as before.
6. **`CompressOrder.inserts: Vec<Insert>`** (default empty, skipped when
   empty): the join splices them, then the compression runs unchanged.

Tests (A):
- `join_pieces` over whole files gives the same bytes as before. A golden
  SHA-256 of today's join of synthetic segments is recorded *before* the
  refactor, then the test is watched red on a perturbed join.
- A splice of synthetic segments (keys every 10 samples):
  - frame count;
  - payload order: the render's samples, then the inserted ones, then the
    render's again;
  - sync flags (each insert's first sample a key, the render's sample after
    it a key);
  - contiguous timestamps.
- A splice whose cut is not at a key frame is refused, and the error names
  the frame.
- `gather_with`:
  - an insert that spans two render segments;
  - an insert at the film's first frame;
  - an insert whose frame count is wrong is refused.
- Mac, `--features videotoolbox`:
  - a mezzanine rendered with `key_frames` has IDRs exactly there (plus the
    GOP's);
  - a VideoToolbox-encoded inserted segment splices (same `hvcC` from a
    second session) and the joined film decodes back to every frame.
- `cargo check -p tuile-film-native --target x86_64-unknown-linux-gnu
  --features nvenc` (compiles the NVENC path; it cannot run here).

## Step B — the composite stage (tuile PR 2, stacked on A)

Branch `feat/composite-at-join`, stacked on A with `gh stack`.

1. **Contract** `tuile_farm::composite`:
   - `CompositeOrder`:
     - `{ run, render_id, chunks, segments: Store, fps, first, last }`;
     - `layer: Layer`;
     - `fade_seconds`;
     - `zoom: (from, to)`;
     - `views: u32` (copies side by side);
     - `codec`, `mbps`: the render's, so the inserted segment comes out of
       the same encoder;
     - `out`: the inserted segment's key in the segments' store.
   - `Layer::Still { key }` | `Layer::Clip { key }` (run store keys).
   - `CompositeDone { key, frames, bytes, fetch_seconds, decode_seconds,
     composite_seconds, encode_seconds }`.
2. **Library** `tuile_film_native::composite`:
   - `Track::range(a, b)` (tuile-mp4): a sub-track of a film, its samples
     where they are in the file. A range starting on a key decodes on its
     own.
   - `decode::pictures(path, a, b, take)`: the machine's decoder (NVDEC on
     Linux with `nvenc`, VideoToolbox on a Mac), pictures in the order
     shown, as 8-bit I420. Ten-bit pictures are dithered by
     `compress::p016_to_nv12`; the render's pictures are 8-bit to begin
     with.
   - `Layer` as pictures:
     - a still is decoded once (`image`) to RGB, then converted to BT.709
       limited Y'CbCr 4:4:4 at its own size;
     - a clip is decoded by the same decoder: H.264 or HEVC in MP4 or MOV,
       its first video track, audio ignored. Its rotation (`tkhd` matrix,
       0/90/180/270) is applied, and it is resampled in time to the film's
       rate, its last picture held.
   - Per frame:
     - a centred window of the layer at the zoom of that frame (`from → to`,
       linear over the range) covers the view's size (cover crop);
     - bilinear resample into each view;
     - `alpha = min(1, t / fade, (n − t) / fade)` over the film's picture.
     - Pure CPU on I420, parallel by rows over views. No new crate:
       `image` is already a dependency of the crate, and the YUV math is
       here and tested.
   - Encode: `setup::film_sink(out, bitrate, codec, [])`, the render's own
     sink.
   - Checks:
     - frames out == `last − first + 1`;
     - the first sample is a key;
     - the stream description equals the render's (else the join would
       refuse it later, so it is refused now, with both descriptions said).
3. **Service**: `Service::composite` and `POST /composite`, beside
   `/compress`. It reads the receipts, fetches only the segments that hold
   `first..=last` — as soon as their receipts are in, the rest of the film
   still rendering — fetches the layer, and waits its turn for the card (the
   renders' semaphore). It writes the inserted segment to the segments'
   store and answers `CompositeDone`.

Tests (B):
- Pure CPU:
  - YUV conversion of known colours;
  - fade ramp (first frame transparent, middle opaque, symmetric, fade
    clamped to a quarter);
  - zoom window (1.00 at the first frame, 1.08 at the last, centred);
  - cover crop of a portrait still into a landscape view;
  - two views get the same picture at the same place;
  - clip time resampling and last-frame hold;
  - rotation by the `tkhd` matrix.
- Mac (videotoolbox), end to end:
  - render a short synthetic mezzanine with keys;
  - composite a still over a range spanning two segments;
  - splice and decode back;
  - the frames outside the range are bit-identical to the plain join's
    decode, and the frames inside carry the still;
  - **a frame is extracted to PNG and opened**.
- Clip end to end on the Mac: a synthetic H.264 clip (VideoToolbox) laid
  over a range.
- Bench: composite seconds for N seconds of range at 1080p and 4K on the Mac
  (VideoToolbox). What remains for the L4 (NVDEC/NVENC) is said.

## Step C — the host (sportstracklive-rails)

Branch `feat/composite-media-at-join` from `origin/production`. tuile is
pinned on PR B's head commit while it is unreleased.

- Key frames: the segment order carries `key_frames` = each pause's
  `first` and `last + 1`, from `Film::pauses()`, known before rendering.
- Photo preparation stays in Rust (`media::prepare_photo`). Each media is
  fetched by its URL and uploaded to the run store
  (`<run>/media/<i>.jpg` or `.mp4`) by a new activity `prepare_media`. It
  runs while the film renders and returns the composite orders' layers.
- Workflow, behind `ctx.patched("media-at-join")`, asked early:
  - once rendering is done (all receipts), one `composite_media` activity
    per pause, in parallel; each posts `/composite` to the render service;
  - then `compress` with `inserts`;
  - no `insert_media` afterwards;
  - `add_music` unchanged.

  Unpatched histories replay as they ran (render → compress →
  insert_media → add_music).
- The ffmpeg path of `insert_media` (`media::filter_graph`, `ffmpeg_args`,
  `compose`, `probe`) is removed. The activity stays registered so that old
  histories can still be completed by a worker: it is the old workflow's
  step until no workflow from before the patch is running. Removing it
  follows once none is (an open question for Laurent: the activity without
  ffmpeg would fail. Either keep the ffmpeg path for in-flight films until
  they drain, or accept that they fail and retry).
- Tests:
  - workflow: activity order with and without the patch, composite jobs
    one per pause, inserts passed to compress;
  - media prep: the uploaded still's size and orientation;
  - the `key_frames` order field.

  Tests run on the k8s builder.

## Step D — validation (not deployed by this work)

The new path needs the render service image (tuile-film-serve with NVDEC
and NVENC) and the stl-render worker deployed. That is Laurent's and the
main session's call. The recipe:

1. A track with a film of 3 photos (one portrait, one with EXIF
   orientation 6, one landscape) and 1 phone clip (portrait iPhone HEVC,
   rotated), mono 1080p 30 fps; then the same as a side-by-side stereo
   film.
2. Render on production's current image (old path) and on the candidate,
   same plan.
3. Compare:
   - frame counts;
   - pause positions (extract the first, middle and last frame of each
     pause to PNG and open them);
   - fade timing;
   - zoom end;
   - the clip's orientation;
   - film duration;
   - files open in QuickTime and a browser.
4. Timings:
   - the old `insert_media` activity duration (8 cores, 4–5 min);
   - the new composite activities (each, and wall-clock from the last
     receipt to the join);
   - the compress's join seconds with and without inserts.

## Open questions

- In-flight workflows started before the patch still call `insert_media`.
  Keep its ffmpeg path until they drain (then a follow-up removes it), or
  drop it now?
- Inserted segments encoded on the L4 must produce the render segments'
  parameter sets byte for byte. They do on VideoToolbox (tested). On NVENC
  the encoder is the same card, driver and settings, so they should, but
  only the L4 can say. If they do not, the fallback is a second sample
  entry in the joined film, which the compression's decoder would then have
  to follow.
- Clips in other codecs than H.264/HEVC (VP9 from some Android phones) are
  refused by the composite, which fails that one range. Should the host
  skip the media (as it skips a media it cannot fetch today) or fail the
  film?
