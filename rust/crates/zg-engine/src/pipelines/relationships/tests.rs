use std::{collections::BTreeMap, fs, path::PathBuf, time::Duration};

use tokio_util::sync::CancellationToken;

use crate::{
    EngineError, EngineRuntimeSnapshot, ZvecGrep,
    api::relationships::RelationshipOptions,
    domain::{
        CodeMetadata, Content, Entity, EntityFragment, EntityId, EntityMetadata, FileIndexStatus,
        FileRecord, FileSnapshot, FragmentId, IndexDescriptor, IndexState, Range, ScanRules,
        SourcePath, SymbolType, TextRange, Workspace,
        model::{EmbeddingModelInfo, Metric, ModelInfo},
    },
    storage::{
        IndexStore,
        graph::{
            Edge, EdgeKind, FileGraph, OpenMode, PendingRef, Provenance, Resolution,
            SqliteGraphStorage,
        },
        types::{IndexedFragment, WorkspaceIndexStorageOptions},
    },
    workspace::{
        CURRENT_INDEX_VERSION,
        lock::{LockMode, acquire_home_lock},
        manifest::{WorkspaceManifest, write_workspace_manifest},
    },
};

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    graph_path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path().canonicalize().expect("canonical root");
        let home = root.join(".zvec-grep");
        let mut manifest = WorkspaceManifest::new(
            Workspace {
                name: "relationship-fixture".into(),
                root: root.clone(),
                scan: ScanRules::default(),
                index: IndexState::Enabled(IndexDescriptor::single(schema())),
                created_epoch_ms: 1,
                updated_epoch_ms: 1,
            },
            home.clone(),
            Some(CURRENT_INDEX_VERSION),
            BTreeMap::new(),
        )
        .expect("manifest");
        manifest.storage_generation = Some(uuid::Uuid::new_v4().to_string());
        let storage_home = manifest.storage_home();
        fs::create_dir_all(&storage_home).expect("generation");
        let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: storage_home.clone(),
            embeddings: vec![schema()],
        })
        .expect("writable index");
        let local = write_symbols(
            &storage,
            "a.rs",
            &[
                ("run", Some("red"), 2, "fn run() {}"),
                ("run", Some("blue"), 10, "fn run() {}"),
                ("invoke", None, 20, "fn invoke() { run(); }"),
            ],
        );
        let remote = write_symbols(&storage, "z.rs", &[("run", None, 3, "fn run() {}")]);
        write_symbols(
            &storage,
            "c.rs",
            &[
                ("helper", Some("red"), 5, "fn helper() { needle(); run(); }"),
                (
                    "helper",
                    Some("blue"),
                    15,
                    "fn helper() { needle(); run(); }",
                ),
                ("", None, 25, "needle is mentioned in non-code content"),
            ],
        );
        storage.close().expect("persist index before reading");
        let graph_path = storage_home.join("graph/graph.sqlite");
        write_graph(&graph_path, &local, &remote[0]);
        write_workspace_manifest(&home, &manifest).expect("publish manifest");
        Self {
            _directory: directory,
            root,
            home,
            graph_path,
        }
    }

    fn options(&self, symbol: &str) -> RelationshipOptions {
        RelationshipOptions {
            root: Some(self.root.clone()),
            symbol: symbol.into(),
            ..RelationshipOptions::default()
        }
    }
}

fn schema() -> EmbeddingModelInfo {
    EmbeddingModelInfo {
        model: ModelInfo {
            provider: "fixture".into(),
            name: "relationships-without-model-runtime".into(),
            endpoint: None,
        },
        dimension: 3,
        metric: Metric::Cosine,
        max_batch_size: 32,
        max_input_tokens: None,
        max_image_bytes: None,
    }
}

