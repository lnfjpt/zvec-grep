use super::*;
use crate::domain::model::EmbeddingMetric;
use crate::domain::{
    ByteRange, CodeMetadata, Content, EntityFragment, EntityId, EntityMetadata, FileSnapshot,
    FragmentId, Range, SourcePath, SymbolType, TextRange,
};
use crate::storage::{
    path::encode_path,
    types::StoragePathFilter,
    zvec::{doc_key, fetch_map, native, string_field, write_docs},
};
use zvec_rust::{Doc, SearchQuery};

fn model() -> EmbeddingModelInfo {
    EmbeddingModelInfo::new(
        crate::domain::model::ModelInfo::new(
            "fixture",
            "fixture",
            [
                crate::domain::ContentKind::Text,
                crate::domain::ContentKind::Code,
                crate::domain::ContentKind::Image,
            ],
            32,
        )
        .expect("valid model metadata"),
        3,
        EmbeddingMetric::Cosine,
        None,
        None,
    )
    .expect("valid embedding metadata")
}

fn metadata_store(path: &Path) -> IndexStore {
    IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        storage_path: path.to_owned(),
        embeddings: vec![model()],
    })
    .expect("metadata storage")
}

impl IndexStore {
    fn apply_fixture_records(
        &self,
        file: &FileRecord,
        entities: &[Entity],
        entries: &[IndexedFragment],
    ) -> EngineResult<()> {
        assert_eq!(
            self.resolve_file_ids(&[file.relative_path().to_path_buf()])?,
            [file.id()]
        );
        let shared = self.shared()?;
        validate_batch(file, entities, entries, &shared.schema)?;
        self.write(|state| {
            super::replace_file(
                state,
                file,
                entities,
                entries,
                &FileGraph::default(),
                FlushPolicy::Immediate,
            )
        })
    }
}

fn entity_document(store: &IndexStore, id: &str) -> Doc {
    store
        .read(|state| {
            Ok(fetch_map(state.entities.collection(), &[id.to_owned()])?
                .remove(id)
                .expect("stored entity"))
        })
        .expect("fetch entity")
}

fn fragment_document(store: &IndexStore, model: &str, id: &str) -> Doc {
    store
        .read(|state| {
            Ok(
                fetch_map(state.fragments.collection(model)?, &[id.to_owned()])?
                    .remove(id)
                    .expect("stored fragment"),
            )
        })
        .expect("fetch fragment")
}

fn file(id: u32, path: impl Into<PathBuf>) -> FileRecord {
    FileRecord::new(
        FileId::new(id),
        crate::domain::SourcePath::new(path).expect("source path"),
        FileSnapshot {
            size_bytes: 100,
            modified_epoch_ms: None,
            content_hash: Some("fixture-hash".into()),
        },
        FileIndexStatus::Indexed {
            indexed_epoch_ms: 1,
            entity_count: 1,
        },
    )
    .expect("file record")
}

/// Metadata is immutable on `Entity`; rebuild with the new metadata.
fn with_metadata(entity: &Entity, metadata: Option<EntityMetadata>) -> Entity {
    Entity::new(
        entity.id().clone(),
        entity.file_id(),
        entity.source_range(),
        entity.content().clone(),
        metadata,
        entity.fragments().to_vec(),
    )
    .expect("rebuilt entity")
}

fn metadata_fragments(
    file_id: u32,
    label: &str,
) -> (FileRecord, Vec<Entity>, Vec<IndexedFragment>) {
    let source = file(file_id, "harvest.rs");
    let mut text = "Harvest outline".to_owned();
    text.push_str(&" ".repeat(20 - text.len()));
    text.push_str("orchard fruit");
    text.push_str(&" ".repeat(40 - text.len()));
    text.push_str(label);
    text.push_str(&" ".repeat(100 - text.len()));
    let content = Content::Text(text);
    let source_range =
        Range::Text(TextRange::from_coordinates(0, 100, 1, 1, 0, 100).expect("owner range"));
    let id = EntityId::new(source.id(), &content, source_range).expect("entity id");
    let owner = Entity::new(
        id.clone(),
        source.id(),
        source_range,
        content,
        Some(EntityMetadata::Code(CodeMetadata {
            symbol_type: Some(SymbolType::Function),
            symbol_name: Some("harvest 春'\\crop".into()),
            scope: Some("Garden".into()),
            signature: Some("pub async fn harvest() -> Crop".into()),
            visibility: None,
            language: None,
            documentation: Some("Produces the seasonal crop.".into()),
            arity: None,
            is_exported: false,
            is_declaration: false,
        })),
        vec![
            EntityFragment {
                id: FragmentId::new(&id, 0),
                range: Range::Byte(ByteRange::new(0, 15).expect("ordered byte offsets")),
            },
            EntityFragment {
                id: FragmentId::new(&id, 1),
                range: Range::Byte(ByteRange::new(20, 40).expect("ordered byte offsets")),
            },
        ],
    )
    .expect("valid entity");
    let entries = owner
        .fragments()
        .iter()
        .zip([
            (vec![0.0, 1.0, 0.0], "Harvest outline\n"),
            (vec![1.0, 0.0, 0.0], "orchard fruit        \n"),
        ])
        .map(|(fragment, (vector, text))| IndexedFragment {
            fts_text: format!("harvest 春'\\crop\nGarden\npub async fn harvest() -> Crop\nProduces the seasonal crop.\n{text}"),
            model: "fixture/fixture".into(),
            entity_id: owner.id().clone(),
            fragment_id: fragment.id.clone(),
            vector,
        })
        .collect();
    (source, vec![owner], entries)
}

