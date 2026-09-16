//! Workspace identities outlive individual index generations and deleted files.
//!
//! Only counters live in memory. Identity rows are queried from zvec. A redo
//! journal reserves each discovery batch before changing collections; IDs are
//! returned only after identities and allocation counters have been flushed.

pub(crate) mod path;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions, TryLockError},
    ops::Range,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use zvec_rust::{
    Collection, CollectionOptions, CollectionSchema, DataType, Doc, FieldSchema, IndexParams,
    SearchQuery,
};

use crate::{
    EngineError, EngineResult,
    domain::{DirectoryId, DirectoryRecord, FileId, FileRecord, SourcePath},
    utils::{atomic_write, sha256_hex_parts, sync_directory},
};

use super::codec::PathRecord;
use path::{decode_path, encode_path, path_key, query_path};

const LEGACY_FILE: &str = "identity.json";
const CATALOG_DIRECTORY: &str = "catalog";
const STAGING_DIRECTORY: &str = "catalog.staging";
const PENDING_FILE: &str = "pending.json";
const LOCK_FILE: &str = "identity.lock";
const VERSION: u32 = 2;
const LEGACY_VERSION: u32 = 1;
const WRITE_BATCH: usize = 1024;

pub(super) struct Catalog {
    path: PathBuf,
    native: NativeCatalog,
    state: Metadata,
    read_only: bool,
    write_failed: bool,
    _lock: File,
}

struct NativeCatalog {
    files: Collection,
    directories: Collection,
    metadata: Collection,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "MetadataRecord")]
struct Metadata {
    version: u32,
    /// None means the full u64 range has been allocated.
    next_file_id: Option<u64>,
    next_directory_id: Option<u64>,
    has_non_unicode_file_names: bool,
    /// Recognizes the already-imported legacy file after a publication crash.
    legacy_import_hash: Option<String>,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            version: VERSION,
            next_file_id: Some(0),
            next_directory_id: Some(0),
            has_non_unicode_file_names: false,
            legacy_import_hash: None,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum MetadataRecord {
    Current {
        version: u32,
        #[serde(deserialize_with = "Deserialize::deserialize")]
        next_file_id: Option<u64>,
        #[serde(deserialize_with = "Deserialize::deserialize")]
        next_directory_id: Option<u64>,
        has_non_unicode_file_names: bool,
        legacy_import_hash: Option<String>,
    },
    Legacy {
        version: u32,
        last_file_id: u64,
        last_directory_id: u64,
        has_non_unicode_file_names: bool,
        legacy_import_hash: Option<String>,
    },
}

impl TryFrom<MetadataRecord> for Metadata {
    type Error = &'static str;