fn write_symbols(
    storage: &IndexStore,
    path: &str,
    definitions: &[(&str, Option<&str>, usize, &str)],
) -> Vec<Entity> {
    let relative_path = SourcePath::new(path).expect("source path");
    let file_id = storage
        .resolve_file_ids(&[relative_path.to_path_buf()])
        .expect("file identity")[0];
    let entities: Vec<_> = definitions
        .iter()
        .map(|&(name, scope, line, text)| {
            let content = Content::Text(text.into());
            let offset = (line - 1) * 100;
            let source_range = Range::Text(
                TextRange::from_coordinates(offset, offset + text.len(), line, line, 0, text.len())
                    .expect("source coordinates"),
            );
            let id = EntityId::new(file_id, &content, source_range).expect("entity identity");
            Entity {
                fragments: (0..2)
                    .map(|ordinal| EntityFragment {
                        id: FragmentId::new(&id, ordinal),
                        range: Range::Full,
                    })
                    .collect(),
                id,
                file_id,
                source_range,
                content,
                metadata: (!name.is_empty()).then(|| {
                    EntityMetadata::Code(CodeMetadata {
                        symbol_name: Some(name.into()),
                        symbol_type: Some(SymbolType::Function),
                        scope: scope.map(str::to_owned),
                        signature: None,
                        documentation: None,
                        ..Default::default()
                    })
                }),
            }
        })
        .collect();
    let file = FileRecord {
        id: file_id,
        relative_path,
        snapshot: FileSnapshot {
            size_bytes: 4096,
            modified_epoch_ms: None,
            content_hash: Some(crate::utils::sha256_hex(path.as_bytes())),
        },
        index_status: FileIndexStatus::NotIndexed,
    };
    let entries: Vec<_> = entities
        .iter()
        .flat_map(|entity| {
            entity.fragments.iter().map(|fragment| IndexedFragment {
                entity_id: entity.id.clone(),
                fragment_id: fragment.id.clone(),
                model: schema().model.reference(),
                vector: vec![1.0, 0.0, 0.0],
                fts_text: match &entity.content {
                    Content::Text(text) => text.clone(),
                    _ => unreachable!("text fixture"),
                },
            })
        })
        .collect();
    storage
        .replace_file(&file, &entities, &entries)
        .expect("index fixture symbols");
    entities
}

fn edge(source: &Entity, target: &str, line: u32) -> Edge {
    Edge {
        kind: EdgeKind::Calls,
        source: source.id.as_str().into(),
        target: target.into(),
        line: Some(line),
        column: Some(4),
        provenance: Provenance::FileLocal,
        metadata: serde_json::Map::default(),
    }
}

fn pending(source: &Entity, name: &str, line: u32) -> PendingRef {
    PendingRef {
        from_node_id: source.id.as_str().into(),
        reference_name: name.into(),
        receiver_name: None,
        reference_kind: EdgeKind::Calls,
        arity: None,
        line,
        col: 8,
        metadata: serde_json::Map::default(),
        candidates: None,
        language: "rust".into(),
        name_tail: name.into(),
    }
}

fn write_graph(path: &std::path::Path, local: &[Entity], remote: &Entity) {
    let mut graph = SqliteGraphStorage::open(path, OpenMode::ReadWrite).expect("graph writer");
    let source = &local[2];
    let missing_id = EntityId::new(
        source.file_id,
        &Content::Text("missing".into()),
        Range::Full,
    )
    .expect("missing target identity");
    let mut edges: Vec<_> = (1..=25)
        .map(|line| edge(source, local[0].id.as_str(), line))
        .chain((30..=52).map(|line| edge(source, local[1].id.as_str(), line)))
        .collect();
    edges.push(edge(source, missing_id.as_str(), 99));
    let mut contains = edge(source, local[0].id.as_str(), 100);
    contains.kind = EdgeKind::Contains;
    edges.push(contains);
    graph
        .write_file_graph(
            source.file_id.get(),
            "local.rs",
            &FileGraph {
                entities: Vec::new(),
                entity_ids: local
                    .iter()
                    .map(|entity| entity.id.as_str().to_owned())
                    .chain(std::iter::once(missing_id.as_str().into()))
                    .collect(),
                edges,
                pending_refs: vec![
                    pending(source, "remote", 101),
                    pending(source, "unresolved", 102),
                ],
            },
            &[],
        )
        .expect("local callsites and pending refs");
    graph
        .write_file_graph(
            remote.file_id.get(),
            "remote.rs",
            &FileGraph {
                entities: Vec::new(),
                entity_ids: vec![remote.id.as_str().into()],
                ..FileGraph::default()
            },
            &[],
        )
        .expect("remote target");
    let reference = graph
        .list_pending_refs(10, 0)
        .expect("pending references")
        .refs
        .into_iter()
        .find(|reference| reference.reference.reference_name == "remote")
        .expect("remote reference");
    graph
        .apply_resolutions(&[Resolution {
            ref_id: reference.id,
            target_id: remote.id.as_str().into(),
            provenance: Provenance::ImportScoped,
        }])
        .expect("resolve cross-file call");
    graph.close().expect("close graph writer");
}

