use super::super::pending::{self, PendingChange, PendingChanges};
use super::*;
use crate::domain::{
    Content, Entity, EntityContent, EntityFragment, EntityMetadata, FileFormat, FileRecord,
    FileSnapshot, SourceRange, SymbolType, TableCell, TableCellRole, TableContent,
};
fn schema() -> WorkspaceIndexEmbeddingSchema {
    WorkspaceIndexEmbeddingSchema {
        provider: "fixture".to_owned(),
        model: "fixture-model".to_owned(),
        dimension: 3,
        metric: EmbeddingMetric::Cosine,
    }
}

fn open(path: &Path, read_only: bool) -> Box<dyn WorkspaceIndexStorage> {
    let options = if read_only {
        WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: path.to_owned(),
        }
    } else {
        WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: path.to_owned(),
            embedding: schema(),
        }
    };
    ZvecStorageFactory::new()
        .open(options)
        .expect("open real zvec storage")
}

fn fixture(id: &str, text: &str, vector: Vec<f32>) -> (FileRecord, IndexedFragment) {
    let id = FileId::new(id).expect("file ID");
    let file = FileRecord {
        id: id.clone(),
        relative_path: PathBuf::from(format!("{}.txt", id.as_str())),
        formats: vec![FileFormat::Text],
        snapshot: FileSnapshot {
            size_bytes: text.len() as u64,
            modified_epoch_ms: Some(1),
            content_hash: Some(crate::utils::sha256_hex(text.as_bytes())),
        },
        index_status: FileIndexStatus::NotIndexed,
    };
    let entry = IndexedFragment {
        fragment: EntityFragment::Standalone(Entity {
            id: EntityId::new(format!("entity-{}", id.as_str())).expect("entity ID"),
            file_id: id,
            range: SourceRange::File,
            content: EntityContent::Source(vec![Content::Text(text.to_owned())]),
            metadata: Some(EntityMetadata::Code {
                symbol_type: SymbolType::Function,
                symbol_name: Some("quoted'\\name\0suffix".to_owned()),
                scope: None,
                node_type: None,
                signature: None,
                documentation: None,
                modifiers: Vec::new(),
            }),
        }),
        vector,
    };
    (file, entry)
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "Keep the storage lifecycle in execution order"
)]
fn persists_filters_and_replaces_complete_files() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let (first, entry) = fixture("first", "orchard\0苹果 数据库", vec![1.0, 0.0, 0.0]);
    let (second, other) = fixture("second", "orchard vineyard", vec![0.0, 1.0, 0.0]);
    storage
        .replace_file(&first, std::slice::from_ref(&entry))
        .expect("first file");
    storage
        .replace_file(&second, &[other])
        .expect("second file");
    let marker = home.join("storage").join(pending::NAME);
    assert!(marker.exists(), "small writes await a checkpoint");

    let filter = StorageSearchFilter {
        file_ids: Some(vec![first.id.clone()]),
        entity_ids: Some(vec![entry.fragment.entity_id().clone()]),
        symbol_names: Some(vec!["quoted'\\name\0suffix".to_owned()]),
        symbol_types: Some(vec![SymbolType::Function]),
    };
    for query in ["orchard", "数据库", "orchard\0"] {
        let hits = storage
            .search_fts(query, 10, Some(&filter))
            .expect("filtered FTS");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].fragment, entry.fragment);
    }
    let hits = storage
        .search_vector(&[1.0, 0.0, 0.0], 10, Some(&filter))
        .expect("filtered ANN");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].file.id, first.id);
    assert_eq!(hits[0].file.snapshot, first.snapshot);
    assert!(hits[0].file.index_status.is_indexed());
    let ranked = storage
        .search_vector(&[1.0, 0.0, 0.0], 10, None)
        .expect("ranked ANN");
    assert_eq!(ranked.len(), 2);
    assert_eq!(ranked[0].fragment, entry.fragment);
    for rejected in [
        StorageSearchFilter {
            file_ids: Some(vec![second.id.clone()]),
            ..filter.clone()
        },
        StorageSearchFilter {
            entity_ids: Some(vec![EntityId::new("missing").expect("entity ID")]),
            ..filter.clone()
        },
        StorageSearchFilter {
            symbol_names: Some(vec!["quoted'\\name suffix".to_owned()]),
            ..filter.clone()
        },
        StorageSearchFilter {
            symbol_types: Some(vec![SymbolType::Value]),
            ..filter.clone()
        },
    ] {
        assert!(
            storage
                .search_fts("orchard", 10, Some(&rejected))
                .expect("FTS exclusion")
                .is_empty()
        );
        assert!(
            storage
                .search_vector(&[1.0, 0.0, 0.0], 10, Some(&rejected))
                .expect("ANN exclusion")
                .is_empty()
        );
    }
    assert_eq!(
        storage
            .get_entity(entry.fragment.entity_id())
            .expect("entity")
            .expect("exists")
            .entity,
        *entry.fragment.as_entity().expect("standalone")
    );
    let empty = StorageSearchFilter {
        file_ids: Some(Vec::new()),
        ..StorageSearchFilter::default()
    };
    assert!(
        storage
            .search_fts("orchard", 10, Some(&empty))
            .expect("empty filter")
            .is_empty()
    );
    assert!(
        storage
            .search_vector(&[1.0, 0.0, 0.0], 10, Some(&empty))
            .expect("empty filter")
            .is_empty()
    );
    assert!(marker.exists(), "same-session reads must not checkpoint");

    storage
        .mark_file_failed(&first, "fixture extraction error")
        .expect("mark failed");
    assert!(
        storage
            .search_fts("数据库", 10, None)
            .expect("old content removed")
            .is_empty()
    );
    assert!(
        storage
            .get_entity(entry.fragment.entity_id())
            .expect("old entity removed")
            .is_none()
    );
    assert_eq!(
        storage.list_files().expect("files")[0].index_status.error(),
        Some("fixture extraction error")
    );
    storage
        .replace_file(&first, &[entry])
        .expect("retry failed file");
    storage.delete_file(&second.id).expect("delete file");
    storage.close().expect("close writer");
    assert!(
        !marker.exists(),
        "normal close checkpoints remaining writes"
    );
    assert_eq!(
        storage.list_files().expect_err("closed lease").code(),
        EngineError::RESOURCE_CLOSED
    );
    let reader = open(home, true);
    assert_eq!(reader.list_files().expect("reopened files").len(), 1);
    assert_eq!(
        reader
            .search_fts("数据库", 10, None)
            .expect("reopened FTS")
            .len(),
        1
    );
    assert_eq!(
        reader
            .search_vector(&[1.0, 0.0, 0.0], 10, None)
            .expect("reopened ANN")
            .len(),
        1
    );
    assert!(reader.delete_file(&first.id).is_err());
    let second_reader = open(home, true);
    reader.close().expect("close one reader");
    assert_eq!(
        second_reader
            .list_files()
            .expect("other lease survives")
            .len(),
        1
    );
    assert!(ZvecStorageFactory::new().delete(home).is_err());
    second_reader.close().expect("close other reader");
    ZvecStorageFactory::new()
        .delete(home)
        .expect("drop storage");
    assert!(!ZvecStorageFactory::new().exists(home).expect("absence"));
}

