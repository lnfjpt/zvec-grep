use super::*;
use crate::domain::model::{Metric, ModelInfo};
use crate::domain::{
    Content, EntityFragment, EntityId, FileSnapshot, FragmentId, Range, SourcePath,
};
use crate::storage::graph::{
    Direction, Edge, EdgeKind, FileGraph, Metadata, PendingRef, Provenance, Resolution,
    SqliteGraphStorage, StoredPendingRef,
};
use rusqlite::Connection;

struct Snapshot {
    file: FileRecord,
    entities: Vec<Entity>,
    fragments: Vec<IndexedFragment>,
    graph: FileGraph,
}

fn open(path: &Path, read_only: bool) -> IndexStore {
    let options = if read_only {
        WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: path.to_owned(),
        }
    } else {
        WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: path.to_owned(),
            embeddings: vec![EmbeddingModelInfo {
                model: ModelInfo {
                    provider: "fixture".into(),
                    name: "graph".into(),
                    endpoint: None,
                },
                dimension: 3,
                metric: Metric::Cosine,
                max_batch_size: 32,
                max_input_tokens: None,
                max_image_bytes: None,
            }],
        }
    };
    IndexStore::open(options).expect("open combined storage")
}

fn snapshot(store: &IndexStore, path: &str, symbols: &[&str]) -> Snapshot {
    let relative_path = SourcePath::new(path).expect("source path");
    let id = store
        .resolve_file_ids(&[relative_path.to_path_buf()])
        .expect("reserve file ID")[0];
    let text = symbols.join("\n");
    let file = FileRecord {
        id,
        relative_path,
        snapshot: FileSnapshot {
            size_bytes: text.len() as u64,
            modified_epoch_ms: Some(1),
            content_hash: Some(crate::utils::sha256_hex(text.as_bytes())),
        },
        index_status: FileIndexStatus::NotIndexed,
    };
    let entities = symbols
        .iter()
        .map(|symbol| {
            let content = Content::Text((*symbol).into());
            let entity_id = EntityId::new(id, &content, Range::Full).expect("entity identity");
            Entity {
                id: entity_id.clone(),
                file_id: id,
                source_range: Range::Full,
                content,
                metadata: None,
                fragments: vec![EntityFragment {
                    id: FragmentId::new(&entity_id, 0),
                    range: Range::Full,
                }],
            }
        })
        .collect::<Vec<_>>();
    let fragments = entities
        .iter()
        .zip(symbols)
        .map(|(entity, symbol)| IndexedFragment {
            entity_id: entity.id.clone(),
            fragment_id: entity.fragments[0].id.clone(),
            model: "fixture/graph".into(),
            vector: vec![1.0, 0.0, 0.0],
            fts_text: (*symbol).into(),
        })
        .collect();
    let graph = FileGraph {
        entity_ids: entities
            .iter()
            .map(|entity| entity.id.as_str().to_owned())
            .collect(),
        ..FileGraph::default()
    };
    Snapshot {
        file,
        entities,
        fragments,
        graph,
    }
}

fn write(store: &IndexStore, snapshot: &Snapshot) {
    store
        .replace_file_with_graph(
            &snapshot.file,
            &snapshot.entities,
            &snapshot.fragments,
            &snapshot.graph,
        )
        .expect("replace canonical and graph records");
}

fn local_call(source: &Entity, target: &Entity) -> Edge {
    Edge {
        kind: EdgeKind::Calls,
        source: source.id.as_str().into(),
        target: target.id.as_str().into(),
        line: Some(1),
        column: Some(0),
        provenance: Provenance::FileLocal,
        metadata: Metadata::new(),
    }
}

fn reference(source: &Entity) -> PendingRef {
    PendingRef {
        from_node_id: source.id.as_str().into(),
        reference_name: "module::target".into(),
        receiver_name: Some("module".into()),
        reference_kind: EdgeKind::Calls,
        arity: Some(0),
        line: 1,
        col: 0,
        metadata: Metadata::new(),
        candidates: None,
        language: "rust".into(),
        name_tail: "target".into(),
    }
}

