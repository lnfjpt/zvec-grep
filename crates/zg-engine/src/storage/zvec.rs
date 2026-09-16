use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use zvec_rust::{
    Collection, CollectionOptions, CollectionSchema, DataType, Doc, FieldSchema, Fts, IndexParams,
    MetricType, SearchQuery,
};

use super::{
    catalog::path::{decode_path, encode_path, path_key, query_path},
    codec,
    spi::{
        IndexedFragment, StoragePathFilter, StorageSearchFilter, StorageSearchHit,
        StorageSearchPath, StoredEntity, WorkspaceIndexEmbeddingSchema,
    },
};
use crate::{
    EngineError, EngineResult,
    domain::{
        Content, DirectoryId, EntityContent, EntityFragment, EntityId, EntityMetadata, FileId,
        FileRecord, SymbolType, validate_fragments,
    },
    models::EmbeddingMetric,
    utils::sha256_hex_parts,
};

const WRITE_BATCH: usize = 1024;
const MAX_TOP_K: usize = 100_000;

pub(super) struct NativeStore {
    files: Collection,
    entities: Collection,
    fragments: Collection,
    vectors: Collection,
    dimension: usize,
    read_only: bool,
}

impl NativeStore {
    pub(super) fn open(
        path: &Path,
        embedding: &WorkspaceIndexEmbeddingSchema,
        read_only: bool,
    ) -> EngineResult<Self> {
        let dimension = u32::try_from(embedding.dimension)
            .ok()
            .filter(|value| (1..=20_000).contains(value))
            .ok_or_else(|| {
                EngineError::invalid_argument(
                    "storage embedding dimension must be between 1 and 20,000",
                )
            })?;
        let files = open_collection(&path.join("files"), &files_schema()?, read_only)?;
        let entities = open_collection(&path.join("entities"), &entities_schema()?, read_only)?;
        let fragments = open_collection(&path.join("fragments"), &fragments_schema()?, read_only)?;
        let metric = match embedding.metric {
            EmbeddingMetric::Cosine => MetricType::Cosine,
            EmbeddingMetric::DotProduct => MetricType::Ip,
            EmbeddingMetric::Euclidean => MetricType::L2,
        };
        let space = vector_collection_name(embedding);
        let vectors = open_collection(
            &path.join(space),
            &vectors_schema(dimension, metric)?,
            read_only,
        )?;
        Ok(Self {
            files,
            entities,
            fragments,
            vectors,
            dimension: embedding.dimension,
            read_only,
        })
    }