#[test]
#[allow(clippy::too_many_lines)]
fn invalidates_interrupted_batches_before_serving_readers() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let (file, original) = fixture("source", "old apple", vec![1.0, 0.0, 0.0]);
    let (deleted, deleted_entry) = fixture("deleted", "ripe pear", vec![0.0, 1.0, 0.0]);
    let (unaffected, unaffected_entry) =
        fixture("unaffected", "stable vineyard", vec![0.0, 0.0, 1.0]);
    let storage = open(home, false);
    for (source, entry) in [
        (&file, &original),
        (&deleted, &deleted_entry),
        (&unaffected, &unaffected_entry),
    ] {
        storage
            .replace_file(source, std::slice::from_ref(entry))
            .expect("initial file");
    }
    storage.close().expect("close writer");
    let (replacement_file, _) = fixture("source", "changed apple", vec![1.0, 0.0, 0.0]);
    let (added, replacement) = fixture("added", "new banana", vec![0.0, 1.0, 0.0]);
    let path = home.join("storage");
    let changes = PendingChanges::from([
        (
            file.id.as_str().to_owned(),
            PendingChange::Reindex(replacement_file.clone()),
        ),
        (
            added.id.as_str().to_owned(),
            PendingChange::Reindex(added.clone()),
        ),
        (
            deleted.id.as_str().to_owned(),
            PendingChange::Delete(deleted.id.clone()),
        ),
    ]);
    pending::write(&path, &changes).expect("durable batch intent");
    // Persist an incomplete batch: one source was removed, another replaced,
    // and the requested deletion has not started.
    let native = NativeStore::open(
        &path,
        &schema(),
        &dictionary::cache_path().expect("dictionary cache"),
        false,
    )
    .expect("native writer");
    native.apply_delete(&file.id).expect("partial mutation");
    let mut partial = added.clone();
    partial.index_status = FileIndexStatus::Indexed {
        indexed_epoch_ms: 1,
        entity_count: 1,
    };
    native
        .apply_replace(&partial, std::slice::from_ref(&replacement))
        .expect("uncheckpointed replacement");
    native.flush().expect("persist partial mutation");
    drop(native);
    let competing_reader = acquire_storage_lock(home, true).expect("another reader's lock");
    let recovery_error = ZvecStorageFactory::new()
        .open(WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: home.to_owned(),
        })
        .err()
        .expect("recovery requires exclusive access");
    assert_eq!(recovery_error.code(), EngineError::RESOURCE_BUSY);
    assert!(path.join(pending::NAME).exists());
    drop(competing_reader);
    let reader = open(home, true);
    assert!(!path.join(pending::NAME).exists());
    let shared_lock =
        acquire_storage_lock(home, true).expect("recovered reader holds a shared lock");
    assert_eq!(
        acquire_storage_lock(home, false)
            .expect_err("recovered reader excludes writers")
            .code(),
        EngineError::RESOURCE_BUSY
    );
    drop(shared_lock);
    for query in ["apple", "banana", "pear"] {
        assert!(
            reader
                .search_fts(query, 10, None)
                .expect("no partial FTS results")
                .is_empty()
        );
    }
    for (source, entry) in [
        (&file, &original),
        (&added, &replacement),
        (&deleted, &deleted_entry),
    ] {
        let filter = StorageSearchFilter {
            file_ids: Some(vec![source.id.clone()]),
            ..StorageSearchFilter::default()
        };
        assert!(
            reader
                .search_vector(&entry.vector, 10, Some(&filter))
                .expect("no partial vectors")
                .is_empty()
        );
        assert!(
            reader
                .get_entity(entry.fragment.entity_id())
                .expect("no partial entities")
                .is_none()
        );
    }
    let recovered = reader.list_files().expect("recovered source metadata");
    assert_eq!(recovered.len(), 3);
    for source in [&replacement_file, &added] {
        let stored = recovered
            .iter()
            .find(|file| file.id == source.id)
            .expect("pending source retained");
        assert_eq!(stored, source);
        let status = &stored.index_status;
        assert_eq!(status.indexed_epoch_ms(), None);
        assert_eq!(status.entity_count(), 0);
        assert_eq!(status.error(), None);
    }
    assert!(!recovered.iter().any(|file| file.id == deleted.id));
    assert_eq!(
        reader
            .search_fts("vineyard", 10, None)
            .expect("unaffected FTS")
            .len(),
        1
    );
    assert_eq!(
        reader
            .search_vector(&unaffected_entry.vector, 10, None)
            .expect("unaffected vector")
            .len(),
        1
    );
    assert!(
        reader
            .get_entity(unaffected_entry.fragment.entity_id())
            .expect("unaffected entity")
            .is_some()
    );
    reader.close().expect("close recovered reader");
    drop(acquire_storage_lock(home, false).expect("closing recovered reader releases its lock"));

    let reader = open(home, true);
    assert_eq!(reader.list_files().expect("durable recovery"), recovered);
    reader.close().expect("close second reader");
    // A corrupt marker must fail closed instead of serving inconsistent collections.
    fs::write(path.join(pending::NAME), b"corrupt pending record").expect("corrupt fixture");
    assert!(
        ZvecStorageFactory::new()
            .open(WorkspaceIndexStorageOptions::ReadOnly {
                storage_path: home.to_owned()
            })
            .is_err()
    );
    assert!(path.join(pending::NAME).exists());
}

