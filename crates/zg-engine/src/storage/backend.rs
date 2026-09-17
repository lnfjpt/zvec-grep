use std::{
    collections::HashMap,
    fs::{self, DirBuilder, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock, Weak},
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    EngineError, EngineResult,
    domain::{FileId, FileIndexStatus, FileRecord, validate_fragments},
    models::EmbeddingMetric,
    utils::{atomic_write as write_record, sync_directory},
};

use super::{
    codec,
    file_ids::FileIds,
    pending::{self, PendingChange, PendingChanges},
    spi::{
        IndexedFragment, StorageResult, StorageSearchFilter, StorageSearchHit, StoredSearchData,
        WorkspaceIndexEmbeddingSchema, WorkspaceIndexStorage, WorkspaceIndexStorageFactory,
        WorkspaceIndexStorageOptions,
    },
    zvec::NativeStore,
};

const VERSION: u32 = 10;
const CHECKPOINT_OPERATIONS: usize = 64;
const CHECKPOINT_BYTES: u64 = 16 * 1024 * 1024;
type StoreRegistry = Mutex<HashMap<PathBuf, Weak<SharedStore>>>;
static STORES: OnceLock<StoreRegistry> = OnceLock::new();
static INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();

pub(crate) struct ZvecStorageFactory;

impl ZvecStorageFactory {
    pub(crate) fn new() -> Self {
        Self
    }
}

struct SharedStore {
    state: Mutex<StoreState>,
    path: PathBuf,
    schema: WorkspaceIndexEmbeddingSchema,
    read_only: bool,
    // The native handles must close before the operating-system lock is released.
    _lock: File,
}

struct StoreState {
    native: NativeStore,
    file_ids: FileIds,
    pending: PendingChanges,
    pending_operations: usize,
    pending_bytes: u64,
    needs_recovery: bool,
    closed: bool,
}

struct ZvecStorage {
    shared: Mutex<Option<Arc<SharedStore>>>,
    read_only: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaRecord {
    version: u32,
    provider: String,
    model: String,
    dimension: usize,
    metric: String,
}

impl SchemaRecord {
    fn new(schema: &WorkspaceIndexEmbeddingSchema) -> Self {
        Self {
            version: VERSION,
            provider: schema.provider.clone(),
            model: schema.model.clone(),
            dimension: schema.dimension,
            metric: match schema.metric {
                EmbeddingMetric::Cosine => "cosine",
                EmbeddingMetric::DotProduct => "dot",
                EmbeddingMetric::Euclidean => "euclidean",
            }
            .to_owned(),
        }
    }