#[tokio::test]
async fn exact_definitions_keep_scopes_order_and_per_symbol_limits() {
    let fixture = Fixture::new();
    let engine = ZvecGrep::new();
    let groups = engine
        .callers(fixture.options(" run "))
        .await
        .expect("callers");
    assert_eq!(
        groups.len(),
        3,
        "exact names exclude content-only FTS matches"
    );
    assert_eq!(
        groups
            .iter()
            .map(|group| group.symbol.name.as_str())
            .collect::<Vec<_>>(),
        ["red::run", "blue::run", "run"]
    );
    assert_eq!(
        groups
            .iter()
            .map(|group| group.total_edges)
            .collect::<Vec<_>>(),
        [25, 23, 1]
    );
    assert_eq!(
        groups
            .iter()
            .map(|group| group.edges.len())
            .collect::<Vec<_>>(),
        [20, 20, 1]
    );
    assert_eq!(groups[0].symbol.file_path, PathBuf::from("a.rs"));
    assert_eq!(
        (groups[0].symbol.start_line, groups[0].symbol.end_line),
        (2, 2)
    );
    assert_eq!(groups[1].symbol.start_line, 10);
    assert_eq!(groups[2].symbol.file_path, PathBuf::from("z.rs"));
    assert_eq!(
        groups[0].edges[0].symbol.as_ref().expect("caller").name,
        "invoke"
    );
    assert_eq!(
        (groups[0].edges[0].line, groups[0].edges[0].column),
        (Some(1), Some(4))
    );
    assert_eq!(
        groups[0].edges[19].line,
        Some(20),
        "keep distinct callsites"
    );

    engine
        .enable_read_session_cache()
        .expect("enable read cache");
    assert_eq!(
        engine
            .callers(fixture.options("run"))
            .await
            .expect("cached relationship read"),
        groups,
    );

    let limited = engine
        .callers(RelationshipOptions {
            limit: Some(2),
            ..fixture.options("run")
        })
        .await
        .expect("limited groups");
    assert_eq!(
        limited
            .iter()
            .map(|group| group.edges.len())
            .collect::<Vec<_>>(),
        [2, 2, 1]
    );
    assert_eq!(limited[0].total_edges, 25);
    let scoped = engine
        .callers(fixture.options("blue::run"))
        .await
        .expect("scoped lookup");
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].symbol.name, "blue::run");
    assert_eq!(scoped[0].total_edges, 23);
    assert_eq!(
        engine.runtime_snapshot(),
        EngineRuntimeSnapshot::default(),
        "relationships do not load embedding models"
    );
    engine.close();
}

#[tokio::test]
async fn callees_keep_repeated_sites_cross_file_edges_and_missing_descriptions() {
    let fixture = Fixture::new();
    let engine = ZvecGrep::new();
    let groups = engine
        .callees(RelationshipOptions {
            limit: Some(100),
            ..fixture.options("invoke")
        })
        .await
        .expect("callees");
    assert_eq!(groups.len(), 1);
    let group = &groups[0];
    assert_eq!(
        group.total_edges, 50,
        "pending and non-call edges are omitted"
    );
    assert_eq!(group.edges.len(), 50);
    assert_eq!(
        group
            .edges
            .iter()
            .filter(|edge| edge
                .symbol
                .as_ref()
                .is_some_and(|symbol| symbol.name == "red::run"))
            .count(),
        25
    );
    assert_eq!(
        group
            .edges
            .iter()
            .filter(|edge| edge
                .symbol
                .as_ref()
                .is_some_and(|symbol| symbol.name == "blue::run"))
            .count(),
        23
    );
    let missing = group
        .edges
        .iter()
        .find(|edge| edge.line == Some(99))
        .expect("missing endpoint retains its callsite");
    assert!(missing.symbol.is_none());
    assert!(serde_json::to_value(missing).expect("serialize edge")["symbol"].is_null());
    let remote = group
        .edges
        .iter()
        .find(|edge| edge.line == Some(101))
        .expect("resolved cross-file edge");
    assert_eq!(remote.column, Some(8));
    assert_eq!(
        remote
            .symbol
            .as_ref()
            .expect("remote description")
            .file_path,
        PathBuf::from("z.rs")
    );
    assert!(group.edges.iter().all(|edge| edge.line != Some(102)));
    engine.close();
}

