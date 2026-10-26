//! Storage operations used by indexing, with a small seam for pipeline tests.

use std::path::PathBuf;

use crate::{
    EngineResult,
    domain::{Entity, FileId, FileRecord},
    storage::{FlushPolicy, IndexStore, graph::FileGraph, types::IndexedFragment},
};

pub(crate) trait IndexStorage: Send + Sync {
    fn is_read_only(&self) -> bool;
    fn list_files(&self) -> EngineResult<Vec<FileRecord>>;
    fn resolve_file_ids(&self, paths: &[PathBuf]) -> EngineResult<Vec<FileId>>;
    fn replace_file(
        &self,
        file: &FileRecord,
        entities: &[Entity],
        entries: &[IndexedFragment],
        graph: &FileGraph,
    ) -> EngineResult<()>;
    /// Persists one deferred replacement batch at its indexing boundary.
    fn flush_batch(&self) -> EngineResult<()>;
    fn mark_file_failed(&self, file: &FileRecord, error: &str) -> EngineResult<()>;
    fn delete_file(&self, file_id: FileId) -> EngineResult<()>;
    fn finalize_writes(&self) -> EngineResult<()>;
}

impl IndexStorage for IndexStore {
    fn is_read_only(&self) -> bool {
        self.is_read_only()
    }

    fn list_files(&self) -> EngineResult<Vec<FileRecord>> {
        self.list_files()
    }

    fn resolve_file_ids(&self, paths: &[PathBuf]) -> EngineResult<Vec<FileId>> {
        self.resolve_file_ids(paths)
    }

    fn replace_file(
        &self,
        file: &FileRecord,
        entities: &[Entity],
        entries: &[IndexedFragment],
        graph: &FileGraph,
    ) -> EngineResult<()> {
        // Bulk indexing defers its flushes to the pipeline's batch boundary;
        // the store's failure and test paths stay immediate.
        self.replace_file_with_graph(file, entities, entries, graph, FlushPolicy::Deferred)
    }

    fn flush_batch(&self) -> EngineResult<()> {
        self.flush_batch()
    }

    fn mark_file_failed(&self, file: &FileRecord, error: &str) -> EngineResult<()> {
        self.mark_file_failed(file, error)
    }

    fn delete_file(&self, file_id: FileId) -> EngineResult<()> {
        self.delete_file(file_id)
    }

    fn finalize_writes(&self) -> EngineResult<()> {
        self.finalize_writes()
    }
}
