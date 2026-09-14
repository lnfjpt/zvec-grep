//! Keep process termination tests in a separate executable: zvec's native lock
//! descriptors can otherwise be inherited from concurrently running unit tests.
mod support;

use std::{
    collections::BTreeSet,
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use support::{
    EmbeddingServer, configure_remote_model, index_options, info_options, native_documents,
    native_file_records,
};
use zg_engine::{
    ZvecGrep,
    api::{
        context::{
            ContextOptions,
            options::{ContextRoute, ContextRouteMode},
        },
        index::{
            IndexOptions,
            progress::{IndexProgressPhase, IndexProgressReporter},
        },
    },
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
const CRASH_FIXTURE_ROOT_ENV: &str = "ZG_ENGINE_STORAGE_CHECKPOINT_CRASH_FIXTURE_ROOT";
const CHECKPOINT_FILES: usize = 64;
const PENDING_FILES: usize = 6;
const TOTAL_FILES: usize = CHECKPOINT_FILES + PENDING_FILES;

#[tokio::test]
async fn checkpoint_crash_fixture() -> TestResult {
    let Some(root) = std::env::var_os(CRASH_FIXTURE_ROOT_ENV) else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    let engine = ZvecGrep::new();
    // Publish an empty initial generation so the interrupted write updates the
    // active index, exercising storage recovery rather than build publication.
    assert_eq!(engine.index(index_options(&root)).await?.files_added, 0);
    let index_path = engine.info(info_options(&root)).await?.index_path;
    for index in 0..TOTAL_FILES {
        fs::write(
            root.join(format!("crash-{index:03}.txt")),
            format!("Orchard document {index}.\n"),
        )?;
    }
    let ready = root.join(".zvec-grep/writer-ready.json");
    engine
        .index(IndexOptions {
            on_progress: Some(IndexProgressReporter::new(move |progress| {
                if progress.phase == IndexProgressPhase::Indexing
                    && progress.files_indexed == Some(TOTAL_FILES)
                {
                    assert_eq!(progress.files_failed, Some(0));
                    fs::write(&ready, serde_json::to_vec(&index_path).expect("index path"))
                        .expect("publish writer readiness");
                    // This callback runs immediately after the write, before final
                    // checkpointing. The parent must kill the still-live writer.
                    loop {
                        thread::park();
                    }
                }
            })),
            ..index_options(&root)
        })
        .await?;
    panic!("fixture completed without stopping at the uncheckpointed batch");
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "keep crash recovery and selective reindex assertions together"
)]
async fn recovers_only_the_uncheckpointed_batch_after_process_termination() -> TestResult {
    let directory = tempfile::Builder::new()
        .prefix("zg-storage-crash-")
        .tempdir()?;
    let root = directory.path().join("workspace");
    fs::create_dir(&root)?;
    let root = fs::canonicalize(root)?;
    let server = EmbeddingServer::start()?;
    configure_remote_model(&root, server.address)?;

    // No zvec collection is opened in this process before spawn. The only other
    // test in this executable is the env-guarded child fixture above.
    let log_path = directory.path().join("child.log");
    let mut child = spawn_writer(&root, &log_path)?;
    let index_path = wait_until_ready(&mut child, &root, &log_path)?;
    let pending_path = index_path.join("pending.json");
    let pending: Value = serde_json::from_slice(&fs::read(&pending_path)?)?;
    let pending_sources = pending["files"]
        .as_array()
        .expect("pending source records")
        .iter()
        .map(|change| {
            assert_eq!(change["kind"], "reindex");
            serde_json::from_str::<Value>(change["source"].as_str().expect("source payload"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(pending_sources.len(), PENDING_FILES);
    // Completion order may vary with embedding concurrency; the persisted intent
    // identifies the actual unfinished files rather than assuming filename order.
    let pending_names = pending_sources
        .iter()
        .map(source_name)
        .collect::<BTreeSet<_>>();
    let indexed_names = (0..TOTAL_FILES)
        .map(|index| format!("crash-{index:03}.txt"))
        .filter(|name| !pending_names.contains(name))
        .collect::<BTreeSet<_>>();
    child.0.kill()?;
    assert!(
        !child.0.wait()?.success(),
        "writer must end through forced termination"
    );
    drop(child);
    let child_log = fs::read_to_string(&log_path)?;
    assert_eq!(
        server.inputs.load(Ordering::Acquire),
        TOTAL_FILES,
        "{child_log}"
    );

    let recovery_requests = server.requests.load(Ordering::Acquire);
    let engine = ZvecGrep::new();
    let info = engine
        .info(info_options(&root))
        .await
        .unwrap_or_else(|error| panic!("open after abrupt termination: {error}\n{child_log}"));
    let status = info.status.expect("recovered status");
    assert_eq!(
        (
            status.files_stored,
            status.files_indexed,
            status.files_pending,
            status.files_failed
        ),
        (TOTAL_FILES, CHECKPOINT_FILES, PENDING_FILES, 0),
        "{child_log}"
    );
    assert_eq!(status.entities_indexed, u64::try_from(CHECKPOINT_FILES)?);
    assert!(!pending_path.exists(), "successful recovery clears intent");
    assert_eq!(
        server.inputs.load(Ordering::Acquire),
        TOTAL_FILES,
        "recovery does not embed"
    );

    assert_eq!(server.requests.load(Ordering::Acquire), recovery_requests);
    let files = native_file_records(&index_path)?;
    assert_eq!(files.len(), TOTAL_FILES);
    for file in &files {
        if let Some(source) = pending_sources
            .iter()
            .find(|source| source_name(source) == source_name(file))
        {
            assert_eq!(
                file, source,
                "recovery preserves the complete source snapshot"
            );
            assert_eq!(
                file["value"]["index_status"],
                json!({"kind": "not_indexed"})
            );
        } else {
            assert_eq!(file["value"]["index_status"]["kind"], "indexed");
            assert_eq!(file["value"]["index_status"]["entity_count"], 1);
        }
    }
    let indexed_source_ids = native_documents(&index_path.join("files"))?
        .into_iter()
        .filter_map(|doc| {
            let file: Value = serde_json::from_str(
                &doc.get_string("payload")
                    .expect("payload")
                    .expect("file payload"),
            )
            .expect("file JSON");
            indexed_names.contains(&source_name(&file)).then(|| {
                doc.get_string("source_id")
                    .expect("source ID")
                    .expect("file source ID")
            })
        })
        .collect::<BTreeSet<_>>();
    let mut searchable_collections = 0;
    for entry in fs::read_dir(&index_path)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if matches!(name, "entities" | "fragments") || name.starts_with("vectors_") {
            searchable_collections += 1;
            let documents = native_documents(&entry.path())?;
            assert_eq!(documents.len(), CHECKPOINT_FILES, "{name}");
            let source_ids = documents
                .iter()
                .map(|doc| {
                    doc.get_string("source_id")
                        .expect("source ID")
                        .expect("document source ID")
                })
                .collect::<BTreeSet<_>>();
            assert_eq!(
                source_ids, indexed_source_ids,
                "{name}: unfinished records must be removed"
            );
        }
    }
    assert_eq!(searchable_collections, 3);
    for mode in [ContextRouteMode::Fts, ContextRouteMode::Vector] {
        assert_eq!(
            search_paths(&engine, &root, mode, Vec::new()).await?,
            indexed_names
        );
        assert!(
            search_paths(
                &engine,
                &root,
                mode,
                pending_names.iter().cloned().collect()
            )
            .await?
            .is_empty()
        );
    }
    let before_reindex = server.inputs.load(Ordering::Acquire);
    let resumed = engine.index(index_options(&root)).await?;
    assert_eq!(
        (
            resumed.files_unchanged,
            resumed.files_pending,
            resumed.files_failed
        ),
        (CHECKPOINT_FILES, PENDING_FILES, 0)
    );
    assert_eq!(
        server.inputs.load(Ordering::Acquire) - before_reindex,
        PENDING_FILES,
        "only unfinished files require embedding again"
    );
    let status = engine
        .info(info_options(&root))
        .await?
        .status
        .expect("resumed status");
    assert_eq!(
        (status.files_indexed, status.files_pending),
        (TOTAL_FILES, 0)
    );
    for mode in [ContextRouteMode::Fts, ContextRouteMode::Vector] {
        assert_eq!(
            search_paths(&engine, &root, mode, Vec::new()).await?,
            indexed_names.union(&pending_names).cloned().collect()
        );
    }
    engine.close();
    Ok(())
}

fn source_name(source: &Value) -> String {
    source["value"]["relative_path"]["value"]
        .as_str()
        .expect("UTF-8 source path")
        .to_owned()
}

async fn search_paths(
    engine: &ZvecGrep,
    root: &Path,
    mode: ContextRouteMode,
    include_paths: Vec<String>,
) -> TestResult<BTreeSet<String>> {
    Ok(engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode,
                query: "orchard".to_owned(),
            }],
            limit: Some(TOTAL_FILES),
            include_paths,
            auto_update: false,
            allow_remote: true,
            ..ContextOptions::default()
        })
        .await?
        .items
        .into_iter()
        .map(|item| {
            item.relative_path
                .to_str()
                .expect("UTF-8 fixture path")
                .to_owned()
        })
        .collect())
}

