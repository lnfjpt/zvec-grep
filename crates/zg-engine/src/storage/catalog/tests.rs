use super::*;

fn paths(paths: &[&str]) -> Vec<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
}

#[test]
fn preserves_ids_across_restarts_and_index_generations() {
    let home = tempfile::tempdir().expect("workspace home");
    let expected = {
        let mut catalog = Catalog::open(home.path(), false).expect("catalog");
        let ids = catalog
            .resolve_file_ids(&paths(&[
                "src/a.rs",
                "README.md",
                "src/nested/b.rs",
                "src/a.rs",
            ]))
            .expect("allocate");
        assert_eq!(
            ids.iter().map(|id| id.get()).collect::<Vec<_>>(),
            [0, 1, 2, 0]
        );
        assert_eq!(
            catalog
                .ancestor_directory_ids(&SourcePath::new("src/nested/b.rs").expect("source path"))
                .expect("ancestors")
                .iter()
                .map(|id| id.get())
                .collect::<Vec<_>>(),
            [0, 1]
        );
        assert!(
            catalog
                .ancestor_directory_ids(&SourcePath::new("README.md").expect("source path"))
                .expect("root file")
                .is_empty()
        );
        assert!(
            catalog
                .directory_id(Path::new(""))
                .expect("root lookup")
                .is_none()
        );
        ids
    };
    // A rebuilt generation can rediscover a subset without forgetting IDs
    // held by a deleted file or by the generation that is still published.
    fs::create_dir_all(home.path().join("generations/new/storage")).expect("new generation");
    let mut catalog = Catalog::open(home.path(), false).expect("reopened catalog");
    let actual = catalog
        .resolve_file_ids(&paths(&["src/a.rs", "src/new.rs", "README.md"]))
        .expect("allocate after restart");
    assert_eq!(actual, [expected[0], FileId::new(3), expected[1]]);
    assert_eq!(
        catalog
            .file_id(Path::new("src/nested/b.rs"))
            .expect("file lookup"),
        Some(expected[2])
    );
}

#[test]
fn locks_all_generations_but_allows_multiple_readers() {
    let home = tempfile::tempdir().expect("workspace home");
    let writer = Catalog::open(home.path(), false).expect("writer");
    assert!(
        matches!(Catalog::open(home.path(), false), Err(error) if error.code() == EngineError::RESOURCE_BUSY)
    );
    assert!(
        matches!(Catalog::open(home.path(), true), Err(error) if error.code() == EngineError::RESOURCE_BUSY)
    );
    drop(writer);
    let _first = Catalog::open(home.path(), true).expect("first reader");
    let mut second = Catalog::open(home.path(), true).expect("second reader");
    assert!(second.resolve_file_ids(&paths(&["a.rs"])).is_err());
    assert!(
        matches!(Catalog::open(home.path(), false), Err(error) if error.code() == EngineError::RESOURCE_BUSY)
    );
}

#[test]
fn explicit_deletion_waits_for_handles_and_keeps_the_lock_file() {
    let home = tempfile::tempdir().expect("workspace home");
    let mut writer = Catalog::open(home.path(), false).expect("writer");
    writer
        .resolve_file_ids(&paths(&["a.txt"]))
        .expect("registered file");
    assert!(
        matches!(Catalog::delete(home.path()), Err(error) if error.code() == EngineError::RESOURCE_BUSY)
    );
    assert!(Catalog::exists(home.path()).expect("catalog exists"));
    drop(writer);
    let reader = Catalog::open(home.path(), true).expect("reader");
    assert!(
        matches!(Catalog::delete(home.path()), Err(error) if error.code() == EngineError::RESOURCE_BUSY)
    );
    drop(reader);
    Catalog::delete(home.path()).expect("explicit deletion");
    assert!(!Catalog::exists(home.path()).expect("catalog removed"));
    assert!(home.path().join(LOCK_FILE).is_file());
    Catalog::delete(home.path()).expect("idempotent deletion");
}

#[test]
fn rejects_invalid_batches_without_consuming_ids() {
    let home = tempfile::tempdir().expect("workspace home");
    let mut catalog = Catalog::open(home.path(), false).expect("catalog");
    for invalid in [
        "", ".", "../a", "/a", "src/../a", "a\0b", "a//b", "a/./b", "a/",
    ] {
        assert!(
            catalog
                .resolve_file_ids(&paths(&["valid/a", invalid]))
                .is_err(),
            "{invalid:?}"
        );
    }
    assert!(
        catalog
            .file_id(Path::new("valid/a"))
            .expect("file lookup")
            .is_none()
    );
    assert_eq!(
        catalog
            .resolve_file_ids(&paths(&["valid/a"]))
            .expect("valid batch")[0]
            .get(),
        0
    );
}