fn assert_projection(store: &IndexStore, entry: &IndexedFragment) {
    let id = entry.fragment_id.as_str();
    let doc = fragment_document(store, "fixture/fixture", id);
    assert_eq!(
        string_field(&doc, "text").expect("prepared searchable text"),
        entry.fts_text.replace('\0', " "),
    );
    assert!(
        !doc.has_field("payload"),
        "search tables contain only projections"
    );
    {
        assert!(!doc.has_field("metadata"));
        assert_eq!(
            string_field(&doc, "symbol_name").expect("indexed owner name"),
            crate::utils::sha256_hex_parts([
                b"symbol".as_slice(),
                b"\0",
                "harvest 春'\\crop".as_bytes()
            ])
        );
        assert_eq!(
            string_field(&doc, "symbol_type").expect("indexed owner type"),
            "function"
        );
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Exercises canonical metadata and source readiness transitions in one index fixture."
)]
fn symbol_lookup_returns_canonical_definitions_with_exact_names_and_scopes() {
    let temporary = tempfile::tempdir().expect("temporary storage");
    let store = metadata_store(temporary.path());
    let (mut source, mut entities, mut entries) = metadata_fragments(0, "first owner");
    let (_, mut other_entities, other_entries) = metadata_fragments(0, "second owner");
    other_entities[0] = match other_entities[0].metadata().cloned() {
        Some(EntityMetadata::Code(mut code)) => {
            code.scope = Some("Meadow".into());
            with_metadata(&other_entities[0], Some(EntityMetadata::Code(code)))
        }
        other => with_metadata(&other_entities[0], other),
    };
    entities.extend(other_entities);
    entries.extend(other_entries);
    source
        .set_index_status(FileIndexStatus::Indexed {
            indexed_epoch_ms: 1,
            entity_count: 2,
        })
        .expect("mark indexed");
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("write definitions");
    let name = "harvest 春'\\crop";
    let mut matches = store.find_symbols(name, None).expect("exact symbols");
    matches.sort_by(|a, b| a.entity.id().cmp(b.entity.id()));
    let mut expected = entities.clone();
    expected.sort_by(|a, b| a.id().cmp(b.id()));
    assert_eq!(
        matches
            .iter()
            .map(|stored| stored.entity.clone())
            .collect::<Vec<_>>(),
        expected
    );
    assert!(matches.iter().all(|stored| stored.file == source));
    assert_eq!(
        store.find_symbols(name, Some("Garden")).expect("scope")[0].entity,
        entities[0]
    );
    for (name, scope) in [
        ("HARVEST 春'\\crop", None),
        ("orchard", None),
        (name, Some("garden")),
        (name, Some("missing")),
    ] {
        assert!(
            store
                .find_symbols(name, scope)
                .expect("no match")
                .is_empty()
        );
    }
    let ids = entities
        .iter()
        .map(|entity| entity.id().clone())
        .collect::<Vec<_>>();
    let requested = [
        ids[0].clone(),
        ids[0].clone(),
        EntityId::from_string("missing".into()),
    ];
    let fetched = store.get_entities(&requested).expect("canonical fetch");
    assert_eq!(fetched.len(), 1);
    assert_eq!(fetched[&ids[0]].entity, entities[0]);

    // Projection fields are only candidate hints; canonical metadata decides membership.
    let mut changed = entities.clone();
    changed[0] = match changed[0].metadata().cloned() {
        Some(EntityMetadata::Code(mut code)) => {
            code.symbol_name = Some("renamed".into());
            with_metadata(&changed[0], Some(EntityMetadata::Code(code)))
        }
        other => with_metadata(&changed[0], other),
    };
    changed[1] = with_metadata(&changed[1], None);
    store
        .write(|state| state.entities.write(&Entities::prepare(&changed)?))
        .expect("change canonical metadata only");
    assert!(
        store
            .find_symbols(name, None)
            .expect("canonical recheck")
            .is_empty()
    );
    store
        .write(|state| state.entities.write(&Entities::prepare(&entities)?))
        .expect("restore canonical names");

    for status in [
        FileIndexStatus::NotIndexed,
        FileIndexStatus::Deleting,
        FileIndexStatus::Failed {
            error: "interrupted".into(),
        },
    ] {
        source.set_index_status(status).expect("set status");
        store
            .write(|state| {
                let directories = state.directories.ensure(source.relative_path())?;
                state.files.put(&source, &directories)
            })
            .expect("mark source unavailable");
        assert!(
            store
                .get_entities(&ids)
                .expect("skip unindexed entities")
                .is_empty()
        );
        assert!(
            store
                .find_symbols(name, None)
                .expect("skip unindexed definitions")
                .is_empty()
        );
    }
    store
        .write(|state| state.files.delete(source.id()))
        .expect("remove source record");
    assert!(
        store
            .get_entities(&ids)
            .expect("skip missing source")
            .is_empty()
    );
    store.close().expect("close");
    assert_eq!(
        store
            .get_entities(&ids)
            .expect_err("closed entity fetch")
            .code(),
        EngineError::RESOURCE_CLOSED
    );
    assert_eq!(
        store
            .find_symbols(name, None)
            .expect_err("closed lookup")
            .code(),
        EngineError::RESOURCE_CLOSED
    );
}

