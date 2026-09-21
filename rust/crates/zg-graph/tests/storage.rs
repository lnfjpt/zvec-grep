use std::path::Path;

use rusqlite::Connection;
use serde_json::json;
use zg_graph::persistence::{
    Direction, Edge, EdgeKind, Error, FileGraph, Metadata, OpenMode, PendingRef, Provenance,
    RefKind, Resolution, ResolutionStats, SqliteGraphStorage,
};

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
        owner_id: owner.into(),
        ref_name: name.into(),
        receiver_name: Some("module".into()),
        ref_kind: RefKind::Calls,
        arity: Some(2),
        line: 3,
        column: 4,
        metadata: Metadata::from_iter([("raw".into(), json!("module.target(x, y)"))]),
    }
}

fn graph(ids: &[&str], edges: Vec<Edge>, refs: Vec<PendingRef>) -> FileGraph {
    FileGraph {
        entity_ids: ids.iter().map(|id| (*id).into()).collect(),
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

#[test]
fn local_queries_preserve_direction_kinds_order_metadata_and_call_sites() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    let edges = vec![
        edge(EdgeKind::Calls, "a", "b", 1),
        edge(EdgeKind::Calls, "a", "b", 2),
        edge(EdgeKind::Calls, "b", "b", 3),
        edge(EdgeKind::Contains, "file", "a", 1),
        edge(EdgeKind::Imports, "a", "b", 4),
        edge(EdgeKind::Extends, "a", "b", 5),
        edge(EdgeKind::Implements, "a", "b", 6),
    ];
    db.write_file_graph(
        "file",
        &graph(&["a", "b"], edges.clone(), vec![reference("a", "external")]),
        &[],
    )
    .expect("write");
    assert_eq!(db.get_callers("b").expect("callers"), edges[..3]);
    assert_eq!(db.get_callees("a").expect("callees"), edges[..2]);
    assert!(db.get_callers("a").expect("no incoming").is_empty());
    assert!(db.get_callees("missing").expect("unknown").is_empty());
    assert!(db.get_callers(" ").is_err());
}

#[test]
fn replacement_and_repeated_deletion_remove_only_owned_rows() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    db.write_file_graph(
        "f1",
        &graph(
            &["a", "b"],
            vec![edge(EdgeKind::Calls, "a", "b", 1)],
            vec![reference("a", "x")],
        ),
        &[],
    )
    .expect("first");
    let other = edge(EdgeKind::Calls, "x", "y", 2);
    db.write_file_graph("f2", &graph(&["x", "y"], vec![other.clone()], vec![]), &[])
        .expect("second");
    let new = edge(EdgeKind::Calls, "c", "d", 3);
    db.write_file_graph(
        "f1",
        &graph(&["c", "d"], vec![new.clone()], vec![]),
        &["a".into(), "b".into()],
    )
    .expect("replace");
    assert!(db.get_callers("b").expect("old edge gone").is_empty());
    assert!(
        db.list_pending_refs(100, 0)
            .expect("old ref gone")
            .refs
            .is_empty()
    );
    assert_eq!(db.get_callees("c").expect("new edge"), vec![new]);
    for _ in 0..2 {
        db.delete_file_graph("f1", &["c".into(), "d".into(), "c".into()])
            .expect("delete");
    }
    assert_eq!(
        db.get_callees("x").expect("other file unchanged"),
        vec![other]
    );
    assert!(db.get_callees("c").expect("deleted").is_empty());
}

