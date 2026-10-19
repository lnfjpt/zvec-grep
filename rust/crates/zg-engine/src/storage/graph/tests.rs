use std::path::Path;

use super::{
    Direction, Edge, EdgeKind, EntityRecord, Error, FileGraph, Metadata, OpenMode, PendingRef,
    Provenance, Resolution, ResolutionStats, SqliteGraphStorage,
};
use rusqlite::Connection;
use serde_json::json;

fn edge(kind: EdgeKind, source: &str, target: &str, line: u32) -> Edge {
    Edge {
        kind,
        source: source.into(),
        target: target.into(),
        line: Some(line),
        column: Some(0),
        provenance: Provenance::FileLocal,
        metadata: Metadata::from_iter([("nested".into(), json!({"evidence": [true, "中文", 7]}))]),
    }
}

fn reference(owner: &str, name: &str) -> PendingRef {
    PendingRef {
        from_node_id: owner.into(),
        reference_name: name.into(),
        receiver_name: Some("module".into()),
        reference_kind: EdgeKind::Calls,
        arity: Some(2),
        candidates: None,
        language: "rust".into(),
        name_tail: name.rsplit("::").next().unwrap_or(name).into(),
        line: 3,
        col: 4,
        metadata: Metadata::from_iter([("raw".into(), json!("module.target(x, y)"))]),
    }
}

fn import_reference(owner: &str, specifier: &str) -> PendingRef {
    PendingRef {
        receiver_name: None,
        reference_kind: EdgeKind::Imports,
        arity: None,
        language: "typescript".into(),
        line: 1,
        col: 0,
        metadata: Metadata::new(),
        ..reference(owner, specifier)
    }
}

fn graph(ids: &[&str], edges: Vec<Edge>, refs: Vec<PendingRef>) -> FileGraph {
    FileGraph {
        entity_ids: ids.iter().map(|id| (*id).into()).collect(),
        entities: Vec::new(),
        edges,
        pending_refs: refs,
    }
}

fn proposal(storage: &SqliteGraphStorage, target: &str) -> Resolution {
    let pending = storage.list_pending_refs(100, 0).expect("read refs");
    let stored = &pending.refs[0];
    Resolution {
        ref_id: stored.id,
        target_id: target.into(),
        provenance: Provenance::ImportScoped,
    }
}

fn open(path: &Path) -> SqliteGraphStorage {
    SqliteGraphStorage::open(path, OpenMode::ReadWrite).expect("open writer")
}

fn record(entity_id: &str, name: &str) -> EntityRecord {
    EntityRecord {
        entity_id: entity_id.into(),
        name: name.into(),
        qualified_name: format!("scope::{name}"),
        is_exported: true,
        arity: Some(1),
        is_declaration: false,
    }
}

#[test]
fn name_index_follows_file_snapshots() {
    let mut storage = SqliteGraphStorage::in_memory().expect("graph opens");

    let mut first = graph(&["entity-1", "entity-2"], Vec::new(), Vec::new());
    first.entities = vec![record("entity-1", "helper"), record("entity-2", "helper")];
    storage
        .write_file_graph(1, "file-1.ts", &first, &[])
        .expect("first write");
    let mut second = graph(&["entity-3"], Vec::new(), Vec::new());
    second.entities = vec![record("entity-3", "helper")];
    storage
        .write_file_graph(2, "file-2.ts", &second, &[])
        .expect("second write");

    let hits = storage.lookup_by_name("helper").expect("name lookup");
    assert_eq!(hits.len(), 3);
    assert!(hits.iter().all(|hit| hit.is_exported));
    assert!(hits.iter().all(|hit| hit.qualified_name == "scope::helper"));
    assert_eq!(storage.lookup_unique("helper").expect("unique"), None);
    assert!(
        storage
            .lookup_by_name("never-indexed")
            .expect("name lookup")
            .is_empty()
    );

    // Rewriting a file replaces its name records in the same snapshot.
    let mut rewrite = graph(&["entity-3"], Vec::new(), Vec::new());
    rewrite.entities = vec![record("entity-3", "solo")];
    storage
        .write_file_graph(2, "file-2.ts", &rewrite, &["entity-3".to_owned()])
        .expect("rewrite");
    let solo = storage
        .lookup_unique("solo")
        .expect("unique")
        .expect("single hit");
    assert_eq!(solo.entity_id, "entity-3");
    assert_eq!(solo.file_id, 2);
    assert_eq!(
        storage.lookup_by_name("helper").expect("name lookup").len(),
        2
    );

    storage
        .delete_file_graph(2, &["entity-3".to_owned()])
        .expect("delete");
    assert!(
        storage
            .lookup_by_name("solo")
            .expect("name lookup")
            .is_empty()
    );

    // Entity records must reference owned snapshot IDs and non-blank names.
    let mut foreign = graph(&["entity-9"], Vec::new(), Vec::new());
    foreign.entities = vec![record("missing", "ghost")];
    assert!(
        storage
            .write_file_graph(3, "file-3.ts", &foreign, &[])
            .is_err()
    );
    let mut duplicate = graph(&["entity-9"], Vec::new(), Vec::new());
    duplicate.entities = vec![record("entity-9", "twin"), record("entity-9", "twin")];
    assert!(
        storage
            .write_file_graph(3, "file-3.ts", &duplicate, &[])
            .is_err()
    );
    let mut blank = graph(&["entity-9"], Vec::new(), Vec::new());
    blank.entities = vec![EntityRecord {
        entity_id: "entity-9".into(),
        name: "  ".into(),
        qualified_name: String::new(),
        is_exported: false,
        arity: None,
        is_declaration: false,
    }];
    assert!(
        storage
            .write_file_graph(3, "file-3.ts", &blank, &[])
            .is_err()
    );
    assert!(matches!(
        storage.lookup_by_name(""),
        Err(Error::InvalidInput(_))
    ));
}

