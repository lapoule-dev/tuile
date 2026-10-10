// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Packages the viewer as a macOS application: `Tuile.app`, with its icon,
//! its `Info.plist` and an ad-hoc signature.
//!
//! ```text
//! cargo run -p tuile-viewer-bundle -- app                # → target/bundle/Tuile.app
//! cargo run -p tuile-viewer-bundle -- app --out ~/Applications
//! cargo run -p tuile-viewer-bundle -- icon               # redraw icon-1024.png
//!
//! # another host's application, or a second copy under another name:
//! cargo run -p tuile-viewer-bundle -- app --out /tmp/apps --name "Tuile Test" \
//!     --identifier dev.lapoule.tuile.viewer.test --scheme tuile-test
//! ```
//!
//! `app` builds the viewer in release, lays the bundle out, writes the
//! property list, packs the icon from the committed master, signs, and then
//! checks its own work with the system's `plutil -lint` and
//! `codesign --verify`. See `examples/wgpu-viewer/macos/README.md` for what the bundle is, what
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

/// What is being packaged: the four words a bundle and the program inside it
/// have to agree on. They are the host's (`tuile_viewer::Identity`); the
/// defaults are the project's own application.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Bundle {
    /// The application's name: the bundle, the menu bar, the icon file.
    name: String,
    /// Reverse-DNS.
    identifier: String,
    /// The package to build, and so the executable's name.
    executable: String,
    /// The URL scheme the application answers to.
    scheme: String,
}

impl Default for Bundle {
    fn default() -> Self {
        Self {
            name: "Tuile".into(),
            identifier: "dev.lapoule.tuile.viewer".into(),
            executable: "tuile-wgpu-viewer".into(),
            scheme: "tuile".into(),
        }
    }
}

/// The scripting dictionary's file name, in the bundle's resources.
const DICTIONARY: &str = "Tuile.sdef";
/// The oldest system the bundle claims to run on: the first one on which the
/// location and graphics interfaces the viewer uses are all present.
const MINIMUM_SYSTEM: &str = "11.0";
/// Why the application wants the machine's location, in the words the
/// system's permission dialog will show. Neutral, and the whole truth.
const LOCATION_PURPOSE: &str = "centres the globe on your current location when you ask \
     it to. The position is used for that view only; it is not stored or sent anywhere.";

/// The workspace root: two directories above this package.
fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn master_png() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("icon-1024.png")
}