fn read_graph<T>(store: &IndexStore, read: impl FnOnce(&SqliteGraphStorage) -> T) -> T {
    store.ensure_graph_available().expect("open graph reader");
    store
        .read(|state| Ok(read(state.graph.as_ref().expect("graph is open"))))
        .expect("read graph under storage lock")
}

fn pending(store: &IndexStore) -> Vec<StoredPendingRef> {
    read_graph(store, |graph| {
        graph.list_pending_refs(100, 0).expect("pending refs").refs
    })
}

fn callers(store: &IndexStore, target: &Entity) -> Vec<Edge> {
    read_graph(store, |graph| {
        graph
            .neighborhood(target.id.as_str(), Direction::In, Some(&[EdgeKind::Calls]))
            .expect("callers")
    })
}

fn resolve(store: &IndexStore, target: &Entity) -> i64 {
    store
        .write(|state| {
            let graph = state.graph.as_mut().expect("graph is open");
            let refs = graph.list_pending_refs(100, 0).expect("pending refs");
            assert_eq!(refs.refs.len(), 1);
            let id = refs.refs[0].id;
            let stats = graph
                .apply_resolutions(&[Resolution {
                    ref_id: id,
                    target_id: target.id.as_str().into(),
                    provenance: Provenance::ImportScoped,
                }])
                .expect("resolve reference under storage lock");
            assert_eq!(stats.resolved, 1);
            Ok(id)
        })
        .expect("write graph under storage lock")
}

fn stored_entities(store: &IndexStore, entities: &[Entity]) -> HashMap<EntityId, Entity> {
    let ids = entities
        .iter()
        .map(|entity| entity.id.clone())
        .collect::<Vec<_>>();
    store
        .read(|state| state.entities.fetch(&ids))
        .expect("fetch canonical entities")
}

#[test]
fn replacement_reads_old_ids_and_invalidates_cross_file_references() {
    let temporary = tempfile::tempdir().expect("workspace");
    let store = open(temporary.path(), false);
    let target = snapshot(&store, "target.rs", &["old first", "old second"]);
    let mut source = snapshot(&store, "source.rs", &["caller"]);
    source
        .graph
        .pending_refs
        .push(reference(&source.entities[0]));
    write(&store, &target);
    write(&store, &source);
    let ref_id = resolve(&store, &target.entities[1]);
    let mut replacement = snapshot(&store, "target.rs", &["new first", "new second"]);
    replacement.graph.edges.push(local_call(
        &replacement.entities[0],
        &replacement.entities[1],
    ));
    write(&store, &replacement);

    assert!(stored_entities(&store, &target.entities).is_empty());
    assert_eq!(stored_entities(&store, &replacement.entities).len(), 2);
    assert!(callers(&store, &target.entities[1]).is_empty());
    assert_eq!(
        callers(&store, &replacement.entities[1]),
        replacement.graph.edges
    );
    let refs = pending(&store);
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].id, ref_id);
    assert_eq!(refs[0].file_id, source.file.id.get());
    assert_eq!(refs[0].reference, source.graph.pending_refs[0]);
}

#[test]
fn unchanged_ids_and_deletion_both_invalidate_incoming_edges() {
    let temporary = tempfile::tempdir().expect("workspace");
    let store = open(temporary.path(), false);
    let target = snapshot(&store, "target.rs", &["target"]);
    let mut source = snapshot(&store, "source.rs", &["caller"]);
    source
        .graph
        .pending_refs
        .push(reference(&source.entities[0]));
    write(&store, &target);
    write(&store, &source);
    let ref_id = resolve(&store, &target.entities[0]);

    write(&store, &target);
    assert!(callers(&store, &target.entities[0]).is_empty());
    assert_eq!(pending(&store)[0].id, ref_id);
    resolve(&store, &target.entities[0]);
    store.delete_file(target.file.id).expect("delete target");
    assert!(stored_entities(&store, &target.entities).is_empty());
    assert!(callers(&store, &target.entities[0]).is_empty());
    assert_eq!(pending(&store)[0].id, ref_id);
    assert_eq!(
        store.list_files().expect("remaining file")[0].id,
        source.file.id
    );

    store
        .delete_file(source.file.id)
        .expect("delete reference owner");
    store.delete_file(source.file.id).expect("repeat deletion");
    assert!(pending(&store).is_empty());
    assert!(store.list_files().expect("files removed").is_empty());
}