#[test]
fn checkpoints_batches_at_the_operation_limit() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let marker = home.join("storage").join(pending::NAME);
    for index in 0..CHECKPOINT_OPERATIONS {
        let (file, entry) = fixture(&format!("batch-{index}"), "orchard", vec![1.0, 0.0, 0.0]);
        storage.replace_file(&file, &[entry]).expect("batch write");
        assert_eq!(marker.exists(), index + 1 < CHECKPOINT_OPERATIONS);
    }

    let (later, entry) = fixture("later", "uncheckpointed banana", vec![0.0, 1.0, 0.0]);
    storage.replace_file(&later, &[entry]).expect("next batch");
    assert_eq!(
        pending::read(&home.join("storage"))
            .expect("next batch marker")
            .len(),
        1
    );
    drop(storage);
    assert!(
        marker.exists(),
        "dropping a writer must retain unfinished batch intent"
    );

    let reader = open(home, true);
    assert!(!marker.exists());
    let files = reader.list_files().expect("recovered files");
    assert_eq!(files.len(), CHECKPOINT_OPERATIONS + 1);
    assert_eq!(
        files
            .iter()
            .filter(|file| file.index_status.is_indexed())
            .count(),
        CHECKPOINT_OPERATIONS
    );
    assert_eq!(
        reader
            .search_fts("orchard", CHECKPOINT_OPERATIONS + 1, None)
            .expect("previous checkpoint survives")
            .len(),
        CHECKPOINT_OPERATIONS
    );
    assert!(
        reader
            .search_fts("banana", 10, None)
            .expect("new batch requires reindexing")
            .is_empty()
    );
    reader.close().expect("close reader");
}

