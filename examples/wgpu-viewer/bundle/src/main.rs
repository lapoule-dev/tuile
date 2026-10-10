// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Packages the viewer as a macOS application: `Tuile.app`, with its icon,
//! its `Info.plist` and an ad-hoc signature.
//!
//! ```text
//! cargo run -p tuile-viewer-bundle -- app                # → target/bundle/Tuile.app
//! cargo run -p tuile-viewer-bundle -- app --out ~/Applications
//! cargo run -p tuile-viewer-bundle -- icon               # redraw macos/icon-1024.png
//! ```
//!
//! `app` builds the viewer in release, lays the bundle out, writes the
//! property list, packs the icon from the committed master, signs, and then
//! checks its own work with the system's `plutil -lint` and
//! `codesign --verify`. See `../macos/README.md` for what the bundle is, what
//! the signature is worth, and why this is a small program rather than an
//! installed packaging tool.
//!
//! The layout, the property list and the icon container are written in Rust
//! and tested here. The signature is the system's `codesign`: it is the tool
//! whose opinion the system then asks, and it is one command.

mod icon;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, ensure, Context};

/// The application's name: the bundle, the menu bar, the icon file.
const NAME: &str = "Tuile";
/// Reverse-DNS, under the project's own domain.
const IDENTIFIER: &str = "dev.lapoule.tuile.viewer";
/// The viewer's package, and so its executable.
const EXECUTABLE: &str = "tuile-wgpu-viewer";
/// The URL scheme the application answers to.
const SCHEME: &str = "tuile";
/// The oldest system the bundle claims to run on: the first one on which the
/// location and graphics interfaces the viewer uses are all present.
const MINIMUM_SYSTEM: &str = "11.0";
/// Why the application wants the machine's location, in the words the
/// system's permission dialog will show. Neutral, and the whole truth.
const LOCATION_PURPOSE: &str = "Tuile centres the globe on your current location when you ask \
     it to. The position is used for that view only; it is not stored or sent anywhere.";

/// The workspace root: three directories above this package.
fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn master_png() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../macos/icon-1024.png")
}

/// `Info.plist`, as the dictionary it is.
///
/// Every key is here for a reason the system gives it:
///
/// - the identity (`CFBundle…`), with the version taken from the workspace so
///   that the bundle never claims a version the code is not;
/// - `CFBundleURLTypes` registers the steering scheme, which is what makes
///   `open "tuile://…"` reach the running application as an Apple event;
/// - the two location strings are what the system shows when it asks the
///   person; without them it does not ask, and the answer is no;
/// - `NSHighResolutionCapable`, or the window is drawn at half resolution and
///   scaled; `NSSupportsAutomaticGraphicsSwitching`, so a machine with two
///   GPUs is not forced onto the hungrier one for a window that is idle.
fn info_plist(version: &str) -> plist::Value {
    use plist::Value::{Array, Boolean, String as Text};
    let text = |s: &str| Text(s.to_owned());
    let mut url_type = plist::Dictionary::new();
    url_type.insert("CFBundleURLName".into(), text(IDENTIFIER));
    url_type.insert("CFBundleTypeRole".into(), text("Viewer"));
    url_type.insert("CFBundleURLSchemes".into(), Array(vec![text(SCHEME)]));

    let mut info = plist::Dictionary::new();
    for (key, value) in [
        ("CFBundleName", text(NAME)),
        ("CFBundleDisplayName", text(NAME)),
        ("CFBundleIdentifier", text(IDENTIFIER)),
        ("CFBundleExecutable", text(EXECUTABLE)),
        ("CFBundleIconFile", text(NAME)),
        ("CFBundlePackageType", text("APPL")),
        ("CFBundleInfoDictionaryVersion", text("6.0")),
        ("CFBundleShortVersionString", text(version)),
        ("CFBundleVersion", text(version)),
        (
            "CFBundleURLTypes",
            Array(vec![plist::Value::Dictionary(url_type)]),
        ),
        ("LSMinimumSystemVersion", text(MINIMUM_SYSTEM)),
        (
            "LSApplicationCategoryType",
            text("public.app-category.reference"),
        ),
        ("NSHighResolutionCapable", Boolean(true)),
        ("NSSupportsAutomaticGraphicsSwitching", Boolean(true)),
        ("NSPrincipalClass", text("NSApplication")),
        ("NSLocationUsageDescription", text(LOCATION_PURPOSE)),
        (
            "NSLocationWhenInUseUsageDescription",
            text(LOCATION_PURPOSE),
        ),
        (
            "NSHumanReadableCopyright",
            text("Copyright © lapoule.dev. MIT OR Apache-2.0."),
        ),
    ] {
        info.insert(key.into(), value);
    }
    plist::Value::Dictionary(info)
}

