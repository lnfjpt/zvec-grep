use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use serde_json::{Value, json};
use tempfile::tempdir;
use zg_engine::{
    EngineError, ZvecGrep,
    api::{
        context::{
            ContextOptions,
            options::{ContextRoute, ContextRouteMode},
            result::ContextItemStatus,
        },
        index::{IndexOptions, options::WorkspaceChange},
        info::InfoOptions,
    },
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn public_engine_persists_searches_updates_and_drops_real_storage() -> TestResult {
    let temporary = tempfile::Builder::new()
        .prefix("engine storage ")
        .tempdir()?;
    // Windows canonical paths have a verbatim prefix; the native boundary must handle it.
    let canonical_root = fs::canonicalize(temporary.path())?;
    let root = canonical_root.as_path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    fs::write(
        root.join("auth.rs"),
        "/// Orchard authentication policy.\npub fn authenticate() -> bool { true }\n",
    )?;
    fs::write(
        root.join("billing.md"),
        "# Billing\n\nInvoices and monthly payments.\n",
    )?;
    fs::write(
        root.join("tmp"),
        "A readable extensionless office memorandum.\n",
    )?;
    fs::write(root.join("opaque"), [0_u8, 255, 0, 128])?;
    fs::write(root.join("state.sqlite"), b"SQLite format 3\0")?;
    fs::create_dir(root.join("nested"))?;

    let engine = ZvecGrep::new();
    let initial = engine.index(index_options(root)).await?;
    assert_eq!(initial.generation, 1);
    assert_eq!(initial.files_added, 3, "{initial:?}");
    assert_eq!(initial.files_failed, 0);
    let info = engine.info(info_options(root)).await?;
    assert!(info.indexed);
    assert!(info.index_path.exists());
    assert_eq!(
        info.workspace_index
            .as_ref()
            .and_then(|index| index.generation),
        Some(1)
    );
    assert_eq!(info.status.as_ref().expect("status").files_indexed, 3);
    assert_eq!(
        fts_paths(&engine, root, "orchard").await?,
        [PathBuf::from("auth.rs")]
    );
    assert_eq!(
        fts_paths(&engine, &root.join("nested"), "invoices").await?,
        [PathBuf::from("billing.md")]
    );

    let hybrid = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            query: Some("orchard authentication".to_owned()),
            auto_update: false,
            allow_remote: true,
            ..ContextOptions::default()
        })
        .await?;
    assert!(
        hybrid
            .items
            .iter()
            .any(|item| item.relative_path == Path::new("auth.rs"))
    );
    let calls = server.requests.load(Ordering::Acquire);
    let unchanged = engine.index(index_options(root)).await?;
    assert_eq!(unchanged.files_unchanged, 3);
    assert_eq!(unchanged.generation, 2);
    assert_eq!(server.requests.load(Ordering::Acquire), calls);
    engine.close();
    assert_eq!(
        engine
            .info(info_options(root))
            .await
            .expect_err("closed engine")
            .code(),
        EngineError::RESOURCE_CLOSED
    );
    drop(engine);

    let engine = ZvecGrep::new();
    assert_eq!(
        engine
            .info(info_options(root))
            .await?
            .workspace_index
            .expect("index")
            .generation,
        Some(2)
    );
    assert_eq!(
        fts_paths(&engine, root, "orchard").await?,
        [PathBuf::from("auth.rs")]
    );
    let vector = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Vector,
                query: "orchard authentication".to_owned(),
            }],
            auto_update: false,
            allow_remote: true,
            ..ContextOptions::default()
        })
        .await?;
    assert!(
        vector
            .items
            .iter()
            .any(|item| item.relative_path == Path::new("auth.rs"))
    );

    fs::write(
        root.join("auth.rs"),
        "/// Vineyard session renewal policy.\npub fn renew_session() -> bool { false }\n",
    )?;
    fs::remove_file(root.join("billing.md"))?;
    fs::write(
        root.join("support.txt"),
        "Customer support handles delivery inquiries.\n",
    )?;
    let updated = engine.index(index_options(root)).await?;
    assert_eq!(
        (
            updated.files_added,
            updated.files_modified,
            updated.files_deleted
        ),
        (1, 1, 1)
    );
    assert_eq!(updated.generation, 3);
    assert!(fts_paths(&engine, root, "orchard").await?.is_empty());
    assert!(fts_paths(&engine, root, "invoices").await?.is_empty());
    assert_eq!(
        fts_paths(&engine, root, "vineyard").await?,
        [PathBuf::from("auth.rs")]
    );
    assert_eq!(
        fts_paths(&engine, root, "delivery").await?,
        [PathBuf::from("support.txt")]
    );

    let before_rebuild = engine.info(info_options(root)).await?;
    let rebuilt = engine
        .index(IndexOptions {
            rebuild: true,
            ..index_options(root)
        })
        .await?;
    assert_eq!(
        (rebuilt.files_added, rebuilt.generation),
        (3, updated.generation + 1)
    );
    let after_rebuild = engine.info(info_options(root)).await?;
    assert_eq!(
        after_rebuild
            .workspace_index
            .as_ref()
            .expect("rebuilt workspace")
            .id,
        before_rebuild
            .workspace_index
            .as_ref()
            .expect("original workspace")
            .id,
    );
    assert_ne!(after_rebuild.index_path, before_rebuild.index_path);
    assert!(after_rebuild.index_path.is_dir());
    assert!(!before_rebuild.index_path.exists());
    let rebuilt_query = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Vector,
                query: "vineyard session renewal".to_owned(),
            }],
            auto_update: false,
            allow_remote: true,
            ..ContextOptions::default()
        })
        .await?;
    assert!(
        rebuilt_query
            .items
            .iter()
            .any(|item| item.relative_path == Path::new("auth.rs"))
    );

    assert!(engine.drop_index(info_options(root)).await?);
    assert!(!engine.drop_index(info_options(root)).await?);
    assert!(!info.index_path.exists());
    assert!(!after_rebuild.index_path.exists());
    assert!(!engine.info(info_options(root)).await?.indexed);
    assert!(root.join("auth.rs").is_file());

    configure_remote_model(root, server.address)?;
    engine.index(index_options(root)).await?;
    let orphaned_index_path = engine.info(info_options(root)).await?.index_path;
    assert!(orphaned_index_path.is_dir());
    fs::remove_file(root.join(".zvec-grep/manifest.json"))?;
    assert!(
        engine.drop_index(info_options(root)).await?,
        "orphaned backend can be removed"
    );
    assert!(!orphaned_index_path.exists());
    assert!(!info.index_path.exists());
    engine.close();
    Ok(())
}