    fn schema(self) -> EngineResult<WorkspaceIndexEmbeddingSchema> {
        if self.version != VERSION {
            return Err(EngineError::storage_failure(format!(
                "unsupported storage schema version {}; expected {VERSION}; rebuild the index",
                self.version
            )));
        }
        if !(1..=20_000).contains(&self.dimension) {
            return Err(EngineError::storage_failure(
                "stored embedding dimension must be in 1..=20,000",
            ));
        }
        let metric = match self.metric.as_str() {
            "cosine" => EmbeddingMetric::Cosine,
            "dot" => EmbeddingMetric::DotProduct,
            "euclidean" => EmbeddingMetric::Euclidean,
            _ => {
                return Err(EngineError::storage_failure(
                    "unknown stored embedding metric",
                ));
            }
        };
        Ok(WorkspaceIndexEmbeddingSchema {
            provider: self.provider,
            model: self.model,
            dimension: self.dimension,
            metric,
        })
    }
}

impl WorkspaceIndexStorageFactory for ZvecStorageFactory {
    fn open(
        &self,
        options: WorkspaceIndexStorageOptions,
    ) -> StorageResult<Box<dyn WorkspaceIndexStorage>> {
        initialize()?;
        let home = options.storage_path();
        if !home.is_absolute() {
            return Err(EngineError::invalid_argument(
                "storage path must be absolute",
            ));
        }
        let read_only = options.is_read_only();
        let outer = home.file_name().and_then(|_| home.parent());
        if !read_only {
            let mut builder = DirBuilder::new();
            builder.recursive(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            if let Err(error) = builder.create(home)
                && !(error.kind() == std::io::ErrorKind::AlreadyExists && home.is_dir())
            {
                return Err(io_error("create index directory", home, &error));
            }
        }
        let home = fs::canonicalize(home)
            .map_err(|error| io_error("locate index directory", home, &error))?;
        let path = home.join("storage");
        let mut registry = registry()?;
        if let Some(shared) = registry.get(&path).and_then(Weak::upgrade) {
            if read_only && shared.read_only {
                return Ok(Box::new(ZvecStorage {
                    shared: Mutex::new(Some(shared)),
                    read_only,
                }));
            }
            return Err(EngineError::resource_busy(
                "workspace storage is already open",
            ));
        }
        let (lock, schema) = prepare_storage(&home, &path, &options)?;
        let native = NativeStore::open(&path, &schema, read_only)?;
        // Readers do not need an allocation map or an O(files) startup scan.
        let file_ids = if read_only {
            FileIds::from_paths([])?
        } else {
            native.load_file_ids()?
        };
        if !read_only {
            // Repeat the same range after failures that leave directories in place.
            sync_directory(&path)?;
            sync_directory(&home)?;
            if let Some(outer) = outer {
                sync_directory(outer)?;
            }
        }
        let shared = Arc::new(SharedStore {
            state: Mutex::new(StoreState {
                native,
                file_ids,
                pending: PendingChanges::new(),
                pending_operations: 0,
                pending_bytes: 0,
                needs_recovery: false,
                closed: false,
            }),
            path: path.clone(),
            schema,
            read_only,
            _lock: lock,
        });
        registry.insert(path, Arc::downgrade(&shared));
        Ok(Box::new(ZvecStorage {
            shared: Mutex::new(Some(shared)),
            read_only,
        }))
    }

    fn exists(&self, storage_path: &Path) -> StorageResult<bool> {
        match fs::metadata(storage_path.join("storage")) {
            Ok(metadata) => Ok(metadata.is_dir()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error("inspect storage", storage_path, &error)),
        }
    }

    fn delete(&self, storage_path: &Path) -> StorageResult<()> {
        if !self.exists(storage_path)? {
            return Ok(());
        }
        let home = fs::canonicalize(storage_path)
            .map_err(|error| io_error("locate storage", storage_path, &error))?;
        let path = home.join("storage");
        let mut registry = registry()?;
        if registry.get(&path).and_then(Weak::upgrade).is_some() {
            return Err(EngineError::resource_busy(
                "cannot delete open workspace storage",
            ));
        }
        let _lock = acquire_storage_lock(&home, false)?;
        fs::remove_dir_all(&path)
            .map_err(|error| io_error("delete workspace storage", &path, &error))?;
        sync_directory(&home)?;
        registry.remove(&path);
        Ok(())
    }
}

impl ZvecStorage {
    fn shared(&self) -> EngineResult<Arc<SharedStore>> {
        self.shared
            .lock()
            .map_err(|_| EngineError::internal("storage lease lock was poisoned"))?
            .as_ref()
            .cloned()
            .ok_or_else(|| EngineError::resource_closed("workspace storage is closed"))
    }

    fn read<T>(&self, operation: impl FnOnce(&NativeStore) -> EngineResult<T>) -> EngineResult<T> {
        let shared = self.shared()?;
        let state = lock_state(&shared)?;
        assert_usable(&state)?;
        operation(&state.native)
    }

