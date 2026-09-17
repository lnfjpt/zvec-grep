//! Compact directory IDs derived from source membership, scoped to one index.
//! The optional snapshot speeds reader startup; source records can rebuild it.
use std::{collections::HashMap, fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    EngineError, EngineResult,
    domain::{DirectoryId, DirectoryRecord, SourcePath},
    utils::atomic_write,
};

use super::path::{decode_path, encode_path};

pub(super) const CACHE_NAME: &str = "directories.json";

pub(super) struct DirectoryIds {
    by_path: HashMap<SourcePath, DirectoryId>,
    by_id: HashMap<DirectoryId, SourcePath>,
    next: Option<u32>,
    dirty: bool,
}

impl Default for DirectoryIds {
    fn default() -> Self {
        Self {
            by_path: HashMap::new(),
            by_id: HashMap::new(),
            next: Some(0),
            dirty: false,
        }
    }
}

impl DirectoryIds {
    fn claim(&mut self, directory: DirectoryRecord) -> EngineResult<()> {
        let DirectoryRecord {
            id,
            relative_path: path,
        } = directory;
        if self.by_path.get(&path).is_some_and(|other| *other != id)
            || self.by_id.get(&id).is_some_and(|other| *other != path)
        {
            return Err(invalid("conflicting directory IDs in source records"));
        }
        if !self.by_id.contains_key(&id) {
            self.by_path.insert(path.clone(), id);
            self.by_id.insert(id, path);
        }
        if self.next.is_some_and(|next| id.get() >= next) {
            self.next = id.get().checked_add(1);
        }
        Ok(())
    }

    pub(super) fn add_source(&mut self, file: &SourcePath, ids: &[u32]) -> EngineResult<()> {
        let ancestors = ancestors(file)?;
        if ancestors.len() != ids.len() {
            return Err(invalid(
                "source directory membership does not match path depth",
            ));
        }
        for (relative_path, id) in ancestors.into_iter().zip(ids) {
            self.claim(DirectoryRecord {
                id: DirectoryId::new(*id),
                relative_path,
            })?;
        }
        self.dirty = true;
        Ok(())
    }

    pub(super) fn resolve(&mut self, file: &SourcePath) -> EngineResult<Vec<DirectoryId>> {
        let ancestors = ancestors(file)?;
        let missing = ancestors
            .iter()
            .filter(|path| !self.by_path.contains_key(*path))
            .count();
        if missing > 0 {
            let next = self
                .next
                .ok_or_else(|| invalid("directory ID range exhausted"))?;
            if u128::from(next) + missing as u128 > u128::from(u32::MAX) + 1 {
                return Err(invalid("directory ID range exhausted"));
            }
        }
        ancestors
            .into_iter()
            .map(|path| {
                if let Some(id) = self.by_path.get(&path) {
                    return Ok(*id);
                }
                let id = DirectoryId::new(
                    self.next
                        .ok_or_else(|| invalid("directory ID range exhausted"))?,
                );
                self.claim(DirectoryRecord {
                    id,
                    relative_path: path,
                })?;
                self.dirty = true;
                Ok(id)
            })
            .collect()
    }

    pub(super) fn get(&self, path: &SourcePath) -> Option<DirectoryId> {
        self.by_path.get(path).copied()
    }

    pub(super) fn read_cache(home: &Path) -> EngineResult<Option<Self>> {
        let bytes = match fs::read(home.join(CACHE_NAME)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(EngineError::from_io("read directory cache", &error)),
        };
        // An invalid derived cache can be rebuilt from source membership.
        Ok(Self::decode_cache(&bytes).ok())
    }

    fn decode_cache(bytes: &[u8]) -> EngineResult<Self> {
        let cache: Cache =
            serde_json::from_slice(bytes).map_err(|error| invalid(error.to_string()))?;
        if cache.version != 1 {
            return Err(invalid("unsupported directory cache version"));
        }
        let mut result = Self::default();
        for (id, path) in cache.directories {
            result.claim(DirectoryRecord {
                id: DirectoryId::new(id),
                relative_path: decode_path(&path)?,
            })?;
        }
        Ok(result)
    }