#[tokio::test]
async fn public_engine_resumes_failed_initial_build_without_publishing_it_early() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    let home = root.join(".zvec-grep");
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    fs::write(root.join("broken.txt"), [255_u8, 254, 255])?;
    fs::write(root.join("stable.txt"), "Stable orchard baseline.\n")?;
    let engine = ZvecGrep::new();
    assert!(engine.index(index_options(root)).await.is_err());
    let info = engine.info(info_options(root)).await?;
    assert!(!info.indexed);
    assert!(
        info.status.is_none(),
        "an unpublished stage is not the active index"
    );
    let pending: Value = serde_json::from_slice(&fs::read(home.join("build.json"))?)?;
    let stage = pending["target"]["storageGeneration"]
        .as_str()
        .expect("stage generation");
    let stage_path = fs::canonicalize(home.join("generations").join(stage).join("storage"))?;
    let files = native_file_records(&stage_path)?;
    let failed = files
        .iter()
        .find(|file| file["value"]["relative_path"]["value"] == "broken.txt")
        .expect("failed source retained");
    assert_eq!(failed["value"]["index_status"]["kind"], "failed");
    assert!(
        !failed["value"]["index_status"]["error"]
            .as_str()
            .expect("failure reason")
            .is_empty()
    );
    assert_eq!(
        files
            .iter()
            .filter(|file| file["value"]["index_status"]["kind"] == "indexed")
            .count(),
        1
    );
    let requests = server.requests.load(Ordering::Acquire);
    fs::write(
        root.join("broken.txt"),
        "Recovered readable nebula documentation.\n",
    )?;
    let query = ContextOptions {
        root: Some(root.to_path_buf()),
        routes: vec![ContextRoute {
            mode: ContextRouteMode::Fts,
            query: "nebula".to_owned(),
        }],
        auto_update: true,
        allow_remote: true,
        ..ContextOptions::default()
    };
    assert!(
        engine.context(query.clone()).await.is_err(),
        "query refresh must not publish a stage"
    );
    assert_eq!(server.requests.load(Ordering::Acquire), requests);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(home.join("build.json"))?)?,
        pending
    );
    engine.close();
    drop(engine);

    let engine = ZvecGrep::new();
    let resumed = engine.index(index_options(root)).await?;
    assert_eq!((resumed.files_unchanged, resumed.files_failed), (1, 0));
    assert!(server.requests.load(Ordering::Acquire) > requests);
    assert!(!home.join("build.json").exists());
    let info = engine.info(info_options(root)).await?;
    assert!(info.indexed);
    assert_eq!(
        info.index_path, stage_path,
        "resume publishes the existing stage"
    );
    let status = info.status.expect("published status");
    assert_eq!((status.files_failed, status.files_indexed), (0, 2));
    let result = engine.context(query).await?;
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].relative_path, Path::new("broken.txt"));
    assert_eq!(result.items[0].status, ContextItemStatus::Fresh);
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
async fn failed_native_rebuild_preserves_active_results_until_its_stage_is_resumed() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    let home = root.join(".zvec-grep");
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    fs::write(
        root.join("note.txt"),
        "Orchard documentation remains available.\n",
    )?;
    let engine = ZvecGrep::new();
    let initial = engine.index(index_options(root)).await?;
    let original_path = engine.info(info_options(root)).await?.index_path;
    let original_manifest = fs::read(home.join("manifest.json"))?;
    fs::write(root.join("note.txt"), [255_u8, 254, 255])?;
    assert!(
        engine
            .index(IndexOptions {
                rebuild: true,
                ..index_options(root)
            })
            .await
            .is_err()
    );
    assert_eq!(fs::read(home.join("manifest.json"))?, original_manifest);
    let pending: Value = serde_json::from_slice(&fs::read(home.join("build.json"))?)?;
    let stage = pending["target"]["storageGeneration"]
        .as_str()
        .expect("stage generation");
    let stage_path = fs::canonicalize(home.join("generations").join(stage).join("storage"))?;
    assert_ne!(stage_path, original_path);
    let failed_records = native_file_records(&stage_path)?;
    assert_eq!(failed_records.len(), 1);
    assert_eq!(failed_records[0]["value"]["index_status"]["kind"], "failed");
    let requests = server.requests.load(Ordering::Acquire);
    let old_results = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Fts,
                query: "orchard".into(),
            }],
            auto_update: true,
            allow_remote: true,
            ..ContextOptions::default()
        })
        .await?;
    assert_eq!(old_results.items.len(), 1);
    assert_eq!(old_results.items[0].relative_path, Path::new("note.txt"));
    assert_eq!(
        old_results.items[0].status,
        ContextItemStatus::PossiblyStale
    );
    assert_eq!(server.requests.load(Ordering::Acquire), requests);
    assert_eq!(fs::read(home.join("manifest.json"))?, original_manifest);
    fs::write(
        root.join("note.txt"),
        "Vineyard replacement documentation.\n",
    )?;
    engine.close();
    drop(engine);

    let engine = ZvecGrep::new();
    let resumed = engine.index(index_options(root)).await?;
    assert_eq!(
        (resumed.generation, resumed.files_failed),
        (initial.generation + 1, 0)
    );
    assert!(!home.join("build.json").exists());
    assert_eq!(
        engine.info(info_options(root)).await?.index_path,
        stage_path
    );
    assert!(!original_path.exists());
    assert!(fts_paths(&engine, root, "orchard").await?.is_empty());
    assert_eq!(
        fts_paths(&engine, root, "vineyard").await?,
        [PathBuf::from("note.txt")]
    );
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn public_engine_recovers_pending_files_without_skipping_unchanged_sources() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    let source_path = root.join("note.txt");
    let contents = "Orchard documentation survives interrupted indexing.\n";
    fs::write(&source_path, contents)?;
    let modified = fs::metadata(&source_path)?.modified()?;

    let engine = ZvecGrep::new();
    let initial = engine.index(index_options(root)).await?;
    assert_eq!((initial.files_added, initial.files_failed), (1, 0));
    assert_eq!(
        fts_paths(&engine, root, "orchard").await?,
        [PathBuf::from("note.txt")]
    );
    let index_path = engine.info(info_options(root)).await?.index_path;
    let requests = server.requests.load(Ordering::Acquire);
    assert!(requests > 0);
    engine.close();
    drop(engine);

    let files_path = index_path.join("files");
    let source = {
        let files = native_documents(&files_path)?;
        assert_eq!(files.len(), 1);
        files[0].get_string("payload")?.expect("source payload")
    };
    // A completed file can still have a pending marker after a crash before marker removal.
    // The actual journal stores reindex intent, never a claim that the batch is complete.
    let mut pending_source: Value = serde_json::from_str(&source)?;
    pending_source["value"]["index_status"] = json!({"kind": "not_indexed"});
    // Preserve its full snapshot so recovery must override the ordinary unchanged-file fast path.
    let pending_path = index_path.join("pending.json");
    fs::write(
        &pending_path,
        serde_json::to_vec(&json!({
            "version": 1,
            "files": [{ "kind": "reindex", "source": serde_json::to_string(&pending_source)? }],
        }))?,
    )?;

    let engine = ZvecGrep::new();
    let status = engine
        .info(info_options(root))
        .await?
        .status
        .expect("status");
    assert_eq!(
        (
            status.files_pending,
            status.files_indexed,
            status.files_failed
        ),
        (1, 0, 0)
    );
    assert!(
        !pending_path.exists(),
        "recovery flushes and clears the marker"
    );
    assert!(fts_paths(&engine, root, "orchard").await?.is_empty());
    assert_eq!(server.requests.load(Ordering::Acquire), requests);
    engine.close();
    drop(engine);

    let files = native_documents(&files_path)?;
    assert_eq!(
        files.len(),
        1,
        "recovery preserves the source for reindexing"
    );
    let original: Value = serde_json::from_str(&source)?;
    assert_eq!(original["value"]["index_status"]["kind"], "indexed");
    let recovered: Value = serde_json::from_str(
        &files[0]
            .get_string("payload")?
            .expect("recovered file payload"),
    )?;
    assert_eq!(recovered["version"], original["version"]);
    for field in ["id", "relative_path", "formats", "snapshot"] {
        assert_eq!(
            recovered["value"][field], original["value"][field],
            "recovery preserves {field}"
        );
    }
    assert_eq!(
        recovered["value"]["index_status"],
        json!({"kind": "not_indexed"})
    );
    let collections = fs::read_dir(&index_path)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                matches!(name, "entities" | "fragments") || name.starts_with("vectors_")
            })
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert_eq!(collections.len(), 3);
    for collection in collections {
        assert!(
            native_documents(&collection)?.is_empty(),
            "recovery clears searchable records in {}",
            collection.display()
        );
    }
    assert_eq!(fs::read_to_string(&source_path)?, contents);
    assert_eq!(fs::metadata(&source_path)?.modified()?, modified);
    assert_eq!(server.requests.load(Ordering::Acquire), requests);

    let engine = ZvecGrep::new();
    let recovered = engine.index(index_options(root)).await?;
    assert_eq!((recovered.files_unchanged, recovered.files_failed), (0, 0));
    let after_reindex = server.requests.load(Ordering::Acquire);
    assert!(
        after_reindex > requests,
        "pending sources must be embedded again"
    );
    assert_eq!(
        fts_paths(&engine, root, "orchard").await?,
        [PathBuf::from("note.txt")]
    );
    let status = engine
        .info(info_options(root))
        .await?
        .status
        .expect("status");
    assert_eq!((status.files_pending, status.files_indexed), (0, 1));
    let vector = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Vector,
                query: "orchard documentation".to_owned(),
            }],
            auto_update: false,
            allow_remote: true,
            ..ContextOptions::default()
        })
        .await?;
    assert!(
        vector
            .items
            .iter()
            .any(|item| item.relative_path == Path::new("note.txt"))
    );
    let after_search = server.requests.load(Ordering::Acquire);
    let unchanged = engine.index(index_options(root)).await?;
    assert_eq!(unchanged.files_unchanged, 1);
    assert_eq!(server.requests.load(Ordering::Acquire), after_search);
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
async fn public_engine_reuses_relative_files_after_moving_workspace() -> TestResult {
    let temporary = tempdir()?;
    let original_root = temporary.path().join("original");
    fs::create_dir_all(original_root.join("src"))?;
    let original_root = fs::canonicalize(original_root)?;
    let server = EmbeddingServer::start()?;
    configure_remote_model(&original_root, server.address)?;
    fs::write(
        original_root.join("src/note.txt"),
        "Orchard relocation preserves workspace identity.\n",
    )?;
    let query = |root: &Path, term: &str| ContextOptions {
        root: Some(root.to_path_buf()),
        routes: vec![ContextRoute {
            mode: ContextRouteMode::Fts,
            query: term.to_owned(),
        }],
        auto_update: false,
        allow_remote: true,
        ..ContextOptions::default()
    };
    let engine = ZvecGrep::new();
    engine.index(index_options(&original_root)).await?;
    let before = engine.context(query(&original_root, "orchard")).await?;
    assert_eq!(before.items.len(), 1);
    let entity_id = before.items[0].entity_id.clone();
    assert!(entity_id.is_some());
    engine.close();
    drop(engine);

    let relocated_root = temporary.path().join("relocated");
    fs::rename(&original_root, &relocated_root)?;
    let relocated_root = fs::canonicalize(relocated_root)?;
    let engine = ZvecGrep::new();
    let after = engine
        .context(query(&relocated_root.join("src"), "orchard"))
        .await?;
    assert_eq!(after.items.len(), 1);
    assert_eq!(after.items[0].entity_id, entity_id);
    assert_eq!(after.items[0].relative_path, Path::new("src/note.txt"));
    assert_eq!(
        after.items[0].absolute_path,
        relocated_root.join("src/note.txt")
    );
    assert_eq!(after.items[0].status, ContextItemStatus::Fresh);

    let calls = server.requests.load(Ordering::Acquire);
    let unchanged = engine.index(index_options(&relocated_root)).await?;
    assert_eq!(unchanged.files_unchanged, 1);
    assert_eq!((unchanged.files_added, unchanged.files_deleted), (0, 0));
    assert_eq!(server.requests.load(Ordering::Acquire), calls);

    fs::write(
        relocated_root.join("src/note.txt"),
        "Vineyard updated content.\n",
    )?;
    let updated = engine
        .index(IndexOptions {
            changes: vec![WorkspaceChange::Upsert(PathBuf::from("src/note.txt"))],
            ..index_options(&relocated_root)
        })
        .await?;
    assert_eq!(updated.files_modified, 1);
    let result = engine.context(query(&relocated_root, "vineyard")).await?;
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].entity_id, entity_id);
    engine.drop_index(info_options(&relocated_root)).await?;
    engine.close();
    Ok(())
}