/// The project's icon, carried in the tool so that it packages a host's
/// application from wherever it is installed.
const MASTER: &[u8] = include_bytes!("../icon-1024.png");

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
fn info_plist(bundle: &Bundle, version: &str) -> plist::Value {
    let Bundle {
        name,
        identifier,
        executable,
        scheme,
    } = bundle;
    let purpose = format!("{name} {LOCATION_PURPOSE}");
    use plist::Value::{Array, Boolean, String as Text};
    let text = |s: &str| Text(s.to_owned());
    let mut url_type = plist::Dictionary::new();
    url_type.insert("CFBundleURLName".into(), text(identifier));
    url_type.insert("CFBundleTypeRole".into(), text("Viewer"));
    url_type.insert("CFBundleURLSchemes".into(), Array(vec![text(scheme)]));

    let mut info = plist::Dictionary::new();
    for (key, value) in [
        ("CFBundleName", text(name)),
        ("CFBundleDisplayName", text(name)),
        ("CFBundleIdentifier", text(identifier)),
        ("CFBundleExecutable", text(executable)),
        ("CFBundleIconFile", text(name)),
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
        // The two keys that make the application scriptable: the runtime is
        // switched on, and told which file holds the dictionary.
        ("NSAppleScriptEnabled", Boolean(true)),
        ("OSAScriptingDefinition", text(DICTIONARY)),
        ("NSLocationUsageDescription", text(&purpose)),
        ("NSLocationWhenInUseUsageDescription", text(&purpose)),
        (
            "NSHumanReadableCopyright",
            text("Copyright © lapoule.dev. MIT OR Apache-2.0."),
        ),
    ] {
        info.insert(key.into(), value);
    }
    // A bundle that is not the project's own tells the program inside it who
    // it is. The public viewer is one binary; packaged under another name — a
    // test copy beside the one a person is using — it has to keep its own
    // support directory and answer its own scheme, and the environment the
    // launcher hands it is the only thing the two copies do not share.
    if *bundle != Bundle::default() {
        let mut environment = plist::Dictionary::new();
        environment.insert("TUILE_VIEWER_NAME".into(), text(name));
        environment.insert("TUILE_VIEWER_IDENTIFIER".into(), text(identifier));
        environment.insert("TUILE_VIEWER_SCHEME".into(), text(scheme));
        info.insert(
            "LSEnvironment".into(),
            plist::Value::Dictionary(environment),
        );
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
fn assemble(
    bundle: &Bundle,
    binary: &Path,
    master: &image::RgbaImage,
    out: &Path,
) -> anyhow::Result<PathBuf> {
    let name = &bundle.name;
    let app = out.join(format!("{name}.app"));
    if app.exists() {
        std::fs::remove_dir_all(&app).with_context(|| format!("replacing {}", app.display()))?;
    }
    let contents = app.join("Contents");
    std::fs::create_dir_all(contents.join("MacOS"))?;
    std::fs::create_dir_all(contents.join("Resources"))?;
    std::fs::copy(binary, contents.join("MacOS").join(&bundle.executable))
        .with_context(|| format!("copying {}", binary.display()))?;
    std::fs::write(
        contents.join("Resources").join(format!("{name}.icns")),
        icns(master)?,
    )?;
    std::fs::write(
        contents.join("Resources").join(DICTIONARY),
        include_str!("../../tuile-viewer/macos/Tuile.sdef"),
    )?;
    info_plist(bundle, env!("CARGO_PKG_VERSION")).to_file_xml(contents.join("Info.plist"))?;
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
fn sign_and_verify(bundle: &Bundle, app: &Path) -> anyhow::Result<()> {
    run(Command::new("codesign")
        .args(["--force", "--sign", "-", "--identifier", &bundle.identifier])
        .arg(app))?;
    run(Command::new("codesign")
        .args(["--verify", "--strict", "--verbose=2"])
        .arg(app))?;
    run(Command::new("plutil")
        .arg("-lint")
        .arg(app.join("Contents/Info.plist")))?;
    // And the dictionary, read the way a script editor reads it: `sdef` fails
    // on a bundle whose dictionary it cannot find or parse.
    run(Command::new("sdef")
        .arg(app)
        .stdout(std::process::Stdio::null()))
}

fn build_the_viewer(package: &str) -> anyhow::Result<PathBuf> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    run(Command::new(cargo).current_dir(workspace()).args([
        "build",
        "--release",
        "-p",
        package,
    ]))?;
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| workspace().join("target"), PathBuf::from);
    Ok(target.join("release").join(package))
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
            let mut icon = None;
            let mut bundle = Bundle::default();
            while let Some(arg) = args.next() {
                let Some(value) = args.next() else {
                    bail!("{arg} needs a value");
                };
                match arg.as_str() {
                    "--out" => out = Some(PathBuf::from(value)),
                    // An executable built elsewhere — by a build machine, or
                    // by a host's own workspace.
                    "--binary" => binary = Some(PathBuf::from(value)),
                    // The host's identity: the same four words its
                    // `tuile_viewer::Identity` says.
                    "--name" => bundle.name = value,
                    "--identifier" => bundle.identifier = value,
                    "--scheme" => bundle.scheme = value,
                    "--package" => bundle.executable = value,
                    // A 1024-pixel PNG, in place of the project's icon.
                    "--icon" => icon = Some(PathBuf::from(value)),
                    _ => bail!("unknown argument {arg:?}"),
                }
            }
            let binary = match binary {
                Some(binary) => binary,
                None => build_the_viewer(&bundle.executable)?,
            };
            let out = out.unwrap_or_else(|| workspace().join("target/bundle"));
            std::fs::create_dir_all(&out)?;
            let master = match icon {
                Some(path) => image::open(&path)
                    .with_context(|| format!("reading the icon {}", path.display()))?,
                None => image::load_from_memory(MASTER).context("reading the icon master")?,
            }
            .to_rgba8();
            let app = assemble(&bundle, &binary, &master, &out)?;
            if cfg!(target_os = "macos") {
                sign_and_verify(&bundle, &app)?;
            } else {
                eprintln!("not signed: this is not macOS");
            }
            println!("{}", app.display());
            Ok(())
        }
        _ => bail!(
            "usage: tuile-viewer-bundle app [--out DIR] [--binary PATH] [--name NAME] \
             [--identifier ID] [--scheme SCHEME] [--package PACKAGE] [--icon PNG] | icon"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> plist::Dictionary {
        info_plist(&Bundle::default(), "1.2.3")
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

    /// The bundler and the viewer are two programs and must agree on four
    /// words. The bundler does not link the viewer — it would drag a renderer
    /// into a packaging tool — so the viewer's default identity is read from
    /// its source and held to the bundler's.
    #[test]
    fn the_bundle_and_the_viewer_agree_on_their_names() {
        let Bundle {
            name,
            identifier,
            executable,
            scheme,
        } = Bundle::default();
        let embed = include_str!("../../tuile-viewer/src/embed.rs");
        for line in [
            format!("name: {name:?}.into(),"),
            format!("bundle_identifier: {identifier:?}.into(),"),
            format!("scheme: {scheme:?}.into(),"),
            format!("executable: {executable:?}.into(),"),
        ] {
            assert!(embed.contains(&line), "{line}");
        }
        let manifest = include_str!("../../../examples/wgpu-viewer/Cargo.toml");
        assert!(manifest.contains(&format!("name = {executable:?}")));
    }

    /// A bundle under another name tells the program inside it so, and the
    /// project's own bundle says nothing: the second copy must not read the
    /// first one's files or answer its URLs.
    #[test]
    fn a_renamed_bundle_hands_its_identity_to_the_program() {
        assert!(!info().contains_key("LSEnvironment"));
        let other = Bundle {
            name: "Tuile Test".into(),
            identifier: "dev.lapoule.tuile.viewer.test".into(),
            scheme: "tuile-test".into(),
            ..Bundle::default()
        };
        let info = info_plist(&other, "1.2.3")
            .into_dictionary()
            .expect("a dictionary");
        let said = info
            .get("LSEnvironment")
            .and_then(plist::Value::as_dictionary)
            .expect("the environment of a renamed bundle");
        for (key, value) in [
            ("TUILE_VIEWER_NAME", "Tuile Test"),
            ("TUILE_VIEWER_IDENTIFIER", "dev.lapoule.tuile.viewer.test"),
            ("TUILE_VIEWER_SCHEME", "tuile-test"),
        ] {
            assert_eq!(said.get(key).and_then(plist::Value::as_string), Some(value));
        }
        assert_eq!(string(&info, "CFBundleName"), "Tuile Test");
        assert_eq!(string(&info, "CFBundleIdentifier"), other.identifier);
    }

    /// The layout the system expects, with an icon holding every size and a
    /// property list that reads back as written.
    #[test]
    fn the_bundle_has_the_layout_an_application_has() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let binary = dir.path().join("viewer");
        std::fs::write(&binary, b"not really an executable").expect("a stand-in binary");
        let app = assemble(&Bundle::default(), &binary, &icon::draw(64), dir.path()).expect("the bundle");

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
        assert_eq!(
            read_back,
            info_plist(&Bundle::default(), env!("CARGO_PKG_VERSION"))
        );

        let icon = std::fs::File::open(contents.join("Resources/Tuile.icns")).expect("an icon");
        let family = icns::IconFamily::read(icon).expect("an icns");
        let mut sides: Vec<_> = family
            .available_icons()
            .iter()
            .map(|kind| kind.pixel_width())
            .collect();
        sides.sort_unstable();
        assert_eq!(sides, [16, 32, 32, 64, 128, 256, 256, 512, 512, 1024]);

        // The dictionary the property list names is in the bundle, whole.
        assert_eq!(string(&info(), "OSAScriptingDefinition"), "Tuile.sdef");
        assert_eq!(
            info()
                .get("NSAppleScriptEnabled")
                .and_then(plist::Value::as_boolean),
            Some(true)
        );
        assert_eq!(
            std::fs::read_to_string(contents.join("Resources/Tuile.sdef"))
                .ok()
                .as_deref(),
            Some(include_str!("../../tuile-viewer/macos/Tuile.sdef"))
        );

        // Assembling again replaces the bundle rather than piling into it.
        std::fs::write(contents.join("Resources/stale"), b"x").expect("a stray file");
        assemble(&Bundle::default(), &binary, &icon::draw(64), dir.path())
            .expect("the bundle, again");
        assert!(!contents.join("Resources/stale").exists());
    }

    /// The round trip through the real thing: a script asks the installed
    /// application where it is, sends it somewhere, and reads that it went.
    ///
    /// ```text
    /// cargo test -p tuile-viewer-bundle -- --ignored --nocapture a_script
    /// ```
    ///
    /// Ignored, because it needs the bundle installed and running (it starts
    /// it if need be), a window server, and — the first time — the person's
    /// consent to one program scripting another. It puts the view back where
    /// it found it. `TUILE_APP` names a bundle other than the one in
    /// `~/Applications`.
    #[test]
    #[ignore = "scripts the installed application"]
    fn a_script_reads_the_view_and_moves_it() {
        let app = std::env::var("TUILE_APP").unwrap_or_else(|_| {
            format!(
                "{}/Applications/Tuile.app",
                std::env::var("HOME").unwrap_or_default()
            )
        });
        let tell = |what: &str| {
            let script = format!("tell application {app:?} to {what}");
            let out = Command::new("osascript")
                .args(["-e", &script])
                .output()
                .expect("osascript runs");
            let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            println!(
                "{what}\n    -> {text}{}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            (out.status.success(), text)
        };
        let reals =
            |text: &str| -> Vec<f64> { text.split(", ").filter_map(|n| n.parse().ok()).collect() };
        let pause = || std::thread::sleep(std::time::Duration::from_millis(1500));

        // The dictionary is there, as a script editor would read it.
        let sdef = Command::new("sdef").arg(&app).output().expect("sdef runs");
        assert!(
            sdef.status.success(),
            "the bundle has no readable dictionary"
        );
        assert!(String::from_utf8_lossy(&sdef.stdout).contains("target longitude"));

        let (ok, before) = tell("get view url");
        assert!(ok, "the application does not answer scripts");
        pause();

        assert!(tell("go to longitude 10.5 latitude -20.25 altitude 5000 heading 90 pitch 45").0);
        pause();
        let (_, view) = tell("get {longitude, latitude, altitude, heading, pitch}");
        let got = reals(&view);
        for (got, want) in got.iter().zip([10.5, -20.25, 5000.0, 90.0, 45.0]) {
            assert!((got - want).abs() < 1e-3, "{view}");
        }
        assert_eq!(got.len(), 5, "{view}");

        // What is left out keeps its value.
        assert!(tell("go to heading 180").0);
        pause();
        let (_, view) = tell("get {longitude, latitude, altitude, heading, pitch}");
        for (got, want) in reals(&view).iter().zip([10.5, -20.25, 5000.0, 180.0, 45.0]) {
            assert!((got - want).abs() < 1e-3, "{view}");
        }
        let (_, target) = tell("get {target longitude, target latitude, range}");
        assert_eq!(reals(&target).len(), 3, "{target}");

        // The turn is eased, and the easing is paced by frames: on a busy
        // machine half a turn takes a few seconds to finish. Asked until it
        // has, within a bound.
        assert!(tell("north up").0);
        let mut heading = f64::NAN;
        for _ in 0..20 {
            pause();
            heading = tell("get heading").1.parse().expect("a heading");
            if heading.min(360.0 - heading) < 1e-3 {
                break;
            }
        }
        assert!(heading.min(360.0 - heading) < 1e-3, "{heading}");

        // Out of bounds is an error the script sees, and moves nothing.
        let (ok, _) = tell("go to latitude 95");
        assert!(!ok, "a latitude of 95 was accepted");
        let (_, latitude) = tell("get latitude");
        assert!(
            latitude.parse::<f64>().is_ok_and(|l| l.abs() < 90.0),
            "{latitude}"
        );

        assert!(tell("set wireframe to true").0);
        pause();
        assert_eq!(tell("get wireframe").1, "true");
        assert!(tell("set wireframe to false").0);
        pause();
        assert_eq!(tell("get wireframe").1, "false");

        // Back where it was, by the URL the application itself gave.
        if before.starts_with("tuile://") {
            run(Command::new("open").arg(&before)).expect("the view is restored");
        }
    }
}