#[test]
fn empty_relationships_clear_previous_local_edges_and_pending_refs() {
    let temporary = tempfile::tempdir().expect("workspace");
    let store = open(temporary.path(), false);
    let mut source = snapshot(&store, "source.rs", &["first", "second"]);
    source
        .graph
        .edges
        .push(local_call(&source.entities[0], &source.entities[1]));
    source
        .graph
        .pending_refs
        .push(reference(&source.entities[0]));
    write(&store, &source);
    store
        .replace_file_with_graph(
            &source.file,
            &source.entities,
            &source.fragments,
            &FileGraph::default(),
        )
        .expect("replace with empty relationships");
    assert!(callers(&store, &source.entities[1]).is_empty());
    assert!(pending(&store).is_empty());
    assert_eq!(stored_entities(&store, &source.entities).len(), 2);
}

#[test]
fn failed_files_clear_their_graph_and_invalidate_incoming_references() {
    let temporary = tempfile::tempdir().expect("workspace");
    let store = open(temporary.path(), false);
    let mut target = snapshot(&store, "target.rs", &["first", "second"]);
    target
        .graph
        .edges
        .push(local_call(&target.entities[0], &target.entities[1]));
    let mut source = snapshot(&store, "source.rs", &["caller"]);
    source
        .graph
        .pending_refs
        .push(reference(&source.entities[0]));
    write(&store, &target);
    write(&store, &source);
    let ref_id = resolve(&store, &target.entities[1]);

    store
        .mark_file_failed(&target.file, "extraction failed")
        .expect("mark failed");
    assert!(stored_entities(&store, &target.entities).is_empty());
    assert!(callers(&store, &target.entities[1]).is_empty());
    assert_eq!(pending(&store)[0].id, ref_id);
    let files = store.list_files().expect("file states");
    assert!(matches!(
        &files.iter().find(|file| file.id == target.file.id).expect("failed file").index_status,
        FileIndexStatus::Failed { error } if error == "extraction failed"
    ));
}

#[test]
fn graph_and_entities_reopen_together_and_read_only_handles_reject_writes() {
    let temporary = tempfile::tempdir().expect("workspace");
    let store = open(temporary.path(), false);
    let mut source = snapshot(&store, "source.rs", &["first", "second"]);
    source
        .graph
        .edges
        .push(local_call(&source.entities[0], &source.entities[1]));
    write(&store, &source);
    store.close().expect("close writer");

    let reader = open(temporary.path(), true);
    assert_eq!(stored_entities(&reader, &source.entities).len(), 2);
    assert_eq!(callers(&reader, &source.entities[1]), source.graph.edges);
    assert!(
        reader
            .replace_file_with_graph(
                &source.file,
                &source.entities,
                &source.fragments,
                &source.graph
            )
            .is_err()
    );
    assert!(reader.delete_file(source.file.id).is_err());
    assert_eq!(callers(&reader, &source.entities[1]), source.graph.edges);
    reader.close().expect("close reader");
    IndexStore::delete(temporary.path()).expect("delete combined storage");
    assert!(!temporary.path().join("graph/graph.sqlite").exists());
}

#[test]
fn older_indexes_without_graph_open_read_only_without_creating_a_database() {
    let temporary = tempfile::tempdir().expect("workspace");
    let store = open(temporary.path(), false);
    let source = snapshot(&store, "source.rs", &["canonical record"]);
    write(&store, &source);
    store.close().expect("close writer");
    let path = temporary.path().join("graph/graph.sqlite");
    fs::remove_file(&path).expect("simulate pre-graph index");

    let reader = open(temporary.path(), true);
    assert!(
        reader
            .read(|state| Ok(state.graph.is_none()))
            .expect("optional graph")
    );
    assert_eq!(stored_entities(&reader, &source.entities).len(), 1);
    assert!(!path.exists());
    reader.close().expect("close reader");
    assert!(!path.exists());
}

