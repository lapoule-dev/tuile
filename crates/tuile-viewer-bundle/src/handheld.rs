// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a phone's application bundle needs from this tool: its property list,
//! its icon and the colour of its launch screen.
//!
//! The bundle itself is laid out and signed by the platform's own build
//! (`crates/tuile-viewer/ios/`, an `xcodegen` project driven by `build.sh`):
//! on a phone the signature is a provisioning question that only the
//! platform's tools can answer. What goes *into* the bundle is decided here,
//! in Rust, and tested here — as it is for the desktop bundle.
//!
//! ```text
//! cargo run -p tuile-viewer-bundle -- phone --out crates/tuile-viewer/ios/Generated
//! ```

use std::path::Path;

use anyhow::Context;

use crate::Bundle;

/// The oldest system the application claims to run on: the first with
/// everything the viewer asks of it — a Metal 3 family GPU through wgpu, the
/// declarative launch screen, the process's memory limit as a call.
pub(crate) const MINIMUM_SYSTEM: &str = "16.0";

/// The launch screen's colour, and the globe's shell's: the same deep ocean
/// blue, so the step from the system's launch screen to the first frame is
/// no step at all. `BARE_GROUND` in `tuile-viewer/src/backdrop.rs`, in sRGB.
pub(crate) const LAUNCH_BACKGROUND: [f64; 3] = [0.09, 0.15, 0.24];

/// The name of the launch colour and of the icon set in the asset catalogue.
const LAUNCH_COLOR: &str = "LaunchBackground";
const APP_ICON: &str = "AppIcon";

/// `Info.plist` for the phone.
///
/// Every key is here for a reason the system gives it:
///
/// - the identity, with the version taken from the workspace;
/// - `UILaunchScreen` names the colour the system paints while the process
///   starts — the shell's, so launch screen and first frame are one picture;
/// - the orientations the viewer handles, every one of them;
/// - `UIRequiredDeviceCapabilities`: the renderer needs Metal;
/// - the location string, without which the system does not ask the person
///   and the answer to "where am I" is no;
/// - `UIFileSharingEnabled` with `LSSupportsOpeningDocumentsInPlace`, which
///   show the application's `Documents` in the Files application: the one
///   place a person can put the access token (see `tuile_viewer` `host`);
/// - `NSAppTransportSecurity` without a single exception: every tile source
///   the application uses is HTTPS. The tile traffic does not go through the
///   system's URL loading — it is the viewer's own TLS client — but the
///   declaration is what the system holds the application to, and it is the
///   truth;
/// - `ITSAppUsesNonExemptEncryption`: HTTPS only, which is exempt;
/// - the configuration file's type and the URL scheme, with
///   `NSPrincipalClass` naming the application class that receives them
///   (see `documents`).
pub(crate) fn info_plist(bundle: &Bundle, version: &str) -> plist::Value {
    use plist::Value::{Array, Boolean, Dictionary, String as Text};
    let Bundle {
        name,
        identifier,
        executable,
        ..
    } = bundle;
    let text = |s: &str| Text(s.to_owned());
    let texts = |v: &[&str]| Array(v.iter().map(|s| text(s)).collect());
    let purpose = format!("{name} {}", crate::LOCATION_PURPOSE);

    let mut launch = plist::Dictionary::new();
    launch.insert("UIColorName".into(), text(LAUNCH_COLOR));
    let mut transport = plist::Dictionary::new();
    transport.insert("NSAllowsArbitraryLoads".into(), Boolean(false));

    let mut info = plist::Dictionary::new();
    for (key, value) in [
        ("CFBundleDevelopmentRegion", text("en")),
        ("CFBundleName", text(name)),
        ("CFBundleDisplayName", text(name)),
        ("CFBundleIdentifier", text(identifier)),
        ("CFBundleExecutable", text(executable)),
        ("CFBundlePackageType", text("APPL")),
        ("CFBundleInfoDictionaryVersion", text("6.0")),
        ("CFBundleShortVersionString", text(version)),
        ("CFBundleVersion", text(version)),
        ("LSRequiresIPhoneOS", Boolean(true)),
        // Xcode writes the same from the project's deployment target; said
        // here too so the two can be held to each other (see the tests).
        ("MinimumOSVersion", text(MINIMUM_SYSTEM)),
        ("UILaunchScreen", Dictionary(launch)),
        ("UIRequiredDeviceCapabilities", texts(&["arm64", "metal"])),
        (
            "UISupportedInterfaceOrientations",
            texts(&[
                "UIInterfaceOrientationPortrait",
                "UIInterfaceOrientationLandscapeLeft",
                "UIInterfaceOrientationLandscapeRight",
            ]),
        ),
        (
            "UISupportedInterfaceOrientations~ipad",
            texts(&[
                "UIInterfaceOrientationPortrait",
                "UIInterfaceOrientationPortraitUpsideDown",
                "UIInterfaceOrientationLandscapeLeft",
                "UIInterfaceOrientationLandscapeRight",
            ]),
        ),
        // White status-bar text over a dark globe.
        ("UIStatusBarStyle", text("UIStatusBarStyleLightContent")),
        ("UIViewControllerBasedStatusBarAppearance", Boolean(false)),
        ("NSLocationWhenInUseUsageDescription", text(&purpose)),
        ("UIFileSharingEnabled", Boolean(true)),
        ("LSSupportsOpeningDocumentsInPlace", Boolean(true)),
        ("NSAppTransportSecurity", Dictionary(transport)),
        ("ITSAppUsesNonExemptEncryption", Boolean(false)),
        (
            "NSHumanReadableCopyright",
            text("Copyright © lapoule.dev. MIT OR Apache-2.0."),
        ),
    ] {
        info.insert(key.into(), value);
    }
    // A configuration file opens here (Files, AirDrop, Mail, a download),
    // and so does an import link; both arrive at the delegate the viewer's
    // principal class gives itself (`tuile_viewer` `desktop`).
    for (key, value) in crate::documents::declarations(bundle) {
        info.insert(key.into(), value);
    }
    info.insert(
        "CFBundleURLTypes".into(),
        crate::documents::url_types(bundle),
    );
    info.insert("NSPrincipalClass".into(), text(PRINCIPAL_CLASS));
    Dictionary(info)
}