#[test]
fn cross_file_resolution_is_idempotent_and_invalidation_requeues_refs() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    let local = edge(EdgeKind::Calls, "a", "b", 1);
    db.write_file_graph(
        "caller",
        &graph(
            &["a", "b"],
            vec![local.clone()],
            vec![reference("a", "remote")],
        ),
        &[],
    )
    .expect("caller");
    db.write_file_graph("target", &graph(&["remote"], vec![], vec![]), &[])
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
    let incoming = db.get_callers("remote").expect("cross edge");
    assert_eq!(incoming.len(), 1);
    assert_eq!(incoming[0].metadata, reference("a", "remote").metadata);
    assert_eq!(incoming[0].line, Some(3));
    db.delete_file_graph("target", &["remote".into()])
        .expect("invalidate");
    assert!(db.get_callers("remote").expect("removed").is_empty());
    assert_eq!(db.get_callees("a").expect("local retained"), vec![local]);
    let renewed = proposal(&db, "replacement");
    assert_eq!(renewed.ref_id, resolution.ref_id);
    assert_eq!(
        db.apply_resolutions(&[renewed])
            .expect("resolve again")
            .resolved,
        1
    );
    db.delete_file_graph("caller", &["a".into(), "b".into()])
        .expect("delete owner");
    assert!(db.get_callers("replacement").expect("cascade").is_empty());
}

#[test]
fn replacing_a_target_with_the_same_id_invalidates_resolutions() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    db.write_file_graph(
        "caller",
        &graph(&["a"], vec![], vec![reference("a", "b")]),
        &[],
    )
    .expect("caller");
    db.write_file_graph("target", &graph(&["b"], vec![], vec![]), &[])
        .expect("target");
    let resolution = proposal(&db, "b");
    db.apply_resolutions(&[resolution]).expect("resolve");
    db.write_file_graph("target", &graph(&["b"], vec![], vec![]), &["b".into()])
        .expect("replace");
    assert!(db.get_callers("b").expect("invalidated").is_empty());
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
            "SELECT count(*) FROM edges WHERE kind = 'imports' AND file_id = 'source-file'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("import count")
    };
    let mut import = reference("source-file", "module");
    import.ref_kind = RefKind::Imports;
    db.write_file_graph("source-file", &graph(&[], vec![], vec![import]), &[])
        .expect("import");
    let resolution = proposal(&db, "target-file");
    db.apply_resolutions(&[resolution]).expect("resolve");
    assert_eq!(import_count(), 1);
    db.delete_file_graph("target-file", &[])
        .expect("delete empty target");
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
    db.write_file_graph("file", &graph(&["a", "b"], edges, refs), &[])
        .expect("large graph");
    assert_eq!(db.get_callees("a").expect("all edges").len(), 1100);
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
        "source",
        &graph(&["owner"], vec![], vec![reference("owner", "last")]),
        &[],
    )
    .expect("source");
    let ids: Vec<String> = (0..1600).map(|i| format!("entity-{i}")).collect();
    db.write_file_graph(
        "target",
        &FileGraph {
            entity_ids: ids.clone(),
            ..FileGraph::default()
        },
        &[],
    )
    .expect("target");
    db.apply_resolutions(&[proposal(&db, "entity-1599")])
        .expect("resolve");
    db.delete_file_graph("target", &ids)
        .expect("delete all chunks");
    assert!(
        db.get_callers("entity-1599")
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
        "file",
        &graph(&["a", "b"], vec![original.clone()], vec![]),
        &[],
    )
    .expect("write");
    let mut nonlocal = original.clone();
    nonlocal.provenance = Provenance::WorkspaceUnique;
    for invalid in [
        graph(&["a", "a"], vec![], vec![]),
        graph(&["file"], vec![], vec![]),
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
            db.write_file_graph("file", &invalid, &["a".into(), "b".into()])
                .is_err()
        );
        assert_eq!(
            db.get_callees("a").expect("original intact"),
            vec![original.clone()]
        );
    }
    assert!(db.delete_file_graph("file", &[String::new()]).is_err());
    assert!(db.delete_file_graph(" ", &[]).is_err());
}

