// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The configuration file's type, declared to the system so that opening a
//! `.tuile` file opens the application: a double-click in the Finder,
//! *Open in…* from Files, AirDrop or Mail on a phone, a view or a share on
//! Android.
//!
//! **Why `.tuile` and not `.toml` or `.tuile.toml`.** The file is TOML, and
//! says so in its type (it conforms to `public.toml`, so a text editor's
//! *Open With* still offers itself). But the systems match a file to an
//! application by its *last* extension: an application that claimed `.toml`
//! would claim every `Cargo.toml` on the machine, and `.tuile.toml` is a
//! `.toml` to them. A short extension of its own is what makes a
//! double-click open the right program, and nothing else.

use crate::Bundle;

/// The file name extension of a configuration file.
pub(crate) const EXTENSION: &str = "tuile";

/// The configuration file's type identifier: the application's, with
/// `.config`.
pub(crate) fn type_identifier(bundle: &Bundle) -> String {
    format!("{}.config", bundle.identifier)
}

/// `UTExportedTypeDeclarations` and `CFBundleDocumentTypes`: the type, and
/// that this application opens it — owner, as a viewer.
pub(crate) fn declarations(bundle: &Bundle) -> [(&'static str, plist::Value); 2] {
    use plist::Value::{Array, Dictionary, String as Text};
    let text = |s: &str| Text(s.to_owned());
    let id = type_identifier(bundle);

    let mut tags = plist::Dictionary::new();
    tags.insert(
        "public.filename-extension".into(),
        Array(vec![text(EXTENSION)]),
    );
    tags.insert(
        "public.mime-type".into(),
        Array(vec![text("application/toml")]),
    );
    let mut declared = plist::Dictionary::new();
    declared.insert("UTTypeIdentifier".into(), text(&id));
    declared.insert(
        "UTTypeDescription".into(),
        text(&format!("{} configuration", bundle.name)),
    );
    declared.insert(
        "UTTypeConformsTo".into(),
        Array(vec![text("public.toml"), text("public.plain-text")]),
    );
    declared.insert("UTTypeTagSpecification".into(), Dictionary(tags));

    let mut document = plist::Dictionary::new();
    document.insert(
        "CFBundleTypeName".into(),
        text(&format!("{} configuration", bundle.name)),
    );
    document.insert("CFBundleTypeRole".into(), text("Viewer"));
    document.insert("LSHandlerRank".into(), text("Owner"));
    document.insert("LSItemContentTypes".into(), Array(vec![text(&id)]));

    [
        (
            "UTExportedTypeDeclarations",
            Array(vec![Dictionary(declared)]),
        ),
        ("CFBundleDocumentTypes", Array(vec![Dictionary(document)])),
    ]
}

/// The URL scheme's declaration, as both platforms spell it.
pub(crate) fn url_types(bundle: &Bundle) -> plist::Value {
    use plist::Value::{Array, Dictionary, String as Text};
    let mut url_type = plist::Dictionary::new();
    url_type.insert("CFBundleURLName".into(), Text(bundle.identifier.clone()));
    url_type.insert("CFBundleTypeRole".into(), Text("Viewer".into()));
    url_type.insert(
        "CFBundleURLSchemes".into(),
        Array(vec![Text(bundle.scheme.clone())]),
    );
    Array(vec![Dictionary(url_type)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_type_is_the_applications_and_conforms_to_toml() {
        let bundle = Bundle::default();
        let [(exported, declared), (documents, opened)] = declarations(&bundle);
        assert_eq!(exported, "UTExportedTypeDeclarations");
        assert_eq!(documents, "CFBundleDocumentTypes");
        let declared = &declared.as_array().expect("an array")[0];
        let declared = declared.as_dictionary().expect("a dictionary");
        assert_eq!(
            declared.get("UTTypeIdentifier").and_then(|v| v.as_string()),
            Some("dev.lapoule.tuile.viewer.config")
        );
        let conforms: Vec<_> = declared
            .get("UTTypeConformsTo")
            .and_then(|v| v.as_array())
            .expect("conformance")
            .iter()
            .filter_map(|v| v.as_string())
            .collect();
        assert!(conforms.contains(&"public.toml"));
        let opened = &opened.as_array().expect("an array")[0];
        let opened = opened.as_dictionary().expect("a dictionary");
        assert_eq!(
            opened.get("LSHandlerRank").and_then(|v| v.as_string()),
            Some("Owner")
        );
    }
}