#[test]
fn local_queries_preserve_direction_kinds_order_metadata_and_call_sites() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    let edges = vec![
        edge(EdgeKind::Calls, "a", "b", 1),
        edge(EdgeKind::Calls, "a", "b", 2),
        edge(EdgeKind::Calls, "b", "b", 3),
        edge(EdgeKind::Contains, "f1", "a", 1),
        edge(EdgeKind::Imports, "a", "b", 4),
        edge(EdgeKind::Extends, "a", "b", 5),
        edge(EdgeKind::Implements, "a", "b", 6),
    ];
    db.write_file_graph(
        1,
        "file-1.ts",
        &graph(&["a", "b"], edges.clone(), vec![reference("a", "external")]),
        &[],
    )
    .expect("write");
    assert_eq!(
        db.neighborhood("b", Direction::In, Some(&[EdgeKind::Calls]))
            .expect("callers"),
        edges[..3]
    );
    assert_eq!(
        db.neighborhood("a", Direction::Out, Some(&[EdgeKind::Calls]))
            .expect("callees"),
        edges[..2]
    );
    assert!(
        db.neighborhood("a", Direction::In, Some(&[EdgeKind::Calls]))
            .expect("no incoming")
            .is_empty()
    );
    assert!(
        db.neighborhood("missing", Direction::Out, Some(&[EdgeKind::Calls]))
            .expect("unknown")
            .is_empty()
    );
    assert!(
        db.neighborhood(" ", Direction::In, Some(&[EdgeKind::Calls]))
            .is_err()
    );
}

#[test]
fn replacement_and_repeated_deletion_remove_only_owned_rows() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    db.write_file_graph(
        2,
        "file-2.ts",
        &graph(
            &["a", "b"],
            vec![edge(EdgeKind::Calls, "a", "b", 1)],
            vec![reference("a", "x")],
        ),
        &[],
    )
    .expect("first");
    let other = edge(EdgeKind::Calls, "x", "y", 2);
    db.write_file_graph(
        3,
        "file-3.ts",
        &graph(&["x", "y"], vec![other.clone()], vec![]),
        &[],
    )
    .expect("second");
    let new = edge(EdgeKind::Calls, "c", "d", 3);
    db.write_file_graph(
        2,
        "file-2.ts",
        &graph(&["c", "d"], vec![new.clone()], vec![]),
        &["a".into(), "b".into()],
    )
    .expect("replace");
    assert!(
        db.neighborhood("b", Direction::In, Some(&[EdgeKind::Calls]))
            .expect("old edge gone")
            .is_empty()
    );
    assert!(
        db.list_pending_refs(100, 0)
            .expect("old ref gone")
            .refs
            .is_empty()
    );
    assert_eq!(
        db.neighborhood("c", Direction::Out, Some(&[EdgeKind::Calls]))
            .expect("new edge"),
        vec![new]
    );
    for _ in 0..2 {
        db.delete_file_graph(2, &["c".into(), "d".into(), "c".into()])
            .expect("delete");
    }
    assert_eq!(
        db.neighborhood("x", Direction::Out, Some(&[EdgeKind::Calls]))
            .expect("other file unchanged"),
        vec![other]
    );
    assert!(
        db.neighborhood("c", Direction::Out, Some(&[EdgeKind::Calls]))
            .expect("deleted")
            .is_empty()
    );
}

#[test]
fn cross_file_resolution_is_idempotent_and_invalidation_requeues_refs() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    let local = edge(EdgeKind::Calls, "a", "b", 1);
    db.write_file_graph(
        4,
        "file-4.ts",
        &graph(
            &["a", "b"],
            vec![local.clone()],
            vec![reference("a", "remote")],
        ),
        &[],
    )
    .expect("caller");
    db.write_file_graph(5, "file-5.ts", &graph(&["remote"], vec![], vec![]), &[])
        .expect("target");
    let resolution = proposal(&db, "remote");
    assert_eq!(
        db.apply_resolutions(std::slice::from_ref(&resolution))
            .expect("resolve"),
        ResolutionStats {
            resolved: 1,
            stale: 0
        }
    );
    assert_eq!(
        db.apply_resolutions(std::slice::from_ref(&resolution))
            .expect("repeat"),
        ResolutionStats {
            resolved: 0,
            stale: 1
        }
    );
    let incoming = db
        .neighborhood("remote", Direction::In, Some(&[EdgeKind::Calls]))
        .expect("cross edge");
    assert_eq!(incoming.len(), 1);
    assert_eq!(incoming[0].metadata, reference("a", "remote").metadata);
    assert_eq!(incoming[0].line, Some(3));
    db.delete_file_graph(5, &["remote".into()])
        .expect("invalidate");
    assert!(
        db.neighborhood("remote", Direction::In, Some(&[EdgeKind::Calls]))
            .expect("removed")
            .is_empty()
    );
    assert_eq!(
        db.neighborhood("a", Direction::Out, Some(&[EdgeKind::Calls]))
            .expect("local retained"),
        vec![local]
    );
    let renewed = proposal(&db, "replacement");
    assert_eq!(renewed.ref_id, resolution.ref_id);
    assert_eq!(
        db.apply_resolutions(&[renewed])
            .expect("resolve again")
            .resolved,
        1
    );
    db.delete_file_graph(4, &["a".into(), "b".into()])
        .expect("delete owner");
    assert!(
        db.neighborhood("replacement", Direction::In, Some(&[EdgeKind::Calls]))
            .expect("cascade")
            .is_empty()
    );
}