#[tokio::test]
async fn fts_fallback_accepts_content_mentions_deduplicates_and_respects_scope() {
    let fixture = Fixture::new();
    let engine = ZvecGrep::new();
    let groups = engine
        .callees(fixture.options("needle"))
        .await
        .expect("FTS recall");
    assert_eq!(
        groups.len(),
        2,
        "two fragments per entity and non-code hits are filtered"
    );
    assert_eq!(groups[0].symbol.name, "red::helper");
    assert_eq!(groups[1].symbol.name, "blue::helper");
    assert!(
        groups
            .iter()
            .all(|group| group.total_edges == 0 && group.edges.is_empty())
    );
    let scoped = engine
        .callers(fixture.options("red::needle"))
        .await
        .expect("scoped FTS recall");
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].symbol.name, "red::helper");
    assert!(
        engine
            .callers(fixture.options("absentneedle"))
            .await
            .expect("no matches")
            .is_empty()
    );
    assert_eq!(engine.runtime_snapshot(), EngineRuntimeSnapshot::default());
    engine.close();
}

#[tokio::test]
async fn missing_graph_errors_without_creating_a_database() {
    let fixture = Fixture::new();
    fs::remove_file(&fixture.graph_path).expect("remove graph fixture");
    let engine = ZvecGrep::new();
    for error in [
        engine
            .callers(fixture.options("run"))
            .await
            .expect_err("missing graph"),
        engine
            .callees(fixture.options("run"))
            .await
            .expect_err("missing graph"),
    ] {
        assert_eq!(error.code(), EngineError::NOT_FOUND);
        assert!(error.to_string().to_lowercase().contains("graph"));
        assert!(
            !fixture.graph_path.exists(),
            "relationship reads never initialize SQLite"
        );
    }
    engine.close();
}

#[tokio::test]
async fn lock_timeouts_cancellation_and_closed_engines_reject_reads() {
    let fixture = Fixture::new();
    let engine = ZvecGrep::new();
    let writer = acquire_home_lock(&fixture.home, LockMode::Write, "relationships.test")
        .expect("hold writer lock");
    let blocked = engine
        .callers(RelationshipOptions {
            lock_timeout_ms: Some(0),
            ..fixture.options("run")
        })
        .await
        .expect_err("locked read");
    assert_eq!(blocked.code(), EngineError::RESOURCE_BUSY);
    let signal = CancellationToken::new();
    let options = RelationshipOptions {
        signal: Some(signal.clone()),
        ..fixture.options("run")
    };
    let (cancelled, ()) = tokio::join!(engine.callees(options), async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        signal.cancel();
    });
    assert_eq!(
        cancelled.expect_err("cancel waiting read").code(),
        EngineError::CANCELLED
    );
    drop(writer);
    assert_eq!(
        engine
            .callers(fixture.options("run"))
            .await
            .expect("reader recovers after failed admission")
            .len(),
        3
    );
    engine.close();
    assert_eq!(
        engine
            .callers(fixture.options("run"))
            .await
            .expect_err("closed callers")
            .code(),
        EngineError::RESOURCE_CLOSED
    );
    assert_eq!(
        engine
            .callees(fixture.options("run"))
            .await
            .expect_err("closed callees")
            .code(),
        EngineError::RESOURCE_CLOSED
    );
}

#[tokio::test]
async fn invalid_queries_fail_before_opening_a_workspace() {
    let directory = tempfile::tempdir().expect("unindexed workspace");
    let engine = ZvecGrep::new();
    for (symbol, limit) in [
        (String::new(), None),
        ("   ".into(), None),
        ("scope::".into(), None),
        ("x".repeat(1025), None),
        ("run".into(), Some(0)),
    ] {
        let options = RelationshipOptions {
            root: Some(directory.path().to_path_buf()),
            symbol,
            limit,
            ..RelationshipOptions::default()
        };
        assert_eq!(
            engine
                .callers(options.clone())
                .await
                .expect_err("invalid callers query")
                .code(),
            EngineError::INVALID_ARGUMENT,
        );
        assert_eq!(
            engine
                .callees(options)
                .await
                .expect_err("invalid callees query")
                .code(),
            EngineError::INVALID_ARGUMENT,
        );
    }
    assert!(!directory.path().join(".zvec-grep").exists());
    engine.close();
}
