//! Path safety helpers.
//!
//! Manifests and archives are untrusted input: they may come from another
//! device, from cloud storage, or from a tampered local file. Every relative
//! path is validated before it is joined onto a filesystem root.

use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

/// Validate an untrusted, manifest-relative path.
///
/// Accepts only plain relative paths made of `Normal` components. Rejects:
/// - empty paths
/// - absolute paths and Windows drive/UNC prefixes
/// - `..` and `.` components
/// - NUL bytes and backslash-escaped separators on non-Windows hosts
///
/// The returned path uses the host separator and is guaranteed to stay below
/// the directory it is later joined to.
pub fn sanitize_relative_path(input: &str) -> Result<PathBuf> {
    if input.is_empty() {
        return Err(Error::UnsafePath("empty path".into()));
    }
    if input.contains('\0') {
        return Err(Error::UnsafePath(format!("NUL byte in path: {input:?}")));
    }
    if input.contains('\\') {
        return Err(Error::UnsafePath(format!(
            "backslash separator in path: {input:?}"
        )));
    }
    if input.starts_with('/') {
        return Err(Error::UnsafePath(format!("absolute path: {input:?}")));
    }
    // Reject Windows drive prefixes regardless of host OS so a manifest built
    // on Windows cannot be replayed on Linux or vice versa.
    let bytes = input.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Err(Error::UnsafePath(format!(
            "windows drive prefix: {input:?}"
        )));
    }

    let mut out = PathBuf::new();
    for component in Path::new(input).components() {
        match component {
            Component::Normal(part) => {
                if part.is_empty() {
                    return Err(Error::UnsafePath(format!("empty component: {input:?}")));
                }
                out.push(part);
            }
            Component::CurDir => {
                return Err(Error::UnsafePath(format!(
                    "explicit current-dir component: {input:?}"
                )));
            }
            Component::ParentDir => {
                return Err(Error::UnsafePath(format!(
                    "path traversal component: {input:?}"
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::UnsafePath(format!(
                    "absolute path component: {input:?}"
                )));
            }
        }
    }

    if out.as_os_str().is_empty() {
        return Err(Error::UnsafePath(format!("empty path: {input:?}")));
    }
    Ok(out)
}

/// Join a validated relative path onto `root`.
pub fn safe_join(root: &Path, relative: &str) -> Result<PathBuf> {
    Ok(root.join(sanitize_relative_path(relative)?))
}

/// Validate a user-supplied absolute root directory.
///
/// Roots must be absolute, must not contain `..`, and must exist as a
/// directory. Existing symlinks are resolved before validation so that two
/// spellings of the same directory compare equally.
pub fn validate_root(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    if !path.is_absolute() {
        return Err(Error::UnsafePath(format!(
            "save location must be an absolute path: {}",
            path.display()
        )));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(Error::UnsafePath(format!(
            "save location may not contain '..': {}",
            path.display()
        )));
    }
    let canonical = path.canonicalize().map_err(|e| Error::io(path, e))?;
    if !canonical.is_dir() {
        return Err(Error::InvalidState(format!(
            "save location is not a directory: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

/// Convert an absolute path below `root` into a manifest-relative `/`-joined
/// string. Fails if the path escapes `root`.
pub fn relative_to(root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(root).map_err(|_| {
        Error::UnsafePath(format!(
            "{} is outside of {}",
            path.display(),
            root.display()
        ))
    })?;

    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => {
                let text = part.to_str().ok_or_else(|| {
                    Error::UnsafePath(format!("non-UTF-8 path component below {}", root.display()))
                })?;
                parts.push(text.to_string());
            }
            _ => {
                return Err(Error::UnsafePath(format!(
                    "unexpected path component in {}",
                    relative.display()
                )));
            }
        }
    }
    Ok(parts.join("/"))
}

/// Names created by SaveSync itself inside save trees; always excluded from
/// snapshots and restore cleanup.
pub fn is_internal_artifact(name: &str) -> bool {
    name == ".savesync" || name.starts_with(".savesync-tmp-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_relative_paths() {
        assert_eq!(
            sanitize_relative_path("slot1/data.bin").unwrap(),
            PathBuf::from("slot1/data.bin")
        );
        assert_eq!(
            sanitize_relative_path("file with space.sav").unwrap(),
            PathBuf::from("file with space.sav")
        );
    }

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        for bad in [
            "../escape",
            "slot/../../escape",
            "/etc/passwd",
            "C:/Windows/system32",
            "..",
            ".",
            "",
            "slot\\..\\escape",
            "a\0b",
        ] {
            assert!(
                sanitize_relative_path(bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn rejects_non_absolute_roots() {
        assert!(validate_root("relative/path").is_err());
    }

    #[test]
    fn rejects_roots_with_parent_components() {
        let dir = tempfile::tempdir().unwrap();
        let with_parent = dir.path().join("sub").join("..");
        assert!(validate_root(with_parent).is_err());
    }

    #[test]
    fn computes_relative_paths() {
        let root = Path::new("/games/save");
        assert_eq!(
            relative_to(root, &root.join("slot1").join("a.bin")).unwrap(),
            "slot1/a.bin"
        );
        assert!(relative_to(root, Path::new("/etc/passwd")).is_err());
    }
}