    fn try_from(record: MetadataRecord) -> Result<Self, Self::Error> {
        let (next_file_id, next_directory_id, has_non_unicode_file_names, legacy_import_hash) =
            match record {
                MetadataRecord::Current {
                    version: VERSION,
                    next_file_id,
                    next_directory_id,
                    has_non_unicode_file_names,
                    legacy_import_hash,
                } => (
                    next_file_id,
                    next_directory_id,
                    has_non_unicode_file_names,
                    legacy_import_hash,
                ),
                MetadataRecord::Legacy {
                    version: LEGACY_VERSION,
                    last_file_id,
                    last_directory_id,
                    has_non_unicode_file_names,
                    legacy_import_hash,
                } => (
                    last_file_id.checked_add(1),
                    last_directory_id.checked_add(1),
                    has_non_unicode_file_names,
                    legacy_import_hash,
                ),
                _ => return Err("unsupported metadata version"),
            };
        Ok(Self {
            version: VERSION,
            next_file_id,
            next_directory_id,
            has_non_unicode_file_names,
            legacy_import_hash,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityEntry {
    id: u64,
    path: String,
}

impl IdentityEntry {
    fn new(path: &Path, id: u64) -> EngineResult<Self> {
        Ok(Self {
            id,
            path: encode_path(&SourcePath::new(path)?)?,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingBatch {
    version: u32,
    before: Metadata,
    after: Metadata,
    files: Vec<IdentityEntry>,
    directories: Vec<IdentityEntry>,
}

/// The legacy format is read only once, before publishing a replacement.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogRecord {
    version: u32,
    last_file_id: u64,
    last_directory_id: u64,
    files: Vec<PathIdentity>,
    directories: Vec<PathIdentity>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PathIdentity {
    path: PathRecord,
    id: u64,
}

struct LegacyCatalog {
    state: Metadata,
    files: Vec<IdentityEntry>,
    directories: Vec<IdentityEntry>,
}

impl Catalog {
    pub(super) fn exists(home: &Path) -> EngineResult<bool> {
        for name in [CATALOG_DIRECTORY, LEGACY_FILE, STAGING_DIRECTORY] {
            if path_exists(&home.join(name))? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn needs_recovery(home: &Path) -> EngineResult<bool> {
        Ok(path_exists(&home.join(LEGACY_FILE))?
            || path_exists(&home.join(STAGING_DIRECTORY))?
            || path_exists(&home.join(CATALOG_DIRECTORY).join(PENDING_FILE))?)
    }

    /// Keep the lock inode so concurrent openers continue to agree on it.
    pub(super) fn delete(home: &Path) -> EngineResult<()> {
        if !path_exists(home)? {
            return Ok(());
        }
        let _lock = acquire_lock(home, false)?;
        for name in [CATALOG_DIRECTORY, STAGING_DIRECTORY] {
            remove_directory(&home.join(name))?;
        }
        remove_file(&home.join(LEGACY_FILE))?;
        sync_directory(home)
    }

    pub(super) fn open(home: &Path, read_only: bool) -> EngineResult<Self> {
        super::backend::initialize()?;
        if !read_only {
            fs::create_dir_all(home)
                .map_err(|error| io_error("create identity directory", home, &error))?;
        }
        let lock = acquire_lock(home, read_only)?;
        if read_only && Self::needs_recovery(home)? {
            return Err(EngineError::resource_busy(
                "workspace identity catalog needs migration or recovery; reopen writable first",
            ));
        }
        let path = home.join(CATALOG_DIRECTORY);
        if !path_exists(&path)? {
            if read_only {
                return Err(EngineError::not_found(
                    "workspace identity catalog does not exist",
                ));
            }
            initialize(home)?;
        }
        let native = NativeCatalog::open(&path, read_only, false)?;
        let state = native.read_metadata()?;
        state.validate()?;
        let mut catalog = Self {
            path,
            native,
            state,
            read_only,
            write_failed: false,
            _lock: lock,
        };
        if !read_only {
            let pending = catalog.path.join(PENDING_FILE);
            if path_exists(&pending)? {
                let bytes = read_file(&pending)?;
                let batch: PendingBatch = decode_json(&bytes, "identity redo journal")?;
                catalog.apply_batch(&batch)?;
                catalog.clear_pending()?;
            }
            catalog.finish_legacy_import(home)?;
            remove_directory(&home.join(STAGING_DIRECTORY))?;
            sync_directory(home)?;
        }
        Ok(catalog)
    }

    pub(super) fn file_id(&self, path: &Path) -> EngineResult<Option<FileId>> {
        Ok(
            NativeCatalog::lookup(&self.native.files, path, self.state.next_file_id)?
                .map(|entry| FileId::new(entry.id)),
        )
    }

    pub(super) fn directory_id(&self, path: &Path) -> EngineResult<Option<DirectoryId>> {
        self.directory(path)
            .map(|record| record.map(|record| record.id))
    }

    fn directory(&self, path: &Path) -> EngineResult<Option<DirectoryRecord>> {
        // The workspace root has no catalog record or directory ID.
        if path.as_os_str().is_empty() {
            return Ok(None);
        }
        NativeCatalog::lookup(&self.native.directories, path, self.state.next_directory_id)?
            .map(|entry| directory_record(&entry))
            .transpose()
    }

    pub(super) fn resolve_file_ids(&mut self, paths: &[PathBuf]) -> EngineResult<Vec<FileId>> {
        let paths = paths
            .iter()
            .map(SourcePath::new)
            .collect::<EngineResult<Vec<_>>>()?;
        if self.write_failed {
            return Err(EngineError::resource_busy(
                "workspace identity write failed; close and reopen storage before allocating IDs",
            ));
        }
        let mut batch = PendingBatch {
            version: VERSION,
            before: self.state.clone(),
            after: self.state.clone(),
            files: Vec::new(),
            directories: Vec::new(),
        };
        let mut files = BTreeMap::new();
        let mut directories = BTreeMap::new();
        let mut ids = Vec::with_capacity(paths.len());
        for path in &paths {
            let existing = match files.get(path) {
                Some(id) => Some(*id),
                None => self.file_id(path)?,
            };
            if let Some(id) = existing {
                ids.push(id);
                continue;
            }
            if self.read_only {
                return Err(EngineError::invalid_argument(
                    "cannot allocate file identities in read-only storage",
                ));
            }
            for directory in ancestor_paths(path) {
                if directories.contains_key(&directory) || self.directory_id(&directory)?.is_some()
                {
                    continue;
                }
                let id = DirectoryId::new(allocate(&mut batch.after.next_directory_id)?);
                batch
                    .directories
                    .push(IdentityEntry::new(&directory, id.get())?);
                directories.insert(directory, id);
            }
            let id = FileId::new(allocate(&mut batch.after.next_file_id)?);
            batch.after.has_non_unicode_file_names |= has_non_unicode_file_name(path);
            batch.files.push(IdentityEntry::new(path, id.get())?);
            files.insert(path.clone(), id);
            ids.push(id);
        }
        if batch.files.is_empty() {
            return Ok(ids);
        }
        let result = (|| {
            atomic_write(&self.path.join(PENDING_FILE), &encode_json(&batch)?)?;
            self.apply_batch(&batch)?;
            self.clear_pending()
        })();
        if let Err(error) = result {
            // An fsync failure can leave committed data visible. Never resume
            // allocation from possibly outdated counters on this handle.
            self.write_failed = true;
            return Err(error);
        }
        Ok(ids)
    }

    pub(super) fn ancestor_directory_ids(
        &self,
        file_path: &SourcePath,
    ) -> EngineResult<Vec<DirectoryId>> {
        if self.file_id(file_path)?.is_none() {
            return Err(EngineError::invalid_argument(format!(
                "file has no registered workspace identity: {}",
                file_path.display()
            )));
        }
        ancestor_paths(file_path)
            .iter()
            .map(|path| {
                self.directory_id(path)?.ok_or_else(|| {
                    invalid_catalog(format!("missing ancestor directory: {}", path.display()))
                })
            })
            .collect()
    }

    pub(super) fn validate_file(&self, file: &FileRecord) -> EngineResult<()> {
        file.validate()?;
        if self.file_id(&file.relative_path)? != Some(file.id) {
            return Err(EngineError::invalid_argument(format!(
                "file ID does not match its registered workspace path: {}",
                file.relative_path.display()
            )));
        }
        Ok(())
    }

    pub(super) fn has_non_unicode_file_names(&self) -> bool {
        self.state.has_non_unicode_file_names
    }

    fn apply_batch(&mut self, batch: &PendingBatch) -> EngineResult<()> {
        self.validate_batch(batch)?;
        NativeCatalog::write_identities(&self.native.directories, &batch.directories)?;
        NativeCatalog::write_identities(&self.native.files, &batch.files)?;
        native(
            self.native.directories.flush(),
            "flush directory identities",
        )?;
        native(self.native.files.flush(), "flush file identities")?;
        self.native.write_metadata(&batch.after)?;
        native(
            self.native.metadata.flush(),
            "flush identity allocation counters",
        )?;
        self.state = batch.after.clone();
        Ok(())
    }

    fn validate_batch(&self, batch: &PendingBatch) -> EngineResult<()> {
        batch.before.validate()?;
        batch.after.validate()?;
        if !matches!(batch.version, VERSION | LEGACY_VERSION)
            || (self.state != batch.before && self.state != batch.after)
            || allocation_end(batch.after.next_file_id) < allocation_end(batch.before.next_file_id)
            || allocation_end(batch.after.next_directory_id)
                < allocation_end(batch.before.next_directory_id)
            || (batch.before.has_non_unicode_file_names && !batch.after.has_non_unicode_file_names)
            || batch.before.legacy_import_hash != batch.after.legacy_import_hash
        {
            return Err(invalid_catalog(
                "redo journal does not match allocation counters",
            ));
        }
        let files = validate_entries(
            &batch.files,
            allocation_end(batch.before.next_file_id)..allocation_end(batch.after.next_file_id),
        )?;
        let directories = validate_entries(
            &batch.directories,
            allocation_end(batch.before.next_directory_id)
                ..allocation_end(batch.after.next_directory_id),
        )?;
        if files.keys().any(|path| has_non_unicode_file_name(path))
            && !batch.after.has_non_unicode_file_names
        {
            return Err(invalid_catalog(
                "redo journal omits non-Unicode filename metadata",
            ));
        }
        for path in files.keys().chain(directories.keys()) {
            for ancestor in ancestor_paths(path) {
                if !directories.contains_key(&ancestor)
                    && NativeCatalog::lookup(
                        &self.native.directories,
                        &ancestor,
                        batch.after.next_directory_id,
                    )?
                    .is_none()
                {
                    return Err(invalid_catalog(
                        "redo journal is missing an ancestor directory",
                    ));
                }
            }
        }
        for (collection, entries, next_id) in [
            (&self.native.files, &batch.files, batch.after.next_file_id),
            (
                &self.native.directories,
                &batch.directories,
                batch.after.next_directory_id,
            ),
        ] {
            for entry in entries {
                let path = decode_path(&entry.path)?;
                if let Some(existing) = NativeCatalog::lookup(collection, &path, next_id)?
                    && existing.id != entry.id
                {
                    return Err(invalid_catalog(
                        "redo journal would reassign an existing path",
                    ));
                }
                if let Some(doc) = fetch_one(collection, &entry.id.to_string())? {
                    let existing = decode_identity(&doc, next_id)?;
                    if decode_path(&existing.path)? != path {
                        return Err(invalid_catalog("redo journal would reuse an existing ID"));
                    }
                }
            }
        }
        Ok(())
    }

    fn clear_pending(&self) -> EngineResult<()> {
        remove_file(&self.path.join(PENDING_FILE))?;
        sync_directory(&self.path)
    }

    fn finish_legacy_import(&self, home: &Path) -> EngineResult<()> {
        let path = home.join(LEGACY_FILE);
        if !path_exists(&path)? {
            return Ok(());
        }
        let bytes = read_file(&path)?;
        if self.state.legacy_import_hash.as_deref() != Some(&sha256_hex_parts([bytes.as_slice()])) {
            return Err(invalid_catalog(
                "legacy identities differ from the imported catalog",
            ));
        }
        remove_file(&path)?;
        sync_directory(home)
    }
}

impl Metadata {
    fn validate(&self) -> EngineResult<()> {
        if self.version != VERSION {
            return Err(invalid_catalog("unsupported metadata version"));
        }
        Ok(())
    }
}

impl NativeCatalog {
    fn open(path: &Path, read_only: bool, create: bool) -> EngineResult<Self> {
        Ok(Self {
            files: open_collection(
                &path.join("file_identities"),
                &identity_schema("file_identities")?,
                read_only,
                create,
            )?,
            directories: open_collection(
                &path.join("directories"),
                &identity_schema("directories")?,
                read_only,
                create,
            )?,
            metadata: open_collection(
                &path.join("metadata"),
                &metadata_schema()?,
                read_only,
                create,
            )?,
        })
    }

    fn lookup(
        collection: &Collection,
        path: &Path,
        next_id: Option<u64>,
    ) -> EngineResult<Option<IdentityEntry>> {
        let path = SourcePath::new(path)?;
        let key = path_key(&path)?;
        let mut query = native(SearchQuery::scalar(2), "create identity lookup")?;
        native(
            query.set_filter(&format!("path_key = '{key}'")),
            "set identity lookup path",
        )?;
        native(query.set_include_vector(false), "omit identity vectors")?;
        let mut docs = native(collection.query(&query), "look up identity by path")?;
        if docs.len() > 1 {
            return Err(invalid_catalog("duplicate registered path"));
        }
        let Some(doc) = docs.pop() else {
            return Ok(None);
        };
        let entry = decode_identity(&doc, next_id)?;
        if decode_path(&entry.path)? != path {
            return Err(invalid_catalog(
                "path lookup returned a different native path",
            ));
        }
        Ok(Some(entry))
    }

    fn read_metadata(&self) -> EngineResult<Metadata> {
        let doc = fetch_one(&self.metadata, "state")?
            .ok_or_else(|| invalid_catalog("missing allocation counters"))?;
        decode_json(
            string_field(&doc, "payload")?.as_bytes(),
            "identity metadata",
        )
    }

    fn write_metadata(&self, state: &Metadata) -> EngineResult<()> {
        let mut doc = native(Doc::new(), "create identity metadata")?;
        doc.set_pk("state");
        let payload = serde_json::to_string(state).map_err(invalid_catalog)?;
        native(doc.add_string("payload", &payload), "set identity metadata")?;
        write_docs(&self.metadata, &[doc])
    }

    fn write_identities(collection: &Collection, entries: &[IdentityEntry]) -> EngineResult<()> {
        for batch in entries.chunks(WRITE_BATCH) {
            let docs = batch
                .iter()
                .map(identity_doc)
                .collect::<EngineResult<Vec<_>>>()?;
            write_docs(collection, &docs)?;
        }
        Ok(())
    }

    fn flush(&self) -> EngineResult<()> {
        for collection in [&self.directories, &self.files, &self.metadata] {
            native(collection.flush(), "flush identity catalog")?;
        }
        Ok(())
    }
}

fn initialize(home: &Path) -> EngineResult<()> {
    let legacy_path = home.join(LEGACY_FILE);
    // Validate all legacy records before creating any replacement collections.
    let legacy = if path_exists(&legacy_path)? {
        Some(decode_legacy(&read_file(&legacy_path)?)?)
    } else {
        None
    };
    let staging = home.join(STAGING_DIRECTORY);
    remove_directory(&staging)?;
    fs::create_dir(&staging)
        .map_err(|error| io_error("create identity staging directory", &staging, &error))?;
    {
        let native = NativeCatalog::open(&staging, false, true)?;
        let state = if let Some(legacy) = &legacy {
            NativeCatalog::write_identities(&native.directories, &legacy.directories)?;
            NativeCatalog::write_identities(&native.files, &legacy.files)?;
            &legacy.state
        } else {
            &Metadata::default()
        };
        native.write_metadata(state)?;
        native.flush()?;
    }
    sync_directory(&staging)?;
    let destination = home.join(CATALOG_DIRECTORY);
    fs::rename(&staging, &destination)
        .map_err(|error| io_error("publish identity catalog", &destination, &error))?;
    sync_directory(home)
}

fn decode_legacy(bytes: &[u8]) -> EngineResult<LegacyCatalog> {
    let record: CatalogRecord = decode_json(bytes, "legacy workspace identities")?;
    if record.version != LEGACY_VERSION {
        return Err(invalid_catalog("unsupported legacy catalog version"));
    }
    let convert = |entries: Vec<PathIdentity>| {
        entries
            .into_iter()
            .map(|entry| IdentityEntry::new(&entry.path.into_path()?, entry.id))
            .collect::<EngineResult<Vec<_>>>()
    };
    let files = convert(record.files)?;
    let directories = convert(record.directories)?;
    let next_file_id = record.last_file_id.checked_add(1);
    let next_directory_id = record.last_directory_id.checked_add(1);
    let file_paths = validate_entries(&files, 1..allocation_end(next_file_id))?;
    let directory_paths = validate_entries(&directories, 1..allocation_end(next_directory_id))?;
    for path in file_paths.keys().chain(directory_paths.keys()) {
        if ancestor_paths(path)
            .iter()
            .any(|ancestor| !directory_paths.contains_key(ancestor))
        {
            return Err(invalid_catalog(
                "missing ancestor directory in legacy identities",
            ));
        }
    }
    Ok(LegacyCatalog {
        state: Metadata {
            version: VERSION,
            next_file_id,
            next_directory_id,
            has_non_unicode_file_names: file_paths
                .keys()
                .any(|path| has_non_unicode_file_name(path)),
            legacy_import_hash: Some(sha256_hex_parts([bytes])),
        },
        files,
        directories,
    })
}

fn validate_entries(
    entries: &[IdentityEntry],
    allocated: Range<u128>,
) -> EngineResult<BTreeMap<PathBuf, u64>> {
    let mut paths = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for entry in entries {
        let path = decode_path(&entry.path)?;
        if !allocated.contains(&u128::from(entry.id)) || !ids.insert(entry.id) {
            return Err(invalid_catalog(
                "duplicate ID or ID outside its allocated range",
            ));
        }
        if paths.insert(path.into_path_buf(), entry.id).is_some() {
            return Err(invalid_catalog("duplicate registered path"));
        }
    }
    Ok(paths)
}

fn identity_schema(name: &str) -> EngineResult<CollectionSchema> {
    let mut schema = native(CollectionSchema::new(name), "define identity collection")?;
    for (name, data_type, nullable, indexed) in [
        ("id", DataType::Uint64, false, false),
        ("path_key", DataType::String, false, true),
        ("relative_path", DataType::String, true, true),
        ("path", DataType::String, false, false),
    ] {
        let mut field = native(
            FieldSchema::new(name, data_type, nullable, 0),
            "define identity field",
        )?;
        if indexed {
            native(
                field.set_index_params(&native(
                    IndexParams::invert(false, name == "relative_path"),
                    "define identity path index",
                )?),
                "attach identity path index",
            )?;
        }
        native(schema.add_field(&field), "add identity field")?;
    }
    Ok(schema)
}

fn metadata_schema() -> EngineResult<CollectionSchema> {
    let mut schema = native(
        CollectionSchema::new("identity_metadata"),
        "define identity metadata collection",
    )?;
    let field = native(
        FieldSchema::new("payload", DataType::String, false, 0),
        "define identity metadata field",
    )?;
    native(schema.add_field(&field), "add identity metadata field")?;
    Ok(schema)
}

fn identity_doc(entry: &IdentityEntry) -> EngineResult<Doc> {
    let path = decode_path(&entry.path)?;
    let mut doc = native(Doc::new(), "create identity record")?;
    doc.set_pk(&entry.id.to_string());
    native(doc.add_u64("id", entry.id), "set identity ID")?;
    native(
        doc.add_string("path_key", &path_key(&path)?),
        "set identity path key",
    )?;
    native(
        doc.add_string("path", &entry.path),
        "set identity native path",
    )?;
    if let Some(path) = query_path(&path) {
        native(
            doc.add_string("relative_path", &path),
            "set identity query path",
        )?;
    } else {
        native(
            doc.set_field_null("relative_path"),
            "set non-Unicode identity query path",
        )?;
    }
    Ok(doc)
}

fn decode_identity(doc: &Doc, next_id: Option<u64>) -> EngineResult<IdentityEntry> {
    let id = native(doc.get_u64("id"), "read identity ID")?
        .ok_or_else(|| invalid_catalog("missing identity ID"))?;
    if next_id.is_some_and(|next| id >= next) || doc.get_pk() != Some(id.to_string().as_str()) {
        return Err(invalid_catalog(
            "identity ID does not match its primary key or allocation range",
        ));
    }
    let encoded = string_field(doc, "path")?;
    let path = decode_path(&encoded)?;
    // zvec omits null scalar fields from fetched and queried documents.
    let query_projection = if doc.has_field("relative_path") {
        native(doc.get_string("relative_path"), "read identity query path")?
    } else {
        None
    };
    if string_field(doc, "path_key")? != path_key(&path)? || query_projection != query_path(&path) {
        return Err(invalid_catalog(
            "identity path projections do not match the native path",
        ));
    }
    Ok(IdentityEntry { id, path: encoded })
}

fn directory_record(entry: &IdentityEntry) -> EngineResult<DirectoryRecord> {
    Ok(DirectoryRecord {
        id: DirectoryId::new(entry.id),
        relative_path: decode_path(&entry.path)?,
    })
}

fn open_collection(
    path: &Path,
    schema: &CollectionSchema,
    read_only: bool,
    create: bool,
) -> EngineResult<Collection> {
    #[cfg(windows)]
    let native_path = dunce::simplified(path);
    #[cfg(not(windows))]
    let native_path = path;
    let text = native_path
        .to_str()
        .ok_or_else(|| EngineError::invalid_argument("zvec storage path must be UTF-8"))?;
    let mut options = native(CollectionOptions::new(), "configure identity collection")?;
    native(
        options.set_read_only(read_only),
        "set identity collection access",
    )?;
    if create {
        native(
            Collection::create_and_open(text, schema, Some(&options)),
            "create identity collection",
        )
    } else {
        native(
            Collection::open(text, Some(&options)),
            "open identity collection",
        )
    }
}

fn fetch_one(collection: &Collection, key: &str) -> EngineResult<Option<Doc>> {
    let mut docs = native(
        collection.fetch_with_options(&[key], None, false),
        "fetch identity record",
    )?;
    if docs.len() > 1 {
        return Err(invalid_catalog("primary key returned multiple records"));
    }
    Ok(docs.pop())
}

fn write_docs(collection: &Collection, docs: &[Doc]) -> EngineResult<()> {
    if docs.is_empty() {
        return Ok(());
    }
    let refs = docs.iter().collect::<Vec<_>>();
    let result = native(collection.upsert(&refs), "write identity records")?;
    if result.results.len() != docs.len()
        || result.error_count != 0
        || result.success_count != u64::try_from(docs.len()).unwrap_or(u64::MAX)
        || result.results.iter().any(|status| !status.success)
    {
        return Err(EngineError::storage_failure(
            "zvec failed to write all identity records",
        ));
    }
    Ok(())
}

fn string_field(doc: &Doc, name: &str) -> EngineResult<String> {
    native(doc.get_string(name), "read identity field")?
        .ok_or_else(|| invalid_catalog(format!("missing identity field {name}")))
}

fn allocate(next_id: &mut Option<u64>) -> EngineResult<u64> {
    let id = next_id.ok_or_else(|| {
        EngineError::storage_failure("workspace identity allocation range is exhausted")
    })?;
    *next_id = id.checked_add(1);
    Ok(id)
}

fn allocation_end(next_id: Option<u64>) -> u128 {
    next_id.map_or(u128::from(u64::MAX) + 1, u128::from)
}

fn ancestor_paths(path: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = path
        .ancestors()
        .skip(1)
        .filter(|path| !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect();
    paths.reverse();
    paths
}

fn has_non_unicode_file_name(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name.to_str().is_none())
}

fn acquire_lock(home: &Path, read_only: bool) -> EngineResult<File> {
    let path = home.join(LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| io_error("open workspace identity lock", &path, &error))?;
    let result = if read_only {
        file.try_lock_shared()
    } else {
        file.try_lock()
    };
    result.map_err(|error| match error {
        TryLockError::WouldBlock => EngineError::resource_busy(format!(
            "workspace identities are locked by another reader or writer: {}",
            home.display()
        )),
        TryLockError::Error(error) => io_error("lock workspace identities", &path, &error),
    })?;
    Ok(file)
}

fn path_exists(path: &Path) -> EngineResult<bool> {
    path.try_exists()
        .map_err(|error| io_error("inspect workspace identities", path, &error))
}

fn read_file(path: &Path) -> EngineResult<Vec<u8>> {
    fs::read(path).map_err(|error| io_error("read workspace identities", path, &error))
}

fn remove_file(path: &Path) -> EngineResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("delete workspace identity file", path, &error)),
    }
}

fn remove_directory(path: &Path) -> EngineResult<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(
            "delete workspace identity directory",
            path,
            &error,
        )),
    }
}

fn decode_json<T: serde::de::DeserializeOwned>(bytes: &[u8], name: &str) -> EngineResult<T> {
    serde_json::from_slice(bytes)
        .map_err(|error| invalid_catalog(format!("cannot decode {name}: {error}")))
}

fn encode_json(value: &impl Serialize) -> EngineResult<Vec<u8>> {
    serde_json::to_vec(value)
        .map_err(|error| invalid_catalog(format!("cannot encode identity journal: {error}")))
}

fn native<T>(result: zvec_rust::Result<T>, operation: &str) -> EngineResult<T> {
    result.map_err(|error| EngineError::storage_failure(format!("zvec {operation}: {error}")))
}

fn invalid_catalog(message: impl std::fmt::Display) -> EngineError {
    EngineError::storage_failure(format!("invalid workspace identity catalog: {message}"))
}

fn io_error(action: &str, path: &Path, error: &std::io::Error) -> EngineError {
    EngineError::from_io(
        format!("cannot {action} '{}': {error}", path.display()),
        error,
    )
}

#[cfg(test)]
mod tests;