#[test]
fn replacing_a_target_with_the_same_id_invalidates_resolutions() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    db.write_file_graph(
        4,
        "file-4.ts",
        &graph(&["a"], vec![], vec![reference("a", "b")]),
        &[],
    )
    .expect("caller");
    db.write_file_graph(5, "file-5.ts", &graph(&["b"], vec![], vec![]), &[])
        .expect("target");
    let resolution = proposal(&db, "b");
    db.apply_resolutions(&[resolution]).expect("resolve");
    db.write_file_graph(
        5,
        "file-5.ts",
        &graph(&["b"], vec![], vec![]),
        &["b".into()],
    )
    .expect("replace");
    assert!(
        db.neighborhood("b", Direction::In, Some(&[EdgeKind::Calls]))
            .expect("invalidated")
            .is_empty()
    );
    assert_eq!(
        db.list_pending_refs(100, 0)
            .expect("pending again")
            .refs
            .len(),
        1
    );
}

#[test]
fn file_level_import_targets_are_invalidated_without_entity_ids() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("graph.sqlite");
    let mut db = open(&path);
    let raw = Connection::open(&path).expect("inspect persisted edges");
    let import_count = || {
        raw.query_row(
            "SELECT count(*) FROM edges WHERE kind = 'imports' AND target IS NOT NULL AND file_id = 6",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("import count")
    };
    let mut import = reference("f6", "module");
    import.reference_kind = EdgeKind::Imports;
    db.write_file_graph(6, "file-6.ts", &graph(&[], vec![], vec![import]), &[])
        .expect("import");
    let resolution = proposal(&db, "f7");
    db.apply_resolutions(&[resolution]).expect("resolve");
    assert_eq!(import_count(), 1);
    db.delete_file_graph(7, &[]).expect("delete empty target");
    assert_eq!(import_count(), 0);
    assert_eq!(db.list_pending_refs(100, 0).expect("pending").refs.len(), 1);
}

#[test]
fn pagination_is_pending_only_and_readers_do_not_truncate_edges() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    let refs = (0..1001)
        .map(|i| reference("a", &format!("target-{i}")))
        .collect();
    let edges = (1..=1100)
        .map(|i| edge(EdgeKind::Calls, "a", "b", i))
        .collect();
    db.write_file_graph(1, "file-1.ts", &graph(&["a", "b"], edges, refs), &[])
        .expect("large graph");
    assert_eq!(
        db.neighborhood("a", Direction::Out, Some(&[EdgeKind::Calls]))
            .expect("all edges")
            .len(),
        1100
    );
    let page = db.list_pending_refs(1000, 0).expect("page1");
    assert_eq!(page.refs.len(), 1000);
    let last = db
        .list_pending_refs(1000, page.next_cursor.expect("cursor"))
        .expect("page2");
    assert_eq!(last.refs.len(), 1);
    assert_eq!(last.next_cursor, None);
    let resolution = proposal(&db, "external");
    db.apply_resolutions(&[resolution]).expect("resolve one");
    assert_eq!(
        db.list_pending_refs(1000, 0)
            .expect("pending only")
            .refs
            .len(),
        1000
    );
    assert!(db.list_pending_refs(0, 0).is_err());
    assert!(db.list_pending_refs(1001, 0).is_err());
    assert!(db.list_pending_refs(1, -1).is_err());
}

#[test]
fn deletion_chunks_large_entity_lists() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    db.write_file_graph(
        8,
        "file-8.ts",
        &graph(&["owner"], vec![], vec![reference("owner", "last")]),
        &[],
    )
    .expect("source");
    let ids: Vec<String> = (0..1600).map(|i| format!("entity-{i}")).collect();
    db.write_file_graph(
        5,
        "file-5.ts",
        &FileGraph {
            entity_ids: ids.clone(),
            ..FileGraph::default()
        },
        &[],
    )
    .expect("target");
    db.apply_resolutions(&[proposal(&db, "entity-1599")])
        .expect("resolve");
    db.delete_file_graph(5, &ids).expect("delete all chunks");
    assert!(
        db.neighborhood("entity-1599", Direction::In, Some(&[EdgeKind::Calls]))
            .expect("last chunk removed")
            .is_empty()
    );
    assert_eq!(db.list_pending_refs(1, 0).expect("requeued").refs.len(), 1);
}