struct CrashFixtureChild(Child);

impl Drop for CrashFixtureChild {
    fn drop(&mut self) {
        // Reap the child even if readiness or an assertion fails.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_writer(root: &Path, log_path: &Path) -> TestResult<CrashFixtureChild> {
    let output = File::create(log_path)?;
    let errors = output.try_clone()?;
    Ok(CrashFixtureChild(
        Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "checkpoint_crash_fixture",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CRASH_FIXTURE_ROOT_ENV, root)
            .stdin(Stdio::null())
            .stdout(output)
            .stderr(errors)
            .spawn()?,
    ))
}

fn wait_until_ready(
    child: &mut CrashFixtureChild,
    root: &Path,
    log_path: &Path,
) -> TestResult<PathBuf> {
    let started = Instant::now();
    let ready = root.join(".zvec-grep/writer-ready.json");
    loop {
        if let Some(status) = child.0.try_wait()? {
            panic!(
                "crash fixture exited before termination: {status}\n{}",
                fs::read_to_string(log_path).unwrap_or_default()
            );
        }
        // The small readiness file may still be in the middle of being written.
        if let Ok(bytes) = fs::read(&ready)
            && let Ok(path) = serde_json::from_slice(&bytes)
        {
            return Ok(path);
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "timed out waiting for crash fixture\n{}",
            fs::read_to_string(log_path).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(20));
    }
}
