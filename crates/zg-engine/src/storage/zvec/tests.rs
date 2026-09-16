use super::*;
use crate::domain::{FileFormat, FileIndexStatus, FileSnapshot};

fn file(id: u64, path: impl Into<PathBuf>) -> FileRecord {
    FileRecord {
        id: FileId::new(id),
        relative_path: crate::domain::SourcePath::new(path).expect("source path"),
        formats: vec![FileFormat::Unknown],
        snapshot: FileSnapshot {
            size_bytes: 0,
            modified_epoch_ms: None,
            content_hash: None,
        },
        index_status: FileIndexStatus::NotIndexed,
    }
}

#[test]
fn source_file_projection_preserves_paths_and_directory_membership_for_all_statuses() {
    super::super::backend::initialize().expect("initialize zvec");
    let directories = [DirectoryId::new(3), DirectoryId::new(8)];
    let mut source = file(12, Path::new("src").join("nested").join("name.rs"));
    source.snapshot.content_hash = Some("fixture-hash".into());
    for status in [
        FileIndexStatus::NotIndexed,
        FileIndexStatus::Failed {
            error: "extractor unavailable".into(),
        },
        FileIndexStatus::Indexed {
            indexed_epoch_ms: 1,
            entity_count: 0,
        },
    ] {
        source.index_status = status;
        let doc = encode_file_doc(&source, &directories).expect("encode source");
        assert_eq!(doc.get_pk(), Some("f12"));
        assert_eq!(
            string_field(&doc, "relative_path").expect("query path"),
            "src/nested/name.rs"
        );
        assert_eq!(
            string_field(&doc, "file_name").expect("file name"),
            "name.rs"
        );
        assert_eq!(
            doc.get_array_u64("ancestor_directory_ids")
                .expect("directories"),
            Some(vec![3, 8])
        );
        assert_eq!(decode_file_doc(&doc).expect("decode source"), source);
    }
    let root_file = file(13, "main.rs");
    let root = encode_file_doc(&root_file, &[]).expect("root source");
    assert_eq!(
        root.get_array_u64("ancestor_directory_ids")
            .expect("root directories")
            .unwrap_or_default(),
        Vec::<u64>::new()
    );
}

#[test]
fn path_enumeration_reads_the_complete_projection_without_decoding_payloads() {
    super::super::backend::initialize().expect("initialize zvec");
    let temporary = tempfile::tempdir().expect("temporary storage");
    let storage_path = temporary.path().join("storage");
    std::fs::create_dir(&storage_path).expect("storage directory");
    let store = NativeStore::open(
        &storage_path,
        &WorkspaceIndexEmbeddingSchema {
            provider: "fixture".into(),
            model: "fixture".into(),
            dimension: 3,
            metric: EmbeddingMetric::Cosine,
        },
        false,
    )
    .expect("open storage");
    let mut expected = Vec::new();
    let mut docs = Vec::new();
    for index in 1..=WRITE_BATCH + 7 {
        let source = file(
            u64::try_from(index).expect("ID"),
            format!("file-{index}.rs"),
        );
        let mut doc = encode_file_doc(&source, &[]).expect("encode source");
        doc.add_string("payload", "invalid full file payload")
            .expect("replace payload");
        docs.push(doc);
        expected.push((source.id, source.relative_path.into_path_buf()));
    }
    write_docs(&store.files, &docs, "write source").expect("write projections");
    let mut actual = store.list_file_paths().expect("read lightweight paths");
    actual.sort_unstable();
    expected.sort_unstable();
    assert_eq!(actual, expected);
    assert!(store.list_files().is_err());
}

#[cfg(unix)]
#[test]
fn non_unicode_file_projection_keeps_native_path_without_a_lossy_query_value() {
    use std::os::unix::ffi::OsStringExt;

    super::super::backend::initialize().expect("initialize zvec");
    let path = PathBuf::from(std::ffi::OsString::from_vec(b"src/\xff.rs".to_vec()));
    let source = file(9, path.clone());
    let doc = encode_file_doc(&source, &[DirectoryId::new(2)]).expect("encode non-Unicode source");
    assert!(!doc.has_field("relative_path"));
    // The native getter exposes an empty STRING as None, despite the field
    // being present and non-null. It is only a disabled query projection.
    assert!(doc.has_field("file_name"));
    assert!(!doc.is_field_null("file_name"));
    assert_eq!(
        doc.get_string("file_name")
            .expect("name")
            .unwrap_or_default(),
        ""
    );
    assert_eq!(
        decode_file_path_doc(&doc).expect("native path"),
        (source.id, path)
    );
    assert_eq!(decode_file_doc(&doc).expect("full source"), source);
}

#[test]
fn full_file_decode_rejects_a_path_projection_from_another_file() {
    super::super::backend::initialize().expect("initialize zvec");
    let source = file(2, "first.rs");
    let mut doc = encode_file_doc(&source, &[]).expect("encode source");
    doc.add_string(
        "path",
        &encode_path(&crate::domain::SourcePath::new("second.rs").expect("source path"))
            .expect("path"),
    )
    .expect("replace path projection");
    assert!(decode_file_doc(&doc).is_err());
}

#[test]
fn full_range_ids_support_native_queries_membership_and_deletion() {
    super::super::backend::initialize().expect("initialize zvec");
    let temporary = tempfile::tempdir().expect("temporary storage");
    let collection = open_collection(
        &temporary.path().join("files"),
        &files_schema().expect("file schema"),
        false,
    )
    .expect("file collection");
    let ids = [0, 1, i64::MAX as u64 + 1, u64::MAX - 1, u64::MAX];
    let sources: Vec<_> = ids
        .into_iter()
        .map(|id| file(id, format!("source-{id}.rs")))
        .collect();
    let docs: Vec<_> = sources
        .iter()
        .map(|source| {
            encode_file_doc(source, &[DirectoryId::new(source.id.get())]).expect("source document")
        })
        .collect();
    write_docs(&collection, &docs, "write source IDs").expect("write source IDs");
    collection.flush().expect("flush IDs");
    for (value, expected_count) in [(true, ids.len()), (false, 0)] {
        let mut query = SearchQuery::scalar(10).expect("constant query");
        query
            .set_filter(&constant_filter(value))
            .expect("constant filter");
        assert_eq!(
            collection.query(&query).expect("constant results").len(),
            expected_count
        );
    }
    for source in &sources {
        for filter in [
            format!("file_id = {}", source.id),
            format!("file_id IN ({})", source.id),
            format!("ancestor_directory_ids CONTAIN_ANY ({})", source.id),
        ] {
            let mut query = SearchQuery::scalar(10).expect("ID query");
            query.set_filter(&filter).expect("ID filter");
            let docs = collection.query(&query).expect("query ID");
            assert_eq!(docs.len(), 1, "{filter}");
            assert_eq!(decode_file_doc(&docs[0]).expect("decode source"), *source);
        }
    }
    collection
        .delete_by_filter(&format!("file_id = {}", u64::MAX))
        .expect("delete maximum ID");
    let query = SearchQuery::scalar(10).expect("remaining sources");
    let mut remaining: Vec<_> = collection
        .query(&query)
        .expect("query after deletion")
        .iter()
        .map(|doc| {
            decode_file_doc(doc)
                .expect("decode remaining source")
                .id
                .get()
        })
        .collect();
    remaining.sort_unstable();
    assert_eq!(remaining, ids[..ids.len() - 1]);
}