#[test]
fn ownership_validation_prevents_cross_file_snapshot_writes() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    let original = edge(EdgeKind::Calls, "a", "b", 1);
    db.write_file_graph(
        1,
        "file-1.ts",
        &graph(&["a", "b"], vec![original.clone()], vec![]),
        &[],
    )
    .expect("write");
    let mut nonlocal = original.clone();
    nonlocal.provenance = Provenance::WorkspaceUnique;
    for invalid in [
        graph(&["a", "a"], vec![], vec![]),
        graph(&["f1"], vec![], vec![]),
        graph(&[" "], vec![], vec![]),
        graph(
            &["a"],
            vec![edge(EdgeKind::Calls, "a", "external", 1)],
            vec![],
        ),
        graph(&["a", "b"], vec![nonlocal], vec![]),
        graph(
            &["a", "b"],
            vec![edge(EdgeKind::Calls, "a", "b", 0)],
            vec![],
        ),
        graph(&[], vec![], vec![reference("external", "b")]),
    ] {
        assert!(
            db.write_file_graph(1, "file-1.ts", &invalid, &["a".into(), "b".into()])
                .is_err()
        );
        assert_eq!(
            db.neighborhood("a", Direction::Out, Some(&[EdgeKind::Calls]))
                .expect("original intact"),
            vec![original.clone()]
        );
    }
    assert!(db.delete_file_graph(1, &[String::new()]).is_err());
}

#[test]
fn sql_failure_rolls_back_replacement_and_invalidation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("graph.sqlite");
    let mut db = open(&path);
    db.write_file_graph(
        8,
        "file-8.ts",
        &graph(&["a"], vec![], vec![reference("a", "b")]),
        &[],
    )
    .expect("source");
    db.write_file_graph(5, "file-5.ts", &graph(&["b"], vec![], vec![]), &[])
        .expect("target");
    db.apply_resolutions(&[proposal(&db, "b")])
        .expect("resolve");
    let raw = Connection::open(&path).expect("raw");
    raw.execute_batch("CREATE TRIGGER fail_insert BEFORE INSERT ON edges BEGIN SELECT RAISE(ABORT, 'injected'); END;").expect("trigger");
    assert!(
        db.write_file_graph(
            5,
            "file-5.ts",
            &graph(&["b"], vec![edge(EdgeKind::Calls, "b", "b", 1)], vec![]),
            &["b".into()]
        )
        .is_err()
    );
    assert_eq!(
        db.neighborhood("b", Direction::In, Some(&[EdgeKind::Calls]))
            .expect("incoming restored")
            .len(),
        1
    );
    assert!(
        db.list_pending_refs(100, 0)
            .expect("status restored")
            .refs
            .is_empty()
    );
}

#[test]
fn sql_failure_rolls_back_an_entire_resolution_batch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("graph.sqlite");
    let mut db = open(&path);
    db.write_file_graph(
        1,
        "file-1.ts",
        &graph(
            &["a"],
            vec![],
            vec![reference("a", "one"), reference("a", "two")],
        ),
        &[],
    )
    .expect("write");
    let refs = db.list_pending_refs(100, 0).expect("refs").refs;
    let proposals: Vec<_> = refs
        .iter()
        .enumerate()
        .map(|(i, stored)| Resolution {
            ref_id: stored.id,
            target_id: format!("target-{i}"),
            provenance: Provenance::WorkspaceUnique,
        })
        .collect();
    Connection::open(&path).expect("raw").execute_batch(
        "CREATE TRIGGER fail_second BEFORE UPDATE OF target ON edges WHEN NEW.target = 'target-1' BEGIN SELECT RAISE(ABORT, 'injected'); END;"
    ).expect("trigger");
    assert!(db.apply_resolutions(&proposals).is_err());
    assert!(
        db.neighborhood("a", Direction::Out, Some(&[EdgeKind::Calls]))
            .expect("no partial edge")
            .is_empty()
    );
    assert_eq!(
        db.list_pending_refs(100, 0).expect("pending retained").refs,
        refs
    );
}

#[test]
fn readonly_connections_reopen_data_and_never_create_or_write() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nested/graph.sqlite");
    assert!(SqliteGraphStorage::open(&path, OpenMode::ReadOnly).is_err());
    assert!(!path.exists());
    assert!(!path.parent().expect("parent").exists());
    let mut db = open(&path);
    let expected = edge(EdgeKind::Calls, "a'\"\\", "b", 1);
    db.write_file_graph(
        1,
        "file-1.ts",
        &graph(&["a'\"\\", "b"], vec![expected.clone()], vec![]),
        &[],
    )
    .expect("escaped ids");
    db.close().expect("close writer");
    let mut reader = SqliteGraphStorage::open(&path, OpenMode::ReadOnly).expect("read-only");
    assert_eq!(
        reader
            .neighborhood("b", Direction::In, Some(&[EdgeKind::Calls]))
            .expect("persisted"),
        vec![expected]
    );
    assert!(
        reader
            .neighborhood("' OR 1=1 --", Direction::In, Some(&[EdgeKind::Calls]))
            .expect("bound parameter")
            .is_empty()
    );
    assert!(reader.delete_file_graph(1, &["b".into()]).is_err());
    assert_eq!(
        reader
            .neighborhood("b", Direction::In, Some(&[EdgeKind::Calls]))
            .expect("unchanged")
            .len(),
        1
    );
    reader.close().expect("close reader");
}

#[test]
fn schemas_reject_foreign_and_newer_databases_without_rewriting_versions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let foreign = dir.path().join("foreign.sqlite");
    Connection::open(&foreign)
        .expect("foreign")
        .execute_batch("CREATE TABLE unrelated (id INTEGER);")
        .expect("table");
    assert!(matches!(
        SqliteGraphStorage::open(&foreign, OpenMode::ReadWrite),
        Err(Error::ForeignDatabase)
    ));
    let path = dir.path().join("newer.sqlite");
    open(&path).close().expect("init");
    let raw = Connection::open(&path).expect("raw");
    raw.pragma_update(None, "user_version", 999)
        .expect("new version");
    for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
        assert!(matches!(
            SqliteGraphStorage::open(&path, mode),
            Err(Error::UnsupportedSchema { version: 999, .. })
        ));
    }
    assert_eq!(
        raw.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("version"),
        999
    );
}