#[test]
fn checkpoints_large_sources_before_the_operation_limit() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let marker = home.join("storage").join(pending::NAME);
    for id in ["first-large", "second-large"] {
        let (mut file, entry) = fixture(id, "orchard", vec![1.0, 0.0, 0.0]);
        // Source metadata represents a large file without allocating its full contents.
        file.snapshot.size_bytes = CHECKPOINT_BYTES / 2;
        storage
            .replace_file(&file, &[entry])
            .expect("large source write");
        assert_eq!(marker.exists(), id == "first-large");
    }
    drop(storage);
    let reader = open(home, true);
    assert_eq!(
        reader
            .search_fts("orchard", 10, None)
            .expect("byte checkpoint persisted both sources")
            .len(),
        2
    );
    reader.close().expect("close reader");
}

#[tokio::test]
async fn finalizes_small_batches_and_preserves_failure_status() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let (file, entry) = fixture("source", "orchard", vec![1.0, 0.0, 0.0]);
    let (failed, _) = fixture("failed", "unreadable", vec![0.0, 1.0, 0.0]);
    storage.replace_file(&file, &[entry]).expect("small write");
    storage
        .mark_file_failed(&failed, "fixture failure")
        .expect("failed source");
    let marker = home.join("storage").join(pending::NAME);
    assert!(marker.exists());
    storage
        .finalize_writes()
        .await
        .expect("explicit checkpoint");
    assert!(!marker.exists());
    storage
        .finalize_writes()
        .await
        .expect("empty checkpoint is idempotent");
    drop(storage);

    let reader = open(home, true);
    assert_eq!(
        reader
            .search_fts("orchard", 10, None)
            .expect("finalized source")
            .len(),
        1
    );
    let files = reader.list_files().expect("finalized file status");
    let stored = files
        .iter()
        .find(|file| file.id == failed.id)
        .expect("failed source remains");
    let status = &stored.index_status;
    assert_eq!(status.error(), Some("fixture failure"));
    assert_eq!(status.indexed_epoch_ms(), None);
    reader.close().expect("close reader");
}

#[test]
fn retains_only_the_latest_intent_for_repeated_file_updates() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let path = home.join("storage");
    let storage = open(home, false);
    let (file, original) = fixture("source", "old apple", vec![1.0, 0.0, 0.0]);
    storage
        .replace_file(&file, &[original])
        .expect("initial replacement");
    storage.delete_file(&file.id).expect("temporary deletion");
    let (mut latest, replacement) = fixture("source", "new banana", vec![0.0, 1.0, 0.0]);
    latest.snapshot.modified_epoch_ms = Some(2);
    storage
        .replace_file(&latest, std::slice::from_ref(&replacement))
        .expect("latest replacement");
    let (deleted, entry) = fixture("deleted", "ripe pear", vec![0.0, 0.0, 1.0]);
    storage
        .replace_file(&deleted, &[entry])
        .expect("another replacement");
    storage.delete_file(&deleted.id).expect("final deletion");
    assert_eq!(
        pending::read(&path).expect("pending intentions"),
        PendingChanges::from([
            (
                latest.id.as_str().to_owned(),
                PendingChange::Reindex(latest.clone())
            ),
            (
                deleted.id.as_str().to_owned(),
                PendingChange::Delete(deleted.id.clone())
            ),
        ])
    );
    assert_eq!(
        storage
            .search_fts("banana", 10, None)
            .expect("latest write visible before checkpoint")
            .len(),
        1
    );
    drop(storage);
    assert!(path.join(pending::NAME).exists());

    let reader = open(home, true);
    let files = reader.list_files().expect("recovered latest intention");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0], latest);
    assert_eq!(files[0].index_status.indexed_epoch_ms(), None);
    assert!(
        reader
            .get_entity(replacement.fragment.entity_id())
            .expect("requires fresh fragments")
            .is_none()
    );
    assert!(
        reader
            .search_vector(&replacement.vector, 10, None)
            .expect("requires fresh embeddings")
            .is_empty()
    );
    assert!(!path.join(pending::NAME).exists());
    reader.close().expect("close reader");
}