#[test]
fn graph_ownership_is_validated_against_entities_before_either_store_changes() {
    let temporary = tempfile::tempdir().expect("workspace");
    let store = open(temporary.path(), false);
    let mut source = snapshot(&store, "source.rs", &["first", "second"]);
    source
        .graph
        .edges
        .push(local_call(&source.entities[0], &source.entities[1]));
    write(&store, &source);
    let files_before = store.list_files().expect("indexed state");
    let mut invalid = source.graph.clone();
    invalid.edges[0].target = "not a canonical entity".into();
    assert!(
        store
            .replace_file_with_graph(&source.file, &source.entities, &source.fragments, &invalid)
            .is_err()
    );
    invalid = source.graph.clone();
    invalid.edges[0].source = "foreign source".into();
    assert!(
        store
            .replace_file_with_graph(&source.file, &source.entities, &source.fragments, &invalid)
            .is_err()
    );
    invalid = source.graph.clone();
    let mut foreign_ref = reference(&source.entities[0]);
    foreign_ref.from_node_id = "foreign owner".into();
    invalid.pending_refs.push(foreign_ref);
    assert!(
        store
            .replace_file_with_graph(&source.file, &source.entities, &source.fragments, &invalid)
            .is_err()
    );

    assert_eq!(store.list_files().expect("unchanged status"), files_before);
    assert_eq!(stored_entities(&store, &source.entities).len(), 2);
    assert_eq!(callers(&store, &source.entities[1]), source.graph.edges);
}

#[test]
fn sqlite_insert_failure_preserves_entities_and_retries_after_reopen() {
    let temporary = tempfile::tempdir().expect("workspace");
    let store = open(temporary.path(), false);
    let mut original = snapshot(&store, "source.rs", &["old first", "old second"]);
    original
        .graph
        .edges
        .push(local_call(&original.entities[0], &original.entities[1]));
    write(&store, &original);
    let mut replacement = snapshot(&store, "source.rs", &["new first", "new second"]);
    replacement.graph.edges.push(local_call(
        &replacement.entities[0],
        &replacement.entities[1],
    ));
    let path = temporary.path().join("graph/graph.sqlite");
    let database = Connection::open(&path).expect("inject SQLite failure");
    database.execute_batch(
        "CREATE TRIGGER reject_insert BEFORE INSERT ON edges BEGIN SELECT RAISE(ABORT, 'test insert failure'); END;",
    ).expect("create failure trigger");

    assert!(
        store
            .replace_file_with_graph(
                &replacement.file,
                &replacement.entities,
                &replacement.fragments,
                &replacement.graph
            )
            .is_err()
    );
    assert_eq!(stored_entities(&store, &original.entities).len(), 2);
    assert!(stored_entities(&store, &replacement.entities).is_empty());
    assert_eq!(callers(&store, &original.entities[1]), original.graph.edges);
    store.close().expect("persist retry state");
    drop(database);

    let reopened = open(temporary.path(), false);
    assert_eq!(
        reopened.list_files().expect("interrupted file")[0].index_status,
        FileIndexStatus::NotIndexed
    );
    Connection::open(&path)
        .expect("remove failure")
        .execute_batch("DROP TRIGGER reject_insert;")
        .expect("drop trigger");
    write(&reopened, &replacement);
    assert!(stored_entities(&reopened, &original.entities).is_empty());
    assert!(callers(&reopened, &original.entities[1]).is_empty());
    assert_eq!(
        callers(&reopened, &replacement.entities[1]),
        replacement.graph.edges
    );
    assert!(
        reopened.list_files().expect("completed retry")[0]
            .index_status
            .is_indexed()
    );
}