#[test]
fn schema_contains_edges_and_entities_and_enforces_json_objects() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("graph.sqlite");
    open(&path).close().expect("init");
    let raw = Connection::open(&path).expect("raw");
    let mut statement = raw
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
        .expect("tables");
    let names: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<rusqlite::Result<_>>()
        .expect("rows");
    assert_eq!(
        names,
        ["edges", "entities", "file_paths", "sqlite_sequence"]
    );
    assert!(raw.execute("INSERT INTO edges (file_id, kind, source, target, provenance, status, metadata) VALUES (1, 'calls', 'a', 'b', 'file_local', 'resolved', '[]')", []).is_err());
    assert!(raw.execute("INSERT INTO edges (file_id, kind, source, target, provenance, status, line) VALUES (1, 'calls', 'a', 'b', 'file_local', 'resolved', 0)", []).is_err());
    assert!(raw.execute("INSERT INTO entities (file_id, entity_id, name, qualified_name, is_exported) VALUES (1, 'a', 'a', 'a', 2)", []).is_err());
    assert!(
        raw.execute(
            "INSERT INTO file_paths (file_id, relative_path) VALUES (1, '  ')",
            []
        )
        .is_err()
    );
}

#[test]
fn file_paths_follow_snapshots_and_anchor_import_resolution() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    db.write_file_graph(4, "src/lib/helper.ts", &graph(&["a"], vec![], vec![]), &[])
        .expect("write");
    assert_eq!(
        db.file_path(4).expect("path"),
        Some("src/lib/helper.ts".to_owned())
    );
    assert_eq!(
        db.file_id_by_path("src/lib/helper.ts").expect("lookup"),
        Some(4)
    );
    assert_eq!(db.file_path(9).expect("absent"), None);
    assert_eq!(db.file_id_by_path("src/missing.ts").expect("missing"), None);
    assert!(matches!(
        db.file_id_by_path("  "),
        Err(Error::InvalidInput(_))
    ));

    // Rewrites replace the row; the path never points at a stale file ID.
    db.write_file_graph(5, "src/lib/helper.ts", &graph(&["a"], vec![], vec![]), &[])
        .expect("rewrite under a new file ID");
    assert_eq!(db.file_path(4).expect("old id"), None);
    assert_eq!(
        db.file_id_by_path("src/lib/helper.ts").expect("path owner"),
        Some(5)
    );

    // Deletion drops the path row with the snapshot.
    db.delete_file_graph(5, &["a".into()]).expect("delete");
    assert_eq!(db.file_id_by_path("src/lib/helper.ts").expect("gone"), None);
}

#[test]
fn record_candidates_is_atomic_and_skips_non_pending_rows() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    db.write_file_graph(
        9,
        "src/consumer.ts",
        &graph(
            &["a"],
            vec![],
            vec![reference("a", "one"), reference("a", "two")],
        ),
        &[],
    )
    .expect("write");
    let mut refs = db.list_pending_refs(100, 0).expect("refs").refs;
    let second = refs.remove(1);
    db.apply_resolutions(&[Resolution {
        ref_id: refs[0].id,
        target_id: "target".into(),
        provenance: Provenance::WorkspaceUnique,
    }])
    .expect("resolve first");

    let entries = vec![
        (refs[0].id, vec!["target".to_owned()]),
        (second.id, vec!["first".to_owned(), "second".to_owned()]),
    ];
    assert_eq!(db.record_candidates(&entries).expect("record"), 1);
    let pending = db.list_pending_refs(100, 0).expect("pending").refs;
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].reference.candidates,
        Some(vec!["first".to_owned(), "second".to_owned()])
    );

    assert!(db.record_candidates(&[(0, vec!["x".into()])]).is_err());
    assert!(db.record_candidates(&[(second.id, Vec::new())]).is_err());
    assert!(db.record_candidates(&[]).is_ok());
}

#[test]
fn imported_file_ids_merge_endpoint_and_entity_targets() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    let mut one = graph(&["first"], vec![], vec![]);
    one.entities = vec![record("first", "helper")];
    db.write_file_graph(1, "one.ts", &one, &[])
        .expect("write one");
    // An entity whose ID collides with the file-endpoint spelling: the
    // endpoint interpretation must win over the entity's owning file.
    let mut two = graph(&["f9"], vec![], vec![]);
    two.entities = vec![record("f9", "colliding")];
    db.write_file_graph(2, "two.ts", &two, &[])
        .expect("write two");
    db.write_file_graph(4, "four.ts", &graph(&["x"], vec![], vec![]), &[])
        .expect("write four");
    db.write_file_graph(
        9,
        "nine.ts",
        &graph(
            &["a"],
            vec![],
            vec![
                import_reference("f9", "./four"),
                import_reference("f9", "./one"),
                import_reference("f9", "./two"),
            ],
        ),
        &[],
    )
    .expect("write nine");
    let ids: Vec<i64> = db
        .list_pending_refs(10, 0)
        .expect("refs")
        .refs
        .iter()
        .map(|reference| reference.id)
        .collect();
    db.apply_resolutions(&[
        Resolution {
            ref_id: ids[0],
            target_id: "f4".into(),
            provenance: Provenance::ImportScoped,
        },
        Resolution {
            ref_id: ids[1],
            target_id: "first".into(),
            provenance: Provenance::WorkspaceUnique,
        },
        Resolution {
            ref_id: ids[2],
            target_id: "f9".into(),
            provenance: Provenance::ImportScoped,
        },
    ])
    .expect("resolve imports");

    // File endpoints from import-scoped targets, the owning file of the
    // entity target, and the endpoint reading of the colliding ID.
    assert_eq!(db.imported_file_ids(9).expect("scope"), vec![1, 4, 9]);
    assert!(
        db.imported_file_ids(4)
            .expect("file without imports")
            .is_empty()
    );
}

