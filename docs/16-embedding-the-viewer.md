# 16 — Embedding the viewer

The interactive viewer is a library, `tuile-viewer`: the window, the camera and
its controls, the start flags, the steering URLs, the scripting dictionary, the
last-view file and the journal. None of it knows where the ground comes from.
`examples/wgpu-viewer` is forty lines of decisions on top of it — the public
connectors — and another program can make different ones and keep everything
else.

## What a host says

A host implements one trait, `tuile_viewer::ViewerHost`, and calls
`tuile_viewer::main` from its own `main`:

```rust
#[async_trait::async_trait]
pub trait ViewerHost: Send + Sync + 'static {
    fn identity(&self) -> Identity;
    fn terrain(&self) -> TerrainChoice;
    fn imagery(&self) -> Vec<ImageryChoice>;
    async fn connect(&self) -> Result<Arc<dyn Sources>, String>;
}

pub fn main(host: impl ViewerHost) -> !;
```

| answer | what it is |
|---|---|
| `Identity` | `name` (title bar, alerts, and the directories below), `bundle_identifier`, `scheme` (the steering URLs: `<scheme>://goto?…`), `executable` (for `--help`), `store` (the name of the on-disk tile store), `credentials` (one sentence for `--help`) |
| `TerrainChoice` | the asset number to ask the sources for, and the namespace its tiles are stored under |
| `ImageryChoice` | per layer: `key` (flag, URL, script), `name` (for a person), `asset`, `attribution`, `cache` (store namespace — one per layer, always) |
| `connect` | finds the host's credentials wherever the host keeps them and returns a `Sources`; an `Err` is a sentence shown to the person, in a window when there is no terminal |

`Sources` is `tuile_planetary::Sources` (re-exported by `tuile-viewer`, and by
`tuile-bake`, which has always taken one): terrain and imagery **by asset
number**.

```rust
#[async_trait::async_trait]
pub trait Sources: Send + Sync {
    async fn terrain(&self, asset: i64)
        -> Result<(LayerJson, Arc<dyn TerrainSource>), String>;
    async fn imagery(&self, asset: i64)
        -> Result<Arc<dyn ImageryProvider>, String>;
}
```

The grid an imagery layer is cut on is not part of the list: the provider says
it (`ImageryProvider::tiling_scheme`), and web-mercator and geographic layers
drape alike.

## What the library does with the answers

- **Directories follow the name.** On macOS:
  `~/Library/Application Support/<name>/` (the `token` file, `last-view`),
  `~/Library/Logs/<name>/viewer.log`; the tile store is
  `<user caches>/tuile/<store>/`. `Identity::support_dir`, `log_file`,
  `credential_file` and `caches_dir` return them.
- **Credentials are the host's business**, and `tuile_viewer::credential(VAR)`
  is there for a host that wants the public binary's rules: the environment
  variable first and always, otherwise the `token` file in the support
  directory, never logged.
- **The store is the library's.** Terrain and every imagery layer go through
  one store, each under the namespace its choice names.
- **Everything else is unchanged**: the flags, the keys, the URL vocabulary,
  the scripting dictionary and its property table are the library's, so a
  script written for one host works on another with the application's name and
  the URL scheme swapped.

## A host, whole

```rust
use std::sync::Arc;
use tuile_viewer::{Identity, ImageryChoice, Sources, TerrainChoice, ViewerHost};

struct Atlas;

#[async_trait::async_trait]
impl ViewerHost for Atlas {
    fn identity(&self) -> Identity {
        Identity {
            name: "Atlas".into(),
            bundle_identifier: "org.example.atlas".into(),
            scheme: "atlas".into(),
            executable: "atlas".into(),
            store: "atlas-tiles".into(),
            credentials: "The key is read from ATLAS_KEY, or from the file \
                          `token` in the application's support directory.".into(),
        }
    }
    fn terrain(&self) -> TerrainChoice {
        TerrainChoice { asset: 1, cache: "terrain-1".into() }
    }
    fn imagery(&self) -> Vec<ImageryChoice> {
        vec![
            ImageryChoice::new("aerial", "Aerial", 10, "© the aerial survey"),
            ImageryChoice::new("labels", "Aerial with labels", 11, "© the aerial survey"),
        ]
    }
    async fn connect(&self) -> Result<Arc<dyn Sources>, String> {
        let key = tuile_viewer::credential("ATLAS_KEY").map_err(|e| e.to_string())?;
        Ok(Arc::new(atlas_service::Tiles::open(key).await?)) // implements Sources
    }
}

fn main() {
    tuile_viewer::main(Atlas)
}
```

## The application bundle

`tuile-viewer-bundle` packages any host's executable; the four words must be
the ones `identity()` returns, or the system delivers the URLs elsewhere:

```bash
cargo run --release -p tuile-viewer-bundle -- app --out ~/Applications \
    --binary target/release/atlas --package atlas \
    --name Atlas --identifier org.example.atlas --scheme atlas --icon atlas-1024.png
```

The same flags package the public viewer a second time under another name — a
copy to run tests against while the first is in use. A renamed bundle of the
public binary carries its three words in `LSEnvironment`
(`TUILE_VIEWER_NAME`, `TUILE_VIEWER_IDENTIFIER`, `TUILE_VIEWER_SCHEME`), which
`tuile_viewer::renamed_by_environment` reads.

## Switching imagery

Every layer of `imagery()` can be chosen at start (`--imagery <key>`) and
changed in a running session — the `I` key, `<scheme>://imagery?name=<key>`, a
script's `set imagery to "<key>"`. The host does nothing for this: a layer is
resolved through `Sources::imagery` the first time it is asked for, on the
session's async runtime, and kept.

The switch itself is the engine's, and is available to any host of
`tuile-planetary`, viewer or not:

- `tuile_planetary::SwitchableImagery` is an `ImageryProvider` holding another,
  with a `generation` that goes up when the one it holds is replaced
  (`switch_to`). Build the globe over it instead of over the provider.
- The loader keeps decoded tiles per generation, stamps each layer of a drape
  with the generation it came from (`ImageryLayer::source` — a consumer shares
  textures by `(source, coord)`), and reports the generation as its
  `TileLoader::epoch`.
- The geometry server refreshes every resident tile from an earlier epoch:
  coarse first, what is on screen first, and **keeping the old content until
  the new has arrived**. The consumer receives a second `Content` under the
  same id and replaces in place — create the new surface, activate it, release
  the old.

No tile is ever without imagery because of a switch, and the two layers may be
cut on different grids.

## What is not pluggable

One terrain per session. The scripting dictionary's title and suite keep the
engine's name in every host. The location service, the tape and the metrics
endpoint are the library's and are not hooks.
