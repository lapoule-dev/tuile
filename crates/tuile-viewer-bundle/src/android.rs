// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! An Android application's manifest and resources: what `aapt2` compiles
//! and links into the APK, beside the native library and the activity's
//! classes (`crates/tuile-viewer/android/build.sh`).
//!
//! Written here, in Rust and tested, for the same reasons as the phone's
//! property list: one place derives the version, the identity and the
//! library's name from what the workspace and the host say, so the package
//! never disagrees with the code inside it.

use std::path::Path;

use anyhow::Context;

use crate::handheld::LAUNCH_BACKGROUND;
use crate::Bundle;

/// The oldest Android the application runs on: 8.0, the first with every
/// interface the viewer and wgpu's two backends use (Vulkan 1.0 and
/// OpenGL ES 3.0 both predate it). Over 97 % of devices in use.
const MINIMUM_SDK: u32 = 26;
/// The Android the application is built and tested against.
pub(crate) const TARGET_SDK: u32 = 35;

/// The launcher icon's sizes, in pixels, by screen density.
const ICON_SIZES: [(&str, u32); 5] = [
    ("mdpi", 48),
    ("hdpi", 72),
    ("xhdpi", 96),
    ("xxhdpi", 144),
    ("xxxhdpi", 192),
];

/// The class of the activity, in the APK's own code (`android/java`).
const ACTIVITY: &str = "dev.lapoule.tuile.TuileActivity";

/// What a host's package is called as a native library: `lib<name>.so`, the
/// name the activity loads — the crate's name, with dashes as underscores.
pub(crate) fn library_name(package: &str) -> String {
    package.replace('-', "_")
}

/// `major.minor.patch` as the increasing integer Android compares versions
/// by: two digits each for minor and patch.
pub(crate) fn version_code(version: &str) -> anyhow::Result<u32> {
    let mut parts = version
        .split(['.', '-', '+'])
        .take(3)
        .map(|p| p.parse::<u32>());
    let mut next = || -> anyhow::Result<u32> {
        parts
            .next()
            .with_context(|| format!("version {version:?} has fewer than three numbers"))?
            .with_context(|| format!("version {version:?} is not numeric"))
    };
    let (major, minor, patch) = (next()?, next()?, next()?);
    anyhow::ensure!(
        minor < 100 && patch < 100,
        "version {version:?}: minor and patch must stay under 100"
    );
    Ok(major * 10_000 + minor * 100 + patch)
}

/// Text made safe for an XML attribute.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// `AndroidManifest.xml`.
///
/// Every line is there for a reason the system gives it:
///
/// - the identity, with the version taken from the workspace;
/// - `INTERNET`, the one permission: tiles come over the network;
/// - OpenGL ES 3.0 required, the floor of the fallback backend; Vulkan is
///   used where the device has it, and not required;
/// - `extractNativeLibs="false"`: the library is mapped straight from the
///   APK, stored uncompressed and page-aligned, rather than copied out;
/// - the activity handles its own configuration changes — a turn of the
///   screen is a resize, not the activity destroyed and the session with it;
/// - `lib_name` names the native library the activity loads;
/// - the import link (`<scheme>://import?src=…`) and the configuration file
///   (`.tuile`, viewed or shared) open the activity, which hands them to the
///   viewer;
/// - no backup: the tile store is a cache, and there is nothing else.
pub(crate) fn manifest(bundle: &Bundle, version: &str) -> anyhow::Result<String> {
    let code = version_code(version)?;
    let package = escape(&bundle.identifier);
    let label = escape(&bundle.name);
    let library = escape(&library_name(&bundle.executable));
    let scheme = escape(&bundle.scheme);
    let extension = crate::documents::EXTENSION;
    let version = escape(version);
    Ok(format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<!-- Written by tuile-viewer-bundle; not to be edited by hand. -->
<manifest xmlns:android="http://schemas.android.com/apk/res/android"
    package="{package}"
    android:versionCode="{code}"
    android:versionName="{version}">
    <uses-sdk android:minSdkVersion="{MINIMUM_SDK}" android:targetSdkVersion="{TARGET_SDK}" />
    <uses-permission android:name="android.permission.INTERNET" />
    <uses-feature android:glEsVersion="0x00030000" android:required="true" />
    <uses-feature android:name="android.hardware.vulkan.version" android:required="false" />
    <application
        android:label="{label}"
        android:icon="@mipmap/ic_launcher"
        android:hasCode="true"
        android:extractNativeLibs="false"
        android:allowBackup="false"
        android:appCategory="maps">
        <activity
            android:name="{ACTIVITY}"
            android:exported="true"
            android:launchMode="singleTask"
            android:theme="@style/Launch"
            android:configChanges="orientation|screenSize|screenLayout|smallestScreenSize|keyboard|keyboardHidden|navigation|uiMode|density">
            <meta-data android:name="android.app.lib_name" android:value="{library}" />
            <intent-filter>
                <action android:name="android.intent.action.MAIN" />
                <category android:name="android.intent.category.LAUNCHER" />
            </intent-filter>
            <!-- An import link, tapped in a browser or a message. -->
            <intent-filter>
                <action android:name="android.intent.action.VIEW" />
                <category android:name="android.intent.category.DEFAULT" />
                <category android:name="android.intent.category.BROWSABLE" />
                <data android:scheme="{scheme}" android:host="import" />
            </intent-filter>
            <!-- A configuration file opened from a file manager or a
                 download, by its extension where the address has one. -->
            <intent-filter>
                <action android:name="android.intent.action.VIEW" />
                <category android:name="android.intent.category.DEFAULT" />
                <data android:scheme="content" />
                <data android:scheme="file" />
                <data android:host="*" />
                <data android:mimeType="*/*" />
                <data android:pathPattern=".*\\.{extension}" />
                <data android:pathPattern=".*\\..*\\.{extension}" />
                <data android:pathPattern=".*\\..*\\..*\\.{extension}" />
            </intent-filter>
            <!-- A configuration file shared from another application, whose
                 address seldom says its name: the activity reads it, and the
                 viewer refuses what is not a configuration. -->
            <intent-filter>
                <action android:name="android.intent.action.SEND" />
                <category android:name="android.intent.category.DEFAULT" />
                <data android:mimeType="application/toml" />
                <data android:mimeType="application/octet-stream" />
                <data android:mimeType="text/plain" />
            </intent-filter>
        </activity>
    </application>
</manifest>
"#
    ))
}