    fn apply(
        &self,
        change: PendingChange,
        bytes: u64,
        operation: impl FnOnce(&NativeStore) -> EngineResult<()>,
    ) -> EngineResult<()> {
        if self.read_only {
            return Err(EngineError::invalid_argument(
                "cannot write read-only workspace storage",
            ));
        }
        let shared = self.shared()?;
        let mut state = lock_state(&shared)?;
        assert_usable(&state)?;
        if let PendingChange::Reindex(file) = &change {
            state.file_ids.validate(file)?;
        }
        let deleted = match &change {
            PendingChange::Delete(id) => Some(*id),
            PendingChange::Reindex(_) => None,
        };
        // Keep every file changed since the last checkpoint. A crash may require
        // reindexing even files whose native writes already completed in memory.
        state.needs_recovery = true;
        // A prepared batch already has durable intent for matching snapshots.
        // Check again here: a checkpoint, deletion, or new snapshot invalidates it.
        if state.pending.get(change.file_id()) != Some(&change) {
            state.pending.insert(*change.file_id(), change);
            pending::write(&shared.path, &state.pending)?;
        }
        operation(&state.native)?;
        if let Some(id) = deleted {
            state.file_ids.remove(id);
        }
        state.pending_operations = state.pending_operations.saturating_add(1);
        state.pending_bytes = state.pending_bytes.saturating_add(bytes);
        if state.pending_operations >= CHECKPOINT_OPERATIONS
            || state.pending_bytes >= CHECKPOINT_BYTES
        {
            checkpoint(&shared.path, &mut state)?;
        } else {
            // The complete in-memory result is readable by this writer. Its
            // durability is confirmed only when the batch is checkpointed.
            state.needs_recovery = false;
        }
        Ok(())
    }
}

fn assert_usable(state: &StoreState) -> EngineResult<()> {
    if state.closed {
        return Err(EngineError::resource_closed("workspace storage is closed"));
    }
    if state.needs_recovery {
        return Err(recovery_required());
    }
    Ok(())
}

fn checkpoint(path: &Path, state: &mut StoreState) -> EngineResult<()> {
    if state.pending.is_empty() {
        return Ok(());
    }
    state.needs_recovery = true;
    state.native.flush()?;
    pending::clear(path)?;
    state.pending.clear();
    state.pending_operations = 0;
    state.pending_bytes = 0;
    state.needs_recovery = false;
    Ok(())
}

#[async_trait]
impl WorkspaceIndexStorage for ZvecStorage {
    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn list_files(&self) -> StorageResult<Vec<FileRecord>> {
        self.read(NativeStore::list_files)
    }

    fn list_file_paths(&self) -> StorageResult<Vec<(FileId, PathBuf)>> {
        self.read(NativeStore::list_file_paths)
    }

    fn resolve_file_ids(&self, paths: &[PathBuf]) -> StorageResult<Vec<FileId>> {
        if self.read_only {
            return Err(EngineError::permission_denied(
                "cannot allocate file identities in read-only storage",
            ));
        }
        let shared = self.shared()?;
        let mut state = lock_state(&shared)?;
        assert_usable(&state)?;
        state.file_ids.resolve(paths)
    }

    fn supports_path_filters(&self) -> bool {
        true
    }

    fn has_non_unicode_file_names(&self) -> StorageResult<bool> {
        if self.read_only {
            return self.read(NativeStore::has_non_unicode_file_names);
        }
        let shared = self.shared()?;
        let state = lock_state(&shared)?;
        assert_usable(&state)?;
        Ok(state.file_ids.has_non_unicode_file_names())
    }

    fn load_search_hits(&self, hits: &[StorageSearchHit]) -> StorageResult<StoredSearchData> {
        self.read(|native| native.load_search_hits(hits))
    }