/// Every size an `.icns` holds, each resampled from the master: 16 to 512
/// points, at one and two pixels per point.
fn icns(master: &image::RgbaImage) -> anyhow::Result<Vec<u8>> {
    use icns::IconType::{
        RGBA32_128x128, RGBA32_128x128_2x, RGBA32_16x16, RGBA32_16x16_2x, RGBA32_256x256,
        RGBA32_256x256_2x, RGBA32_32x32, RGBA32_32x32_2x, RGBA32_512x512, RGBA32_512x512_2x,
    };
    let mut family = icns::IconFamily::new();
    for kind in [
        RGBA32_16x16,
        RGBA32_16x16_2x,
        RGBA32_32x32,
        RGBA32_32x32_2x,
        RGBA32_128x128,
        RGBA32_128x128_2x,
        RGBA32_256x256,
        RGBA32_256x256_2x,
        RGBA32_512x512,
        RGBA32_512x512_2x,
    ] {
        let side = kind.pixel_width();
        let scaled = if side == master.width() {
            master.clone()
        } else {
            // Lanczos: the seams between tiles are the icon, and a softer
            // filter loses them first.
            image::imageops::resize(master, side, side, image::imageops::FilterType::Lanczos3)
        };
        let picture =
            icns::Image::from_data(icns::PixelFormat::RGBA, side, side, scaled.into_raw())?;
        family.add_icon_with_type(&picture, kind)?;
    }
    let mut bytes = Vec::new();
    family.write(&mut bytes)?;
    Ok(bytes)
}

/// Lays the bundle out under `out` and returns its path. Replaces a bundle of
/// the same name: a stale file left inside one invalidates its signature.
fn assemble(binary: &Path, master: &image::RgbaImage, out: &Path) -> anyhow::Result<PathBuf> {
    let app = out.join(format!("{NAME}.app"));
    if app.exists() {
        std::fs::remove_dir_all(&app).with_context(|| format!("replacing {}", app.display()))?;
    }
    let contents = app.join("Contents");
    std::fs::create_dir_all(contents.join("MacOS"))?;
    std::fs::create_dir_all(contents.join("Resources"))?;
    std::fs::copy(binary, contents.join("MacOS").join(EXECUTABLE))
        .with_context(|| format!("copying {}", binary.display()))?;
    std::fs::write(
        contents.join("Resources").join(format!("{NAME}.icns")),
        icns(master)?,
    )?;
    info_plist(env!("CARGO_PKG_VERSION")).to_file_xml(contents.join("Info.plist"))?;
    // Eight bytes every application bundle has carried since before this
    // format had a property list: the type, and no creator.
    std::fs::write(contents.join("PkgInfo"), "APPL????")?;
    Ok(app)
}

fn run(command: &mut Command) -> anyhow::Result<()> {
    let status = command
        .status()
        .with_context(|| format!("starting {command:?}"))?;
    ensure!(status.success(), "{command:?} failed: {status}");
    Ok(())
}

/// Signs ad hoc, and asks the system whether it agrees with the result.
///
/// Ad hoc — an identity of `-` — is a signature with no signer: it seals the
/// bundle and gives it a stable identity on *this* machine, which is what the
/// system keys a permission grant on. It proves nothing to another machine.
fn sign_and_verify(app: &Path) -> anyhow::Result<()> {
    run(Command::new("codesign")
        .args(["--force", "--sign", "-", "--identifier", IDENTIFIER])
        .arg(app))?;
    run(Command::new("codesign")
        .args(["--verify", "--strict", "--verbose=2"])
        .arg(app))?;
    run(Command::new("plutil")
        .arg("-lint")
        .arg(app.join("Contents/Info.plist")))
}