/// `res/values/launch.xml`: the launch theme — no title bar, the whole
/// screen, and the globe's shell's blue behind the window from the first
/// frame the system draws, so there is no step to the viewer's own.
pub(crate) fn launch_theme() -> String {
    let [r, g, b] = LAUNCH_BACKGROUND.map(|c| (c * 255.0).round() as u8);
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<!-- Written by tuile-viewer-bundle; not to be edited by hand. -->
<resources>
    <color name="launch">#{r:02X}{g:02X}{b:02X}</color>
    <style name="Launch" parent="@android:style/Theme.Material.NoActionBar.Fullscreen">
        <item name="android:windowBackground">@color/launch</item>
        <item name="android:colorBackground">@color/launch</item>
    </style>
</resources>
"#
    )
}

/// Writes the manifest and the resources — the theme and the launcher icon
/// at every density — under `out`, replacing what was there.
pub(crate) fn resources(
    bundle: &Bundle,
    version: &str,
    master: &image::RgbaImage,
    out: &Path,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(out)?;
    std::fs::write(out.join("AndroidManifest.xml"), manifest(bundle, version)?)?;
    let res = out.join("res");
    if res.exists() {
        std::fs::remove_dir_all(&res)?;
    }
    let values = res.join("values");
    std::fs::create_dir_all(&values)?;
    std::fs::write(values.join("launch.xml"), launch_theme())?;
    for (density, side) in ICON_SIZES {
        let dir = res.join(format!("mipmap-{density}"));
        std::fs::create_dir_all(&dir)?;
        image::imageops::resize(master, side, side, image::imageops::FilterType::Lanczos3)
            .save(dir.join("ic_launcher.png"))
            .with_context(|| format!("writing the {density} icon"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_becomes_an_increasing_code() {
        assert_eq!(version_code("0.8.0").unwrap(), 800);
        assert_eq!(version_code("1.2.3").unwrap(), 10_203);
        assert_eq!(version_code("0.10.1-rc.1").unwrap(), 1_001);
        assert!(version_code("0.8.0").unwrap() < version_code("0.8.1").unwrap());
        assert!(version_code("0.9.99").unwrap() < version_code("1.0.0").unwrap());
        assert!(version_code("0.8").is_err());
        assert!(version_code("0.100.0").is_err());
        assert!(version_code("a.b.c").is_err());
    }

    /// The manifest names the host's library, the activity that loads it, the
    /// one permission and the version — and escapes what it is given.
    #[test]
    fn the_manifest_opens_links_and_configuration_files() {
        let xml = manifest(&Bundle::default(), "0.8.0").expect("a manifest");
        assert!(xml.contains(r#"<data android:scheme="tuile" android:host="import" />"#));
        assert!(xml.contains(r#"android:pathPattern=".*\\.tuile""#), "{xml}");
        assert!(xml.contains("android.intent.action.SEND"));
    }

    #[test]
    fn the_manifest_carries_the_identity_the_library_and_the_version() {
        let bundle = Bundle::default();
        let xml = manifest(&bundle, "0.8.0").unwrap();
        assert!(xml.contains(r#"package="dev.lapoule.tuile.viewer""#));
        assert!(xml.contains(r#"android:versionCode="800""#));
        assert!(xml.contains(r#"android:versionName="0.8.0""#));
        assert!(xml.contains(r#"android:value="tuile_wgpu_viewer""#));
        assert!(xml.contains(r#"android:name="dev.lapoule.tuile.TuileActivity""#));
        assert!(xml.contains("android.permission.INTERNET"));
        assert_eq!(xml.matches("uses-permission").count(), 1, "one permission");
        assert!(xml.contains(r#"android:extractNativeLibs="false""#));
        assert!(
            xml.contains("screenSize"),
            "a turn of the screen is a resize"
        );
        let odd = Bundle {
            name: "Tom & \"Jerry\"".into(),
            ..Bundle::default()
        };
        let xml = manifest(&odd, "0.8.0").unwrap();
        assert!(xml.contains(r#"android:label="Tom &amp; &quot;Jerry&quot;""#));
    }

    #[test]
    fn the_launch_theme_is_the_shell_blue() {
        let theme = launch_theme();
        assert!(theme.contains("#17263D"), "{theme}");
        assert!(theme.contains("NoActionBar.Fullscreen"));
    }

    #[test]
    fn resources_hold_an_icon_per_density() {
        let dir = tempfile::tempdir().unwrap();
        let master = image::RgbaImage::from_pixel(64, 64, image::Rgba([10, 20, 30, 255]));
        resources(&Bundle::default(), "0.8.0", &master, dir.path()).unwrap();
        assert!(dir.path().join("AndroidManifest.xml").is_file());
        assert!(dir.path().join("res/values/launch.xml").is_file());
        for (density, side) in ICON_SIZES {
            let icon = image::open(
                dir.path()
                    .join(format!("res/mipmap-{density}/ic_launcher.png")),
            )
            .unwrap();
            assert_eq!((icon.width(), icon.height()), (side, side));
        }
    }
}