/// The application class the viewer defines and names here, the one that
/// receives opened files and links. The same string as `tuile_viewer`'s
/// `desktop::system::open::APPLICATION`.
pub(crate) const PRINCIPAL_CLASS: &str = "TuileApplication";

/// The asset catalogue: the icon, one 1024-pixel image that the platform's
/// compiler cuts into every size, and the launch colour.
fn asset_catalogue(master: &image::RgbaImage, out: &Path) -> anyhow::Result<()> {
    let catalogue = out.join("Assets.xcassets");
    if catalogue.exists() {
        std::fs::remove_dir_all(&catalogue)?;
    }
    let icons = catalogue.join(format!("{APP_ICON}.appiconset"));
    let colour = catalogue.join(format!("{LAUNCH_COLOR}.colorset"));
    std::fs::create_dir_all(&icons)?;
    std::fs::create_dir_all(&colour)?;
    let info = serde_json::json!({ "author": "tuile-viewer-bundle", "version": 1 });
    std::fs::write(
        catalogue.join("Contents.json"),
        serde_json::to_string_pretty(&serde_json::json!({ "info": info }))?,
    )?;

    // An application icon may not be transparent: flattened onto the shell's
    // blue, which is the colour the master's own background tends toward.
    let side = 1024;
    let scaled = if master.width() == side {
        master.clone()
    } else {
        image::imageops::resize(master, side, side, image::imageops::FilterType::Lanczos3)
    };
    let background = LAUNCH_BACKGROUND.map(|c| (c * 255.0).round() as u8);
    let flat = image::RgbImage::from_fn(side, side, |x, y| {
        let p = scaled.get_pixel(x, y).0;
        let a = f32::from(p[3]) / 255.0;
        let mix = |c: u8, b: u8| (f32::from(c) * a + f32::from(b) * (1.0 - a)).round() as u8;
        image::Rgb([
            mix(p[0], background[0]),
            mix(p[1], background[1]),
            mix(p[2], background[2]),
        ])
    });
    flat.save(icons.join("icon-1024.png"))
        .context("writing the icon")?;
    std::fs::write(
        icons.join("Contents.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "images": [{
                "filename": "icon-1024.png",
                "idiom": "universal",
                "platform": "ios",
                "size": "1024x1024",
            }],
            "info": info,
        }))?,
    )?;

    let component = |c: f64| format!("{c:.3}");
    std::fs::write(
        colour.join("Contents.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "colors": [{
                "idiom": "universal",
                "color": {
                    "color-space": "srgb",
                    "components": {
                        "red": component(LAUNCH_BACKGROUND[0]),
                        "green": component(LAUNCH_BACKGROUND[1]),
                        "blue": component(LAUNCH_BACKGROUND[2]),
                        "alpha": "1.000",
                    },
                },
            }],
            "info": info,
        }))?,
    )?;
    Ok(())
}

