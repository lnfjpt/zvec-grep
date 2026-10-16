use super::*;
use crate::storage::graph::EntityRecord;

fn status(store: &IndexStore, file: &FileRecord, status: FileIndexStatus) {
    let mut file = file.clone();
    file.index_status = status;
    store
        .write(|state| {
            let directories = state.directories.ensure(&file.relative_path)?;
            state.files.put(&file, &directories)?;
            state.files.flush()
        })
        .expect("set interrupted file state");
}

fn named(store: &IndexStore, path: &str) -> Snapshot {
    let mut target = snapshot(store, path, &["target"]);
    target.graph.entities.push(EntityRecord {
        entity_id: target.entities[0].id.as_str().into(),
        name: "target".into(),
        qualified_name: "target".into(),
        is_exported: true,
        arity: Some(0),
    });
    write(store, &target);
    target
}

#[test]
fn hidden_reference_pages_preserve_cursor_and_do_not_block_ready_files() {
    let temporary = tempfile::tempdir().unwrap();
    let store = open(temporary.path(), false);
    let mut hidden = snapshot(&store, "hidden.rs", &["hidden"]);
    for line in 1..=2 {
        let mut reference = reference(&hidden.entities[0]);
        reference.line = line;
        hidden.graph.pending_refs.push(reference);
    }
    write(&store, &hidden);
    let mut ready = snapshot(&store, "ready.rs", &["ready"]);
    ready.graph.pending_refs.push(reference(&ready.entities[0]));
    write(&store, &ready);
    let hidden_ids: Vec<_> = pending(&store)
        .into_iter()
        .filter(|r| r.file_id == hidden.file.id.get())
        .map(|r| r.id)
        .collect();
    status(&store, &hidden.file, FileIndexStatus::NotIndexed);
    store
        .with_resolution_session(|session| {
            let first = session.list_pending_refs(1, 0)?;
            assert!(first.refs.is_empty());
            let second = session.list_pending_refs(1, first.next_cursor.unwrap())?;
            assert!(second.refs.is_empty());
            let third = session.list_pending_refs(1, second.next_cursor.unwrap())?;
            assert_eq!(third.refs.len(), 1);
            assert_eq!(third.refs[0].file_id, ready.file.id.get());
            assert!(third.next_cursor.is_none());
            assert_eq!(session.apply_failures(&hidden_ids)?, 0);
            assert_eq!(session.apply_failures(&[third.refs[0].id])?, 1);
            Ok(())
        })
        .unwrap();
    assert_eq!(pending(&store).len(), 2, "hidden references stay retryable");
}

#[test]
fn incomplete_candidates_defer_uniqueness_and_ready_names_still_resolve() {
    let temporary = tempfile::tempdir().unwrap();
    let store = open(temporary.path(), false);
    let hidden = named(&store, "hidden.rs");
    let ready = named(&store, "ready.rs");
    status(&store, &hidden.file, FileIndexStatus::Deleting);
    store
        .with_resolution_session(|session| {
            assert!(
                session.lookup_candidates("target")?.is_none(),
                "one hidden candidate must not imply a unique ready target"
            );
            assert_eq!(session.file_id_by_path("hidden.rs")?, None);
            assert_eq!(session.file_path(hidden.file.id.get())?, None);
            assert!(
                session.path_has_module("hidden", &["rs"])?,
                "unfinished is not external"
            );
            assert_eq!(
                session.file_id_by_path("ready.rs")?,
                Some(ready.file.id.get())
            );
            Ok(())
        })
        .unwrap();
    store.delete_file(hidden.file.id).unwrap();
    store
        .with_resolution_session(|session| {
            let candidates = session.lookup_candidates("target")?.unwrap();
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0].entity_id, ready.entities[0].id.as_str());
            Ok(())
        })
        .unwrap();
}

#[test]
fn writeback_requires_issued_references_and_visible_canonical_targets() {
    let temporary = tempfile::tempdir().unwrap();
    let store = open(temporary.path(), false);
    let target = named(&store, "target.rs");
    let hidden = named(&store, "hidden.rs");
    status(&store, &hidden.file, FileIndexStatus::NotIndexed);
    let mut source = snapshot(&store, "source.rs", &["caller"]);
    source
        .graph
        .pending_refs
        .push(reference(&source.entities[0]));
    write(&store, &source);
    let id = pending(&store)[0].id;
    let proposal = |target_id: &str| Resolution {
        ref_id: id,
        target_id: target_id.into(),
        provenance: Provenance::WorkspaceUnique,
    };
    store
        .with_resolution_session(|session| {
            let valid = proposal(target.entities[0].id.as_str());
            assert_eq!(
                session.apply_resolutions(&[valid.clone()])?.resolved,
                0,
                "must first read the reference in this session"
            );
            session.list_pending_refs(100, 0)?;
            for endpoint in [
                "absent".to_owned(),
                hidden.entities[0].id.as_str().to_owned(),
                format!("f{}", hidden.file.id.get()),
            ] {
                let stats = session.apply_resolutions(&[proposal(&endpoint)])?;
                assert_eq!((stats.resolved, stats.stale), (0, 1));
            }
            assert_eq!(
                session.record_candidates(&[(id, vec![hidden.entities[0].id.as_str().into()])])?,
                0
            );
            assert_eq!(
                session.record_candidates(&[(id, vec![target.entities[0].id.as_str().into()])])?,
                1
            );
            assert_eq!(session.apply_resolutions(&[valid])?.resolved, 1);
            assert_eq!(
                session.apply_failures(&[id])?,
                0,
                "resolved rows cannot be changed to failed"
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(callers(&store, &target.entities[0]).len(), 1);
    store.close().unwrap();
    let reader = open(temporary.path(), true);
    assert!(
        reader
            .with_resolution_session::<()>(|_| panic!("read-only session must be rejected"))
            .is_err()
    );
    reader.close().unwrap();
    assert!(
        reader
            .with_resolution_session::<()>(|_| panic!("closed session must be rejected"))
            .is_err()
    );
}

#[test]
fn file_endpoint_writeback_and_import_scope_obey_file_visibility() {
    let temporary = tempfile::tempdir().unwrap();
    let store = open(temporary.path(), false);
    let target = named(&store, "target.rs");
    let mut source = snapshot(&store, "source.rs", &["caller"]);
    let mut import = reference(&source.entities[0]);
    import.reference_kind = EdgeKind::Imports;
    import.reference_name = "./target".into();
    source.graph.pending_refs.push(import);
    write(&store, &source);
    store
        .with_resolution_session(|session| {
            let reference = session.list_pending_refs(10, 0)?.refs.remove(0);
            let file_id = session.file_id_by_path("target.rs")?.unwrap();
            let stats = session.apply_resolutions(&[Resolution {
                ref_id: reference.id,
                target_id: format!("f{file_id}"),
                provenance: Provenance::ImportScoped,
            }])?;
            assert_eq!(stats.resolved, 1);
            assert_eq!(
                session.imported_file_ids(source.file.id.get())?,
                vec![file_id]
            );
            Ok(())
        })
        .unwrap();
    status(&store, &target.file, FileIndexStatus::Deleting);
    store
        .with_resolution_session(|session| {
            assert!(session.imported_file_ids(source.file.id.get())?.is_empty());
            Ok(())
        })
        .unwrap();
}