#[test]
fn symbol_candidates_are_bounded_before_deduplication_across_model_tables() {
    let temporary = tempfile::tempdir().expect("temporary storage");
    let first = model();
    let second = EmbeddingModelInfo::new(
        crate::domain::model::ModelInfo::new(
            "fixture",
            "later",
            [
                crate::domain::ContentKind::Text,
                crate::domain::ContentKind::Code,
                crate::domain::ContentKind::Image,
            ],
            32,
        )
        .expect("valid model metadata"),
        3,
        EmbeddingMetric::Cosine,
        None,
        None,
    )
    .expect("valid embedding metadata");
    let store = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        storage_path: temporary.path().to_owned(),
        embeddings: vec![first.clone(), second.clone()],
    })
    .expect("multiple model tables");
    for (file_id, model, count) in [(0, first, 2), (1, second, 60)] {
        let (source, _, _) = metadata_fragments(file_id, "unused");
        let source = FileRecord::new(
            source.id(),
            SourcePath::new(format!("source-{file_id}.rs")).expect("path"),
            source.snapshot().clone(),
            FileIndexStatus::Indexed {
                indexed_epoch_ms: 1,
                entity_count: count,
            },
        )
        .expect("source record");
        let mut entities = Vec::new();
        let mut entries = Vec::new();
        for index in 0..count {
            let (_, owners, mut projections) =
                metadata_fragments(file_id, &format!("owner {index}"));
            for entry in &mut projections {
                entry.model = model.model().reference();
            }
            entities.extend(owners);
            entries.extend(projections);
        }
        store
            .apply_fixture_records(&source, &entities, &entries)
            .expect("write symbols");
    }
    let name = "harvest 春'\\crop";
    let ids = store
        .read(|state| state.fragments.find_symbol_entity_ids(name))
        .expect("bounded records");
    assert_eq!(ids.len(), 100);
    let expected = ids.into_iter().collect::<HashSet<_>>();
    let matches = store
        .find_symbols(name, None)
        .expect("deduplicated definitions");
    assert_eq!(
        matches
            .into_iter()
            .map(|stored| stored.entity.id().clone())
            .collect::<HashSet<_>>(),
        expected
    );
    assert!(
        expected.len() < 100,
        "multiple fragments resolve to one definition"
    );
}

#[test]
fn missing_or_invalid_graph_does_not_create_a_database_or_break_search() {
    let temporary = tempfile::tempdir().expect("temporary storage");
    let store = metadata_store(temporary.path());
    let (source, entities, entries) = metadata_fragments(0, "owner");
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("write fixture");
    store.close().expect("close writer");
    let graph_path = temporary.path().join("graph/graph.sqlite");
    fs::remove_file(&graph_path).expect("simulate legacy graph-less index");
    let reader = IndexStore::open(WorkspaceIndexStorageOptions::ReadOnly {
        storage_path: temporary.path().to_owned(),
    })
    .expect("open existing graph-less index");
    assert_eq!(
        reader
            .ensure_graph_available()
            .expect_err("missing graph")
            .code(),
        EngineError::NOT_FOUND
    );
    assert!(
        reader
            .neighborhood("missing", Direction::In, Some(&[EdgeKind::Calls]))
            .is_err()
    );
    assert!(
        reader
            .neighborhood("missing", Direction::Out, Some(&[EdgeKind::Calls]))
            .is_err()
    );
    assert!(!graph_path.exists());
    assert!(
        !reader
            .search_fts("orchard", 10, None)
            .expect("ordinary search")
            .is_empty()
    );
    fs::create_dir_all(graph_path.parent().expect("graph directory")).expect("create directory");
    fs::write(&graph_path, b"invalid SQLite graph").expect("broken existing graph");
    assert_eq!(
        reader
            .ensure_graph_available()
            .expect_err("invalid graph")
            .code(),
        EngineError::STORAGE_FAILURE
    );
    assert!(
        !reader
            .search_fts("orchard", 10, None)
            .expect("search ignores graph")
            .is_empty()
    );
    reader.close().expect("close reader");
    assert_eq!(
        fs::read(&graph_path).expect("graph unchanged"),
        b"invalid SQLite graph"
    );
}

