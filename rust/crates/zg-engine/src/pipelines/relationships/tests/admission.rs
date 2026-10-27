use super::*;

#[tokio::test]
async fn missing_graph_errors_without_creating_a_database() {
    let fixture = Fixture::new();
    fs::remove_file(&fixture.graph_path).expect("remove graph fixture");
    let engine = &fixture.engine;
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
}

#[tokio::test]
async fn lock_timeouts_cancellation_and_closed_engines_reject_reads() {
    let fixture = Fixture::new();
    let engine = &fixture.engine;
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
    assert_eq!(fixture.callers("run").await.matches.len(), 3);
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
        ("::".into(), None),
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
    }
    let missing = engine
        .callers(RelationshipOptions {
            root: Some(directory.path().to_path_buf()),
            symbol: "run".into(),
            ..RelationshipOptions::default()
        })
        .await
        .expect_err("index required");
    assert!(missing.message().contains("zg --index"));
    assert!(!directory.path().join(".zvec-grep").exists());
    engine.close();
}

#[tokio::test]
async fn relationship_queries_borrow_incremental_writers_even_when_registered_late() {
    let fixture = Fixture::new();
    let indexing = WorkspaceIndexService::new();
    let session = fixture.writer_session();
    let options = RelationshipOptions {
        lock_timeout_ms: Some(2_000),
        ..fixture.options("red::run")
    };
    let (groups, writer) = tokio::join!(
        service::query(&indexing, &options, None, RelationshipDirection::Callers),
        async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            indexing.writers.register(session, true)
        }
    );
    let groups = groups
        .expect("borrow newly registered writer while home lock remains held")
        .matches;
    assert_eq!(groups.len(), 1);
    assert!(groups[0].truncated);
    let outgoing = RelationshipOptions {
        lock_timeout_ms: Some(0),
        ..fixture.options("invoke")
    };
    let groups = service::query(&indexing, &outgoing, None, RelationshipDirection::Callees)
        .await
        .expect("borrow active writer without waiting for workspace lock")
        .matches;
    assert!(groups[0].truncated);
    let signal = CancellationToken::new();
    signal.cancel();
    let cancelled = RelationshipOptions {
        signal: Some(signal),
        ..outgoing.clone()
    };
    let error = service::query(&indexing, &cancelled, None, RelationshipDirection::Callees)
        .await
        .expect_err("cancel before borrowing");
    assert_eq!(error.code(), EngineError::CANCELLED);
    writer.retire().await;
    let error = service::query(&indexing, &outgoing, None, RelationshipDirection::Callees)
        .await
        .expect_err("retired writer cannot be borrowed");
    assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
    writer.session.storage.close().expect("close writer");
}

#[tokio::test]
async fn relationship_queries_never_borrow_unpublished_generations() {
    let fixture = Fixture::new();
    let indexing = WorkspaceIndexService::new();
    let manifest = fixture.manifest();
    let lock = Arc::new(
        acquire_home_lock(&fixture.home, LockMode::Write, "rebuild").expect("writer lock"),
    );
    for registered in [false, true] {
        let mut unpublished = manifest.clone();
        unpublished.storage_generation = Some(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(unpublished.storage_home()).expect("rebuild directory");
        let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: unpublished.storage_home(),
            embeddings: vec![schema()],
        })
        .expect("unpublished storage");
        let writer = indexing.writers.register(
            WriterSession::new(storage, Vec::new(), unpublished, Arc::clone(&lock)),
            registered,
        );
        let options = RelationshipOptions {
            lock_timeout_ms: Some(0),
            ..fixture.options("run")
        };
        let error = service::query(&indexing, &options, None, RelationshipDirection::Callers)
            .await
            .expect_err("do not query an unpublished index even if registered");
        assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
        writer.retire().await;
        writer.session.storage.close().expect("close rebuild");
    }
}

#[tokio::test]
async fn waiting_relationship_reads_leave_the_async_worker_free_and_retain_the_writer_lease() {
    let fixture = Fixture::new();
    let indexing = Arc::new(WorkspaceIndexService::new());
    let session = fixture.writer_session();
    let writer = indexing.writers.register(session, true);
    let session = Arc::clone(&writer.session);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocking_writer = std::thread::spawn(move || {
        session
            .storage
            .with_relationship_read(|_| {
                ready_tx.send(()).expect("signal locked store");
                let _ = release_rx.recv_timeout(Duration::from_secs(3));
                Ok(())
            })
            .expect("hold store lock");
    });
    ready_rx.recv().expect("writer acquired store");
    let options = fixture.options("red::run");
    let query = tokio::spawn(async move {
        service::query(&indexing, &options, None, RelationshipDirection::Callers).await
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        !query.is_finished(),
        "native read is still waiting for the store"
    );
    // Cancelling the async waiter cannot release the native task's writer lease.
    query.abort();
    assert!(query.await.expect_err("aborted query").is_cancelled());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), writer.retire())
            .await
            .is_err()
    );
    release_tx
        .send(())
        .expect("Tokio worker was not blocked until the writer timed out");
    blocking_writer.join().expect("writer thread");
    tokio::time::timeout(Duration::from_secs(3), writer.retire())
        .await
        .expect("native query releases lease");
    writer.session.storage.close().expect("close writer");
}