#[tokio::test]
async fn failed_marker_write_blocks_access_and_close_releases_the_lease() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let path = home.join("storage");
    let storage = open(home, false);
    let (file, entry) = fixture("source", "orchard", vec![1.0, 0.0, 0.0]);
    storage
        .replace_file(&file, std::slice::from_ref(&entry))
        .expect("healthy pending write");
    let marker = path.join(pending::NAME);
    let preserved = path.join("preserved-pending.json");
    fs::rename(&marker, &preserved).expect("preserve durable intent");
    fs::create_dir(&marker).expect("obstruct marker replacement");
    let (other, replacement) = fixture("other", "banana", vec![0.0, 1.0, 0.0]);
    assert!(storage.replace_file(&other, &[replacement]).is_err());
    for error in [
        storage
            .list_files()
            .expect_err("failed writer cannot list sources"),
        storage
            .search_fts("orchard", 10, None)
            .expect_err("failed writer cannot search"),
        storage
            .search_vector(&entry.vector, 10, None)
            .expect_err("failed writer cannot search vectors"),
        storage
            .get_entity(entry.fragment.entity_id())
            .expect_err("failed writer cannot read entities"),
        storage
            .delete_file(&file.id)
            .expect_err("failed writer cannot mutate"),
        storage
            .finalize_writes()
            .await
            .expect_err("failed writer cannot checkpoint"),
    ] {
        assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
    }
    fs::remove_dir(&marker).expect("remove obstruction");
    fs::rename(&preserved, &marker).expect("restore durable intent");
    assert!(
        storage.close().is_err(),
        "closing reports unfinished writes"
    );
    assert!(marker.exists(), "failed close must retain recovery intent");
    drop(acquire_storage_lock(home, false).expect("failed close releases its exclusive lease"));

    let reader = open(home, true);
    let files = reader.list_files().expect("recover earlier writes");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0], file);
    assert!(
        reader
            .search_fts("orchard", 10, None)
            .expect("earlier pending write invalidated")
            .is_empty()
    );
    assert!(!marker.exists());
    reader.close().expect("close reader");
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "verify invalid writes leave the same pending session usable"
)]
fn rejects_invalid_writes_without_poisoning_storage() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let (file, mut entry) = fixture("source", "healthy document", vec![1.0, 0.0, 0.0]);
    for invalid in [vec![1.0], vec![f32::NAN, 0.0, 0.0], vec![0.0, 0.0, 0.0]] {
        entry.vector = invalid;
        assert_eq!(
            storage
                .replace_file(&file, std::slice::from_ref(&entry))
                .expect_err("invalid vector")
                .code(),
            EngineError::INVALID_ARGUMENT
        );
        assert!(!home.join("storage").join(pending::NAME).exists());
        assert!(
            storage
                .list_files()
                .expect("storage still usable")
                .is_empty()
        );
    }
    entry.vector = vec![1.0, 0.0, 0.0];
    storage
        .replace_file(&file, std::slice::from_ref(&entry))
        .expect("valid write after rejections");
    let marker = home.join("storage").join(pending::NAME);
    let healthy_pending = fs::read(&marker).expect("healthy pending batch");
    entry.vector = vec![1.0];
    assert_eq!(
        storage
            .replace_file(&file, &[entry])
            .expect_err("invalid replacement of pending source")
            .code(),
        EngineError::INVALID_ARGUMENT
    );
    assert_eq!(
        fs::read(&marker).expect("pending batch preserved"),
        healthy_pending
    );
    let (_, mut invalid_table) = fixture("source", "rejected table", vec![1.0, 0.0, 0.0]);
    let EntityFragment::Standalone(entity) = &mut invalid_table.fragment else {
        panic!("fixture must be a standalone entity");
    };
    entity.content = EntityContent::Source(vec![Content::Table(TableContent {
        row_count: 1,
        column_count: 1,
        cells: vec![TableCell {
            row: 0,
            column: 0,
            row_span: 0,
            column_span: 1,
            contents: vec![Content::Text("invalid zero-height cell".to_owned())],
            kind: TableCellRole::Data,
        }],
    })]);
    let error = storage
        .replace_file(&file, &[invalid_table])
        .expect_err("invalid table must be rejected before writing intent");
    assert_eq!(error.code(), EngineError::INVALID_ARGUMENT);
    assert!(error.message().contains("table cell span"));
    assert_eq!(
        fs::read(&marker).expect("invalid table preserves pending batch"),
        healthy_pending
    );
    assert_eq!(
        storage
            .search_fts("healthy", 10, None)
            .expect("invalid input leaves pending writes readable")
            .len(),
        1
    );
    assert!(
        ZvecStorageFactory::new()
            .open(WorkspaceIndexStorageOptions::ReadOnly {
                storage_path: home.to_owned()
            })
            .is_err(),
        "readers cannot open during a writer lease"
    );
    storage.close().expect("close writer");
    let mut incompatible = schema();
    incompatible.dimension = 4;
    assert!(
        ZvecStorageFactory::new()
            .open(WorkspaceIndexStorageOptions::ReadWrite {
                storage_path: home.to_owned(),
                embedding: incompatible
            })
            .is_err()
    );
    let storage = open(home, false);
    assert_eq!(
        storage
            .list_files()
            .expect("schema rejection preserved index")
            .len(),
        1
    );
    storage.close().expect("close writer");
}