fn native_file_records(index_path: &Path) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
    native_documents(&index_path.join("files"))?
        .iter()
        .map(|document| {
            let payload = document.get_string("payload")?.expect("file payload");
            Ok(serde_json::from_str(&payload)?)
        })
        .collect()
}

fn native_documents(path: &Path) -> Result<Vec<zvec_rust::Doc>, Box<dyn std::error::Error>> {
    #[cfg(windows)]
    let path = dunce::simplified(path);
    let mut options = zvec_rust::CollectionOptions::new()?;
    options.set_read_only(true)?;
    let collection = zvec_rust::Collection::open(
        path.to_str().expect("UTF-8 collection path"),
        Some(&options),
    )?;
    let documents = collection
        .iter_with_options(None, false)?
        .collect::<Result<Vec<_>, _>>()?;
    collection.close()?;
    Ok(documents)
}

fn index_options(root: &Path) -> IndexOptions {
    IndexOptions {
        root: Some(root.to_path_buf()),
        allow_remote: true,
        ..IndexOptions::default()
    }
}

fn info_options(root: &Path) -> InfoOptions {
    InfoOptions {
        root: Some(root.to_path_buf()),
        include_status: true,
    }
}

async fn fts_paths(
    engine: &ZvecGrep,
    root: &Path,
    query: &str,
) -> Result<Vec<PathBuf>, EngineError> {
    let result = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Fts,
                query: query.to_owned(),
            }],
            auto_update: false,
            allow_remote: true,
            ..ContextOptions::default()
        })
        .await?;
    let mut paths = result
        .items
        .into_iter()
        .map(|item| item.relative_path)
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn configure_remote_model(root: &Path, address: SocketAddr) -> std::io::Result<()> {
    let home = root.join(".zvec-grep");
    fs::create_dir_all(&home)?;
    // Seed credentials in a configuration fixture without changing process-wide environment.
    let manifest = json!({
        "manifestVersion": 1, "id": "fixture-workspace", "name": "fixture", "path": home,
        "rootPaths": [{ "absolutePath": root, "recursive": true }],
        "indexPolicy": "enabled", "embedding": { "provider": "qwen", "model": "text-embedding-v4", "dimension": 1024, "metric": "cosine" },
        "indexVersion": null, "createdTime": 1, "updatedTime": 1,
        "embeddingRuntime": { "apiKey": "local-test-key", "endpoint": format!("http://{address}/embeddings") }
    });
    fs::write(home.join("manifest.json"), serde_json::to_vec(&manifest)?)
}