#[test]
fn store_graph_reads_preserve_call_sites_and_share_the_storage_lifetime() {
    use crate::storage::graph::{EdgeKind, FileGraph, Metadata, Provenance};

    let temporary = tempfile::tempdir().expect("temporary storage");
    let store = metadata_store(temporary.path());
    let (file, entities, entries) = metadata_fragments(0, "owner");
    store
        .apply_fixture_records(&file, &entities, &entries)
        .expect("canonical endpoints");
    let caller = format!("f{}", file.id().get());
    let callee = entities[0].id().as_str().to_owned();
    store.close().expect("initialize index");
    let graph_path = temporary.path().join("graph/graph.sqlite");
    let mut graph =
        SqliteGraphStorage::open(&graph_path, OpenMode::ReadWrite).expect("graph fixture");
    let edges = (1..=25)
        .map(|line| Edge {
            kind: EdgeKind::Calls,
            source: caller.clone(),
            target: callee.clone(),
            line: Some(line),
            column: Some(0),
            provenance: Provenance::FileLocal,
            metadata: Metadata::new(),
        })
        .collect::<Vec<_>>();
    let mut persisted = edges.clone();
    persisted.push(Edge {
        kind: EdgeKind::Contains,
        ..edges[0].clone()
    });
    graph
        .write_file_graph(
            0,
            "fixture.rs",
            &FileGraph {
                entities: Vec::new(),
                entity_ids: vec![callee.clone()],
                edges: persisted,
                pending_refs: Vec::new(),
            },
            &[],
        )
        .expect("write graph");
    graph.close().expect("close graph fixture");
    let options = WorkspaceIndexStorageOptions::ReadOnly {
        storage_path: temporary.path().to_owned(),
    };
    let reader = IndexStore::open(options.clone()).expect("reader");
    let second = IndexStore::open(options).expect("shared reader");
    reader.ensure_graph_available().expect("existing graph");
    assert_eq!(
        reader
            .neighborhood(&callee, Direction::In, Some(&[EdgeKind::Calls]))
            .expect("all callers"),
        edges
    );
    assert_eq!(
        reader
            .neighborhood(&caller, Direction::Out, Some(&[EdgeKind::Calls]))
            .expect("all callees"),
        edges
    );
    assert!(
        reader
            .neighborhood(&caller, Direction::In, Some(&[EdgeKind::Calls]))
            .expect("direction")
            .is_empty()
    );
    assert!(
        reader
            .neighborhood("missing", Direction::Out, Some(&[EdgeKind::Calls]))
            .expect("unknown endpoint")
            .is_empty()
    );
    assert_eq!(
        reader
            .neighborhood(" ", Direction::In, Some(&[EdgeKind::Calls]))
            .expect_err("blank endpoint")
            .code(),
        EngineError::INVALID_ARGUMENT
    );
    reader.close().expect("close one lease");
    assert_eq!(
        reader
            .ensure_graph_available()
            .expect_err("closed graph read")
            .code(),
        EngineError::RESOURCE_CLOSED
    );
    assert_eq!(
        reader
            .neighborhood(&caller, Direction::Out, Some(&[EdgeKind::Calls]))
            .expect_err("closed graph query")
            .code(),
        EngineError::RESOURCE_CLOSED
    );
    assert_eq!(
        second
            .neighborhood(&callee, Direction::In, Some(&[EdgeKind::Calls]))
            .expect("remaining lease"),
        edges
    );
    second.close().expect("close last reader");
    metadata_store(temporary.path())
        .close()
        .expect("storage lock released");
}

