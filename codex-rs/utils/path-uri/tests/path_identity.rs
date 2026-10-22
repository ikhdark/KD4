#![allow(clippy::unwrap_used)]

use base64::Engine;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::LegacyAppPathString;
use codex_utils_path_uri::PathConvention;
use codex_utils_path_uri::PathUri;
use std::path::Path;

fn windows_path(raw: &str) -> PathUri {
    LegacyAppPathString::from_path(Path::new(raw))
        .to_path_uri(PathConvention::Windows)
        .unwrap()
}

#[test]
fn slash_unc_fallback_keeps_its_declared_windows_convention() {
    for raw in ["//localhost/share/repo", "/\\localhost/share/repo"] {
        let uri = windows_path(raw);
        let restored: PathUri =
            serde_json::from_str(&serde_json::to_string(&uri).unwrap()).unwrap();
        assert_eq!(restored, uri);
        assert_eq!(
            restored.infer_path_convention(),
            Some(PathConvention::Windows)
        );
        assert_eq!(restored.inferred_native_path_string(), raw);
        assert!(LegacyAppPathString::from_path_uri(&restored, PathConvention::Posix).is_err());
        assert!(!restored.inferred_native_path_string().contains('\0'));
    }
}

#[test]
fn localhost_unc_hierarchy_stops_at_the_original_share() {
    for raw in [r"\\localhost\share\repo", "//localhost/share/repo"] {
        let root = windows_path(raw);
        let child = root.join(r"src\a%2Fb.rs").unwrap();
        assert_eq!(child.basename().as_deref(), Some("a%2Fb.rs"));
        assert!(child.starts_with(&root));
        assert!(!child.starts_with(&windows_path(r"\\localhost\share2")));
        assert_eq!(
            child.parent().unwrap().parent().unwrap(),
            windows_path(r"\\localhost\share\repo")
        );
        assert_eq!(
            root.join(r"..\..\..\marker").unwrap(),
            windows_path(r"\\localhost\share\marker")
        );
        assert_eq!(
            root.join(r"\marker").unwrap(),
            windows_path(r"\\localhost\share\marker")
        );
        let share = root.parent().unwrap();
        assert_eq!(share, windows_path(r"\\localhost\share"));
        assert!(share.parent().is_none());
        assert!(!root.starts_with(&PathUri::parse("file:///share/repo").unwrap()));
    }
}

#[test]
fn legacy_slash_payload_is_not_silently_promoted_to_windows() {
    let bytes = "//localhost/share/repo"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let uri = PathUri::parse(&format!("file:///%00/bad/path/{payload}")).unwrap();
    // An untagged slash-leading payload can also represent POSIX bytes with
    // NULs. It cannot retroactively acquire authoritative Windows identity.
    assert_ne!(uri.infer_path_convention(), Some(PathConvention::Windows));
    assert!(uri.to_abs_path().is_err());
}

#[test]
fn ordinary_localhost_file_url_remains_local_not_unc() {
    assert_eq!(
        PathUri::parse("file://localhost/C:/repo").unwrap(),
        PathUri::parse("file:///C:/repo").unwrap(),
    );
    assert_ne!(
        windows_path(r"\\localhost\C:\repo"),
        PathUri::parse("file:///C:/repo").unwrap()
    );
}

#[test]
fn safe_extended_aliases_share_the_checked_native_constructor() {
    for (ordinary, extended) in [
        (r"C:\repo\source.rs", r"\\?\C:\repo\source.rs"),
        (
            r"\\server\share\source.rs",
            r"\\?\UNC\server\share\source.rs",
        ),
    ] {
        let expected =
            PathUri::from_abs_path(&AbsolutePathBuf::from_absolute_path_checked(ordinary).unwrap());
        let actual = windows_path(extended);
        assert_eq!(actual, expected);
        assert_eq!(
            actual.to_abs_path().unwrap(),
            expected.to_abs_path().unwrap()
        );
        assert_eq!(windows_path(r"C:\base").join(extended).unwrap(), expected);
    }
}

#[test]
fn namespace_sensitive_native_paths_survive_serialization() {
    use std::os::windows::ffi::OsStrExt;

    for raw in [r"\\?\C:\repo\name.", r"\\?\C:\repo\name "] {
        let native = AbsolutePathBuf::from_absolute_path_checked(raw).unwrap();
        let uri = PathUri::from_abs_path(&native);
        assert_ne!(uri, windows_path(&raw[4..]));
        let restored: PathUri =
            serde_json::from_str(&serde_json::to_string(&uri).unwrap()).unwrap();
        assert_eq!(
            restored
                .to_abs_path()
                .unwrap()
                .as_path()
                .as_os_str()
                .encode_wide()
                .collect::<Vec<_>>(),
            native
                .as_path()
                .as_os_str()
                .encode_wide()
                .collect::<Vec<_>>(),
        );
    }
}

#[test]
fn unc_parent_reuses_the_ordinary_constructor_after_removing_a_surrogate() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    let raw = r"\\server\share\repo\"
        .encode_utf16()
        .chain([0xd800])
        .collect::<Vec<_>>();
    let native = PathBuf::from(OsString::from_wide(&raw));
    let uri = PathUri::from_host_native_path(native).unwrap();
    let parent = uri.parent().unwrap();
    assert_eq!(parent, PathUri::parse("file://server/share/repo").unwrap());
    assert_eq!(
        parent.to_abs_path().unwrap(),
        AbsolutePathBuf::from_absolute_path_checked(r"\\server\share\repo").unwrap(),
    );
}