    /// Runs after native flush and before clearing the existing file write journal.
    pub(super) fn write_cache(&mut self, home: &Path) -> EngineResult<()> {
        if !self.dirty {
            return Ok(());
        }
        let mut directories = self
            .by_id
            .iter()
            .map(|(id, path)| Ok((id.get(), encode_path(path)?)))
            .collect::<EngineResult<Vec<_>>>()?;
        directories.sort_unstable_by_key(|(id, _)| *id);
        let bytes = serde_json::to_vec(&Cache {
            version: 1,
            directories,
        })
        .map_err(|error| invalid(error.to_string()))?;
        atomic_write(&home.join(CACHE_NAME), &bytes)?;
        self.dirty = false;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cache {
    version: u32,
    directories: Vec<(u32, String)>,
}

fn ancestors(path: &SourcePath) -> EngineResult<Vec<SourcePath>> {
    let mut paths = path
        .ancestors()
        .skip(1)
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(SourcePath::new)
        .collect::<EngineResult<Vec<_>>>()?;
    paths.reverse();
    Ok(paths)
}

fn invalid(message: impl Into<String>) -> EngineError {
    EngineError::storage_failure(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconstructs_compact_membership_and_rejects_conflicts() {
        let mut ids = DirectoryIds::default();
        let path = SourcePath::new("src/deep/file.rs").expect("path");
        ids.add_source(&path, &[7, 9]).expect("source");
        assert_eq!(
            ids.resolve(&path).expect("IDs"),
            [DirectoryId::new(7), DirectoryId::new(9)]
        );
        let sibling = SourcePath::new("src/new/file.rs").expect("path");
        assert_eq!(
            ids.resolve(&sibling).expect("IDs"),
            [DirectoryId::new(7), DirectoryId::new(10)]
        );
        assert!(ids.add_source(&path, &[7]).is_err());
        assert!(ids.add_source(&path, &[8, 9]).is_err());
        assert!(
            ids.add_source(&SourcePath::new("other/file.rs").expect("path"), &[7])
                .is_err()
        );
    }

    #[test]
    fn u32_exhaustion_does_not_partially_allocate_ancestors() {
        let mut ids = DirectoryIds::default();
        ids.add_source(&SourcePath::new("old/file").expect("path"), &[u32::MAX - 1])
            .expect("source");
        let nested = SourcePath::new("new/nested/file").expect("path");
        assert!(ids.resolve(&nested).is_err());
        assert_eq!(ids.get(&SourcePath::new("new").expect("path")), None);
        let last = SourcePath::new("new/file").expect("path");
        assert_eq!(
            ids.resolve(&last).expect("last ID"),
            [DirectoryId::new(u32::MAX)]
        );
        assert_eq!(
            ids.resolve(&last).expect("existing ID"),
            [DirectoryId::new(u32::MAX)]
        );
        assert!(ids.resolve(&nested).is_err());
        assert!(
            ids.resolve(&SourcePath::new("root-file").expect("path"))
                .expect("no ancestors")
                .is_empty()
        );
        assert!(DirectoryIds::decode_cache(&serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "directories": [[u64::from(u32::MAX) + 1, encode_path(&SourcePath::new("too-large").expect("path")).expect("encoded path")]]
        })).expect("JSON")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cache_round_trip_preserves_native_paths_and_invalid_cache_is_discarded() {
        use std::os::unix::ffi::OsStringExt;
        let home = tempfile::tempdir().expect("home");
        let mut ids = DirectoryIds::default();
        for byte in [0xfe, 0xff] {
            let file = SourcePath::new(std::ffi::OsString::from_vec(vec![byte, b'/', b'f']))
                .expect("path");
            ids.resolve(&file).expect("ID");
        }
        ids.write_cache(home.path()).expect("cache");
        let decoded = DirectoryIds::read_cache(home.path())
            .expect("read")
            .expect("cache");
        assert_eq!(decoded.by_path, ids.by_path);
        assert_eq!(decoded.by_path.len(), 2);
        fs::write(home.path().join(CACHE_NAME), b"broken cache").expect("corrupt cache");
        assert!(
            DirectoryIds::read_cache(home.path())
                .expect("rebuild fallback")
                .is_none()
        );
    }
}
