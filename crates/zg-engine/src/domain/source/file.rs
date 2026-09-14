use std::path::{Component, Path, PathBuf};

use crate::{EngineError, EngineResult, utils::sha256_hex_parts};

use super::format::{FileCategory, FileFormat};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct FileId(String);

impl FileId {
    #[track_caller]
    pub(crate) fn new(value: impl Into<String>) -> EngineResult<Self> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(EngineError::invalid_argument("file id must not be blank"));
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// File identity is scoped to the workspace, independent of its current root directory.
    pub(crate) fn for_path(workspace_id: &str, relative_path: &Path) -> EngineResult<Self> {
        if workspace_id.trim().is_empty() {
            return Err(EngineError::invalid_argument(
                "workspace id must not be blank",
            ));
        }
        validate_relative_path(relative_path)?;
        // Components remove redundant separators and interior `.` segments before hashing.
        let relative_path: PathBuf = relative_path.components().collect();
        Ok(Self(sha256_hex_parts([
            workspace_id.as_bytes(),
            b"\0",
            relative_path.as_os_str().as_encoded_bytes(),
        ])))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileSnapshot {
    pub size_bytes: u64,
    pub modified_epoch_ms: Option<u64>,
    pub content_hash: Option<String>,
}

/// Describes a file relative to its workspace's sole root and its last observed state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceFile {
    pub id: FileId,
    pub relative_path: PathBuf,
    pub formats: Vec<FileFormat>,
    pub snapshot: FileSnapshot,
}

impl SourceFile {
    pub(crate) fn has_category(&self, category: FileCategory) -> bool {
        self.formats
            .iter()
            .any(|format| format.categories().contains(&category))
    }

    #[track_caller]
    pub(crate) fn validate(&self) -> EngineResult<()> {
        validate_relative_path(&self.relative_path)?;
        if self.formats.is_empty()
            || (self.formats.len() > 1 && self.formats.contains(&FileFormat::Unknown))
            || self
                .formats
                .iter()
                .enumerate()
                .any(|(index, format)| self.formats[..index].contains(format))
        {
            return Err(EngineError::invalid_argument(
                "source formats must be non-empty and unique; unknown must stand alone",
            ));
        }
        Ok(())
    }
}

fn validate_relative_path(path: &Path) -> EngineResult<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(EngineError::invalid_argument(
            "source relative path must stay within its workspace root",
        ));
    }
    if path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(EngineError::invalid_argument(
            "source file path must not contain NUL",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_id_rejects_blank_inputs_without_normalizing() {
        for blank in ["", " ", "\t\n", "\u{2003}"] {
            assert!(FileId::new(blank).is_err());
        }
        assert_eq!(
            FileId::new(" file-1 ").expect("file id").as_str(),
            " file-1 "
        );
    }

    #[test]
    fn validates_paths_and_format_sets_without_reading_the_file() {
        let file = SourceFile {
            id: FileId::new("file").expect("id"),
            relative_path: PathBuf::from("nested/fixture.rs"),
            formats: vec![FileFormat::Rust],
            snapshot: FileSnapshot {
                size_bytes: 0,
                modified_epoch_ms: None,
                content_hash: None,
            },
        };
        file.validate().expect("valid source");
        assert!(file.has_category(FileCategory::Code));
        assert!(!file.has_category(FileCategory::Binary));
        for path in [
            "",
            ".",
            "./fixture.rs",
            "../fixture.rs",
            "nested/../fixture.rs",
            "/fixture.rs",
            "bad\0path",
        ] {
            let mut invalid = file.clone();
            invalid.relative_path = PathBuf::from(path);
            assert!(invalid.validate().is_err(), "{path}");
        }
        for formats in [
            vec![],
            vec![FileFormat::Rust, FileFormat::Rust],
            vec![FileFormat::Rust, FileFormat::Unknown],
        ] {
            let mut invalid = file.clone();
            invalid.formats = formats;
            assert!(invalid.validate().is_err());
        }
        let mut unknown = file;
        unknown.formats = vec![FileFormat::Unknown];
        unknown
            .validate()
            .expect("unknown is a valid classification");
    }

    #[test]
    fn file_identity_uses_workspace_and_normalized_relative_path() {
        let id = FileId::for_path("workspace", Path::new("nested/fixture.rs")).expect("file ID");
        for path in [
            "nested/fixture.rs",
            "nested//fixture.rs",
            "nested/./fixture.rs",
        ] {
            assert_eq!(
                FileId::for_path("workspace", Path::new(path)).expect("file ID"),
                id
            );
        }
        assert_ne!(
            FileId::for_path("other", Path::new("nested/fixture.rs")).expect("other workspace"),
            id
        );
        assert_ne!(
            FileId::for_path("workspace", Path::new("other.rs")).expect("other file"),
            id
        );
        assert!(FileId::for_path(" ", Path::new("fixture.rs")).is_err());
        for path in ["", ".", "../fixture.rs", "/fixture.rs", "bad\0path"] {
            assert!(
                FileId::for_path("workspace", Path::new(path)).is_err(),
                "{path}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_identity_preserves_non_unicode_path_bytes() {
        use std::os::unix::ffi::OsStrExt;

        let first = Path::new(std::ffi::OsStr::from_bytes(b"file-\xff.rs"));
        let second = Path::new(std::ffi::OsStr::from_bytes(b"file-\xfe.rs"));
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());
        assert_ne!(
            FileId::for_path("workspace", first).expect("first ID"),
            FileId::for_path("workspace", second).expect("second ID"),
        );
    }

    #[cfg(windows)]
    #[test]
    fn file_identity_preserves_non_unicode_path_units() {
        use std::os::windows::ffi::OsStringExt;

        let first = PathBuf::from(std::ffi::OsString::from_wide(&[0x0066, 0xd800]));
        let second = PathBuf::from(std::ffi::OsString::from_wide(&[0x0066, 0xd801]));
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());
        assert_ne!(
            FileId::for_path("workspace", &first).expect("first ID"),
            FileId::for_path("workspace", &second).expect("second ID"),
        );
    }
}