/// A credential the application reads: the environment variable it is
/// named after, what the settings page calls it, and — for a developer's own
/// device only — a value to write into the bundle.
pub(crate) struct Credential {
    /// The variable's name, the host's: `TUILE_TILES_TOKEN`, say. Also the settings key.
    pub variable: String,
    /// What the settings page shows: `Tile service token`, say.
    pub title: String,
    /// A build-time default, written into `Info.plist`.
    ///
    /// **Anything in a bundle can be read back out of the `.app`.** This is
    /// for builds that never leave the developer's own device; a build for
    /// the App Store, TestFlight or anyone else must not carry one.
    pub bundled: Option<String>,
}

/// The `Info.plist` key a bundled credential is written under. The viewer
/// reads the same key (`tuile_viewer` `host::bundled_key`).
pub(crate) fn bundled_key(variable: &str) -> String {
    format!("{}{variable}", BUNDLED_PREFIX.trim())
}

/// Shared with the viewer, which reads the key: one file, two readers.
const BUNDLED_PREFIX: &str = include_str!("../../tuile-viewer/ios/bundled-credential-prefix.txt");

/// The key the viewer reads the imagery compression under: the same string
/// as `tuile_viewer`'s `IMAGERY_COMPRESSION_KEY`.
pub(crate) const IMAGERY_COMPRESSION_KEY: &str = "imagery_compression";

/// The key the settings page keeps the 3D Tiles switch under: the same as
/// `tuile_viewer`'s `SHOW_TILESETS_KEY`.
pub(crate) const SHOW_TILESETS_KEY: &str = "show_tilesets";

/// The settings page: one secure text field for the credential, under the
/// key the application reads first, and the choice of how imagery is held on
/// the GPU — off unless the person picks otherwise, read at launch.
fn settings_bundle(credential: &Credential, out: &Path) -> anyhow::Result<()> {
    use plist::Value::{Array, Boolean, Dictionary, String as Text};
    let settings = out.join("Settings.bundle");
    if settings.exists() {
        std::fs::remove_dir_all(&settings)?;
    }
    std::fs::create_dir_all(&settings)?;
    let text = |s: &str| Text(s.to_owned());
    let mut group = plist::Dictionary::new();
    group.insert("Type".into(), text("PSGroupSpecifier"));
    group.insert("Title".into(), text("Access"));
    group.insert(
        "FooterText".into(),
        text(
            "Read first, before any token built into the application and before \
             the file \"token\" in Files.",
        ),
    );
    let mut field = plist::Dictionary::new();
    for (key, value) in [
        ("Type", text("PSTextFieldSpecifier")),
        ("Title", text(&credential.title)),
        ("Key", text(&credential.variable)),
        ("IsSecure", Boolean(true)),
        ("KeyboardType", text("Alphabet")),
        ("AutocapitalizationType", text("None")),
        ("AutocorrectionType", text("No")),
        ("DefaultValue", text("")),
    ] {
        field.insert(key.into(), value);
    }
    let mut rendering = plist::Dictionary::new();
    rendering.insert("Type".into(), text("PSGroupSpecifier"));
    rendering.insert("Title".into(), text("Rendering"));
    rendering.insert(
        "FooterText".into(),
        text(
            "Imagery held block-compressed on the GPU: a quarter of the memory, \
             encoded at upload. Auto picks ASTC 4x4 on a phone. Applies at the \
             next launch.",
        ),
    );
    let mut compression = plist::Dictionary::new();
    let names = ["off", "auto", "bc7", "astc"];
    for (key, value) in [
        ("Type", text("PSMultiValueSpecifier")),
        ("Title", text("Imagery compression")),
        ("Key", text(IMAGERY_COMPRESSION_KEY)),
        ("DefaultValue", text("off")),
        (
            "Titles",
            Array(
                ["Off (RGBA8)", "Auto", "BC7", "ASTC 4x4"]
                    .iter()
                    .map(|t| text(t))
                    .collect(),
            ),
        ),
        ("Values", Array(names.iter().map(|v| text(v)).collect())),
    ] {
        compression.insert(key.into(), value);
    }
    // The host's 3D Tiles over the globe — on a desktop and a page a switch
    // in the menu, here a switch on this page: off unless turned on.
    let mut tiles_group = plist::Dictionary::new();
    tiles_group.insert("Type".into(), text("PSGroupSpecifier"));
    tiles_group.insert("Title".into(), text("3D Tiles"));
    tiles_group.insert(
        "FooterText".into(),
        text(
            "A photographic mesh of the surface — buildings, trees, relief — \
             drawn over the globe where it exists, its credits below the \
             picture. Applies at the next launch.",
        ),
    );
    let mut tiles = plist::Dictionary::new();
    for (key, value) in [
        ("Type", text("PSToggleSwitchSpecifier")),
        ("Title", text("Photorealistic 3D Tiles")),
        ("Key", text(SHOW_TILESETS_KEY)),
        ("DefaultValue", Boolean(false)),
    ] {
        tiles.insert(key.into(), value);
    }
    let mut root = plist::Dictionary::new();
    root.insert(
        "PreferenceSpecifiers".into(),
        Array(vec![
            Dictionary(group),
            Dictionary(field),
            Dictionary(rendering),
            Dictionary(compression),
            Dictionary(tiles_group),
            Dictionary(tiles),
        ]),
    );
    Dictionary(root).to_file_xml(settings.join("Root.plist"))?;
    Ok(())
}