#[test]
fn stores_shared_metadata_once_and_filters_windows_by_owner_fields() {
    let temporary = tempfile::tempdir().expect("temporary storage");
    let store = metadata_store(temporary.path());
    let (source, entities, mut entries) = metadata_fragments(0, "owner");
    entries[0].fts_text = "prepared\0projection\n".into();
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("write file");
    store.checkpoint().expect("flush storage");
    let owner = &entities[0];
    let entity_doc = entity_document(&store, owner.id().as_str());
    assert_eq!(
        Some(
            serde_json::from_str::<EntityMetadata>(
                &string_field(&entity_doc, "metadata").expect("metadata field")
            )
            .expect("metadata JSON")
        ),
        owner.metadata().cloned()
    );
    assert_eq!(
        store
            .read(|state| fetch_map(
                state.entities.collection(),
                &[entities[0].fragments()[1].id.as_str().to_owned()]
            ))
            .expect("window entity lookup")
            .len(),
        0
    );
    for entry in &entries {
        assert_projection(&store, entry);
    }

    let filter = StorageSearchFilter {
        symbol_names: Some(vec!["harvest 春'\\crop".into()]),
        symbol_types: Some(vec![SymbolType::Function]),
        ..StorageSearchFilter::default()
    };
    let hits = store
        .search_fts("orchard", 10, Some(&filter))
        .expect("filtered FTS");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].document_id, entities[0].fragments()[1].id.as_str());
    let vectors = store
        .search_vector("fixture/fixture", &entries[1].vector, 10, Some(&filter))
        .expect("filtered vector retrieval");
    assert_eq!(vectors.len(), 2);
    assert!(
        vectors
            .iter()
            .any(|hit| hit.document_id == entities[0].fragments()[1].id.as_str())
    );
    let loaded = store
        .load_search_hits(&hits)
        .expect("selected window details");
    assert_eq!(loaded.entities.len(), 1);
    assert_eq!(loaded.entities[owner.id()].entity, *owner);
    assert_eq!(loaded.entities[owner.id()].file, source);
    assert_eq!(
        loaded.fragments[entities[0].fragments()[1].id.as_str()],
        entities[0].fragments()[1]
    );

    for rejected in [
        StorageSearchFilter {
            symbol_names: Some(vec!["harvest".into()]),
            ..filter.clone()
        },
        StorageSearchFilter {
            symbol_types: Some(vec![SymbolType::Class]),
            ..filter
        },
    ] {
        assert!(
            store
                .search_fts("orchard", 10, Some(&rejected))
                .expect("filtered FTS")
                .is_empty()
        );
        assert!(
            store
                .search_vector("fixture/fixture", &entries[1].vector, 10, Some(&rejected))
                .expect("filtered vector retrieval")
                .is_empty()
        );
    }
}

#[test]
fn retrieval_defers_corrupt_metadata_until_selected_details_are_loaded() {
    let temporary = tempfile::tempdir().expect("temporary storage");
    let store = metadata_store(temporary.path());
    let (source, entities, entries) = metadata_fragments(0, "owner");
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("write file");
    let mut doc = entity_document(&store, entities[0].id().as_str());
    doc.add_string("metadata", "invalid metadata JSON")
        .expect("corrupt metadata");
    store
        .write(|state| state.entities.write(&[doc]))
        .expect("write corrupt metadata");
    store.checkpoint().expect("flush storage");

    for hits in [
        store
            .search_fts("orchard", 10, None)
            .expect("lightweight FTS"),
        store
            .search_vector("fixture/fixture", &entries[1].vector, 10, None)
            .expect("lightweight vectors"),
    ] {
        let window = hits
            .iter()
            .find(|hit| hit.document_id == entities[0].fragments()[1].id.as_str())
            .expect("window recalled without decoding metadata");
        assert_eq!(window.entity_id, *entities[0].id());
        let error = store
            .load_search_hits(std::slice::from_ref(window))
            .err()
            .expect("selected corrupt details must fail");
        assert!(error.message().contains("invalid entity metadata"));
    }
}

