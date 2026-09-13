use std::{
    fs::{self, DirBuilder},
    io::Read,
    path::Path,
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

pub(super) fn prepare(path: &Path) -> EngineResult<()> {
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