    fn search_fts(
        &self,
        query: &str,
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> StorageResult<Vec<StorageSearchHit>> {
        self.read(|native| native.search_fts(query, limit, filter))
    }

    fn search_vector(
        &self,
        vector: &[f32],
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> StorageResult<Vec<StorageSearchHit>> {
        let shared = self.shared()?;
        validate_vector(vector, &shared.schema)?;
        self.read(|native| native.search_vector(vector, limit, filter))
    }

    fn replace_file(&self, file: &FileRecord, entries: &[IndexedFragment]) -> StorageResult<()> {
        let shared = self.shared()?;
        validate_batch(file, entries, &shared.schema)?;
        let mut file = file.clone();
        file.index_status = FileIndexStatus::Indexed {
            indexed_epoch_ms: now_epoch_ms()?,
            entity_count: u64::try_from(
                entries
                    .iter()
                    .filter(|entry| entry.fragment.as_entity().is_some())
                    .count(),
            )
            .map_err(|_| EngineError::invalid_argument("entity count exceeds u64"))?,
        };
        file.validate()?;
        self.apply(
            PendingChange::reindex(&file),
            estimated_write_bytes(&file, entries),
            |native| native.apply_replace(&file, entries),
        )
    }

    fn prepare_file_replacements(&self, files: &[&FileRecord]) -> StorageResult<()> {
        if self.read_only {
            return Err(EngineError::invalid_argument(
                "cannot write read-only workspace storage",
            ));
        }
        for file in files {
            file.validate()?;
        }
        let shared = self.shared()?;
        let mut state = lock_state(&shared)?;
        assert_usable(&state)?;
        for file in files {
            state.file_ids.validate(file)?;
        }
        let changes = files
            .iter()
            .map(|file| PendingChange::reindex(file))
            .filter(|change| state.pending.get(change.file_id()) != Some(change))
            .collect::<Vec<_>>();
        if changes.is_empty() {
            return Ok(());
        }
        state.needs_recovery = true;
        for change in changes {
            state.pending.insert(*change.file_id(), change);
        }
        // Publish the whole recovery set before any corresponding native mutation.
        // Prepared but unwritten files can safely be reindexed after an interruption.
        pending::write(&shared.path, &state.pending)?;
        state.needs_recovery = false;
        Ok(())
    }

    fn mark_file_failed(&self, file: &FileRecord, error: &str) -> StorageResult<()> {
        file.validate()?;
        let mut file = file.clone();
        file.index_status = FileIndexStatus::Failed {
            error: error.to_owned(),
        };
        file.validate()?;
        self.apply(PendingChange::reindex(&file), 0, |native| {
            native.apply_replace(&file, &[])
        })
    }

    fn delete_file(&self, file_id: FileId) -> StorageResult<()> {
        self.apply(PendingChange::Delete(file_id), 0, |native| {
            native.apply_delete(file_id)
        })
    }

    async fn finalize_writes(&self) -> StorageResult<()> {
        if self.read_only {
            return Ok(());
        }
        let shared = self.shared()?;
        let mut state = lock_state(&shared)?;
        assert_usable(&state)?;
        checkpoint(&shared.path, &mut state)
    }

    fn close(&self) -> StorageResult<()> {
        // Serialize concurrent closes through the final checkpoint as well as
        // taking the lease, so none can return while its writes are persisting.
        let mut lease = self
            .shared
            .lock()
            .map_err(|_| EngineError::internal("storage lease lock was poisoned"))?;
        let Some(shared) = lease.take() else {
            return Ok(());
        };
        if self.read_only {
            return Ok(());
        }
        let mut state = lock_state(&shared)?;
        // Reject a write that acquired its Arc before close but is still waiting
        // for this lock. A successful close commits every accepted write.
        state.closed = true;
        if state.needs_recovery {
            return Err(recovery_required());
        }
        checkpoint(&shared.path, &mut state)
    }
}

fn estimated_write_bytes(file: &FileRecord, entries: &[IndexedFragment]) -> u64 {
    // Bound the batch using source size and raw vectors without serializing a
    // second copy of fragment contents. An oversized file forces a checkpoint.
    entries
        .iter()
        .fold(file.snapshot.size_bytes, |bytes, entry| {
            bytes.saturating_add(
                u64::try_from(entry.vector.len())
                    .unwrap_or(u64::MAX)
                    .saturating_mul(4),
            )
        })
}

fn recover_pending(native: &NativeStore, changes: PendingChanges) -> EngineResult<()> {
    for change in changes.into_values() {
        match change {
            PendingChange::Reindex(mut file) => {
                file.index_status = FileIndexStatus::NotIndexed;
                native.apply_replace(&file, &[])?;
            }
            PendingChange::Delete(id) => native.apply_delete(id)?,
        }
    }
    Ok(())
}

fn validate_batch(
    file: &FileRecord,
    entries: &[IndexedFragment],
    schema: &WorkspaceIndexEmbeddingSchema,
) -> EngineResult<()> {
    file.validate()?;
    validate_fragments(file.id, entries.iter().map(|entry| &entry.fragment))?;
    for entry in entries {
        codec::validate_fragment(&entry.fragment)?;
        validate_vector(&entry.vector, schema)?;
    }
    Ok(())
}

fn validate_vector(vector: &[f32], schema: &WorkspaceIndexEmbeddingSchema) -> EngineResult<()> {
    if vector.len() != schema.dimension || vector.iter().any(|value| !value.is_finite()) {
        return Err(EngineError::invalid_argument(format!(
            "expected a finite {}-dimensional vector, got {} values",
            schema.dimension,
            vector.len()
        )));
    }
    if schema.metric == EmbeddingMetric::Cosine && vector.iter().all(|value| *value == 0.0) {
        return Err(EngineError::invalid_argument(
            "cosine vectors must have a non-zero norm",
        ));
    }
    Ok(())
}

pub(super) fn initialize() -> EngineResult<()> {
    match INITIALIZED.get_or_init(|| {
        let config = zvec_rust::ConfigBuilder::new()
            .num_threads(2)
            .memory_limit(512 * 1024 * 1024)
            .build();
        zvec_rust::initialize(Some(&config)).map_err(|error| error.to_string())
    }) {
        Ok(()) => Ok(()),
        Err(message) => Err(EngineError::storage_failure(format!(
            "cannot initialize zvec: {message}"
        ))),
    }
}

// Check the version before decoding fields: legacy records contain dictionary
// paths, and their native FTS collections must be rebuilt with SDK defaults.
fn read_schema(path: &Path) -> EngineResult<SchemaRecord> {
    let record: serde_json::Value = read_json(path)?;
    if record.get("version").and_then(serde_json::Value::as_u64) != Some(u64::from(VERSION)) {
        return Err(EngineError::storage_failure(format!(
            "unsupported storage schema version {}; expected {VERSION}; rebuild the index",
            record.get("version").unwrap_or(&serde_json::Value::Null)
        )));
    }
    serde_json::from_value(record).map_err(|error| json_error(&error))
}

fn load_schema(
    path: &Path,
    options: &WorkspaceIndexStorageOptions,
) -> EngineResult<WorkspaceIndexEmbeddingSchema> {
    let descriptor = path.join("schema.json");
    if descriptor.exists() {
        let schema = read_schema(&descriptor)?.schema()?;
        if let WorkspaceIndexStorageOptions::ReadWrite { embedding, .. } = options
            && &schema != embedding
        {
            return Err(EngineError::invalid_argument(
                "stored embedding schema differs from the requested model; rebuild the index",
            ));
        }
        return Ok(schema);
    }
    let WorkspaceIndexStorageOptions::ReadWrite { embedding, .. } = options else {
        return Err(EngineError::not_found(
            "workspace storage schema does not exist",
        ));
    };
    if !(1..=20_000).contains(&embedding.dimension) {
        return Err(EngineError::invalid_argument(
            "embedding dimension must be in 1..=20,000",
        ));
    }
    write_record(
        &descriptor,
        &serde_json::to_vec(&SchemaRecord::new(embedding)).map_err(|error| json_error(&error))?,
    )?;
    Ok(embedding.clone())
}

fn registry() -> EngineResult<MutexGuard<'static, HashMap<PathBuf, Weak<SharedStore>>>> {
    let mut stores = STORES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| EngineError::internal("storage registry lock was poisoned"))?;
    stores.retain(|_, store| store.strong_count() != 0);
    Ok(stores)
}

fn lock_state(shared: &SharedStore) -> EngineResult<MutexGuard<'_, StoreState>> {
    shared
        .state
        .lock()
        .map_err(|_| EngineError::internal("workspace storage lock was poisoned"))
}

fn prepare_storage(
    home: &Path,
    path: &Path,
    options: &WorkspaceIndexStorageOptions,
) -> EngineResult<(File, WorkspaceIndexEmbeddingSchema)> {
    let read_only = options.is_read_only();
    let mut shared = read_only;
    loop {
        let lock = acquire_storage_lock(home, shared)?;
        let marker = path.join(pending::NAME);
        if shared && marker.exists() {
            // Release before changing lock modes; Windows does not convert held locks.
            shared = false;
            continue;
        }
        if !read_only {
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
                return Err(io_error("create storage directory", path, &error));
            }
        }
        let schema = load_schema(path, options)?;
        if marker.exists() {
            // Decode the entire batch before touching native data. Recovery
            // needs writable handles even when the caller only wants to search.
            let changes = pending::read(path)?;
            let native = NativeStore::open(path, &schema, false)?;
            let mut file_ids = native.load_file_ids()?;
            // The journal may contain new source records absent from native storage.
            // Remove deleted owners first, then reject every conflicting mapping
            // before touching any collection.
            for change in changes.values() {
                if let PendingChange::Delete(id) = change {
                    file_ids.remove(*id);
                }
            }
            for change in changes.values() {
                if let PendingChange::Reindex(file) = change {
                    file_ids.claim(file.id, file.relative_path.clone())?;
                }
            }
            recover_pending(&native, changes)?;
            native.flush()?;
            pending::clear(path)?;
        }
        if read_only && !shared {
            // Recheck after reacquiring: another writer may run between locks.
            shared = true;
            continue;
        }
        return Ok((lock, schema));
    }
}