/// Writes `Info.plist`, the asset catalogue and — with a credential — the
/// settings page under `out`.
pub(crate) fn resources(
    bundle: &Bundle,
    master: &image::RgbaImage,
    out: &Path,
    credential: Option<&Credential>,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(out)?;
    let mut info = info_plist(bundle, env!("CARGO_PKG_VERSION"));
    if let Some(credential) = credential {
        settings_bundle(credential, out)?;
        if let (Some(value), Some(dict)) = (
            credential
                .bundled
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty()),
            info.as_dictionary_mut(),
        ) {
            dict.insert(
                bundled_key(&credential.variable),
                plist::Value::String(value.to_owned()),
            );
        }
    }
    info.to_file_xml(out.join("Info.plist"))?;
    asset_catalogue(master, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> plist::Dictionary {
        info_plist(&Bundle::default(), "1.2.3")
            .into_dictionary()
            .expect("a dictionary")
    }

    fn string(info: &plist::Dictionary, key: &str) -> Option<String> {
        info.get(key)
            .and_then(plist::Value::as_string)
            .map(str::to_owned)
    }

    #[test]
    fn the_phone_opens_configuration_files_and_import_links() {
        let info = info();
        assert!(info.get("CFBundleDocumentTypes").is_some());
        assert!(info.get("UTExportedTypeDeclarations").is_some());
        let schemes: Vec<String> = info
            .get("CFBundleURLTypes")
            .and_then(plist::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(plist::Value::as_dictionary)
            .filter_map(|t| t.get("CFBundleURLSchemes")?.as_array())
            .flatten()
            .filter_map(|s| s.as_string().map(str::to_owned))
            .collect();
        assert_eq!(schemes, ["tuile"]);
        assert_eq!(
            string(&info, "NSPrincipalClass").as_deref(),
            Some(PRINCIPAL_CLASS)
        );
        // The class the viewer defines is the one named here.
        assert!(include_str!("../../tuile-viewer/src/desktop.rs")
            .contains(&format!("APPLICATION: &str = \"{PRINCIPAL_CLASS}\"")));
        assert!(include_str!("../../tuile-viewer/src/desktop.rs")
            .contains(&format!("#[name = \"{PRINCIPAL_CLASS}\"]")));
    }

    #[test]
    fn the_phone_property_list_carries_the_identity_and_the_launch_screen() {
        let info = info();
        assert_eq!(
            string(&info, "CFBundleIdentifier").as_deref(),
            Some("dev.lapoule.tuile.viewer")
        );
        assert_eq!(
            string(&info, "CFBundleExecutable").as_deref(),
            Some("tuile-wgpu-viewer")
        );
        assert_eq!(
            string(&info, "CFBundleShortVersionString").as_deref(),
            Some("1.2.3")
        );
        assert_eq!(
            info.get("LSRequiresIPhoneOS")
                .and_then(plist::Value::as_boolean),
            Some(true)
        );
        let launch = info
            .get("UILaunchScreen")
            .and_then(plist::Value::as_dictionary)
            .expect("a launch screen");
        assert_eq!(
            launch.get("UIColorName").and_then(plist::Value::as_string),
            Some(LAUNCH_COLOR)
        );
        let needs: Vec<_> = info
            .get("UIRequiredDeviceCapabilities")
            .and_then(plist::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(plist::Value::as_string)
            .collect();
        assert!(needs.contains(&"metal"));
    }

    /// Every tile source is HTTPS: no exception to the transport rules, and
    /// the location string that makes the system ask the person.
    #[test]
    fn the_phone_property_list_allows_no_plain_http_and_asks_for_the_location() {
        let info = info();
        let transport = info
            .get("NSAppTransportSecurity")
            .and_then(plist::Value::as_dictionary)
            .expect("the transport rules");
        assert_eq!(
            transport
                .get("NSAllowsArbitraryLoads")
                .and_then(plist::Value::as_boolean),
            Some(false)
        );
        assert_eq!(transport.len(), 1, "an exception crept in: {transport:?}");
        assert!(string(&info, "NSLocationWhenInUseUsageDescription")
            .is_some_and(|s| s.contains("current location")));
        // The token goes in Documents, which only these two keys show.
        for key in ["UIFileSharingEnabled", "LSSupportsOpeningDocumentsInPlace"] {
            assert_eq!(
                info.get(key).and_then(plist::Value::as_boolean),
                Some(true),
                "{key}"
            );
        }
    }

    /// The launch colour is the shell's: read from the viewer's source, so
    /// the two cannot drift apart.
    #[test]
    fn the_launch_screen_is_the_colour_of_the_shell() {
        let backdrop = include_str!("../../tuile-viewer/src/backdrop.rs");
        let [r, g, b] = LAUNCH_BACKGROUND;
        assert!(
            backdrop.contains(&format!(
                "const BARE_GROUND: [f32; 4] = [{r}, {g}, {b}, 1.0];"
            )),
            "the launch screen and the shell disagree"
        );
    }

    /// The property list and the Xcode project claim the same oldest system.
    #[test]
    fn the_plist_and_the_project_agree_on_the_oldest_system() {
        let project = include_str!("../../tuile-viewer/ios/project.yml");
        assert!(
            project.contains(&format!("iOS: \"{MINIMUM_SYSTEM}\"")),
            "project.yml deploys to another iOS than {MINIMUM_SYSTEM}"
        );
        assert_eq!(
            string(&info(), "MinimumOSVersion").as_deref(),
            Some(MINIMUM_SYSTEM)
        );
    }

    /// The settings page has one secure field under the variable's name;
    /// a bundled default lands under the key the viewer reads, and only when
    /// there is one — a build without it carries no such key at all.
    #[test]
    fn the_credential_is_a_settings_field_and_optionally_a_bundled_default() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let credential = |bundled: Option<&str>| Credential {
            variable: "SOME_TOKEN".into(),
            title: "Some token".into(),
            bundled: bundled.map(str::to_owned),
        };
        resources(
            &Bundle::default(),
            &crate::icon::draw(64),
            dir.path(),
            Some(&credential(None)),
        )
        .expect("written");
        let root = plist::Value::from_file(dir.path().join("Settings.bundle/Root.plist"))
            .expect("a settings page");
        let fields = root
            .as_dictionary()
            .and_then(|d| d.get("PreferenceSpecifiers"))
            .and_then(plist::Value::as_array)
            .expect("specifiers");
        let field = fields
            .iter()
            .filter_map(plist::Value::as_dictionary)
            .find(|f| {
                f.get("Type").and_then(plist::Value::as_string) == Some("PSTextFieldSpecifier")
            })
            .expect("a text field");
        assert_eq!(
            field.get("Key").and_then(plist::Value::as_string),
            Some("SOME_TOKEN")
        );
        assert_eq!(
            field.get("IsSecure").and_then(plist::Value::as_boolean),
            Some(true)
        );
        // The imagery compression: a choice of four, off by default.
        let choice = fields
            .iter()
            .filter_map(plist::Value::as_dictionary)
            .find(|f| {
                f.get("Type").and_then(plist::Value::as_string) == Some("PSMultiValueSpecifier")
            })
            .expect("a choice of imagery compression");
        assert_eq!(
            choice.get("Key").and_then(plist::Value::as_string),
            Some(IMAGERY_COMPRESSION_KEY)
        );
        assert_eq!(
            choice.get("DefaultValue").and_then(plist::Value::as_string),
            Some("off")
        );
        let values: Vec<&str> = choice
            .get("Values")
            .and_then(plist::Value::as_array)
            .expect("values")
            .iter()
            .filter_map(plist::Value::as_string)
            .collect();
        assert_eq!(values, ["off", "auto", "bc7", "astc"]);
        // The 3D Tiles: a switch, off by default.
        let switch = fields
            .iter()
            .filter_map(plist::Value::as_dictionary)
            .find(|f| {
                f.get("Type").and_then(plist::Value::as_string) == Some("PSToggleSwitchSpecifier")
            })
            .expect("a 3D Tiles switch");
        assert_eq!(
            switch.get("Key").and_then(plist::Value::as_string),
            Some(SHOW_TILESETS_KEY)
        );
        assert_eq!(
            switch
                .get("DefaultValue")
                .and_then(plist::Value::as_boolean),
            Some(false)
        );
        let info = plist::Value::from_file(dir.path().join("Info.plist")).expect("a plist");
        let key = bundled_key("SOME_TOKEN");
        assert!(info.as_dictionary().is_some_and(|d| !d.contains_key(&key)));

        for blank in ["", "  "] {
            resources(
                &Bundle::default(),
                &crate::icon::draw(64),
                dir.path(),
                Some(&credential(Some(blank))),
            )
            .expect("written");
            let info = plist::Value::from_file(dir.path().join("Info.plist")).expect("a plist");
            assert!(
                info.as_dictionary().is_some_and(|d| !d.contains_key(&key)),
                "{blank:?}"
            );
        }
        resources(
            &Bundle::default(),
            &crate::icon::draw(64),
            dir.path(),
            Some(&credential(Some("abc.def\n"))),
        )
        .expect("written");
        let info = plist::Value::from_file(dir.path().join("Info.plist")).expect("a plist");
        assert_eq!(
            info.as_dictionary()
                .and_then(|d| d.get(&key))
                .and_then(plist::Value::as_string),
            Some("abc.def")
        );
        // The viewer reads the same key: the same file, and the same literal
        // the viewer's own test pins.
        assert_eq!(key, "TuileBundledCredential_SOME_TOKEN");
        assert!(include_str!("../../tuile-viewer/src/host.rs")
            .contains("include_str!(\"../ios/bundled-credential-prefix.txt\")"));
    }

    #[test]
    fn the_resources_are_a_plist_and_a_catalogue() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        resources(&Bundle::default(), &crate::icon::draw(64), dir.path(), None).expect("written");
        let read_back = plist::Value::from_file(dir.path().join("Info.plist")).expect("a plist");
        assert_eq!(
            read_back,
            info_plist(&Bundle::default(), env!("CARGO_PKG_VERSION"))
        );
        let icon = image::open(
            dir.path()
                .join("Assets.xcassets/AppIcon.appiconset/icon-1024.png"),
        )
        .expect("an icon");
        assert_eq!((icon.width(), icon.height()), (1024, 1024));
        assert!(
            !icon.color().has_alpha(),
            "an application icon may not be transparent"
        );
        for json in [
            "Assets.xcassets/Contents.json",
            "Assets.xcassets/AppIcon.appiconset/Contents.json",
            "Assets.xcassets/LaunchBackground.colorset/Contents.json",
        ] {
            let text = std::fs::read_to_string(dir.path().join(json)).expect(json);
            serde_json::from_str::<serde_json::Value>(&text).expect(json);
        }
    }
}