#[test]
fn invalid_file_states_and_owners_never_start_a_pending_batch() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let storage = open(directory.path(), false);
    let (file, entry) = fixture("state", "source content", vec![1.0, 0.0, 0.0]);
    let marker = directory.path().join("storage").join(pending::NAME);
    let mut unread = file.clone();
    unread.snapshot.content_hash = None;
    assert!(
        storage
            .replace_file(&unread, std::slice::from_ref(&entry))
            .is_err()
    );
    for error in ["", " ", "\n"] {
        assert!(storage.mark_file_failed(&file, error).is_err());
    }
    let mut other = file.clone();
    other.id = FileId::new("other-source").expect("other file ID");
    assert!(
        storage
            .replace_file(&other, std::slice::from_ref(&entry))
            .is_err()
    );
    assert!(!marker.exists());
    assert!(storage.list_files().expect("no partial records").is_empty());
    storage
        .replace_file(&file, &[])
        .expect("successful empty extraction");
    storage.close().expect("checkpoint empty result");
    let reader = open(directory.path(), true);
    let files = reader.list_files().expect("read empty indexed source");
    assert_eq!(files.len(), 1);
    assert!(files[0].index_status.is_indexed());
    assert_eq!(files[0].index_status.entity_count(), 0);
    assert_eq!(files[0].snapshot, file.snapshot);
    reader.close().expect("close reader");
}

#[test]
fn native_replacements_require_a_consistent_complete_file_state() {
    let directory = tempfile::tempdir().expect("fixture directory");
    open(directory.path(), false)
        .close()
        .expect("initialize storage");
    let native = NativeStore::open(
        &directory.path().join("storage"),
        &schema(),
        &dictionary::cache_path().expect("dictionary cache"),
        false,
    )
    .expect("native writer");
    let (mut file, entry) = fixture("state", "source content", vec![1.0, 0.0, 0.0]);
    for status in [
        FileIndexStatus::NotIndexed,
        FileIndexStatus::Failed {
            error: "extraction failed".to_owned(),
        },
        FileIndexStatus::Indexed {
            indexed_epoch_ms: 1,
            entity_count: 0,
        },
        FileIndexStatus::Indexed {
            indexed_epoch_ms: 1,
            entity_count: 2,
        },
    ] {
        file.index_status = status;
        assert!(
            native
                .apply_replace(&file, std::slice::from_ref(&entry))
                .is_err()
        );
    }
    assert!(
        native
            .list_files()
            .expect("no partial mutations")
            .is_empty()
    );
    file.index_status = FileIndexStatus::Indexed {
        indexed_epoch_ms: 1,
        entity_count: 1,
    };
    native
        .apply_replace(&file, std::slice::from_ref(&entry))
        .expect("consistent result");
    assert_eq!(
        native
            .get_entity(entry.fragment.entity_id())
            .expect("stored entity")
            .expect("entity exists")
            .file,
        file
    );
    file.index_status = FileIndexStatus::NotIndexed;
    native
        .apply_replace(&file, &[])
        .expect("discard interrupted result");
    assert!(
        native
            .get_entity(entry.fragment.entity_id())
            .expect("no partial entity")
            .is_none()
    );
    assert_eq!(native.list_files().expect("reindex marker"), vec![file]);
}