#[test]
fn invalid_and_stale_resolution_proposals_do_not_mutate_pending_refs() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    db.write_file_graph(
        9,
        "file-9.ts",
        &graph(&["a"], vec![], vec![reference("a", "b")]),
        &[],
    )
    .expect("write");
    let valid = proposal(&db, "b");
    for invalid in [
        Resolution {
            ref_id: 0,
            ..valid.clone()
        },
        Resolution {
            target_id: " ".into(),
            ..valid.clone()
        },
        Resolution {
            provenance: Provenance::FileLocal,
            ..valid.clone()
        },
    ] {
        assert!(db.apply_resolutions(&[valid.clone(), invalid]).is_err());
        assert_eq!(
            db.list_pending_refs(100, 0)
                .expect("not resolved")
                .refs
                .len(),
            1
        );
        assert!(
            db.neighborhood("a", Direction::Out, Some(&[EdgeKind::Calls]))
                .expect("no edge")
                .is_empty()
        );
    }
    let stale = Resolution {
        ref_id: i64::MAX,
        ..valid.clone()
    };
    assert_eq!(
        db.apply_resolutions(&[stale, valid]).expect("mixed batch"),
        ResolutionStats {
            stale: 1,
            resolved: 1
        }
    );
}

#[test]
fn independent_connections_observe_commits_and_skip_deleted_refs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("graph.sqlite");
    let mut writer = open(&path);
    writer
        .write_file_graph(
            9,
            "file-9.ts",
            &graph(&["a"], vec![], vec![reference("a", "b")]),
            &[],
        )
        .expect("write");
    let mut resolver = open(&path);
    let reader = SqliteGraphStorage::open(&path, OpenMode::ReadOnly).expect("reader");
    let old = proposal(&resolver, "b");
    writer.delete_file_graph(9, &["a".into()]).expect("delete");
    assert_eq!(resolver.apply_resolutions(&[old]).expect("stale").stale, 1);
    assert!(
        reader
            .list_pending_refs(100, 0)
            .expect("committed delete")
            .refs
            .is_empty()
    );
}

#[test]
fn neighborhood_filters_direction_and_kinds() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    let edges = vec![
        edge(EdgeKind::Calls, "a", "b", 1),
        // Equal payloads are distinct rows; Both only deduplicates by row ID.
        edge(EdgeKind::Calls, "a", "b", 1),
        edge(EdgeKind::Calls, "b", "b", 3),
        edge(EdgeKind::Imports, "b", "a", 4),
        edge(EdgeKind::Contains, "f1", "b", 5),
        edge(EdgeKind::Extends, "a", "c", 6),
    ];
    db.write_file_graph(
        1,
        "file-1.ts",
        &graph(
            &["a", "b", "c"],
            edges.clone(),
            vec![reference("b", "pending")],
        ),
        &[],
    )
    .expect("write");
    assert_eq!(
        db.neighborhood("b", Direction::Both, None).expect("both"),
        edges[..5]
    );
    assert_eq!(
        db.neighborhood("b", Direction::In, None).expect("in"),
        vec![
            edges[0].clone(),
            edges[1].clone(),
            edges[2].clone(),
            edges[4].clone()
        ]
    );
    assert_eq!(
        db.neighborhood("b", Direction::Out, None).expect("out"),
        edges[2..4]
    );
    assert_eq!(
        db.neighborhood(
            "b",
            Direction::Both,
            Some(&[EdgeKind::Imports, EdgeKind::Calls, EdgeKind::Calls])
        )
        .expect("kinds"),
        edges[..4]
    );
    assert!(
        db.neighborhood("b", Direction::Both, Some(&[]))
            .expect("empty kinds")
            .is_empty()
    );
    assert!(
        db.neighborhood("missing", Direction::Both, None)
            .expect("missing")
            .is_empty()
    );
    assert!(db.neighborhood(" ", Direction::Both, None).is_err());
    assert_eq!(
        db.neighborhood("f1", Direction::Out, None)
            .expect("file endpoint"),
        vec![edges[4].clone()]
    );
}

#[test]
fn neighborhood_returns_all_edges_without_a_limit() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    let edges: Vec<_> = (1..=1100)
        .map(|line| edge(EdgeKind::Calls, "a", "b", line))
        .collect();
    db.write_file_graph(
        1,
        "file-1.ts",
        &graph(&["a", "b"], edges.clone(), vec![]),
        &[],
    )
    .expect("write");
    assert_eq!(
        db.neighborhood("a", Direction::Both, None)
            .expect("all edges"),
        edges
    );
}