#[test]
fn retains_next_id_without_live_entries_and_stops_at_exhaustion() {
    let home = tempfile::tempdir().expect("workspace home");
    let state = Metadata {
        next_file_id: Some(u64::MAX),
        ..Metadata::default()
    };
    seed_counters(home.path(), &state);
    let mut catalog = Catalog::open(home.path(), false).expect("catalog");
    assert!(
        catalog
            .resolve_file_ids(&paths(&["last.rs", "src/overflow.rs"]))
            .is_err()
    );
    assert_eq!(
        catalog.state, state,
        "failed batch must not reserve any IDs"
    );
    assert_eq!(catalog.file_id(Path::new("last.rs")).expect("lookup"), None);
    assert_eq!(
        catalog.directory_id(Path::new("src")).expect("lookup"),
        None
    );
    let id = catalog
        .resolve_file_ids(&paths(&["last.rs"]))
        .expect("last identity")[0];
    assert_eq!(id.get(), u64::MAX);
    drop(catalog);
    let mut catalog = Catalog::open(home.path(), false).expect("reopen exhausted catalog");
    assert!(catalog.resolve_file_ids(&paths(&["overflow.rs"])).is_err());
    assert_eq!(
        catalog
            .file_id(Path::new("overflow.rs"))
            .expect("file lookup"),
        None
    );
    assert_eq!(
        catalog
            .resolve_file_ids(&paths(&["last.rs"]))
            .expect("existing identity"),
        [id]
    );
}

#[test]
fn directory_exhaustion_rolls_back_the_complete_discovery_batch() {
    let home = tempfile::tempdir().expect("workspace home");
    let state = Metadata {
        next_directory_id: None,
        ..Metadata::default()
    };
    seed_counters(home.path(), &state);
    let mut catalog = Catalog::open(home.path(), false).expect("catalog");
    assert!(
        catalog
            .resolve_file_ids(&paths(&["root.rs", "src/a.rs"]))
            .is_err()
    );
    assert!(
        catalog
            .file_id(Path::new("root.rs"))
            .expect("file lookup")
            .is_none()
    );
    assert_eq!(
        catalog
            .resolve_file_ids(&paths(&["root.rs"]))
            .expect("root file needs no directory")[0]
            .get(),
        0
    );
}

#[test]
fn validates_the_exact_file_mapping_before_storage_writes() {
    use crate::domain::{FileFormat, FileIndexStatus, FileSnapshot};

    let home = tempfile::tempdir().expect("workspace home");
    let mut catalog = Catalog::open(home.path(), false).expect("catalog");
    let ids = catalog
        .resolve_file_ids(&paths(&["a.txt", "b.txt"]))
        .expect("registered files");
    let mut file = FileRecord {
        id: ids[0],
        relative_path: SourcePath::new("a.txt").expect("source path"),
        formats: vec![FileFormat::Text],
        snapshot: FileSnapshot {
            size_bytes: 0,
            modified_epoch_ms: None,
            content_hash: None,
        },
        index_status: FileIndexStatus::NotIndexed,
    };
    catalog.validate_file(&file).expect("registered mapping");
    file.id = ids[1];
    assert!(catalog.validate_file(&file).is_err());
    file.relative_path = SourcePath::new("unregistered.txt").expect("source path");
    assert!(catalog.validate_file(&file).is_err());
}