    pub(super) fn list_files(&self) -> EngineResult<Vec<FileRecord>> {
        let iterator = native(
            self.files.iter_with_options(None, false),
            "iterate source files",
        )?;
        let mut files = iterator
            .map(|doc| decode_file_doc(&native(doc, "read source file")?))
            .collect::<EngineResult<Vec<_>>>()?;
        files.sort_by(|left, right| {
            left.relative_path
                .cmp(&right.relative_path)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(files)
    }

    /// Enumerate complete native paths without loading file snapshots or status.
    pub(super) fn list_file_paths(&self) -> EngineResult<Vec<(FileId, PathBuf)>> {
        let iterator = native(
            self.files
                .iter_with_options(Some(&["file_id", "path"]), false),
            "iterate source paths",
        )?;
        iterator
            .map(|doc| decode_file_path_doc(&native(doc, "read source path")?))
            .collect()
    }

    pub(super) fn get_file(&self, id: FileId) -> EngineResult<Option<FileRecord>> {
        let key = file_key(id);
        fetch_one(&self.files, &key)?
            .as_ref()
            .map(decode_file_doc)
            .transpose()
    }

    pub(super) fn get_entity(&self, id: &EntityId) -> EngineResult<Option<StoredEntity>> {
        let key = primary_key("fragment", id.as_str());
        let Some(doc) = fetch_one(&self.entities, &key)? else {
            return Ok(None);
        };
        let fragment = decode_fragment_doc(&doc)?;
        let entity = fragment
            .as_entity()
            .ok_or_else(|| corrupt("window found in entity collection"))?
            .clone();
        if entity.id != *id {
            return Err(corrupt("entity ID does not match its primary key"));
        }
        let file = self
            .get_file(entity.file_id)?
            .ok_or_else(|| corrupt("entity references a missing file"))?;
        validate_indexed_owner(&file, entity.file_id)?;
        Ok(Some(StoredEntity { entity, file }))
    }

    pub(super) fn search_fts(
        &self,
        query: &str,
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        if limit == 0 || empty_filter(filter) {
            return Ok(Vec::new());
        }
        let mut fts = native(Fts::new(), "create full-text request")?;
        native(
            fts.set_match_string(&index_text(query)),
            "set full-text query",
        )?;
        let mut request = native(
            SearchQuery::fts("text", &fts, top_k(limit)?),
            "create full-text query",
        )?;
        configure_query(&mut request, filter, &["payload", "file_id", "entity_id"])?;
        let docs = native(self.fragments.query(&request), "search full-text index")?;
        self.hydrate(
            docs.into_iter()
                .map(|doc| {
                    let score = f64::from(doc.get_score());
                    (doc, score)
                })
                .collect(),
            StorageSearchPath::Fts,
        )
    }

    pub(super) fn search_vector(
        &self,
        vector: &[f32],
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        self.validate_vector(vector)?;
        if limit == 0 || empty_filter(filter) {
            return Ok(Vec::new());
        }
        let mut request = native(
            SearchQuery::new("embedding", vector, top_k(limit)?),
            "create vector query",
        )?;
        configure_query(&mut request, filter, &["fragment_id"])?;
        let ranked = native(self.vectors.query(&request), "search vector index")?
            .into_iter()
            .map(|doc| {
                Ok((
                    string_field(&doc, "fragment_id")?,
                    f64::from(doc.get_score()),
                ))
            })
            .collect::<EngineResult<Vec<_>>>()?;
        let mut docs = fetch_map(
            &self.fragments,
            &ranked.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
        )?;
        let fragments = ranked
            .into_iter()
            .map(|(key, score)| {
                Ok((
                    docs.remove(&key)
                        .ok_or_else(|| corrupt("vector references a missing fragment"))?,
                    score,
                ))
            })
            .collect::<EngineResult<Vec<_>>>()?;
        self.hydrate(fragments, StorageSearchPath::Vector)
    }

    fn hydrate(
        &self,
        docs: Vec<(Doc, f64)>,
        path: StorageSearchPath,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        let fragments = docs
            .into_iter()
            .map(|(doc, score)| Ok((decode_fragment_doc(&doc)?, score)))
            .collect::<EngineResult<Vec<_>>>()?;
        let keys = fragments
            .iter()
            .map(|(fragment, _)| file_key(*fragment.file_id()))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let files = fetch_map(&self.files, &keys)?
            .into_iter()
            .map(|(key, doc)| Ok((key, decode_file_doc(&doc)?)))
            .collect::<EngineResult<HashMap<_, _>>>()?;
        fragments
            .into_iter()
            .map(|(fragment, score)| {
                let file = files
                    .get(&file_key(*fragment.file_id()))
                    .ok_or_else(|| corrupt("fragment references a missing file"))?
                    .clone();
                validate_indexed_owner(&file, *fragment.file_id())?;
                Ok(StorageSearchHit {
                    fragment,
                    file,
                    path,
                    score,
                })
            })
            .collect()
    }

    pub(super) fn apply_replace(
        &self,
        file: &FileRecord,
        entries: &[IndexedFragment],
        directories: &[DirectoryId],
    ) -> EngineResult<()> {
        self.assert_writable()?;
        validate_fragments(file.id, entries.iter().map(|entry| &entry.fragment))?;
        let entity_count = u64::try_from(
            entries
                .iter()
                .filter(|entry| entry.fragment.as_entity().is_some())
                .count(),
        )
        .map_err(|_| EngineError::invalid_argument("entity count exceeds u64"))?;
        if (!file.index_status.is_indexed() && !entries.is_empty())
            || file.index_status.entity_count() != entity_count
        {
            return Err(EngineError::invalid_argument(
                "file index status does not match its fragments",
            ));
        }
        let file_doc = encode_file_doc(file, directories)?;
        let mut fragments = Vec::with_capacity(entries.len());
        let mut entities = Vec::new();
        let mut vectors = Vec::with_capacity(entries.len());
        for entry in entries {
            self.validate_vector(&entry.vector)?;
            let payload = codec::encode_fragment(&entry.fragment)?;
            let mut fragment = fragment_doc(&entry.fragment, file, directories)?;
            native(
                fragment.add_string("payload", &payload),
                "encode fragment payload",
            )?;
            native(
                fragment.add_string("text", &lexical_text(&entry.fragment)),
                "encode searchable text",
            )?;
            fragments.push(fragment);
            if entry.fragment.as_entity().is_some() {
                let mut entity = identity_doc(&entry.fragment)?;
                native(
                    entity.add_string("payload", &payload),
                    "encode entity payload",
                )?;
                entities.push(entity);
            }
            let mut vector = fragment_doc(&entry.fragment, file, directories)?;
            native(
                vector.add_string(
                    "fragment_id",
                    &primary_key("fragment", entry.fragment.document_id()),
                ),
                "encode vector owner",
            )?;
            native(
                vector.add_vector_f32("embedding", &entry.vector),
                "encode embedding vector",
            )?;
            vectors.push(vector);
        }
        self.delete_documents(file.id)?;
        write_docs(&self.entities, &entities, "write entities")?;
        write_docs(&self.fragments, &fragments, "write fragments")?;
        write_docs(&self.vectors, &vectors, "write vectors")?;
        write_docs(&self.files, &[file_doc], "publish source file")
    }

    pub(super) fn apply_delete(&self, id: FileId) -> EngineResult<()> {
        self.assert_writable()?;
        self.delete_documents(id)?;
        native(
            self.files
                .delete_by_filter(&format!("file_id = {}", id.get())),
            "delete source file",
        )
    }

    fn delete_documents(&self, id: FileId) -> EngineResult<()> {
        let filter = format!("file_id = {}", id.get());
        for (collection, operation) in [
            (&self.vectors, "delete source vectors"),
            (&self.fragments, "delete source fragments"),
            (&self.entities, "delete source entities"),
        ] {
            native(collection.delete_by_filter(&filter), operation)?;
        }
        Ok(())
    }

    pub(super) fn flush(&self) -> EngineResult<()> {
        self.assert_writable()?;
        for collection in [&self.entities, &self.fragments, &self.vectors, &self.files] {
            native(collection.flush(), "flush storage collection")?;
        }
        Ok(())
    }

    fn assert_writable(&self) -> EngineResult<()> {
        if self.read_only {
            Err(EngineError::permission_denied(
                "cannot modify read-only index storage",
            ))
        } else {
            Ok(())
        }
    }

    fn validate_vector(&self, vector: &[f32]) -> EngineResult<()> {
        if vector.len() != self.dimension || vector.iter().any(|value| !value.is_finite()) {
            return Err(EngineError::invalid_argument(format!(
                "expected {} finite embedding values, got {}",
                self.dimension,
                vector.len()
            )));
        }
        Ok(())
    }
}

pub(super) fn vector_collection_name(embedding: &WorkspaceIndexEmbeddingSchema) -> String {
    format!(
        "vectors_{}",
        primary_key(
            "space",
            &format!(
                "{}\0{}\0{}\0{:?}",
                embedding.provider, embedding.model, embedding.dimension, embedding.metric
            )
        )
    )
}

fn open_collection(
    path: &Path,
    schema: &CollectionSchema,
    read_only: bool,
) -> EngineResult<Collection> {
    let text = native_path(path)?;
    let mut options = native(CollectionOptions::new(), "configure collection")?;
    native(
        options.set_read_only(read_only),
        "set collection access mode",
    )?;
    if path.exists() {
        native(
            Collection::open(text, Some(&options)),
            &format!("open collection {}", path.display()),
        )
    } else if read_only {
        Err(EngineError::not_found(format!(
            "index collection does not exist: {}",
            path.display()
        )))
    } else {
        native(
            Collection::create_and_open(text, schema, Some(&options)),
            &format!("create collection {}", path.display()),
        )
    }
}

fn native_path(path: &Path) -> EngineResult<&str> {
    // zvec rejects the `?` in Windows verbatim prefixes. Simplify only when
    // the regular path identifies the same location; retain internal paths.
    #[cfg(windows)]
    let path = dunce::simplified(path);
    path.to_str().ok_or_else(|| {
        EngineError::invalid_argument(format!(
            "zvec storage path must be UTF-8: {}",
            path.display()
        ))
    })
}

fn scalar(
    schema: &mut CollectionSchema,
    name: &str,
    data_type: DataType,
    nullable: bool,
    indexed: bool,
) -> EngineResult<()> {
    let mut field = native(
        FieldSchema::new(name, data_type, nullable, 0),
        "define scalar field",
    )?;
    if indexed {
        native(
            field.set_index_params(&native(
                IndexParams::invert(
                    !matches!(data_type, DataType::Uint64 | DataType::ArrayUint64),
                    false,
                ),
                "define scalar index",
            )?),
            "attach scalar index",
        )?;
    }
    native(schema.add_field(&field), "add scalar field")
}

fn identity_schema(name: &str) -> EngineResult<CollectionSchema> {
    let mut schema = native(CollectionSchema::new(name), "create collection schema")?;
    scalar(&mut schema, "file_id", DataType::Uint64, false, true)?;
    scalar(&mut schema, "entity_id", DataType::String, false, true)?;
    Ok(schema)
}

fn retrieval_schema(name: &str) -> EngineResult<CollectionSchema> {
    let mut schema = identity_schema(name)?;
    file_membership_schema(&mut schema)?;
    scalar(&mut schema, "symbol_name", DataType::String, true, true)?;
    scalar(&mut schema, "symbol_type", DataType::String, true, true)?;
    Ok(schema)
}

fn file_membership_schema(schema: &mut CollectionSchema) -> EngineResult<()> {
    scalar(
        schema,
        "ancestor_directory_ids",
        DataType::ArrayUint64,
        false,
        true,
    )?;
    wildcard_string(schema, "file_name", false)
}

fn wildcard_string(schema: &mut CollectionSchema, name: &str, nullable: bool) -> EngineResult<()> {
    let mut field = native(
        FieldSchema::new(name, DataType::String, nullable, 0),
        "define indexed path string",
    )?;
    native(
        field.set_index_params(&native(
            IndexParams::invert(false, true),
            "define path string index",
        )?),
        "attach path string index",
    )?;
    native(schema.add_field(&field), "add indexed path string")
}

fn files_schema() -> EngineResult<CollectionSchema> {
    let mut schema = native(CollectionSchema::new("files"), "create files schema")?;
    scalar(&mut schema, "file_id", DataType::Uint64, false, true)?;
    scalar(&mut schema, "path_key", DataType::String, false, true)?;
    // Native paths are stored separately from their optional Unicode projection.
    scalar(&mut schema, "path", DataType::String, false, false)?;
    wildcard_string(&mut schema, "relative_path", true)?;
    file_membership_schema(&mut schema)?;
    scalar(&mut schema, "payload", DataType::String, false, false)?;
    Ok(schema)
}

fn entities_schema() -> EngineResult<CollectionSchema> {
    let mut schema = identity_schema("entities")?;
    scalar(&mut schema, "payload", DataType::String, false, false)?;
    Ok(schema)
}

fn fragments_schema() -> EngineResult<CollectionSchema> {
    let mut schema = retrieval_schema("fragments")?;
    scalar(&mut schema, "payload", DataType::String, false, false)?;
    let mut text = native(
        FieldSchema::new("text", DataType::String, false, 0),
        "define full-text field",
    )?;
    native(
        text.set_index_params(&native(
            IndexParams::fts(Some("jieba"), Some(&["lowercase"]), None),
            "define full-text index",
        )?),
        "attach full-text index",
    )?;
    native(schema.add_field(&text), "add full-text field")?;
    Ok(schema)
}

fn vectors_schema(dimension: u32, metric: MetricType) -> EngineResult<CollectionSchema> {
    let mut schema = retrieval_schema("vectors")?;
    scalar(&mut schema, "fragment_id", DataType::String, false, false)?;
    let mut vector = native(
        FieldSchema::new("embedding", DataType::VectorFp32, false, dimension),
        "define vector field",
    )?;
    native(
        vector.set_index_params(&native(
            IndexParams::hnsw(metric, 16, 200),
            "define vector index",
        )?),
        "attach vector index",
    )?;
    native(schema.add_field(&vector), "add vector field")?;
    Ok(schema)
}

fn validate_indexed_owner(file: &FileRecord, file_id: FileId) -> EngineResult<()> {
    if file.id != file_id
        || !file.index_status.is_indexed()
        || file.index_status.entity_count() == 0
    {
        return Err(corrupt(
            "fragment references a file without a successful index",
        ));
    }
    Ok(())
}

fn encode_file_doc(file: &FileRecord, directories: &[DirectoryId]) -> EngineResult<Doc> {
    let mut doc = native(Doc::new(), "create file record")?;
    let key = file_key(file.id);
    doc.set_pk(&key);
    native(
        doc.add_u64("file_id", file.id.get()),
        "encode file identity",
    )?;
    native(
        doc.add_string("path_key", &path_key(&file.relative_path)?),
        "encode exact path key",
    )?;
    native(
        doc.add_string("path", &encode_path(&file.relative_path)?),
        "encode native file path",
    )?;
    if let Some(path) = query_path(&file.relative_path) {
        native(
            doc.add_string("relative_path", &path),
            "encode queryable file path",
        )?;
    }
    file_membership_doc(&mut doc, file, directories)?;
    native(
        doc.add_string("payload", &codec::encode_file(file)?),
        "encode file payload",
    )?;
    Ok(doc)
}

fn decode_file_doc(doc: &Doc) -> EngineResult<FileRecord> {
    let file = codec::decode_file(&string_field(doc, "payload")?)?;
    let (id, path) = decode_file_path_doc(doc)?;
    if id != file.id
        || path != file.relative_path.as_path()
        || string_field(doc, "path_key")? != path_key(&file.relative_path)?
    {
        return Err(corrupt("file identity differs from its indexed path"));
    }
    Ok(file)
}

fn decode_file_path_doc(doc: &Doc) -> EngineResult<(FileId, PathBuf)> {
    let id = FileId::new(u64_field(doc, "file_id")?);
    if doc_key(doc)? != file_key(id) {
        return Err(corrupt("file identity differs from its primary key"));
    }
    Ok((
        id,
        decode_path(&string_field(doc, "path")?)?.into_path_buf(),
    ))
}

fn identity_doc(fragment: &EntityFragment) -> EngineResult<Doc> {
    let mut doc = native(Doc::new(), "create fragment record")?;
    doc.set_pk(&primary_key("fragment", fragment.document_id()));
    native(
        doc.add_u64("file_id", fragment.file_id().get()),
        "encode source identity",
    )?;
    native(
        doc.add_string(
            "entity_id",
            &primary_key("fragment", fragment.entity_id().as_str()),
        ),
        "encode entity identity",
    )?;
    Ok(doc)
}

fn fragment_doc(
    fragment: &EntityFragment,
    file: &FileRecord,
    directories: &[DirectoryId],
) -> EngineResult<Doc> {
    let mut doc = identity_doc(fragment)?;
    file_membership_doc(&mut doc, file, directories)?;
    if let Some(EntityMetadata::Code {
        symbol_type,
        symbol_name,
        ..
    }) = fragment.metadata()
    {
        native(
            doc.add_string("symbol_type", symbol_type_name(*symbol_type)),
            "encode symbol type",
        )?;
        if let Some(name) = symbol_name {
            native(
                doc.add_string("symbol_name", &primary_key("symbol", name)),
                "encode symbol name",
            )?;
        }
    }
    Ok(doc)
}

fn file_membership_doc(
    doc: &mut Doc,
    file: &FileRecord,
    directories: &[DirectoryId],
) -> EngineResult<()> {
    native(
        doc.add_array_u64(
            "ancestor_directory_ids",
            &directories.iter().map(|id| id.get()).collect::<Vec<_>>(),
        ),
        "encode ancestor directories",
    )?;
    // Non-Unicode names have no STRING representation. The catalog disables name
    // pushdown in this workspace; exact native paths remain in FileRecord.
    let name = file
        .relative_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    native(doc.add_string("file_name", name), "encode file name")?;
    Ok(())
}

fn decode_fragment_doc(doc: &Doc) -> EngineResult<EntityFragment> {
    let fragment = codec::decode_fragment(&string_field(doc, "payload")?)?;
    if doc_key(doc)? != primary_key("fragment", fragment.document_id())
        || u64_field(doc, "file_id")? != fragment.file_id().get()
        || string_field(doc, "entity_id")? != primary_key("fragment", fragment.entity_id().as_str())
    {
        return Err(corrupt("fragment identity differs from its index fields"));
    }
    Ok(fragment)
}

fn fetch_one(collection: &Collection, key: &str) -> EngineResult<Option<Doc>> {
    let mut docs = native(
        collection.fetch_with_options(&[key], None, false),
        "fetch stored record",
    )?;
    if docs.len() > 1 {
        return Err(corrupt("primary key returned multiple records"));
    }
    Ok(docs.pop())
}

fn fetch_map(collection: &Collection, keys: &[String]) -> EngineResult<HashMap<String, Doc>> {
    let mut result = HashMap::with_capacity(keys.len());
    for batch in keys.chunks(WRITE_BATCH) {
        let refs = batch.iter().map(String::as_str).collect::<Vec<_>>();
        for doc in native(
            collection.fetch_with_options(&refs, None, false),
            "fetch stored records",
        )? {
            result.insert(doc_key(&doc)?.to_owned(), doc);
        }
    }
    Ok(result)
}

fn write_docs(collection: &Collection, docs: &[Doc], operation: &str) -> EngineResult<()> {
    for batch in docs.chunks(WRITE_BATCH) {
        let refs = batch.iter().collect::<Vec<_>>();
        let result = native(collection.upsert(&refs), operation)?;
        if result.results.len() != batch.len()
            || result.error_count != 0
            || result.success_count != u64::try_from(batch.len()).unwrap_or(u64::MAX)
            || result.results.iter().any(|status| !status.success)
        {
            let detail = result
                .results
                .iter()
                .find(|status| !status.success)
                .map_or_else(
                    || "incomplete write result".to_owned(),
                    |status| format!("{}: {}", status.code, status.message),
                );
            return Err(EngineError::storage_failure(format!(
                "{operation}: {detail}"
            )));
        }
    }
    Ok(())
}

fn configure_query(
    query: &mut SearchQuery,
    filter: Option<&StorageSearchFilter>,
    fields: &[&str],
) -> EngineResult<()> {
    native(
        query.set_include_vector(false),
        "omit vectors from search output",
    )?;
    native(
        query.set_output_fields(fields),
        "select search output fields",
    )?;
    if let Some(filter) = build_filter(filter)? {
        native(query.set_filter(&filter), "set search filter")?;
    }
    Ok(())
}

fn build_filter(filter: Option<&StorageSearchFilter>) -> EngineResult<Option<String>> {
    let Some(filter) = filter else {
        return Ok(None);
    };
    let mut clauses = Vec::new();
    if let Some(path) = &filter.path
        && !matches!(path, StoragePathFilter::All)
    {
        clauses.push(path_filter(path, false)?);
    }
    if let Some(ids) = &filter.file_ids {
        clauses.push(if ids.is_empty() {
            constant_filter(false)
        } else {
            format!(
                "file_id IN ({})",
                ids.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        });
    }
    if let Some(ids) = &filter.entity_ids {
        clauses.push(in_filter(
            "entity_id",
            ids.iter().map(|id| primary_key("fragment", id.as_str())),
        ));
    }
    if let Some(names) = &filter.symbol_names {
        clauses.push(in_filter(
            "symbol_name",
            names.iter().map(|name| primary_key("symbol", name)),
        ));
    }
    if let Some(types) = &filter.symbol_types {
        clauses.push(in_filter(
            "symbol_type",
            types.iter().map(|kind| symbol_type_name(*kind).to_owned()),
        ));
    }
    Ok((!clauses.is_empty()).then(|| clauses.join(" AND ")))
}

fn in_filter(field: &str, values: impl Iterator<Item = String>) -> String {
    format!(
        "{field} IN ({})",
        values
            .map(|value| quote(&value))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Push negation to leaves: native SQL supports `NOT CONTAIN_ANY`, but no unary `NOT`.
fn path_filter(filter: &StoragePathFilter, negated: bool) -> EngineResult<String> {
    Ok(match filter {
        StoragePathFilter::All => constant_filter(!negated),
        StoragePathFilter::None => constant_filter(negated),
        StoragePathFilter::Directory(id) => {
            if negated {
                // Native NOT CONTAIN_ANY omits empty arrays; root files have
                // no ancestors and must remain in the complement.
                format!(
                    "(ancestor_directory_ids NOT CONTAIN_ANY ({id}) OR array_length(ancestor_directory_ids) = 0)"
                )
            } else {
                format!("ancestor_directory_ids CONTAIN_ANY ({id})")
            }
        }
        StoragePathFilter::FileNameExact(name) => {
            if name.contains(['\\', '\0']) {
                return Err(EngineError::invalid_argument(
                    "this file name requires catalog matching",
                ));
            }
            format!(
                "file_name {} {}",
                if negated { "!=" } else { "=" },
                quote(name)
            )
        }
        StoragePathFilter::FileNamePrefix(_) | StoragePathFilter::FileNameSuffix(_) if negated => {
            return Err(EngineError::invalid_argument(
                "negated filename wildcards require catalog matching",
            ));
        }
        StoragePathFilter::FileNamePrefix(prefix) => {
            // Inverted prefix lookup does not unescape LIKE literals either.
            let wildcard = if prefix.contains(['_', '%', '\\']) {
                "%%"
            } else {
                "%"
            };
            format!(
                "file_name LIKE {}",
                quote(&format!("{}{wildcard}", like_literal(prefix)))
            )
        }
        StoragePathFilter::FileNameSuffix(suffix) => {
            // The native suffix inversion still keeps the leading '%' in 0.7.1
            // as a literal. Two equivalent '%' operators select its correct
            // forward LIKE evaluator until that native bug is fixed.
            format!(
                "file_name LIKE {}",
                quote(&format!("%%{}", like_literal(suffix)))
            )
        }
        StoragePathFilter::And(filters) => boolean_filter(filters, !negated, negated)?,
        StoragePathFilter::Or(filters) => boolean_filter(filters, negated, negated)?,
        StoragePathFilter::Not(filter) => path_filter(filter, !negated)?,
    })
}

fn constant_filter(value: bool) -> String {
    // Every stored document has a file ID, including ID zero.
    format!("file_id IS {}NULL", if value { "NOT " } else { "" })
}

fn boolean_filter(
    filters: &[StoragePathFilter],
    conjunction: bool,
    negated: bool,
) -> EngineResult<String> {
    if filters.is_empty() {
        return Ok(constant_filter(conjunction));
    }
    let operator = if conjunction { " AND " } else { " OR " };
    Ok(format!(
        "({})",
        filters
            .iter()
            .map(|filter| path_filter(filter, negated))
            .collect::<EngineResult<Vec<_>>>()?
            .join(operator)
    ))
}

fn like_literal(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn empty_filter(filter: Option<&StorageSearchFilter>) -> bool {
    filter.is_some_and(|filter| {
        filter.file_ids.as_ref().is_some_and(Vec::is_empty)
            || filter.entity_ids.as_ref().is_some_and(Vec::is_empty)
            || filter.symbol_names.as_ref().is_some_and(Vec::is_empty)
            || filter.symbol_types.as_ref().is_some_and(Vec::is_empty)
    })
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "\\'"))
}

fn top_k(limit: usize) -> EngineResult<i32> {
    if limit > MAX_TOP_K {
        return Err(EngineError::invalid_argument(format!(
            "storage query limit must not exceed {MAX_TOP_K}"
        )));
    }
    i32::try_from(limit)
        .map_err(|_| EngineError::invalid_argument("storage query limit is too large"))
}

fn lexical_text(fragment: &EntityFragment) -> String {
    let mut output = String::new();
    if let Some(metadata) = fragment.metadata() {
        match metadata {
            EntityMetadata::Code {
                symbol_name,
                scope,
                signature,
                documentation,
                ..
            } => {
                for value in [symbol_name, scope, signature, documentation]
                    .into_iter()
                    .flatten()
                {
                    output.push_str(value);
                    output.push('\n');
                }
            }
            EntityMetadata::Markdown { heading, scope, .. } => {
                for value in [heading, scope].into_iter().flatten() {
                    output.push_str(value);
                    output.push('\n');
                }
            }
        }
    }
    match fragment {
        EntityFragment::Standalone(entity) | EntityFragment::Representative(entity) => {
            match &entity.content {
                EntityContent::Source(contents) => append_contents(&mut output, contents),
                EntityContent::Outline(text) => output.push_str(text),
            }
        }
        EntityFragment::Window(window) => append_contents(&mut output, &window.contents),
    }
    if output.contains('\0') {
        output.replace('\0', " ")
    } else {
        output
    }
}

// C strings cannot contain NUL; indexed projections preserve token boundaries.
fn index_text(text: &str) -> Cow<'_, str> {
    if text.contains('\0') {
        Cow::Owned(text.replace('\0', " "))
    } else {
        Cow::Borrowed(text)
    }
}

fn append_contents(output: &mut String, contents: &[Content]) {
    for content in contents {
        match content {
            Content::Text(text) => output.push_str(text),
            Content::Image(image) => {
                output.push_str("[image:");
                output.push_str(image.format().as_str());
                output.push(']');
            }
            Content::Table(table) => {
                for cell in &table.cells {
                    append_contents(output, &cell.contents);
                }
            }
        }
        output.push('\n');
    }
}

fn symbol_type_name(value: SymbolType) -> &'static str {
    match value {
        SymbolType::Module => "module",
        SymbolType::Class => "class",
        SymbolType::Interface => "interface",
        SymbolType::Function => "function",
        SymbolType::Value => "value",
        SymbolType::Alias => "alias",
    }
}

fn file_key(id: FileId) -> String {
    format!("f{}", id.get())
}

fn u64_field(doc: &Doc, name: &str) -> EngineResult<u64> {
    native(doc.get_u64(name), "read numeric field")?.ok_or_else(|| corrupt("missing numeric field"))
}

fn primary_key(namespace: &str, value: &str) -> String {
    sha256_hex_parts([namespace.as_bytes(), b"\0", value.as_bytes()])
}

fn doc_key(doc: &Doc) -> EngineResult<&str> {
    doc.get_pk()
        .ok_or_else(|| corrupt("stored record has no primary key"))
}
fn string_field(doc: &Doc, field: &str) -> EngineResult<String> {
    native(doc.get_string(field), "read stored field")?
        .ok_or_else(|| corrupt(&format!("stored field {field} is missing")))
}
fn corrupt(message: &str) -> EngineError {
    EngineError::storage_failure(message)
}
#[track_caller]
fn native<T>(result: zvec_rust::Result<T>, operation: &str) -> EngineResult<T> {
    result.map_err(|error| EngineError::storage_failure(format!("zvec {operation}: {error}")))
}

#[cfg(test)]
mod tests;