#[test]
fn graph_queries_skip_unrelated_resolved_edges() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let mut db = SqliteGraphStorage::in_memory().expect("open");
    let expected = edge(EdgeKind::Calls, "needle", "needle", 1);
    db.write_file_graph(
        1,
        "file-1.ts",
        &graph(&["needle"], vec![expected.clone()], vec![]),
        &[],
    )
    .expect("write one relevant edge");
    let steps = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&steps);
    db.connection.progress_handler(
        100,
        Some(move || {
            counter.fetch_add(100, Ordering::Relaxed);
            false
        }),
    );
    let mut previous = 0;
    for unrelated in [10_000, 100_000] {
        db.connection
            .execute(
                "WITH RECURSIVE n(x) AS (
                    VALUES(?1) UNION ALL SELECT x + 1 FROM n WHERE x < ?2
                 )
                 INSERT INTO edges (file_id, source, target, kind, status, provenance)
                 SELECT 2, 'source-' || x, 'target-' || x, 'calls', 'resolved', 'file_local'
                 FROM n",
                [previous + 1, unrelated],
            )
            .expect("populate unrelated edges");
        previous = unrelated;
        // Place the pending row after the unrelated resolved rows: a missing
        // pending index would make even this one-row queue scan through them.
        db.write_file_graph(
            3,
            "file-3.ts",
            &graph(&["waiting"], vec![], vec![reference("waiting", "external")]),
            &[],
        )
        .expect("write pending reference");
        steps.store(0, Ordering::Relaxed);
        let pending = db.list_pending_refs(1, 0).expect("read pending queue");
        assert_eq!(pending.refs.len(), 1);
        assert_eq!(pending.refs[0].reference.reference_name, "external");
        assert!(pending.next_cursor.is_none());
        let executed = steps.load(Ordering::Relaxed);
        assert!(
            executed < 1_000,
            "pending, unrelated={unrelated}: {executed} steps"
        );
        for direction in [Direction::In, Direction::Out, Direction::Both] {
            for kinds in [
                None,
                Some([EdgeKind::Calls].as_slice()),
                Some([EdgeKind::Calls, EdgeKind::Imports].as_slice()),
            ] {
                steps.store(0, Ordering::Relaxed);
                assert_eq!(
                    db.neighborhood("needle", direction, kinds)
                        .expect("query sparse neighborhood"),
                    std::slice::from_ref(&expected)
                );
                // Count VM instructions, not elapsed time. The old status-index
                // scan needs over 50,000 steps even with only 10,000 unrelated edges.
                let executed = steps.load(Ordering::Relaxed);
                assert!(
                    executed < 1_000,
                    "{direction:?}, kinds={kinds:?}, unrelated={unrelated}: {executed} steps"
                );
            }
        }
    }
}

#[test]
fn prior_schema_version_requires_rebuild_without_mutation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("old.sqlite");
    let raw = Connection::open(&path).expect("raw");
    raw.execute_batch("PRAGMA application_id = 1514623568; PRAGMA user_version = 1; CREATE TABLE pending_refs (id INTEGER PRIMARY KEY);").expect("old schema");
    for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
        assert!(matches!(
            SqliteGraphStorage::open(&path, mode),
            Err(Error::UnsupportedSchema { version: 1, .. })
        ));
    }
    assert_eq!(
        raw.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("version"),
        1
    );
    assert_eq!(
        raw.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'pending_refs'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .expect("old table"),
        1
    );
}

#[test]
fn resolution_retains_reference_context_and_invalidation_resets_candidates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("graph.sqlite");
    let mut db = open(&path);
    let mut original = reference("source", "module::target");
    original.candidates = Some(vec!["target".into(), "alternative".into()]);
    db.write_file_graph(
        6,
        "file-6.ts",
        &graph(&["source"], vec![], vec![original.clone()]),
        &[],
    )
    .expect("write");
    let stored = db.list_pending_refs(100, 0).expect("refs").refs.remove(0);
    assert_eq!(stored.reference, original);
    assert!(
        db.neighborhood("source", Direction::Out, Some(&[EdgeKind::Calls]))
            .expect("pending excluded")
            .is_empty()
    );
    db.apply_resolutions(&[proposal(&db, "target")])
        .expect("resolve");
    let raw = Connection::open(&path).expect("raw");
    let (status, candidates): (String, String) = raw
        .query_row(
            "SELECT status, candidates FROM edges WHERE id = ?",
            [stored.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("retained ref");
    assert_eq!(status, "resolved");
    assert_eq!(
        raw.query_row("SELECT count(*) FROM edges", [], |r| r.get::<_, i64>(0))
            .expect("one row"),
        1
    );
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&candidates).expect("json"),
        original.candidates.clone().expect("candidates")
    );
    let (source, target, ref_id): (String, String, i64) = raw
        .query_row("SELECT source, target, id FROM edges", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .expect("edge");
    assert_eq!(
        (source.as_str(), target.as_str(), ref_id),
        ("source", "target", stored.id)
    );
    db.delete_file_graph(7, &["target".into()])
        .expect("invalidate");
    let requeued = db
        .list_pending_refs(100, 0)
        .expect("requeued")
        .refs
        .remove(0);
    original.candidates = None;
    assert_eq!(requeued.id, stored.id);
    assert_eq!(requeued.reference, original);
    assert!(
        db.neighborhood("source", Direction::Out, Some(&[EdgeKind::Calls]))
            .expect("edge removed")
            .is_empty()
    );
    db.apply_resolutions(&[proposal(&db, "new-target")])
        .expect("resolve again");
    db.delete_file_graph(6, &["source".into()])
        .expect("delete source");
    assert_eq!(
        raw.query_row("SELECT count(*) FROM edges", [], |r| r.get::<_, i64>(0))
            .expect("count"),
        0
    );
}