#[test]
fn sqlite_invalidation_failure_keeps_old_ids_available_for_delete_retry() {
    let temporary = tempfile::tempdir().expect("workspace");
    let store = open(temporary.path(), false);
    let target = snapshot(&store, "target.rs", &["target"]);
    let mut source = snapshot(&store, "source.rs", &["caller"]);
    source
        .graph
        .pending_refs
        .push(reference(&source.entities[0]));
    write(&store, &target);
    write(&store, &source);
    let ref_id = resolve(&store, &target.entities[0]);
    let path = temporary.path().join("graph/graph.sqlite");
    let database = Connection::open(&path).expect("inject SQLite failure");
    database.execute_batch(
        "CREATE TRIGGER reject_invalidation BEFORE UPDATE OF target ON edges BEGIN SELECT RAISE(ABORT, 'test update failure'); END;",
    ).expect("create failure trigger");

    assert!(store.delete_file(target.file.id).is_err());
    assert_eq!(stored_entities(&store, &target.entities).len(), 1);
    assert_eq!(callers(&store, &target.entities[0]).len(), 1);
    assert!(pending(&store).is_empty());
    store.close().expect("persist deletion retry");
    drop(database);

    let reopened = open(temporary.path(), false);
    let files = reopened.list_files().expect("interrupted deletion");
    assert_eq!(
        files
            .iter()
            .find(|file| file.id == target.file.id)
            .expect("target retained")
            .index_status,
        FileIndexStatus::Deleting
    );
    Connection::open(&path)
        .expect("remove failure")
        .execute_batch("DROP TRIGGER reject_invalidation;")
        .expect("drop trigger");
    reopened
        .delete_file(target.file.id)
        .expect("retry deletion");
    assert!(stored_entities(&reopened, &target.entities).is_empty());
    assert!(callers(&reopened, &target.entities[0]).is_empty());
    assert_eq!(pending(&reopened)[0].id, ref_id);
}

#[test]
fn retry_replaces_a_committed_graph_when_canonical_entities_are_still_old() {
    let temporary = tempfile::tempdir().expect("workspace");
    let store = open(temporary.path(), false);
    let original = snapshot(&store, "target.rs", &["old first", "old second"]);
    let mut source = snapshot(&store, "source.rs", &["caller"]);
    source
        .graph
        .pending_refs
        .push(reference(&source.entities[0]));
    write(&store, &original);
    write(&store, &source);
    let ref_id = resolve(&store, &original.entities[1]);
    let mut interrupted = snapshot(
        &store,
        "target.rs",
        &["interrupted first", "interrupted second"],
    );
    interrupted.graph.edges.push(local_call(
        &interrupted.entities[0],
        &interrupted.entities[1],
    ));

    // Persist exactly the state before zvec replacement, then release every handle.
    store
        .write(|state| {
            let directories = state.directories.ensure(&interrupted.file.relative_path)?;
            state.files.put(&interrupted.file, &directories)?;
            state.files.flush()?;
            state
                .graph
                .as_mut()
                .expect("graph is open")
                .write_file_graph(
                    original.file.id.get(),
                    "target.rs",
                    &interrupted.graph,
                    &state
                        .entities
                        .list_ids(original.file.id)?
                        .into_iter()
                        .map(|id| id.as_str().to_owned())
                        .collect::<Vec<_>>(),
                )
                .map_err(graph_error)
        })
        .expect("commit graph with durable unfinished status");
    store.close().expect("close interrupted write");

    let reopened = open(temporary.path(), false);
    assert_eq!(stored_entities(&reopened, &original.entities).len(), 2);
    assert!(stored_entities(&reopened, &interrupted.entities).is_empty());
    assert_eq!(
        callers(&reopened, &interrupted.entities[1]),
        interrupted.graph.edges
    );
    let mut replacement = snapshot(&reopened, "target.rs", &["final first", "final second"]);
    replacement.graph.edges.push(local_call(
        &replacement.entities[0],
        &replacement.entities[1],
    ));
    write(&reopened, &replacement);

    assert!(stored_entities(&reopened, &original.entities).is_empty());
    assert!(callers(&reopened, &interrupted.entities[1]).is_empty());
    assert_eq!(stored_entities(&reopened, &replacement.entities).len(), 2);
    assert_eq!(
        callers(&reopened, &replacement.entities[1]),
        replacement.graph.edges
    );
    assert_eq!(pending(&reopened)[0].id, ref_id);
}

