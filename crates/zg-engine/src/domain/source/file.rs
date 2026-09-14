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
        let (encoding, path_bytes) = identity_path(relative_path)?;
        let workspace_length = u64::try_from(workspace_id.len())
            .map_err(|_| EngineError::invalid_argument("workspace ID is too long"))?
            .to_le_bytes();
        Ok(Self(sha256_hex_parts([
            b"file-v2\0".as_slice(),
            &workspace_length,
            workspace_id.as_bytes(),
            encoding,
            b"\0",
            &path_bytes,
        ])))
    }
}

/// Unicode identities use UTF-8 components separated by `/`. Non-Unicode paths
/// use tagged, lossless platform encodings and are not portable across platforms.
fn identity_path(path: &Path) -> EngineResult<(&'static [u8], Vec<u8>)> {
    validate_relative_path(path)?;
    let components: Vec<_> = path.components().map(Component::as_os_str).collect();
    if let Some(parts) = components
        .iter()
        .map(|part| part.to_str())
        .collect::<Option<Vec<_>>>()
    {
        return Ok((b"utf8", parts.join("/").into_bytes()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let parts: Vec<_> = components.iter().map(|part| part.as_bytes()).collect();
        Ok((b"unix-bytes", parts.join(&b'/')))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let path: PathBuf = components.into_iter().collect();
        let bytes = path
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect();
        Ok((b"windows-utf16le", bytes))
    }
    #[cfg(not(any(unix, windows)))]
    Err(EngineError::invalid_argument(
        "non-Unicode file identity is unsupported on this platform",
    ))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FileIndexStatus {
    NotIndexed,
    Indexed {
        indexed_epoch_ms: u64,
        entity_count: u64,
    },
    Failed {
        error: String,
    },
}

impl FileIndexStatus {
    pub(crate) const fn is_indexed(&self) -> bool {
        self.indexed_epoch_ms().is_some()
    }

    pub(crate) const fn indexed_epoch_ms(&self) -> Option<u64> {
        match self {
            Self::Indexed {
                indexed_epoch_ms, ..
            } => Some(*indexed_epoch_ms),
            Self::NotIndexed | Self::Failed { .. } => None,
        }
    }

    pub(crate) const fn entity_count(&self) -> u64 {
        match self {
            Self::Indexed { entity_count, .. } => *entity_count,
            Self::NotIndexed | Self::Failed { .. } => 0,
        }
    }

    pub(crate) fn error(&self) -> Option<&str> {
        match self {
            Self::Failed { error } => Some(error),
            Self::NotIndexed | Self::Indexed { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileSnapshot {
    pub size_bytes: u64,
    pub modified_epoch_ms: Option<u64>,
    pub content_hash: Option<String>,
}

/// A file relative to its workspace's sole root. An indexed snapshot describes
/// the exact source bytes used by its committed entities and fragments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileRecord {
    pub id: FileId,
    pub relative_path: PathBuf,
    pub formats: Vec<FileFormat>,
    pub snapshot: FileSnapshot,
    pub index_status: FileIndexStatus,
}

impl FileRecord {
    pub(crate) fn has_category(&self, category: FileCategory) -> bool {
        self.formats
            .iter()
            .any(|format| format.categories().contains(&category))
    }

    #[track_caller]
    pub(crate) fn validate(&self) -> EngineResult<()> {
        validate_relative_path(&self.relative_path)?;
        if self.index_status.is_indexed() && self.snapshot.content_hash.is_none() {
            return Err(EngineError::invalid_argument(
                "indexed files must have a content hash",
            ));
        }
        if self
            .index_status
            .error()
            .is_some_and(|error| error.trim().is_empty())
        {
            return Err(EngineError::invalid_argument(
                "failed files must have a non-blank error",
            ));
        }
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
        let file = FileRecord {
            id: FileId::new("file").expect("id"),
            relative_path: PathBuf::from("nested/fixture.rs"),
            formats: vec![FileFormat::Rust],
            snapshot: FileSnapshot {
                size_bytes: 0,
                modified_epoch_ms: None,
                content_hash: None,
            },
            index_status: FileIndexStatus::NotIndexed,
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
    fn validates_index_status_against_the_snapshot() {
        let mut file = FileRecord {
            id: FileId::new("file").expect("file ID"),
            relative_path: PathBuf::from("empty.txt"),
            formats: vec![FileFormat::Text],
            snapshot: FileSnapshot {
                size_bytes: 0,
                modified_epoch_ms: None,
                content_hash: None,
            },
            index_status: FileIndexStatus::NotIndexed,
        };
        file.validate().expect("unread file");
        assert!(!file.index_status.is_indexed());
        assert_eq!(file.index_status.indexed_epoch_ms(), None);
        assert_eq!(file.index_status.entity_count(), 0);
        assert_eq!(file.index_status.error(), None);
        file.index_status = FileIndexStatus::Indexed {
            indexed_epoch_ms: 7,
            entity_count: 0,
        };
        assert!(
            file.validate().is_err(),
            "indexed source must identify its actual bytes"
        );
        file.snapshot.content_hash = Some(crate::utils::sha256_hex(b""));
        file.validate().expect("success with zero entities");
        assert!(file.index_status.is_indexed());
        assert_eq!(file.index_status.indexed_epoch_ms(), Some(7));
        for error in ["", " ", "\t\n"] {
            file.index_status = FileIndexStatus::Failed {
                error: error.to_owned(),
            };
            assert!(file.validate().is_err());
        }
        file.index_status = FileIndexStatus::Failed {
            error: "cannot decode input".to_owned(),
        };
        file.snapshot.content_hash = None;
        file.validate()
            .expect("failure before source bytes are read");
        assert_eq!(file.index_status.error(), Some("cannot decode input"));
    }

    #[test]
    fn unicode_identity_uses_the_versioned_utf8_wire_format() {
        let path = Path::new("nested").join("正文.rs");
        assert_eq!(
            identity_path(&path).expect("identity path"),
            (b"utf8".as_slice(), "nested/正文.rs".as_bytes().to_vec())
        );
        // Pin the persistent identity protocol independently of native path encoding.
        assert_eq!(
            FileId::for_path("workspace", &path)
                .expect("file ID")
                .as_str(),
            "b3e4ee053bd36d1f96abaeb8283e25f6ebea6deb1db082ab022121e94992034f"
        );
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
        assert_eq!(
            identity_path(first).expect("native identity"),
            (b"unix-bytes".as_slice(), b"file-\xff.rs".to_vec())
        );
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
        assert_eq!(
            identity_path(&first).expect("native identity"),
            (b"windows-utf16le".as_slice(), vec![0x66, 0x00, 0x00, 0xd8])
        );
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());
        assert_ne!(
            FileId::for_path("workspace", &first).expect("first ID"),
            FileId::for_path("workspace", &second).expect("second ID"),
        );
    }
}