#[test]
fn sql_failure_rolls_back_replacement_and_invalidation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("graph.sqlite");
    let mut db = open(&path);
    db.write_file_graph(
        "source",
        &graph(&["a"], vec![], vec![reference("a", "b")]),
        &[],
    )
    .expect("source");
    db.write_file_graph("target", &graph(&["b"], vec![], vec![]), &[])
        .expect("target");
    db.apply_resolutions(&[proposal(&db, "b")])
        .expect("resolve");
    let raw = Connection::open(&path).expect("raw");
    raw.execute_batch("CREATE TRIGGER fail_insert BEFORE INSERT ON edges BEGIN SELECT RAISE(ABORT, 'injected'); END;").expect("trigger");
    assert!(
        db.write_file_graph(
            "target",
            &graph(&["b"], vec![edge(EdgeKind::Calls, "b", "b", 1)], vec![]),
            &["b".into()]
        )
        .is_err()
    );
    assert_eq!(db.get_callers("b").expect("incoming restored").len(), 1);
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
        "file",
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
        "CREATE TRIGGER fail_second BEFORE INSERT ON edges WHEN NEW.target = 'target-1' BEGIN SELECT RAISE(ABORT, 'injected'); END;"
    ).expect("trigger");
    assert!(db.apply_resolutions(&proposals).is_err());
    assert!(db.get_callees("a").expect("no partial edge").is_empty());
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
        "file",
        &graph(&["a'\"\\", "b"], vec![expected.clone()], vec![]),
        &[],
    )
    .expect("escaped ids");
    db.close().expect("close writer");
    let mut reader = SqliteGraphStorage::open(&path, OpenMode::ReadOnly).expect("read-only");
    assert_eq!(reader.get_callers("b").expect("persisted"), vec![expected]);
    assert!(
        reader
            .get_callers("' OR 1=1 --")
            .expect("bound parameter")
            .is_empty()
    );
    assert!(reader.delete_file_graph("file", &["b".into()]).is_err());
    assert_eq!(reader.get_callers("b").expect("unchanged").len(), 1);
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
fn schema_contains_only_edges_and_pending_refs_and_enforces_json_objects() {
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
    assert_eq!(names, ["edges", "pending_refs"]);
    assert!(raw.execute("INSERT INTO edges (file_id, kind, source, target, provenance, metadata) VALUES ('f', 'calls', 'a', 'b', 'file_local', '[]')", []).is_err());
    assert!(raw.execute("INSERT INTO edges (file_id, kind, source, target, provenance, line) VALUES ('f', 'calls', 'a', 'b', 'file_local', 0)", []).is_err());
}

#[test]
fn invalid_and_stale_resolution_proposals_do_not_mutate_pending_refs() {
    let mut db = SqliteGraphStorage::in_memory().expect("open");
    db.write_file_graph("f", &graph(&["a"], vec![], vec![reference("a", "b")]), &[])
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
        assert!(db.get_callees("a").expect("no edge").is_empty());
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
        .write_file_graph("f", &graph(&["a"], vec![], vec![reference("a", "b")]), &[])
        .expect("write");
    let mut resolver = open(&path);
    let reader = SqliteGraphStorage::open(&path, OpenMode::ReadOnly).expect("reader");
    let old = proposal(&resolver, "b");
    writer
        .delete_file_graph("f", &["a".into()])
        .expect("delete");
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
        edge(EdgeKind::Calls, "a", "b", 2),
        edge(EdgeKind::Calls, "b", "b", 3),
        edge(EdgeKind::Imports, "b", "a", 4),
        edge(EdgeKind::Contains, "file", "b", 5),
        edge(EdgeKind::Extends, "a", "c", 6),
    ];
    db.write_file_graph(
        "file",
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
        db.neighborhood("file", Direction::Out, None)
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
    db.write_file_graph("file", &graph(&["a", "b"], edges.clone(), vec![]), &[])
        .expect("write");
    assert_eq!(
        db.neighborhood("a", Direction::Both, None)
            .expect("all edges"),
        edges
    );
}