#[test]
fn reference_schema_defaults_and_state_constraints() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("graph.sqlite");
    open(&path).close().expect("init");
    let raw = Connection::open(&path).expect("raw");
    raw.execute("INSERT INTO edges (source, file_id, reference_name, kind, line, col) VALUES ('a', 1, 'b', 'calls', 1, 0)", []).expect("defaults");
    let defaults: (String, String, String, Option<String>) = raw
        .query_row(
            "SELECT language, name_tail, status, candidates FROM edges",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("defaults");
    assert_eq!(
        defaults,
        ("unknown".into(), String::new(), "pending".into(), None)
    );
    for update in [
        "candidates = '{}'",
        "status = 'invalid'",
        "status = 'resolved'",
        "target = 'b'",
        "provenance = 'import_scoped'",
        "reference_name = NULL",
    ] {
        assert!(
            raw.execute(&format!("UPDATE edges SET {update}"), [])
                .is_err(),
            "{update}"
        );
    }
    raw.execute(
        "UPDATE edges SET status = 'resolved', target = 'b', provenance = 'import_scoped'",
        [],
    )
    .expect("resolved");
    assert!(
        raw.execute("UPDATE edges SET status = 'pending'", [])
            .is_err()
    );
    raw.execute(
        "UPDATE edges SET status = 'failed', target = NULL, provenance = NULL",
        [],
    )
    .expect("failed");
    let db = SqliteGraphStorage::open(&path, OpenMode::ReadOnly).expect("reader");
    assert!(
        db.list_pending_refs(100, 0)
            .expect("failed excluded")
            .refs
            .is_empty()
    );
    assert!(
        db.neighborhood("a", Direction::Out, Some(&[EdgeKind::Calls]))
            .expect("failed excluded")
            .is_empty()
    );
    raw.execute("UPDATE edges SET status = 'pending'", [])
        .expect("retry");
    assert_eq!(db.list_pending_refs(100, 0).expect("pending").refs.len(), 1);
}

#[test]
fn replaced_reference_ids_are_not_reused() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    let snapshot = graph(&["a"], vec![], vec![reference("a", "b")]);
    db.write_file_graph(1, "file-1.ts", &snapshot, &[])
        .expect("write");
    let old = proposal(&db, "b");
    db.write_file_graph(1, "file-1.ts", &snapshot, &["a".into()])
        .expect("replace");
    let new = proposal(&db, "b");
    assert!(new.ref_id > old.ref_id);
    assert_eq!(db.apply_resolutions(&[old]).expect("deleted ref").stale, 1);
    assert_eq!(
        db.apply_resolutions(&[new]).expect("current ref").resolved,
        1
    );
}

#[test]
fn file_ids_round_trip_as_u32_and_sqlite_rejects_out_of_range_values() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("graph.sqlite");
    let mut db = open(&path);
    for id in [0, u32::MAX] {
        let key = format!("f{id}");
        db.write_file_graph(
            id,
            &format!("file-{id}.ts"),
            &graph(&[], vec![], vec![reference(&key, "external")]),
            &[],
        )
        .expect("write boundary");
    }
    let refs = db.list_pending_refs(100, 0).expect("read").refs;
    assert_eq!(
        refs.iter().map(|r| r.file_id).collect::<Vec<_>>(),
        vec![0, u32::MAX]
    );
    let raw = Connection::open(&path).expect("raw");
    assert_eq!(
        raw.query_row(
            "SELECT count(*) FROM edges WHERE typeof(file_id) = 'integer'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .expect("integer storage"),
        2
    );
    for invalid in [-1_i64, i64::from(u32::MAX) + 1] {
        assert!(
            raw.execute("UPDATE edges SET file_id = ?", [invalid])
                .is_err()
        );
    }
    db.close().expect("close");
    let mut db = open(&path);
    assert_eq!(db.list_pending_refs(100, 0).expect("reopen").refs, refs);
    db.delete_file_graph(u32::MAX, &[]).expect("delete max");
    assert_eq!(
        db.list_pending_refs(100, 0).expect("remaining").refs[0].file_id,
        0
    );
}

#[test]
fn contains_references_use_the_same_kinds_as_resolved_edges() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    let mut pending = reference("owner", "member");
    pending.reference_kind = EdgeKind::Contains;
    db.write_file_graph(
        1,
        "file-1.ts",
        &graph(&["owner"], vec![], vec![pending.clone()]),
        &[],
    )
    .expect("write contains ref");
    assert_eq!(
        db.list_pending_refs(100, 0).expect("pending").refs[0].reference,
        pending
    );
    db.apply_resolutions(&[proposal(&db, "member")])
        .expect("resolve contains");
    let edges = db
        .neighborhood("owner", Direction::Out, Some(&[EdgeKind::Contains]))
        .expect("contains edges");
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].target, "member");
    assert!(
        db.neighborhood("owner", Direction::Out, Some(&[EdgeKind::Calls]))
            .expect("calls only")
            .is_empty()
    );
    db.delete_file_graph(2, &["member".into()])
        .expect("invalidate");
    assert_eq!(
        db.list_pending_refs(100, 0).expect("requeued").refs[0].reference,
        pending
    );
}