fn acquire_storage_lock(home: &Path, shared: bool) -> EngineResult<File> {
    let path = home.join("storage.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| io_error("open storage lock", &path, &error))?;
    let result = if shared {
        file.try_lock_shared()
    } else {
        file.try_lock()
    };
    result.map_err(|error| storage_lock_error(home, error))?;
    Ok(file)
}

fn storage_lock_error(home: &Path, error: std::fs::TryLockError) -> EngineError {
    match error {
        std::fs::TryLockError::WouldBlock => EngineError::resource_busy(format!(
            "workspace storage is locked by another reader or writer: {}",
            home.display()
        )),
        std::fs::TryLockError::Error(error) => io_error("lock workspace storage", home, &error),
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> EngineResult<T> {
    let bytes = fs::read(path).map_err(|error| io_error("read storage record", path, &error))?;
    serde_json::from_slice(&bytes).map_err(|error| {
        EngineError::storage_failure(format!(
            "invalid storage record {}: {error}",
            path.display()
        ))
    })
}

pub(super) fn io_error(action: &str, path: &Path, error: &std::io::Error) -> EngineError {
    EngineError::from_io(
        format!("cannot {action} {}: {error}", path.display()),
        error,
    )
}

fn json_error(error: &serde_json::Error) -> EngineError {
    EngineError::storage_failure(format!("cannot encode storage record: {error}"))
}

fn recovery_required() -> EngineError {
    EngineError::resource_busy("storage has an unfinished write; close and reopen it to recover")
}

fn now_epoch_ms() -> EngineResult<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|value| u64::try_from(value.as_millis()).ok())
        .ok_or_else(|| EngineError::internal("system clock cannot represent the index timestamp"))
}

#[cfg(test)]
mod tests;
