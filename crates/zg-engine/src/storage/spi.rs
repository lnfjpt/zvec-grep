//! Storage interfaces consumed by indexing, search, and workspace lifecycle services.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::{
    EngineError,
    domain::{DirectoryId, Entity, EntityFragment, EntityId, FileId, FileRecord, SymbolType},
    models::EmbeddingMetric,
};

pub(crate) type StorageResult<T> = Result<T, EngineError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceIndexEmbeddingSchema {
    pub provider: String,
    pub model: String,
    pub dimension: usize,
    pub metric: EmbeddingMetric,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceIndexStorageOptions {
    ReadOnly {
        storage_path: PathBuf,
        /// Resolved workspace home for the shared identity catalog, not the source root.
        workspace_path: PathBuf,
    },
    ReadWrite {
        storage_path: PathBuf,
        /// Resolved workspace home for the shared identity catalog, not the source root.
        workspace_path: PathBuf,
        embedding: WorkspaceIndexEmbeddingSchema,
    },
}

impl WorkspaceIndexStorageOptions {
    pub(crate) fn storage_path(&self) -> &Path {
        match self {
            Self::ReadOnly { storage_path, .. } | Self::ReadWrite { storage_path, .. } => {
                storage_path
            }
        }
    }

    pub(crate) fn workspace_path(&self) -> &Path {
        match self {
            Self::ReadOnly { workspace_path, .. } | Self::ReadWrite { workspace_path, .. } => {
                workspace_path
            }
        }
    }

    pub(crate) const fn is_read_only(&self) -> bool {
        matches!(self, Self::ReadOnly { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredEntity {
    pub entity: Entity,
    pub file: FileRecord,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IndexedFragment {
    pub fragment: EntityFragment,
    pub vector: Vec<f32>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StorageSearchFilter {
    pub path: Option<StoragePathFilter>,
    pub file_ids: Option<Vec<FileId>>,
    pub entity_ids: Option<Vec<EntityId>>,
    pub symbol_names: Option<Vec<String>>,
    pub symbol_types: Option<Vec<SymbolType>>,
}

/// Boolean predicates over metadata duplicated on both retrieval collections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StoragePathFilter {
    All,
    None,
    Directory(DirectoryId),
    FileNameExact(String),
    FileNamePrefix(String),
    FileNameSuffix(String),
    And(Vec<Self>),
    Or(Vec<Self>),
    Not(Box<Self>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StorageSearchPath {
    Fts,
    Vector,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StorageSearchHit {
    pub fragment: EntityFragment,
    pub file: FileRecord,
    pub path: StorageSearchPath,
    pub score: f64,
}

/// Opens and manages concrete workspace-index storage instances.
pub(crate) trait WorkspaceIndexStorageFactory: Send + Sync {
    fn open(
        &self,
        options: WorkspaceIndexStorageOptions,
    ) -> StorageResult<Box<dyn WorkspaceIndexStorage>>;

    fn exists(&self, storage_path: &Path) -> StorageResult<bool>;

    fn delete(&self, storage_path: &Path) -> StorageResult<()>;
}

/// Persistence operations required by the indexing and indexed-search pipelines.
#[async_trait]
pub(crate) trait WorkspaceIndexStorage: Send + Sync {
    fn is_read_only(&self) -> bool;

    fn list_files(&self) -> StorageResult<Vec<FileRecord>>;

    /// Enumerate every stored file path without loading content or index status.
    /// This includes failed and pending files, but excludes historical identities.
    fn list_file_paths(&self) -> StorageResult<Vec<(FileId, PathBuf)>> {
        Ok(self
            .list_files()?
            .into_iter()
            .map(|file| (file.id, file.relative_path.into_path_buf()))
            .collect())
    }

    /// Durably reserves workspace identities before extraction or index writes.
    fn resolve_file_ids(&self, _paths: &[PathBuf]) -> StorageResult<Vec<FileId>> {
        Err(EngineError::storage_failure(
            "file identity allocation is unsupported",
        ))
    }

    fn supports_path_filters(&self) -> bool {
        false
    }

    fn directory_id(&self, _path: &Path) -> StorageResult<Option<DirectoryId>> {
        Ok(None)
    }

    fn has_non_unicode_file_names(&self) -> StorageResult<bool> {
        Ok(true)
    }

    fn get_entity(&self, entity_id: &EntityId) -> StorageResult<Option<StoredEntity>>;

    fn search_fts(
        &self,
        query: &str,
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> StorageResult<Vec<StorageSearchHit>>;

    fn search_vector(
        &self,
        vector: &[f32],
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> StorageResult<Vec<StorageSearchHit>>;

    /// Applies a complete file replacement to the current writer. Durability is
    /// confirmed by a batch checkpoint, `finalize_writes`, or successful `close`.
    /// An interrupted batch is discarded and its files are marked for reindexing.
    fn replace_file(&self, file: &FileRecord, entries: &[IndexedFragment]) -> StorageResult<()>;

    /// Optionally persists recovery intent for upcoming file replacements together.
    /// This does not publish replacements or confirm durability of previous writes.
    /// Recovery may mark every prepared file for reindexing, including unwritten files.
    /// `replace_file` remains safe without this hint or after an intervening checkpoint.
    fn prepare_file_replacements(&self, _files: &[&FileRecord]) -> StorageResult<()> {
        Ok(())
    }

    fn mark_file_failed(&self, file: &FileRecord, error: &str) -> StorageResult<()>;

    fn delete_file(&self, file_id: FileId) -> StorageResult<()>;

    /// Persists all accepted writes and clears their pending recovery records.
    async fn finalize_writes(&self) -> StorageResult<()>;

    /// Closes this lease. A healthy writer checkpoints its remaining batch;
    /// a failed writer releases its resources and leaves recovery records intact.
    fn close(&self) -> StorageResult<()>;
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::WorkspaceIndexStorageOptions;

    #[test]
    fn storage_options_keep_mode_and_path_together() {
        let path = PathBuf::from("workspace-index");
        let options = WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: path.clone(),
            workspace_path: path.clone(),
        };

        assert!(options.is_read_only());
        assert_eq!(options.storage_path(), path);
    }
}