#[test]
fn rejects_legacy_schemas_before_opening_collections() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let path = home.join("storage");
    fs::create_dir(&path).expect("storage directory");
    for version in [1, 2, 3, 4] {
        let mut record = SchemaRecord::new(&schema(), &home.join("legacy-dictionary"));
        record.version = version;
        fs::write(
            path.join("schema.json"),
            serde_json::to_vec(&record).expect("legacy schema"),
        )
        .expect("write legacy schema");
        for options in [
            WorkspaceIndexStorageOptions::ReadOnly {
                storage_path: home.to_owned(),
            },
            WorkspaceIndexStorageOptions::ReadWrite {
                storage_path: home.to_owned(),
                embedding: schema(),
            },
        ] {
            let error = ZvecStorageFactory::new()
                .open(options)
                .err()
                .expect("legacy schema is incompatible");
            assert!(
                error
                    .message()
                    .contains(&format!("unsupported storage schema version {version}"))
            );
            assert!(error.message().contains("rebuild the index"));
        }
    }
    assert_eq!(fs::read_dir(path).expect("storage files").count(), 1);
}

#[test]
fn rejects_dictionary_cache_changes_before_opening_collections() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let path = home.join("storage");
    fs::create_dir(&path).expect("storage directory");
    let record = SchemaRecord::new(&schema(), &home.join("old-dictionary-cache"));
    fs::write(
        path.join("schema.json"),
        serde_json::to_vec(&record).expect("schema with a different cache"),
    )
    .expect("write old dictionary path");
    for options in [
        WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: home.to_owned(),
        },
        WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: home.to_owned(),
            embedding: schema(),
        },
    ] {
        let error = ZvecStorageFactory::new()
            .open(options)
            .err()
            .expect("dictionary cache changed");
        assert!(error.message().contains("dictionary cache differs"));
        assert!(error.message().contains("rebuild the index"));
    }
    assert_eq!(fs::read_dir(path).expect("storage files").count(), 1);
    assert!(!home.join("old-dictionary-cache").exists());
}

#[test]
fn writes_fragments_across_native_batch_boundaries() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let home = directory.path();
    let storage = open(home, false);
    let (file, prototype) = fixture("batch", "harvest", vec![1.0, 0.0, 0.0]);
    let entries = (0..1025)
        .map(|index| {
            let mut entity = prototype.fragment.as_entity().expect("standalone").clone();
            entity.id = EntityId::new(format!("batch-entity-{index}")).expect("entity ID");
            IndexedFragment {
                fragment: EntityFragment::Standalone(entity),
                vector: prototype.vector.clone(),
            }
        })
        .collect::<Vec<_>>();
    storage
        .replace_file(&file, &entries)
        .expect("batched write");
    let last = entries.last().expect("last batch entry");
    let filter = StorageSearchFilter {
        entity_ids: Some(vec![last.fragment.entity_id().clone()]),
        ..StorageSearchFilter::default()
    };
    assert_eq!(
        storage
            .search_fts("harvest", 1030, None)
            .expect("all batches")
            .len(),
        entries.len()
    );
    assert_eq!(
        storage
            .search_vector(&prototype.vector, 10, Some(&filter))
            .expect("last batch ANN")[0]
            .fragment,
        last.fragment
    );
    storage
        .replace_file(&file, &[])
        .expect("replace with empty file");
    assert!(
        storage
            .search_fts("harvest", 10, None)
            .expect("no stale fragments")
            .is_empty()
    );
    assert!(
        storage
            .get_entity(last.fragment.entity_id())
            .expect("no stale entity")
            .is_none()
    );
    storage.close().expect("close storage");
}