#[test]
fn native_search_and_loading_preserve_compact_ids_without_reencoding() {
    let temporary = tempfile::tempdir().expect("temporary storage");
    let store = metadata_store(temporary.path());
    let (source, entities, entries) = metadata_fragments(0, "owner");
    let entity_id = entities[0].id();
    let window_id = entities[0].fragments()[1].id.as_str();
    assert_eq!(entity_id.as_str().len(), 32);
    assert_eq!(window_id.len(), 40);
    assert!(entity_id.as_str().starts_with("00000000"));
    assert_eq!(window_id, format!("{}00000001", entity_id.as_str()));
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("write compact IDs");
    store.checkpoint().expect("flush storage");
    let filter = StorageSearchFilter {
        entity_ids: Some(vec![entity_id.clone()]),
        ..StorageSearchFilter::default()
    };
    for hits in [
        store
            .search_fts("orchard", 10, Some(&filter))
            .expect("ID-filtered FTS"),
        store
            .search_vector("fixture/fixture", &entries[1].vector, 10, Some(&filter))
            .expect("ID-filtered vectors"),
    ] {
        assert!(hits.iter().all(|hit| &hit.entity_id == entity_id));
        let window = hits
            .iter()
            .find(|hit| hit.document_id == window_id)
            .expect("window ID preserved");
        let loaded = store
            .load_search_hits(std::slice::from_ref(window))
            .expect("load compact IDs");
        let owner = &entities[0];
        assert_eq!(loaded.entities[owner.id()].entity, *owner);
        assert_eq!(loaded.fragments[window_id], owner.fragments()[1]);
    }
    let entity_doc = entity_document(&store, entity_id.as_str());
    assert_eq!(
        doc_key(&entity_doc).expect("entity primary key"),
        entity_id.as_str()
    );
    assert_eq!(
        string_field(&entity_doc, "entity_id").expect("stored owner ID"),
        entity_id.as_str()
    );
    let doc = fragment_document(&store, "fixture/fixture", window_id);
    assert_eq!(doc_key(&doc).expect("fragment primary key"), window_id);
    assert_eq!(
        string_field(&doc, "entity_id").expect("stored owner ID"),
        entity_id.as_str()
    );
    assert_eq!(
        string_field(&doc, "document_id").expect("stored window ID"),
        window_id
    );
}

#[test]
fn entity_content_is_canonical_and_fragments_have_one_model() {
    let home = tempfile::tempdir().expect("storage");
    let text = EmbeddingModelInfo::new(
        crate::domain::model::ModelInfo::new(
            "fixture",
            "fixture",
            [
                crate::domain::ContentKind::Text,
                crate::domain::ContentKind::Code,
                crate::domain::ContentKind::Image,
            ],
            32,
        )
        .expect("valid model metadata"),
        3,
        EmbeddingMetric::Cosine,
        None,
        None,
    )
    .expect("valid embedding metadata");
    let mut other = text.clone();
    {
        let mut metadata = serde_json::to_value(&other).expect("model metadata");
        metadata["model"]["name"] = serde_json::json!("other");
        other = serde_json::from_value(metadata).expect("valid model metadata");
    }
    let store = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        storage_path: home.path().to_owned(),
        embeddings: vec![text, other],
    })
    .expect("two models");
    let (source, entities, mut entries) = metadata_fragments(0, "owner");
    entries[1].model = "fixture/other".into();
    assert!(
        fragments::validate_projections(&entities, &entries).is_err(),
        "one entity cannot span model tables"
    );
    assert!(store.list_files().expect("unmodified").is_empty());
    entries[1].model = "fixture/fixture".into();
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("one model owns entire entity");
    let hits = store.search_fts("orchard", 10, None).expect("window match");
    assert_eq!(hits.len(), 1);
    let loaded = store.load_search_hits(&hits).expect("canonical bundle");
    assert_eq!(
        loaded.fragments.len(),
        2,
        "one owner read loads all its fragments"
    );
    assert_eq!(
        loaded.fragments[entities[0].fragments()[0].id.as_str()],
        entities[0].fragments()[0]
    );
    assert_eq!(
        loaded.fragments[entities[0].fragments()[1].id.as_str()],
        entities[0].fragments()[1]
    );
    store
        .read(|state| {
            for model in ["fixture/fixture", "fixture/other"] {
                let schema = state.fragments.collection(model)?.schema().expect("schema");
                assert!(schema.has_index("text"));
                assert!(schema.has_index("embedding"));
                assert!(!schema.has_field("payload"));
            }
            Ok(())
        })
        .expect("search schemas");
    // Removing a derived projection does not remove or redefine the canonical fragment.
    store
        .write(|state| state.fragments.delete_file(source.id()))
        .expect("remove projection");
    assert_eq!(
        store
            .load_search_hits(&hits)
            .expect("canonical data survives")
            .fragments,
        loaded.fragments
    );
}

