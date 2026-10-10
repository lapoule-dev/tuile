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
`Contents/Resources/Tuile.icns` and `Tuile.sdef`, signs the bundle and then
checks its own work with `plutil -lint`, `codesign --verify --strict` and
`sdef`; it fails if any of them does.

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

A running application — or one that is not running yet — is steered from
outside in two ways: URLs, and a scripting dictionary.

```bash
# a URL, from a shell
open "tuile://goto?lon=-2.86&lat=52.51&altitude=1200&heading=0&pitch=30"

# a URL, from AppleScript
osascript -e 'tell application "Tuile" to open location "tuile://north"'

# the dictionary: ask where the view is, and send it somewhere
osascript -e 'tell application "Tuile" to get {longitude, latitude, altitude, heading, pitch}'
osascript -e 'tell application "Tuile" to go to longitude -4.02 latitude 5.34 altitude 3000'
```

### URLs

| URL | effect |
|---|---|
| `tuile://goto?lon=&lat=&altitude=&heading=&pitch=` | puts the eye there; what is left out keeps its present value |
| `tuile://north` | north up, about the point at the centre of the view |
| `tuile://here` | centres on the current location |
| `tuile://view?freeze=on\|off&wireframe=on\|off` | the two display switches |
| `tuile://imagery?name=<layer>` | drapes another imagery layer; `name=next` takes the one after the present |

The parameters are the command-line flags' — same names, same units, same
bounds, one table in the source. In the window, `C` copies the `goto` link of
the present view to the clipboard.

**A URL starts the application when it is not running**, and that start is a
plain one: it opens on the view the last session ended on (the default view the
first time), then goes where the URL says, and it carries no environment — a
`TUILE_SHELL` given to an earlier `open --env` is not there any more.

### The scripting dictionary

`sdef ~/Applications/Tuile.app` prints it; Script Editor opens it. On the
application object, read-only unless noted:

| property | |
|---|---|
| `longitude`, `latitude` | the eye, degrees, east and north positive |
| `altitude` | the eye, metres above the ellipsoid |
| `heading`, `pitch` | degrees clockwise from north; degrees below the horizon |
| `target longitude`, `target latitude`, `target height` | the point of the ellipsoid at the centre of the view |
| `range` | metres from the eye to that point |
| `field of view` | vertical, degrees |
| `view width`, `view height` | the drawable surface, device pixels |
| `wireframe`, `frozen` | the display switches — **read and write** |
| `view url` | the `tuile://goto?…` of the present view |
| `tile count` | tiles drawn in the last frame |
| `settled` | every tile drawn is the one selected, no coarser stand-in left: wait for this before capturing |

| `imagery` | the imagery layer draped, by its key; **settable** |
| `imagery name`, `imagery attribution` | the same layer as a person reads it, and whose pictures they are |

and three commands: `go to` (`longitude`, `latitude`, `altitude`, `heading`,
`pitch`, all optional), `north up`, `center on current location`.

```applescript
tell application "Tuile"
    go to longitude 2.3522 latitude 48.8566 altitude 2500 pitch 35
    repeat until settled
        delay 0.5
    end repeat
    get {target longitude, target latitude, range}
end tell
```

The answers come from a snapshot the render loop publishes once a frame, so a
question never waits on a frame and is at most one frame old; a command is
queued and carried out by the render loop on its next turn, so read a property
a moment *after* a command, not in the same breath. The target is taken on the
ellipsoid, not on the relief: over mountains the true ground point is nearer
than `range` says. A value out of the flags' bounds is an error the script
receives (number -50, with the sentence the command line would print), and
nothing moves.

The window's title carries the position too — `Tuile — 52.5100°N 2.8600°W ·
1200 m` — refreshed a few times a second, for a person and for anything that
can read a window's name.

### Imagery layers

The layers are the host's list; the public viewer has three, shown by
`--help`:

| key | name | what it is |
|---|---|---|
| `aerial` | Aerial | aerial and satellite photography (the default) |
| `labels` | Aerial with labels | the same, with roads and place names drawn over it |
| `satellite` | Satellite | one cloudless mosaic of the whole planet: coarser, and the same colour everywhere |

A layer is chosen at start with `--imagery <key>`, and changed in a running
session by the `I` key (the next layer), by `tuile://imagery?name=<key>`, or
by a script:

```applescript
tell application "Tuile" to set imagery to "labels"
tell application "Tuile" to get {imagery, imagery name, imagery attribution}
```

**The ground is never bare while the layer changes.** Every tile keeps the old
imagery until its new drape has arrived, and is then replaced in place —
coarse tiles first, so the globe turns over and then sharpens. For a while the
two layers are on screen together. Each layer is stored under its own
namespace of the tile store, so coming back to one is served from disk. The
title bar names the layer and its attribution; the journal
(`~/Library/Logs/Tuile/viewer.log`) records how long each change took to cross
the screen and what it cost in GPU memory.

A layer the service does not have for this account changes nothing: the title
says it is not available and the present layer stays.

### What steering can and cannot do

It moves the camera, flips two display switches, picks among the host's
imagery layers, and reads the view. It reads
no file, runs no command and carries no credential; there is no network
listener and no socket. A URL that is malformed, unknown, out of bounds or over
512 bytes is dropped whole with one line in the log.

**The current location is not handed on.** `here` and `center on current
location` go through the system's own permission, like the key. Once the view
has been centred there, `longitude`, `latitude`, `target longitude`, `target
latitude` and `view url` answer `missing value`, the title says "position
withheld", and the view is not remembered for the next start — until a `go to`
puts the view somewhere named. The permission you gave the application is not
passed to whoever scripts it.

## When it stops

A session started from an icon has no terminal, so it leaves its reasons in
`~/Library/Logs/Tuile/viewer.log`: a line when it starts, a line saying why it
ended — the window was closed, Esc, quit from the menu, a signal, an error, a
panic. A failure to start is also shown in a window. A session whose last line
is `started` was killed outright.

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