#[test]
fn resolution_session_remains_available_while_unfinished_files_are_hidden() {
    let temporary = tempfile::tempdir().expect("workspace");
    let store = open(temporary.path(), false);
    let source = snapshot(&store, "source.rs", &["caller"]);
    write(&store, &source);
    store
        .write(|state| {
            let directories = state.directories.ensure(&source.file.relative_path)?;
            state.files.put(&source.file, &directories)?;
            state.files.flush()
        })
        .expect("persist interrupted status");
    store
        .with_resolution_session(|session| {
            assert_eq!(session.file_path(source.file.id.get())?, None);
            assert_eq!(session.file_id_by_path("source.rs")?, None);
            Ok(())
        })
        .expect("unrelated resolution need not wait for this file");
    write(&store, &source);
    store
        .with_resolution_session(|_| Ok(()))
        .expect("resolve recovered snapshot");
}

#[test]
fn process_exit_during_canonical_replacement_recovers_from_durable_status() {
    const CHILD_ROOT: &str = "ZG_TEST_GRAPH_CRASH_ROOT";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let store = open(Path::new(&root), false);
        let original = snapshot(&store, "target.rs", &["old first", "old second"]);
        let mut source = snapshot(&store, "source.rs", &["caller"]);
        source
            .graph
            .pending_refs
            .push(reference(&source.entities[0]));
        write(&store, &original);
        write(&store, &source);
        resolve(&store, &original.entities[1]);
        let mut interrupted = snapshot(&store, "target.rs", &["partial first", "partial second"]);
        interrupted.graph.edges.push(local_call(
            &interrupted.entities[0],
            &interrupted.entities[1],
        ));
        store
            .write(|state| {
                let directories = state.directories.ensure(&interrupted.file.relative_path)?;
                state.files.put(&interrupted.file, &directories)?;
                state.directories.flush()?;
                state.files.flush()?;
                let old_ids = state
                    .entities
                    .list_ids(original.file.id)?
                    .into_iter()
                    .map(|id| id.as_str().to_owned())
                    .collect::<Vec<_>>();
                state
                    .graph
                    .as_mut()
                    .unwrap()
                    .write_file_graph(
                        original.file.id.get(),
                        "target.rs",
                        &interrupted.graph,
                        &old_ids,
                    )
                    .map_err(graph_error)?;
                state.entities.delete_file(original.file.id)?;
                state
                    .entities
                    .write(&Entities::prepare(&interrupted.entities[..1])?)?;
                state.entities.flush()
            })
            .expect("persist graph and only part of the canonical replacement");
        // Skip IndexStore::close and all Rust destructors, leaving SQLite's WAL
        // and zvec's partially replaced content for a fresh process to reopen.
        std::process::exit(77);
    }
    let temporary = tempfile::tempdir().expect("workspace");
    let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "storage::store::graph_tests::process_exit_during_canonical_replacement_recovers_from_durable_status", "--nocapture"])
        .env(CHILD_ROOT, temporary.path())
        .output().expect("run interrupted writer");
    assert_eq!(
        status.status.code(),
        Some(77),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let store = open(temporary.path(), false);
    let target = store
        .list_files()
        .expect("durable status")
        .into_iter()
        .find(|file| file.relative_path.to_path_buf() == PathBuf::from("target.rs"))
        .unwrap();
    assert_eq!(target.index_status, FileIndexStatus::NotIndexed);
    let interrupted = snapshot(&store, "target.rs", &["partial first", "partial second"]);
    assert_eq!(stored_entities(&store, &interrupted.entities).len(), 1);
    assert!(
        store
            .neighborhood(interrupted.entities[0].id.as_str(), Direction::Out, None)
            .unwrap()
            .is_empty()
    );
    let replacement = snapshot(&store, "target.rs", &["final target"]);
    write(&store, &replacement);
    assert!(stored_entities(&store, &interrupted.entities).is_empty());
    assert_eq!(stored_entities(&store, &replacement.entities).len(), 1);
    assert_eq!(
        pending(&store).len(),
        1,
        "incoming reference survives for re-resolution"
    );
    store
        .with_resolution_session(|_| Ok(()))
        .expect("recovered graph may be resolved");
}

mod resolution_tests;