#[test]
fn fragment_ids_cannot_be_reused_by_another_file_in_a_different_model_table() {
    let home = tempfile::tempdir().expect("storage");
    let first = EmbeddingModelInfo::new(
        crate::domain::model::ModelInfo::new(
            "fixture",
            "fixture",
            [
                crate::domain::ContentKind::Text,
                crate::domain::ContentKind::Code,
                crate::domain::ContentKind::Image,
            ],
            32,
        )
        .expect("valid model metadata"),
        3,
        EmbeddingMetric::Cosine,
        None,
        None,
    )
    .expect("valid embedding metadata");
    let mut second = first.clone();
    {
        let mut metadata = serde_json::to_value(&second).expect("model metadata");
        metadata["model"]["name"] = serde_json::json!("other");
        second = serde_json::from_value(metadata).expect("valid model metadata");
    }
    let store = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
        storage_path: home.path().to_owned(),
        embeddings: vec![first, second],
    })
    .expect("two models");
    let (source, entities, entries) = metadata_fragments(0, "owner");
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("original owner");
    let foreign_source = FileRecord::new(
        FileId::new(1),
        SourcePath::new("other.rs").expect("path"),
        source.snapshot().clone(),
        source.index_status().clone(),
    )
    .expect("file record");
    let owner = &entities[0];
    let foreign_id = EntityId::new(foreign_source.id(), owner.content(), owner.source_range())
        .expect("entity id");
    let foreign_entities = vec![
        Entity::new(
            foreign_id,
            foreign_source.id(),
            owner.source_range(),
            owner.content().clone(),
            owner.metadata().cloned(),
            owner.fragments().to_vec(),
        )
        .expect("valid entity with foreign fragments"),
    ];
    let mut foreign = entries.clone();
    for entry in &mut foreign {
        entry.model = "fixture/other".into();
        entry.entity_id = foreign_entities[0].id().clone();
    }
    assert!(
        store
            .apply_fixture_records(&foreign_source, &foreign_entities, &foreign)
            .is_err()
    );
    assert_eq!(store.list_files().expect("original file").len(), 1);
    assert_eq!(
        store
            .search_fts("orchard", 10, None)
            .expect("original fragments")
            .len(),
        1
    );
    assert!(
        store
            .search_vector("fixture/other", &[1.0, 0.0, 0.0], 10, None)
            .expect("no foreign fragments")
            .is_empty()
    );
}

#[test]
fn interrupted_replacements_remain_readable_and_retry_removes_stale_fragments() {
    let home = tempfile::tempdir().expect("storage");
    let store = metadata_store(home.path());
    let (source, entities, entries) = metadata_fragments(0, "old-owner");
    let source = FileRecord::new(
        source.id(),
        SourcePath::new("src/nested/harvest.rs").expect("path"),
        source.snapshot().clone(),
        source.index_status().clone(),
    )
    .expect("file record");
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("initial file");
    let old_hits = store.search_fts("orchard", 10, None).expect("old hits");

    // Simulate interruption after the retry marker but before replacing old rows.
    let mut unfinished = source.clone();
    unfinished
        .set_index_status(FileIndexStatus::NotIndexed)
        .expect("file status");
    store
        .write(|state| {
            let directories = state.directories.ensure(unfinished.relative_path())?;
            state.files.put(&unfinished, &directories)
        })
        .expect("retry state");
    store.checkpoint().expect("persist interrupted state");
    drop(store);

    let store = metadata_store(home.path());
    assert_eq!(
        store.list_files().expect("unchanged retry state"),
        [unfinished]
    );
    assert_eq!(
        store
            .load_search_hits(&old_hits)
            .expect("partial data remains readable")
            .entities[entities[0].id()]
        .entity,
        entities[0],
    );
    let (_, mut replacement, mut projections) = metadata_fragments(0, "new-owner");
    replacement[0] = Entity::new(
        replacement[0].id().clone(),
        replacement[0].file_id(),
        replacement[0].source_range(),
        replacement[0].content().clone(),
        replacement[0].metadata().cloned(),
        replacement[0].fragments()[..1].to_vec(),
    )
    .expect("valid replacement");
    projections.truncate(1);
    store
        .apply_fixture_records(&source, &replacement, &projections)
        .expect("retry replacement");
    store
        .apply_fixture_records(&source, &replacement, &projections)
        .expect("idempotent retry");
    assert_eq!(store.list_files().expect("completed file"), [source]);
    assert!(
        store
            .search_fts("orchard", 10, None)
            .expect("stale text removed")
            .is_empty()
    );
    assert!(
        store
            .load_search_hits(&old_hits)
            .expect("missing old owners are skipped")
            .entities
            .is_empty()
    );
    let new_hits = store.search_fts("Harvest", 10, None).expect("new results");
    assert_eq!(new_hits.len(), 1);
    assert_eq!(new_hits[0].entity_id, *replacement[0].id());
}

#[test]
fn interrupted_deletions_keep_identity_until_retry_finishes() {
    let home = tempfile::tempdir().expect("storage");
    let store = metadata_store(home.path());
    let (source, entities, entries) = metadata_fragments(0, "owner");
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("initial file");
    let mut deleting = source.clone();
    deleting
        .set_index_status(FileIndexStatus::Deleting)
        .expect("file status");
    store
        .write(|state| state.files.mark_deleting(source.id()))
        .expect("delete intent");
    // One collection can be cleared while another still contains old results.
    store
        .write(|state| state.entities.delete_file(source.id()))
        .expect("delete entities");
    store.checkpoint().expect("persist interruption");
    drop(store);

    let store = metadata_store(home.path());
    assert_eq!(
        store.list_files().expect("deletion still pending"),
        [deleting]
    );
    let hits = store.search_fts("orchard", 10, None).expect("stale hit");
    assert_eq!(hits.len(), 1);
    assert!(
        store
            .load_search_hits(&hits)
            .expect("missing entities are skipped")
            .entities
            .is_empty()
    );
    store.delete_file(source.id()).expect("finish deleting");
    store.delete_file(source.id()).expect("repeat deletion");
    assert!(store.list_files().expect("no files").is_empty());
    assert!(
        store
            .search_fts("orchard", 10, None)
            .expect("no stale hits")
            .is_empty()
    );
}