struct EmbeddingServer {
    address: SocketAddr,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl EmbeddingServer {
    fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let worker = thread::spawn({
            let stop = Arc::clone(&stop);
            let requests = Arc::clone(&requests);
            move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    requests.fetch_add(1, Ordering::Release);
                    respond(stream.expect("mock HTTP connection"))
                        .expect("mock embedding response");
                }
            }
        });
        Ok(Self {
            address,
            requests,
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for EmbeddingServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                assert!(result.is_ok(), "mock embedding server failed");
            }
        }
    }
}

fn respond(mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut request = Vec::new();
    let mut buffer = [0; 4096];
    let (header_end, content_length) = loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        request.extend_from_slice(&buffer[..count]);
        if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
            assert!(headers.contains("authorization: bearer local-test-key"));
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .expect("content length")
                .trim()
                .parse::<usize>()
                .expect("valid content length");
            break (end + 4, length);
        }
    };
    while request.len() < header_end + content_length {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        request.extend_from_slice(&buffer[..count]);
    }
    let body: Value = serde_json::from_slice(&request[header_end..header_end + content_length])?;
    let dimension = usize::try_from(body["dimensions"].as_u64().expect("dimensions"))
        .expect("usize dimensions");
    let data = body["input"]
        .as_array()
        .expect("text inputs")
        .iter()
        .enumerate()
        .map(|(index, text)| {
            let mut vector = vec![0.0_f32; dimension];
            for word in text
                .as_str()
                .expect("text")
                .split(|character: char| !character.is_alphanumeric())
                .filter(|word| !word.is_empty())
            {
                let hash = word.to_lowercase().bytes().fold(0usize, |hash, byte| {
                    hash.wrapping_mul(31).wrapping_add(usize::from(byte))
                });
                vector[hash % dimension] += 1.0;
            }
            json!({ "index": index, "embedding": vector })
        })
        .collect::<Vec<_>>();
    let response = serde_json::to_vec(&json!({ "data": data }))?;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.len()
    )?;
    stream.write_all(&response)
}
