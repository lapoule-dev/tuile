# The viewer as a macOS application

`tuile-wgpu-viewer` is a command-line program. On macOS it can also be packaged
as `Tuile.app`: the same executable, in a bundle with an icon, that starts from
the Dock or the Finder, can be steered by other programs, and can be granted
the current location.

## Building it

```bash
cargo run --release -p tuile-viewer-bundle -- app --out ~/Applications
```

This builds the viewer in release and writes `~/Applications/Tuile.app`
(`target/bundle/` without `--out`). The bundler lays out
`Contents/MacOS/tuile-wgpu-viewer`, `Contents/Info.plist`,
`Contents/Resources/Tuile.icns`, signs the bundle and then checks its own work
with `plutil -lint` and `codesign --verify --strict`; it fails if either does.

The executable inside the bundle still takes every command-line flag when it is
run from a terminal.

## The access token

An application started from an icon has no shell environment. The token is read

1. from `CESIUM_ION_TOKEN` in the environment, if it is set — always first;
2. otherwise from `~/Library/Application Support/Tuile/token`.

The file holds the token on a line of its own (a `NAME=value` line also reads).
Create it yourself and keep it to yourself:

```bash
mkdir -p ~/Library/Application\ Support/Tuile
install -m 600 /dev/null ~/Library/Application\ Support/Tuile/token
$EDITOR ~/Library/Application\ Support/Tuile/token
```

No build creates this file, nothing puts a token in the bundle, and the token
is never logged. With neither the variable nor the file, the application says
so in a window and names the path.

## Steering the app

A running application — or one that is not running yet — takes commands as
URLs. The system delivers them as Apple events, so anything that can open a URL
can steer the view:

```bash
# from a shell
open "tuile://goto?lon=-2.86&lat=52.51&altitude=1200&heading=0&pitch=30"

# from AppleScript
osascript -e 'tell application "Tuile" to open location "tuile://north"'

# ask it where it is: the answer is one line, a goto URL
open "tuile://report" && cat ~/Library/Caches/Tuile/view.txt
```

| URL | effect |
|---|---|
| `tuile://goto?lon=&lat=&altitude=&heading=&pitch=` | puts the eye there; what is left out keeps its present value |
| `tuile://north` | north up, about the point at the centre of the view |
| `tuile://here` | centres on the current location |
| `tuile://view?freeze=on\|off&wireframe=on\|off` | the two display switches |
| `tuile://report` | writes the present view to `~/Library/Caches/Tuile/view.txt` |

The parameters are the command-line flags' — same names, same units, same
bounds, one table in the source. In the window, `C` copies the `goto` link of
the present view to the clipboard.

**What a URL can do is all in that table.** It moves the camera and flips two
display switches. It reads no file, runs no command and carries no credential.
Anything else — an unknown command or parameter, a value out of bounds, a URL
over 512 bytes — is dropped whole with one line in the log. `here` goes through
the system's own permission like the key does, and a view centred on the
current location is **not** written by `report` (the previous report is
removed): the permission you gave the application is not handed on to whoever
sends it a URL.

There is no network listener and no socket: the only way in is the URL scheme.

There is no scripting dictionary. `open location` above is the standard
"open URL" Apple event, not a vocabulary of the application's own; a dictionary
(`go to` with named parameters, a readable `view` property) would need an
`.sdef` in the bundle and command classes registered with the scripting
runtime.

## The current location

`L`, `--here` and `tuile://here` ask the system's location service. From the
bundle the system shows its permission dialog once — with the sentence in
`Info.plist` — and remembers the answer against the bundle's signature. Run
unbundled from a terminal, the same executable is never asked: the request
stays undetermined, and the viewer gives up after a minute and says so.

## What is signed, and what is not

The bundle is signed **ad hoc** (`codesign --sign -`): a seal with no signer.
That is enough for this machine — the system has a stable identity to attach
the location permission to, and the bundle is checked against tampering — and
it is worth nothing on another one. A build meant to be handed to someone else
additionally needs a Developer ID certificate, the hardened runtime with the
location entitlement, and notarisation; none of that is attempted here.
Rebuilding changes the ad-hoc signature, and the system may ask about the
location again.

## The icon

`icon-1024.png` is drawn by the bundler — a globe cut into the tiling scheme's
quadtree, finer toward the centre of the view — and not by hand:

```bash
cargo run --release -p tuile-viewer-bundle -- icon
```

No imagery and no third-party artwork go into it; a test holds the committed
file to the generator's output. The `.icns` (16 to 512 points, at 1× and 2×) is
packed from it at bundling time and is not committed.

## Why a small program and not a packaging tool

| tool | what it would do here | decision |
|---|---|---|
| `cargo-bundle` | `.app` from `[package.metadata.bundle]`, icon included | not used, and not tried: its documented metadata covers the identity, the icon and URL schemes; whether it can carry the two location strings was not established |
| `cargo-packager` | `.app`/`.dmg` from metadata, merges a custom plist, signing hooks | not used *yet*, and not tried: on paper it fits, and is the one to move to when a disk image or a Developer ID signature is wanted; today it is a large tool to install and pin for four files, and its output could not be checked by `cargo test` |
| `plist` | writes and reads back `Info.plist` | **used** |
| `icns` + `image` | packs the `.icns` from the master, in Rust, no `iconutil` | **used** |
| `apple-codesign` (`rcodesign`) | signing in Rust, ad hoc to notarised | not used: a very large dependency for one ad-hoc seal, and the system's `codesign` is the judge of the result either way |
| system `codesign`, `plutil` | sign, then verify | **used**, for the signature and as the bundler's own check |

The three crates are dependencies of `tuile-viewer-bundle` only. The two tools
marked "not tried" were judged from their documentation, not by running them.