#[test]
fn result_loading_skips_missing_files_and_canonical_fragments() {
    let home = tempfile::tempdir().expect("storage");
    let store = metadata_store(home.path());
    let (source, mut entities, entries) = metadata_fragments(0, "owner");
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("initial file");
    let hits = store.search_fts("orchard", 10, None).expect("window hit");
    entities[0] = Entity::new(
        entities[0].id().clone(),
        entities[0].file_id(),
        entities[0].source_range(),
        entities[0].content().clone(),
        entities[0].metadata().cloned(),
        entities[0].fragments()[..1].to_vec(),
    )
    .expect("valid entity with one remaining fragment");
    store
        .write(|state| state.entities.write(&Entities::prepare(&entities)?))
        .expect("partial replacement");
    let loaded = store
        .load_search_hits(&hits)
        .expect("missing canonical fragment is skipped");
    assert!(!loaded.fragments.contains_key(&hits[0].document_id));

    store
        .write(|state| state.files.delete(source.id()))
        .expect("missing file");
    let loaded = store
        .load_search_hits(&hits)
        .expect("missing owner file is skipped");
    assert!(loaded.entities.is_empty());
    assert!(loaded.fragments.is_empty());
}

#[test]
fn maximum_u32_directory_id_survives_reopen_and_filters_both_retrieval_collections() {
    let temporary = tempfile::tempdir().expect("storage");
    metadata_store(temporary.path())
        .close()
        .expect("create tables");
    let directory_table =
        Directories::open(&temporary.path().join("storage"), false).expect("directory table");
    let mut doc = Doc::new().expect("directory doc");
    doc.set_pk(&format!("d{}", u32::MAX));
    doc.add_u32("directory_id", u32::MAX).expect("directory ID");
    doc.add_string(
        "path",
        &encode_path(&SourcePath::new("edge").expect("path")).expect("path record"),
    )
    .expect("directory path");
    write_docs(
        directory_table.collection(),
        &[doc],
        "seed maximum directory ID",
    )
    .expect("stored directory");
    directory_table.flush().expect("persist directory");
    drop(directory_table);

    let store = metadata_store(temporary.path());
    let (source, entities, entries) = metadata_fragments(0, "owner");
    let source = FileRecord::new(
        source.id(),
        SourcePath::new("edge/harvest.rs").expect("path"),
        source.snapshot().clone(),
        source.index_status().clone(),
    )
    .expect("file record");
    store
        .apply_fixture_records(&source, &entities, &entries)
        .expect("maximum membership");
    store.checkpoint().expect("checkpoint");
    store
        .write(|state| {
            native(state.files.collection().optimize(), "optimize files")?;
            native(
                state.directories.collection().optimize(),
                "optimize directories",
            )?;
            native(
                state.fragments.collection("fixture/fixture")?.optimize(),
                "optimize membership",
            )
        })
        .expect("optimize tables");
    store.close().expect("close writer");
    let reader = IndexStore::open(WorkspaceIndexStorageOptions::ReadOnly {
        storage_path: temporary.path().to_owned(),
    })
    .expect("reopen");
    let filter = StorageSearchFilter {
        path: Some(StoragePathFilter::Directory(
            SourcePath::new("edge").expect("path"),
        )),
        ..StorageSearchFilter::default()
    };
    assert_eq!(
        reader
            .search_fts("orchard", 10, Some(&filter))
            .expect("FTS membership")
            .len(),
        1
    );
    assert_eq!(
        reader
            .search_vector("fixture/fixture", &[1.0, 0.0, 0.0], 10, Some(&filter))
            .expect("vector membership")
            .len(),
        2
    );
    let docs = reader
        .read(|state| {
            native(
                state
                    .files
                    .collection()
                    .query(&SearchQuery::scalar(1).expect("query")),
                "read source records",
            )
        })
        .expect("source records");
    assert_eq!(
        docs[0].get_u32("file_id").expect("u32 file ID"),
        Some(source.id().get())
    );
    assert_eq!(
        docs[0]
            .get_array_u32("ancestor_directory_ids")
            .expect("u32 ancestors"),
        Some(vec![u32::MAX])
    );
}