fn build_the_viewer() -> anyhow::Result<PathBuf> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    run(Command::new(cargo).current_dir(workspace()).args([
        "build",
        "--release",
        "-p",
        EXECUTABLE,
    ]))?;
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| workspace().join("target"), PathBuf::from);
    Ok(target.join("release").join(EXECUTABLE))
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("icon") => {
            let path = master_png();
            icon::draw(icon::MASTER).save(&path)?;
            println!("{}", path.display());
            Ok(())
        }
        Some("app") => {
            let mut out = None;
            let mut binary = None;
            while let Some(arg) = args.next() {
                let value = args.next().map(PathBuf::from);
                match (arg.as_str(), value) {
                    ("--out", Some(dir)) => out = Some(dir),
                    // An executable built elsewhere — by a build machine, say.
                    ("--binary", Some(path)) => binary = Some(path),
                    _ => bail!("unknown or incomplete argument {arg:?}"),
                }
            }
            let binary = match binary {
                Some(binary) => binary,
                None => build_the_viewer()?,
            };
            let out = out.unwrap_or_else(|| workspace().join("target/bundle"));
            std::fs::create_dir_all(&out)?;
            let master = image::open(master_png())
                .context("reading the icon master")?
                .to_rgba8();
            let app = assemble(&binary, &master, &out)?;
            if cfg!(target_os = "macos") {
                sign_and_verify(&app)?;
            } else {
                eprintln!("not signed: this is not macOS");
            }
            println!("{}", app.display());
            Ok(())
        }
        _ => bail!("usage: tuile-viewer-bundle app [--out DIR] [--binary PATH] | icon"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> plist::Dictionary {
        info_plist("1.2.3")
            .into_dictionary()
            .expect("the property list is a dictionary")
    }

    fn string(info: &plist::Dictionary, key: &str) -> String {
        info.get(key)
            .and_then(plist::Value::as_string)
            .unwrap_or_else(|| unreachable!("{key} is missing or not a string"))
            .to_owned()
    }

    #[test]
    fn the_property_list_carries_the_identity_and_the_version_it_was_given() {
        let info = info();
        assert_eq!(string(&info, "CFBundleName"), "Tuile");
        assert_eq!(
            string(&info, "CFBundleIdentifier"),
            "dev.lapoule.tuile.viewer"
        );
        assert_eq!(string(&info, "CFBundleExecutable"), "tuile-wgpu-viewer");
        assert_eq!(string(&info, "CFBundlePackageType"), "APPL");
        assert_eq!(string(&info, "CFBundleShortVersionString"), "1.2.3");
        assert_eq!(string(&info, "CFBundleVersion"), "1.2.3");
        assert_eq!(string(&info, "LSMinimumSystemVersion"), "11.0");
        assert_eq!(
            info.get("NSHighResolutionCapable")
                .and_then(plist::Value::as_boolean),
            Some(true)
        );
    }

    /// Without both strings the system does not ask the person, and the answer
    /// to "where am I" is no.
    #[test]
    fn the_property_list_says_why_the_location_is_wanted() {
        let info = info();
        for key in [
            "NSLocationUsageDescription",
            "NSLocationWhenInUseUsageDescription",
        ] {
            assert!(string(&info, key).contains("current location"), "{key}");
        }
    }

    #[test]
    fn the_property_list_registers_the_steering_scheme() {
        let info = info();
        let schemes: Vec<_> = info
            .get("CFBundleURLTypes")
            .and_then(plist::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(plist::Value::as_dictionary)
            .filter_map(|t| t.get("CFBundleURLSchemes"))
            .filter_map(plist::Value::as_array)
            .flatten()
            .filter_map(plist::Value::as_string)
            .collect();
        assert_eq!(schemes, ["tuile"]);
    }

    /// The bundler and the viewer are two programs and must agree on three
    /// words. They cannot share a constant — the viewer is a binary — so the
    /// viewer's source is read and held to the bundler's.
    #[test]
    fn the_bundle_and_the_viewer_agree_on_their_names() {
        let steer = include_str!("../../src/steer.rs");
        assert!(steer.contains(&format!("const SCHEME: &str = {SCHEME:?};")));
        let host = include_str!("../../src/host.rs");
        assert!(host.contains(&format!("const APP_NAME: &str = {NAME:?};")));
        let manifest = include_str!("../../Cargo.toml");
        assert!(manifest.contains(&format!("name = {EXECUTABLE:?}")));
    }

    /// The layout the system expects, with an icon holding every size and a
    /// property list that reads back as written.
    #[test]
    fn the_bundle_has_the_layout_an_application_has() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let binary = dir.path().join("viewer");
        std::fs::write(&binary, b"not really an executable").expect("a stand-in binary");
        let app = assemble(&binary, &icon::draw(64), dir.path()).expect("the bundle");

        assert_eq!(app, dir.path().join("Tuile.app"));
        let contents = app.join("Contents");
        assert_eq!(
            std::fs::read(contents.join("MacOS/tuile-wgpu-viewer"))
                .ok()
                .as_deref(),
            Some(&b"not really an executable"[..])
        );
        assert_eq!(
            std::fs::read(contents.join("PkgInfo")).ok().as_deref(),
            Some(&b"APPL????"[..])
        );
        let read_back = plist::Value::from_file(contents.join("Info.plist")).expect("a plist");
        assert_eq!(read_back, info_plist(env!("CARGO_PKG_VERSION")));

        let icon = std::fs::File::open(contents.join("Resources/Tuile.icns")).expect("an icon");
        let family = icns::IconFamily::read(icon).expect("an icns");
        let mut sides: Vec<_> = family
            .available_icons()
            .iter()
            .map(|kind| kind.pixel_width())
            .collect();
        sides.sort_unstable();
        assert_eq!(sides, [16, 32, 32, 64, 128, 256, 256, 512, 512, 1024]);

        // Assembling again replaces the bundle rather than piling into it.
        std::fs::write(contents.join("Resources/stale"), b"x").expect("a stray file");
        assemble(&binary, &icon::draw(64), dir.path()).expect("the bundle, again");
        assert!(!contents.join("Resources/stale").exists());
    }
}
