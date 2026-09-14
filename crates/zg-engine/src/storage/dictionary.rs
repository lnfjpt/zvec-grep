use std::{
    fs::{self, DirBuilder},
    io::Read,
    path::{Path, PathBuf},
};

use flate2::read::GzDecoder;

use crate::{
    EngineError, EngineResult,
    utils::{atomic_write, sha256_hex, sync_directory},
};

const DICTIONARIES: [(&str, &[u8], &str); 2] = [
    (
        "jieba.dict.utf8",
        include_bytes!("../../resources/jieba/jieba.dict.utf8.gz"),
        "6f7d4350e8861ef4139b2e3a6fad05430c19ae71f4b8378190edecac8aae2e6a",
    ),
    (
        "hmm_model.utf8",
        include_bytes!("../../resources/jieba/hmm_model.utf8.gz"),
        "f17790586ac86dd048c8adffed052c4bd2b28ed0682972c1275e59040c0589a7",
    ),
];

/// Engine assets live outside individual workspaces so moving a workspace does
/// not invalidate the absolute dictionary path persisted by the native FTS index.
pub(super) fn cache_path() -> EngineResult<PathBuf> {
    let path = if let Some(path) = std::env::var_os("ZVEC_GREP_DICTIONARY_CACHE") {
        PathBuf::from(path)
    } else {
        crate::config::global_config_path()?
            .parent()
            .ok_or_else(|| EngineError::invalid_argument("global config path has no parent"))?
            .join("cache/jieba-v1")
    };
    validate_cache_path(&path)?;
    Ok(path.components().collect())
}

pub(super) fn prepare_cache(path: &Path) -> EngineResult<()> {
    validate_cache_path(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| EngineError::invalid_argument("dictionary cache path has no parent"))?;
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(parent)
        .map_err(|error| io_error("create dictionary cache parents", parent, &error))?;
    prepare(path)
}

fn validate_cache_path(path: &Path) -> EngineResult<()> {
    if !path.is_absolute() || path.to_str().is_none() {
        return Err(EngineError::invalid_argument(
            "dictionary cache must be an absolute UTF-8 path; check ZVEC_GREP_DICTIONARY_CACHE",
        ));
    }
    Ok(())
}

fn prepare(path: &Path) -> EngineResult<()> {
    let mut initialized = false;
    for (name, compressed, checksum) in DICTIONARIES {
        let destination = path.join(name);
        if fs::read(&destination).is_ok_and(|bytes| checksum_matches(&bytes, checksum)) {
            continue;
        }
        let mut bytes = Vec::new();
        GzDecoder::new(compressed)
            .read_to_end(&mut bytes)
            .map_err(|error| io_error("decode bundled dictionary", &destination, &error))?;
        if !checksum_matches(&bytes, checksum) {
            return Err(EngineError::storage_failure(
                "bundled dictionary checksum mismatch",
            ));
        }
        if !initialized {
            let mut builder = DirBuilder::new();
            builder.recursive(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            if let Err(error) = builder.create(path)
                && !(error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir())
            {
                return Err(io_error("create dictionary directory", path, &error));
            }
            sync_directory(path)?;
            sync_directory(path.parent().unwrap_or_else(|| Path::new(".")))?;
            initialized = true;
        }
        atomic_write(&destination, &bytes)?;
    }
    Ok(())
}

fn checksum_matches(bytes: &[u8], checksum: &str) -> bool {
    sha256_hex(bytes) == checksum
}

fn io_error(action: &str, path: &Path, error: &std::io::Error) -> EngineError {
    EngineError::from_io(format!("cannot {action} {}", path.display()), error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_restores_bundled_dictionaries_offline() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let path = directory.path().join("user/cache/jieba-v1");
        prepare_cache(&path).expect("prepare cache with missing parents");
        fs::remove_file(path.join(DICTIONARIES[0].0)).expect("evict dictionary");
        fs::write(path.join(DICTIONARIES[1].0), b"corrupt").expect("corrupt model");
        prepare_cache(&path).expect("repair cache from bundled resources");
        for (name, _, checksum) in DICTIONARIES {
            assert!(checksum_matches(
                &fs::read(path.join(name)).expect("cached file"),
                checksum
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for directory in [path.as_path(), path.parent().expect("cache parent")] {
                assert_eq!(
                    fs::metadata(directory)
                        .expect("cache directory")
                        .permissions()
                        .mode()
                        & 0o777,
                    0o700
                );
            }
        }
        assert!(prepare_cache(Path::new("relative-cache")).is_err());
    }

    #[test]
    fn dictionary_initialization_requires_an_existing_parent_and_directory_target() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let missing = directory.path().join("missing");
        let path = missing.join("dictionary");
        let error = prepare(&path).expect_err("missing dictionary parent");
        assert!(error.to_string().contains("create dictionary directory"));
        assert!(error.to_string().contains(&path.display().to_string()));
        assert!(!missing.exists());

        let path = directory.path().join("dictionary");
        fs::write(&path, b"keep this file").expect("existing file");
        let error = prepare(&path).expect_err("dictionary target is a file");
        assert!(error.to_string().contains("create dictionary directory"));
        assert_eq!(
            fs::read(&path).expect("existing file retained"),
            b"keep this file"
        );
    }
}