#[test]
fn rejects_corrupt_catalogs_before_exposing_identities() {
    let entry =
        |path: &str, id| serde_json::json!({"path": {"encoding": "utf8", "value": path}, "id": id});
    let valid = serde_json::json!({
        "version": LEGACY_VERSION, "last_file_id": 2, "last_directory_id": 1,
        "files": [entry("src/a", 1), entry("b", 2)], "directories": [entry("src", 1)]
    });
    assert!(decode_legacy(&serde_json::to_vec(&valid).expect("JSON")).is_ok());
    let modifications = [
        ("version", serde_json::json!(VERSION + 1)),
        ("last_file_id", serde_json::json!(1)),
        ("last_file_id", serde_json::json!(-1)),
        ("last_file_id", serde_json::json!(1e20)),
        ("files", serde_json::json!([entry("a", 1), entry("b", 1)])),
        ("files", serde_json::json!([entry("a", 1), entry("a", 2)])),
        ("files", serde_json::json!([entry("a", 0)])),
        ("files", serde_json::json!([entry("../a", 1)])),
        ("directories", serde_json::json!([])),
    ];
    for (field, value) in modifications {
        let mut invalid = valid.clone();
        invalid[field] = value;
        assert!(
            decode_legacy(&serde_json::to_vec(&invalid).expect("JSON")).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn failed_publication_requires_reopening_before_new_allocations() {
    let home = tempfile::tempdir().expect("workspace home");
    let mut catalog = Catalog::open(home.path(), false).expect("catalog");
    let pending = catalog.path.join(PENDING_FILE);
    fs::create_dir(&pending).expect("block publication");
    assert!(catalog.resolve_file_ids(&paths(&["a"])).is_err());
    assert!(
        catalog
            .file_id(Path::new("a"))
            .expect("file lookup")
            .is_none()
    );
    fs::remove_dir(&pending).expect("remove obstruction");
    assert!(
        matches!(catalog.resolve_file_ids(&paths(&["b"])), Err(error) if error.code() == EngineError::RESOURCE_BUSY)
    );
}

#[cfg(unix)]
#[test]
fn preserves_non_unicode_paths_without_conflating_lossy_names() {
    use std::os::unix::ffi::OsStringExt;

    let home = tempfile::tempdir().expect("workspace home");
    let first = PathBuf::from(std::ffi::OsString::from_vec(b"src/\xff.rs".to_vec()));
    let second = PathBuf::from(std::ffi::OsString::from_vec(b"src/\xfe.rs".to_vec()));
    let ids = {
        let mut catalog = Catalog::open(home.path(), false).expect("catalog");
        assert!(!catalog.has_non_unicode_file_names());
        let ids = catalog
            .resolve_file_ids(&[first.clone(), second.clone()])
            .expect("paths");
        assert_ne!(ids[0], ids[1]);
        assert!(catalog.has_non_unicode_file_names());
        ids
    };
    let catalog = Catalog::open(home.path(), true).expect("reader");
    assert_eq!(catalog.file_id(&first).expect("first lookup"), Some(ids[0]));
    assert_eq!(
        catalog.file_id(&second).expect("second lookup"),
        Some(ids[1])
    );
    assert!(catalog.has_non_unicode_file_names());
}

fn seed_counters(home: &Path, state: &Metadata) {
    let catalog = Catalog::open(home, false).expect("create catalog");
    catalog.native.write_metadata(state).expect("seed metadata");
    catalog.native.flush().expect("flush metadata");
}

#[test]
fn allocation_metadata_requires_explicit_counters() {
    let original = serde_json::to_value(Metadata::default()).expect("metadata");
    for field in ["next_file_id", "next_directory_id"] {
        let mut missing = original.clone();
        missing.as_object_mut().expect("object").remove(field);
        assert!(serde_json::from_value::<Metadata>(missing).is_err());
        for invalid in [serde_json::json!(-1), serde_json::json!(1e20)] {
            let mut record = original.clone();
            record[field] = invalid;
            assert!(serde_json::from_value::<Metadata>(record).is_err());
        }
    }
}

#[test]
fn reopens_legacy_native_counters_and_recovers_reserved_ids() {
    let home = tempfile::tempdir().expect("workspace home");
    {
        let catalog = Catalog::open(home.path(), false).expect("catalog");
        let existing = IdentityEntry::new(Path::new("existing.rs"), 11).expect("existing file");
        NativeCatalog::write_identities(&catalog.native.files, &[existing]).expect("old file");
        let before = serde_json::json!({
            "version": LEGACY_VERSION, "last_file_id": 11, "last_directory_id": 4,
            "has_non_unicode_file_names": false, "legacy_import_hash": null,
        });
        let mut doc = Doc::new().expect("metadata document");
        doc.set_pk("state");
        doc.add_string("payload", &before.to_string())
            .expect("old metadata");
        write_docs(&catalog.native.metadata, &[doc]).expect("write old metadata");
        catalog.native.flush().expect("flush old catalog");
        let mut after = before.clone();
        after["last_file_id"] = 12.into();
        after["last_directory_id"] = 5.into();
        let directory = IdentityEntry::new(Path::new("src"), 5).expect("directory");
        NativeCatalog::write_identities(
            &catalog.native.directories,
            std::slice::from_ref(&directory),
        )
        .expect("partially committed batch");
        catalog.native.directories.flush().expect("flush directory");
        let pending = serde_json::json!({
            "version": LEGACY_VERSION, "before": before, "after": after,
            "files": [IdentityEntry::new(Path::new("src/new.rs"), 12).expect("new file")],
            "directories": [directory],
        });
        fs::write(catalog.path.join(PENDING_FILE), pending.to_string()).expect("old journal");
    }
    let mut catalog = Catalog::open(home.path(), false).expect("recover old catalog");
    assert_eq!(
        catalog
            .file_id(Path::new("existing.rs"))
            .expect("existing ID"),
        Some(FileId::new(11))
    );
    assert_eq!(
        catalog
            .file_id(Path::new("src/new.rs"))
            .expect("recovered ID"),
        Some(FileId::new(12))
    );
    assert_eq!(
        catalog
            .directory_id(Path::new("src"))
            .expect("recovered directory"),
        Some(DirectoryId::new(5))
    );
    assert_eq!(
        catalog
            .resolve_file_ids(&paths(&["next.rs"]))
            .expect("continue allocation"),
        [FileId::new(13)]
    );
    assert!(!catalog.path.join(PENDING_FILE).exists());
    drop(catalog);
    let catalog = Catalog::open(home.path(), true).expect("reopen new metadata");
    assert_eq!(catalog.state.next_file_id, Some(14));
}

fn legacy_bytes() -> Vec<u8> {
    let entry =
        |path: &str, id| serde_json::json!({"path": {"encoding": "utf8", "value": path}, "id": id});
    serde_json::to_vec(&serde_json::json!({
        "version": LEGACY_VERSION, "last_file_id": 19, "last_directory_id": 8,
        "files": [entry("src/a.rs", 3), entry("README.md", 11)],
        "directories": [entry("src", 5)],
    }))
    .expect("legacy bytes")
}

#[test]
fn migrates_legacy_catalog_with_history_and_allocation_high_water_marks() {
    let home = tempfile::tempdir().expect("workspace home");
    fs::write(home.path().join(LEGACY_FILE), legacy_bytes()).expect("legacy file");
    assert!(Catalog::needs_recovery(home.path()).expect("migration required"));
    assert!(Catalog::open(home.path(), true).is_err());
    {
        let mut catalog = Catalog::open(home.path(), false).expect("migrated catalog");
        assert!(!home.path().join(LEGACY_FILE).exists());
        assert_eq!(
            catalog
                .file_id(Path::new("src/a.rs"))
                .expect("lookup")
                .expect("id")
                .get(),
            3
        );
        assert_eq!(
            catalog
                .directory_id(Path::new("src"))
                .expect("lookup")
                .expect("id")
                .get(),
            5
        );
        assert_eq!(
            catalog
                .resolve_file_ids(&paths(&["other/b.rs"]))
                .expect("new file")[0]
                .get(),
            20
        );
        assert_eq!(
            catalog
                .directory_id(Path::new("other"))
                .expect("lookup")
                .expect("id")
                .get(),
            9
        );
        assert!(
            home.path()
                .join(CATALOG_DIRECTORY)
                .join("file_identities")
                .is_dir()
        );
        assert!(!catalog.path.join(PENDING_FILE).exists());
    }
    let catalog = Catalog::open(home.path(), true).expect("reader");
    assert_eq!(
        catalog
            .file_id(Path::new("README.md"))
            .expect("lookup")
            .expect("id")
            .get(),
        11
    );
    assert!(!Catalog::needs_recovery(home.path()).expect("clean catalog"));
}

#[test]
fn completes_import_after_publication_and_rejects_unrelated_legacy_data() {
    let home = tempfile::tempdir().expect("workspace home");
    super::super::backend::initialize().expect("zvec");
    fs::write(home.path().join(LEGACY_FILE), legacy_bytes()).expect("legacy file");
    initialize(home.path()).expect("publish without legacy cleanup");
    assert!(home.path().join(LEGACY_FILE).exists());
    assert!(Catalog::open(home.path(), true).is_err());
    drop(Catalog::open(home.path(), false).expect("finish migration"));
    assert!(!home.path().join(LEGACY_FILE).exists());
    fs::write(home.path().join(LEGACY_FILE), b"{}").expect("different legacy");
    assert!(Catalog::open(home.path(), false).is_err());
    assert!(home.path().join(LEGACY_FILE).exists());
}

#[test]
fn rejects_legacy_corruption_before_creating_a_catalog() {
    let home = tempfile::tempdir().expect("workspace home");
    fs::write(home.path().join(LEGACY_FILE), b"{}").expect("corrupt legacy");
    assert!(Catalog::open(home.path(), false).is_err());
    assert!(!home.path().join(CATALOG_DIRECTORY).exists());
    assert!(!home.path().join(STAGING_DIRECTORY).exists());
    assert_eq!(
        fs::read(home.path().join(LEGACY_FILE)).expect("legacy retained"),
        b"{}"
    );
}

#[test]
fn discards_only_unpublished_partial_initialization() {
    let home = tempfile::tempdir().expect("workspace home");
    fs::create_dir_all(home.path().join(STAGING_DIRECTORY).join("file_identities"))
        .expect("partial initialization");
    assert!(Catalog::exists(home.path()).expect("catalog exists"));
    assert!(Catalog::open(home.path(), true).is_err());
    let mut catalog = Catalog::open(home.path(), false).expect("retry initialization");
    assert_eq!(
        catalog
            .resolve_file_ids(&paths(&["a"]))
            .expect("new identity")[0]
            .get(),
        0
    );
    drop(catalog);
    fs::remove_dir_all(home.path().join(CATALOG_DIRECTORY).join("directories"))
        .expect("damage published catalog");
    assert!(Catalog::open(home.path(), false).is_err());
}

fn reserved_batch(catalog: &Catalog) -> PendingBatch {
    let mut after = catalog.state.clone();
    let file_id = allocate(&mut after.next_file_id).expect("allocate file");
    let directory_id = allocate(&mut after.next_directory_id).expect("allocate directory");
    PendingBatch {
        version: VERSION,
        before: catalog.state.clone(),
        files: vec![IdentityEntry::new(Path::new("src/a.rs"), file_id).expect("file")],
        directories: vec![IdentityEntry::new(Path::new("src"), directory_id).expect("directory")],
        after,
    }
}

#[test]
fn replays_all_partial_batch_boundaries_before_allowing_new_allocations() {
    // The journal, directory rows, file rows and counters may be committed in
    // any prefix of this sequence when the process dies.
    for committed_steps in 0..=3 {
        let home = tempfile::tempdir().expect("workspace home");
        {
            let mut catalog = Catalog::open(home.path(), false).expect("catalog");
            catalog
                .resolve_file_ids(&paths(&["root.rs"]))
                .expect("existing identity");
            let batch = reserved_batch(&catalog);
            atomic_write(
                &catalog.path.join(PENDING_FILE),
                &encode_json(&batch).expect("journal"),
            )
            .expect("reserve batch");
            if committed_steps >= 1 {
                NativeCatalog::write_identities(&catalog.native.directories, &batch.directories)
                    .expect("directory rows");
                catalog
                    .native
                    .directories
                    .flush()
                    .expect("flush directories");
            }
            if committed_steps >= 2 {
                NativeCatalog::write_identities(&catalog.native.files, &batch.files)
                    .expect("file rows");
                catalog.native.files.flush().expect("flush files");
            }
            if committed_steps >= 3 {
                catalog
                    .native
                    .write_metadata(&batch.after)
                    .expect("counters");
                catalog.native.metadata.flush().expect("flush counters");
            }
        }
        assert!(Catalog::needs_recovery(home.path()).expect("pending batch"));
        assert!(Catalog::open(home.path(), true).is_err());
        {
            let mut catalog = Catalog::open(home.path(), false).expect("recovered catalog");
            assert_eq!(
                catalog
                    .file_id(Path::new("src/a.rs"))
                    .expect("lookup")
                    .expect("id")
                    .get(),
                1
            );
            assert_eq!(
                catalog
                    .directory_id(Path::new("src"))
                    .expect("lookup")
                    .expect("id")
                    .get(),
                0
            );
            assert_eq!(
                catalog
                    .resolve_file_ids(&paths(&["src/a.rs", "new.rs"]))
                    .expect("next allocation")
                    .iter()
                    .map(|id| id.get())
                    .collect::<Vec<_>>(),
                [1, 2]
            );
        }
        assert!(!Catalog::needs_recovery(home.path()).expect("clean catalog"));
        assert!(Catalog::open(home.path(), true).is_ok());
    }
}

#[test]
fn rejects_redo_that_would_reassign_an_existing_path() {
    let home = tempfile::tempdir().expect("workspace home");
    {
        let mut catalog = Catalog::open(home.path(), false).expect("catalog");
        catalog
            .resolve_file_ids(&paths(&["src/a.rs"]))
            .expect("existing identity");
        let mut batch = reserved_batch(&catalog);
        batch.directories.clear();
        batch.after.next_directory_id = batch.before.next_directory_id;
        atomic_write(
            &catalog.path.join(PENDING_FILE),
            &encode_json(&batch).expect("journal"),
        )
        .expect("corrupt journal");
    }
    assert!(Catalog::open(home.path(), false).is_err());
    assert!(
        home.path()
            .join(CATALOG_DIRECTORY)
            .join(PENDING_FILE)
            .exists()
    );
}
