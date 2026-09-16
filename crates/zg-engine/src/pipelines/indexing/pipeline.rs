use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use futures_util::{StreamExt, stream::FuturesUnordered};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use zg_host_native::{
    DiscoveredFile, DiscoveryOptions as HostDiscoveryOptions, HostError, HostErrorSite,
    ReadBatchRequest, RootSpec, ScanRequest, ScanSnapshot, SourceFile as HostSource, TaskControl,
    WorkspaceScannerPort,
};

use crate::{
    EngineError, ErrorSite,
    api::{
        index::{
            options::WorkspaceChange,
            progress::{
                IndexEmbeddingProgress, IndexProgress, IndexProgressPhase, IndexProgressReporter,
            },
            result::{IndexResult, SkippedFile, SkippedFileReason, TimingEntry},
        },
        info::result::{WorkspaceIndexInfo, WorkspaceIndexPolicy, WorkspaceIndexStatus},
    },
    domain::{
        EntityFragment, FileCategory, FileFormat, FileId, FileIndexStatus, FileRecord,
        FileSnapshot, ImageContent, validate_fragments,
    },
    extraction::{
        ImageSource, IndexingExtractionFragment, SourceKind, TextSource, extract_for_indexing,
        source_kind, vector_content_for_fragment,
    },
    models::{
        EmbeddingInput, EmbeddingInputKind, EmbeddingModelInfo, EmbeddingOptions, EmbeddingPurpose,
        EmbeddingResult, ModelError, ModelRuntimeLease,
    },
    storage::spi::{IndexedFragment, WorkspaceIndexStorage},
    utils::{collapse_whitespace, decode_text, sha256_hex},
};

use super::input_budget::index_chunk_options;

const MAX_SKIPPED_FILE_SAMPLES: usize = 20;
const COMMIT_BATCH_FILES: usize = 64;
const EMBEDDING_TRANSIENT_MAX_RETRIES: usize = 3;
const EMBEDDING_RATE_LIMIT_MAX_RETRIES: usize = 6;
const EMBEDDING_TRANSIENT_RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
const EMBEDDING_RATE_LIMIT_RETRY_BASE_DELAY: Duration = Duration::from_secs(2);
const EMBEDDING_TRANSIENT_RETRY_MAX_DELAY: Duration = Duration::from_secs(8);
const EMBEDDING_RATE_LIMIT_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
const EMBEDDING_RETRY_JITTER_MILLIS: u64 = 500;
const EMBEDDING_SUCCESS_STREAK_MIN: usize = 4;

#[async_trait]
pub(crate) trait IndexEmbeddingRuntime: Send + Sync {
    fn info(&self) -> &EmbeddingModelInfo;

    async fn embed(
        &self,
        contents: &[EmbeddingInput],
        options: EmbeddingOptions,
        progress: Option<IndexProgressReporter>,
    ) -> Result<EmbeddingResult, ModelError>;
}

#[async_trait]
impl IndexEmbeddingRuntime for ModelRuntimeLease {
    fn info(&self) -> &EmbeddingModelInfo {
        self.info()
    }

    async fn embed(
        &self,
        contents: &[EmbeddingInput],
        options: EmbeddingOptions,
        progress: Option<IndexProgressReporter>,
    ) -> Result<EmbeddingResult, ModelError> {
        self.embed(contents, options, progress).await
    }
}

pub(crate) struct IndexingContext<'context> {
    pub workspace_index: &'context WorkspaceIndexInfo,
    pub storage: &'context dyn WorkspaceIndexStorage,
    pub scanner: &'context dyn WorkspaceScannerPort,
    pub embedding_model: &'context dyn IndexEmbeddingRuntime,
    pub embedding_concurrency: Option<usize>,
    pub on_progress: Option<IndexProgressReporter>,
    pub signal: Option<CancellationToken>,
    pub changes: &'context [WorkspaceChange],
}

pub(crate) async fn index_workspace(
    context: &IndexingContext<'_>,
) -> Result<IndexResult, EngineError> {
    validate_context(context)?;
    let started = Instant::now();
    let mut timings = TimingCollector::default();

    let first = run_index_pass(context, &mut timings, None).await?;
    let mut passes = vec![first];
    if passes[0].stats.files_failed > 0 {
        let succeeded = passes[0].stats.files_indexed;
        let files_total = passes[0].diff.pending_count();
        report(
            context,
            IndexProgress {
                phase: IndexProgressPhase::Scanning,
                files_total: Some(files_total),
                files_indexed: Some(succeeded),
                files_failed: Some(passes[0].stats.files_failed),
                detail: Some(format!(
                    "retrying {} failed files",
                    passes[0].stats.files_failed
                )),
                embedding: None,
            },
        );
        passes.push(
            run_index_pass(
                context,
                &mut timings,
                Some(ProgressBase {
                    files_succeeded: succeeded,
                    files_total,
                }),
            )
            .await?,
        );
    }

    throw_if_cancelled(context.signal.as_ref())?;
    let final_pass = passes.last().expect("an index pass is always present");
    report(
        context,
        IndexProgress {
            phase: IndexProgressPhase::Indexing,
            files_total: Some(final_pass.diff.pending_count()),
            files_indexed: Some(final_pass.stats.files_indexed + final_pass.stats.files_failed),
            files_failed: Some(final_pass.stats.files_failed),
            detail: Some("finalizing index".to_owned()),
            embedding: None,
        },
    );
    let finalize_started = Instant::now();
    context.storage.finalize_writes().await.map_err(|error| {
        EngineError::storage_failure(format!("failed to finalize index storage: {error}"))
    })?;
    timings.record("index_optimize", finalize_started.elapsed(), 1);

    let result = build_index_result(context, &passes, started.elapsed(), timings);
    if result.files_failed > 0 {
        report(
            context,
            IndexProgress {
                phase: IndexProgressPhase::Done,
                files_total: Some(final_pass.diff.pending_count()),
                files_indexed: Some(final_pass.stats.files_indexed),
                files_failed: Some(result.files_failed),
                detail: Some("indexing completed with failed files".to_owned()),
                embedding: None,
            },
        );
        return Err(EngineError::internal(format!(
            "indexing completed with {} failed files: {}",
            result.files_failed,
            final_pass
                .stats
                .failed_files
                .iter()
                .take(5)
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    report(
        context,
        IndexProgress {
            phase: IndexProgressPhase::Done,
            files_total: Some(final_pass.diff.pending_count()),
            files_indexed: Some(final_pass.stats.files_indexed),
            files_failed: Some(0),
            detail: Some("indexing complete".to_owned()),
            embedding: None,
        },
    );
    Ok(result)
}

pub(crate) async fn get_workspace_index_status(
    workspace_index: &WorkspaceIndexInfo,
    storage: &dyn WorkspaceIndexStorage,
    scanner: &dyn WorkspaceScannerPort,
    signal: Option<CancellationToken>,
) -> Result<WorkspaceIndexStatus, EngineError> {
    let stored_files = storage.list_files()?;
    let control = task_control(signal);
    let snapshot = scanner
        .discover(
            &ScanRequest {
                roots: vec![host_root(workspace_index)],
                scope_paths: Vec::new(),
            },
            &control,
        )
        .await
        .map_err(map_host_error)?;
    let (discovered_files, _) =
        classify_files(workspace_index, snapshot.files, &stored_files, &control).await?;
    let mut diff = compute_diff(discovered_files, &stored_files);
    resolve_status_modifications(scanner, &control, &mut diff).await?;

    let pending_files = stored_files
        .iter()
        .filter(|file| !file.index_status.is_indexed())
        .collect::<Vec<_>>();
    let indexed_files = stored_files
        .iter()
        .filter(|file| file.index_status.is_indexed())
        .collect::<Vec<_>>();

    Ok(WorkspaceIndexStatus {
        files_scanned: diff.files_scanned,
        files_stored: stored_files.len(),
        files_indexed: indexed_files.len(),
        entities_indexed: indexed_files
            .iter()
            .map(|file| file.index_status.entity_count())
            .sum(),
        indexed_size_bytes: indexed_files
            .iter()
            .map(|file| file.snapshot.size_bytes)
            .sum(),
        files_pending: pending_files.len(),
        files_failed: pending_files
            .iter()
            .filter(|file| file.index_status.error().is_some())
            .count(),
        files_added: diff.added,
        files_modified: diff.modified,
        files_deleted: diff.deleted.len(),
        files_unchanged: diff.unchanged,
    })
}

#[derive(Clone, Copy)]
struct ProgressBase {
    files_succeeded: usize,
    files_total: usize,
}

struct IndexPassResult {
    files_scanned: usize,
    diff: DiffPlan,
    stats: IndexStats,
    skipped: Vec<SkippedFile>,
}

#[derive(Default)]
struct IndexStats {
    files_indexed: usize,
    files_failed: usize,
    entities_created: usize,
    failed_files: Vec<PathBuf>,
    failed_reasons: Vec<String>,
}

#[derive(Default)]
struct DiffPlan {
    files_scanned: usize,
    added: usize,
    modified: usize,
    pending: usize,
    unchanged: usize,
    deleted: Vec<FileRecord>,
    candidates: Vec<IndexCandidate>,
}

impl DiffPlan {
    const fn pending_count(&self) -> usize {
        self.added + self.modified + self.pending
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CandidateKind {
    Added,
    Modified,
    Pending,
}

struct IndexCandidate {
    kind: CandidateKind,
    file: FileRecord,
    discovered: DiscoveredFile,
    existing: Option<FileRecord>,
    detection_error: Option<EngineError>,
}

struct ScannedFile {
    file: FileRecord,
    discovered: DiscoveredFile,
    detection_error: Option<EngineError>,
}

async fn run_index_pass(
    context: &IndexingContext<'_>,
    timings: &mut TimingCollector,
    progress_base: Option<ProgressBase>,
) -> Result<IndexPassResult, EngineError> {
    throw_if_cancelled(context.signal.as_ref())?;
    report(
        context,
        IndexProgress {
            phase: IndexProgressPhase::Scanning,
            files_total: progress_base.map(|base| base.files_total),
            files_indexed: progress_base.map(|base| base.files_succeeded),
            files_failed: None,
            detail: Some(if progress_base.is_some() {
                "scanning retry candidates".to_owned()
            } else if context.changes.is_empty() {
                "scanning files".to_owned()
            } else {
                "scanning changed paths".to_owned()
            }),
            embedding: None,
        },
    );

    let scope = ChangeScope::from_changes(&context.workspace_index.root, context.changes)?;
    let all_stored = context.storage.list_files()?;
    let existing = scope.filter_stored(&context.workspace_index.root, &all_stored);
    let scan_started = Instant::now();
    let control = task_control(context.signal.clone());
    let snapshot = context
        .scanner
        .discover(
            &ScanRequest {
                roots: vec![host_root(context.workspace_index)],
                scope_paths: scope.scan_paths(),
            },
            &control,
        )
        .await
        .map_err(map_host_error)?;
    let mut skipped = skipped_files(&snapshot);
    let (scanned, classification_skips) = classify_files(
        context.workspace_index,
        snapshot.files,
        &all_stored,
        &control,
    )
    .await?;
    skipped.extend(classification_skips);
    skipped.truncate(MAX_SKIPPED_FILE_SAMPLES);
    let scanned = scope.filter_scanned(&context.workspace_index.root, scanned);
    timings.record("index_scan", scan_started.elapsed(), scanned.len());

    let diff_started = Instant::now();
    let mut diff = compute_diff(scanned, &existing);
    timings.record("index_diff", diff_started.elapsed(), diff.files_scanned);
    report(
        context,
        IndexProgress {
            phase: IndexProgressPhase::Scanning,
            files_total: Some(progress_base.map_or(diff.candidates.len(), |base| base.files_total)),
            files_indexed: Some(progress_base.map_or(0, |base| base.files_succeeded)),
            files_failed: None,
            detail: Some(format!(
                "{} candidates, {} deleted, {} unchanged",
                diff.candidates.len(),
                diff.deleted.len(),
                diff.unchanged
            )),
            embedding: None,
        },
    );

    let delete_started = Instant::now();
    for file in &diff.deleted {
        throw_if_cancelled(context.signal.as_ref())?;
        context.storage.delete_file(&file.id).map_err(|error| {
            EngineError::storage_failure(format!(
                "delete stale file {}: {error}",
                file.relative_path.display()
            ))
        })?;
    }
    timings.record(
        "index_delete_stale",
        delete_started.elapsed(),
        diff.deleted.len(),
    );

    let stats = index_candidates(context, &control, &mut diff, timings, progress_base).await?;
    Ok(IndexPassResult {
        files_scanned: diff.files_scanned,
        diff,
        stats,
        skipped,
    })
}

fn compute_diff(scanned: Vec<ScannedFile>, existing_files: &[FileRecord]) -> DiffPlan {
    let existing_by_id = existing_files
        .iter()
        .cloned()
        .map(|file| (file.id.clone(), file))
        .collect::<HashMap<_, _>>();
    let mut seen = HashSet::new();
    let mut plan = DiffPlan {
        files_scanned: scanned.len(),
        ..DiffPlan::default()
    };

    for scanned in scanned {
        seen.insert(scanned.file.id.clone());
        let existing = existing_by_id.get(&scanned.file.id).cloned();
        let kind = match &existing {
            None => CandidateKind::Added,
            Some(existing) if !existing.index_status.is_indexed() => CandidateKind::Pending,
            Some(existing)
                if scanned.detection_error.is_none()
                    && existing.snapshot.modified_epoch_ms.is_some()
                    && existing.snapshot.size_bytes == scanned.file.snapshot.size_bytes
                    && existing.snapshot.modified_epoch_ms
                        == scanned.file.snapshot.modified_epoch_ms
                    && existing.snapshot.content_hash.is_some() =>
            {
                plan.unchanged += 1;
                continue;
            }
            Some(_) => CandidateKind::Modified,
        };
        plan.increment(kind);
        plan.candidates.push(IndexCandidate {
            kind,
            file: scanned.file,
            discovered: scanned.discovered,
            detection_error: scanned.detection_error,
            existing,
        });
    }
    plan.deleted = existing_by_id
        .into_values()
        .filter(|file| !seen.contains(&file.id))
        .collect();
    plan.deleted
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    plan
}

impl DiffPlan {
    fn increment(&mut self, kind: CandidateKind) {
        match kind {
            CandidateKind::Added => self.added += 1,
            CandidateKind::Modified => self.modified += 1,
            CandidateKind::Pending => self.pending += 1,
        }
    }

    fn resolve_modified_as_unchanged(&mut self) {
        self.modified = self.modified.saturating_sub(1);
        self.unchanged += 1;
    }
}

async fn resolve_status_modifications(
    scanner: &dyn WorkspaceScannerPort,
    control: &TaskControl,
    diff: &mut DiffPlan,
) -> Result<(), EngineError> {
    let mut same_content = 0;
    for candidate in &diff.candidates {
        if candidate.kind != CandidateKind::Modified || candidate.detection_error.is_some() {
            continue;
        }
        let source = read_source(scanner, control, &candidate.discovered).await?;
        let hash = sha256_hex(&source.bytes);
        if candidate.existing.as_ref().is_some_and(|existing| {
            existing.snapshot.size_bytes == candidate.file.snapshot.size_bytes
                && existing.snapshot.content_hash.as_deref() == Some(hash.as_str())
        }) {
            same_content += 1;
        }
    }
    diff.modified = diff.modified.saturating_sub(same_content);
    diff.unchanged += same_content;
    Ok(())
}

struct PreparedFragment {
    fragment: EntityFragment,
    embedding_content: EmbeddingInput,
}

struct PreparedFile {
    file: FileRecord,
    fragments: Vec<PreparedFragment>,
}

enum PreparedCandidate {
    File(Box<PreparedFile>),
    Unchanged,
}

type EmbeddingFuture<'context> =
    Pin<Box<dyn Future<Output = EmbeddingBatchOutcome> + Send + 'context>>;

#[expect(
    clippy::too_many_lines,
    reason = "the bounded prepare/schedule/drain loop is clearest as one linear orchestration"
)]
async fn index_candidates(
    context: &IndexingContext<'_>,
    control: &TaskControl,
    diff: &mut DiffPlan,
    timings: &mut TimingCollector,
    progress_base: Option<ProgressBase>,
) -> Result<IndexStats, EngineError> {
    let policy = resolve_embedding_policy(
        context.embedding_concurrency,
        context.embedding_model.info(),
    )?;
    let scheduler = Arc::new(EmbeddingScheduler::new(policy));
    let max_batch_size = context.embedding_model.info().limits.max_batch_size;
    let mut stats = IndexStats::default();
    let mut current_batch = Vec::new();
    let mut current_fragments = 0;
    let mut running: FuturesUnordered<EmbeddingFuture<'_>> = FuturesUnordered::new();

    report_indexing(
        context,
        &stats,
        diff,
        progress_base,
        None,
        Some(scheduler.snapshot()),
    );

    let candidates = std::mem::take(&mut diff.candidates);
    for mut candidate in candidates {
        throw_if_cancelled(context.signal.as_ref())?;
        report_indexing(
            context,
            &stats,
            diff,
            progress_base,
            Some(format!(
                "reading {}",
                candidate.file.relative_path.display()
            )),
            None,
        );
        let prepare_started = Instant::now();
        let prepared = if let Some(error) = candidate.detection_error.take() {
            Err(error)
        } else {
            prepare_candidate(context, control, &candidate).await
        };
        timings.record("index_prepare", prepare_started.elapsed(), 1);
        let prepared = match prepared {
            Ok(PreparedCandidate::Unchanged) => {
                diff.resolve_modified_as_unchanged();
                continue;
            }
            Ok(PreparedCandidate::File(prepared)) => *prepared,
            Err(error) => {
                let reason = mark_file_failed(context.storage, &candidate.file, "prepare", &error)?;
                record_file_failed(&mut stats, &candidate.file, &reason);
                report_indexing(
                    context,
                    &stats,
                    diff,
                    progress_base,
                    Some(format!("failed {}", candidate.file.relative_path.display())),
                    None,
                );
                continue;
            }
        };

        if prepared.fragments.is_empty() {
            let commit_started = Instant::now();
            if let Err(error) = commit_file(context.storage, prepared, Vec::new(), &mut stats) {
                let reason =
                    mark_file_failed(context.storage, &error.file, "commit", &error.error)?;
                record_file_failed(&mut stats, &error.file, &reason);
            }
            timings.record("index_commit", commit_started.elapsed(), 1);
            report_indexing(context, &stats, diff, progress_base, None, None);
            continue;
        }

        if prepared.fragments.len() > max_batch_size {
            if !current_batch.is_empty() {
                push_embedding(
                    &mut running,
                    std::mem::take(&mut current_batch),
                    context,
                    Arc::clone(&scheduler),
                );
                current_fragments = 0;
            }
            push_embedding(
                &mut running,
                vec![prepared],
                context,
                Arc::clone(&scheduler),
            );
        } else {
            if current_fragments > 0
                && current_fragments + prepared.fragments.len() > max_batch_size
            {
                push_embedding(
                    &mut running,
                    std::mem::take(&mut current_batch),
                    context,
                    Arc::clone(&scheduler),
                );
                current_fragments = 0;
            }
            current_fragments += prepared.fragments.len();
            current_batch.push(prepared);
            if current_fragments == max_batch_size {
                push_embedding(
                    &mut running,
                    std::mem::take(&mut current_batch),
                    context,
                    Arc::clone(&scheduler),
                );
                current_fragments = 0;
            }
        }

        if running.len() >= scheduler.task_concurrency()
            && let Some(outcome) = running.next().await
        {
            apply_embedding_outcome(context, diff, progress_base, timings, &mut stats, outcome)?;
        }
    }

    if !current_batch.is_empty() {
        push_embedding(&mut running, current_batch, context, Arc::clone(&scheduler));
    }
    while let Some(outcome) = running.next().await {
        apply_embedding_outcome(context, diff, progress_base, timings, &mut stats, outcome)?;
    }
    throw_if_cancelled(context.signal.as_ref())?;
    Ok(stats)
}

fn push_embedding<'context>(
    running: &mut FuturesUnordered<EmbeddingFuture<'context>>,
    files: Vec<PreparedFile>,
    context: &'context IndexingContext<'context>,
    scheduler: Arc<EmbeddingScheduler>,
) {
    running.push(Box::pin(embed_prepared_files(
        files,
        context.embedding_model,
        scheduler,
        context.signal.clone(),
        context.on_progress.clone(),
    )));
}

fn apply_embedding_outcome(
    context: &IndexingContext<'_>,
    diff: &DiffPlan,
    progress_base: Option<ProgressBase>,
    timings: &mut TimingCollector,
    stats: &mut IndexStats,
    outcome: EmbeddingBatchOutcome,
) -> Result<(), EngineError> {
    timings.record("index_embedding", outcome.duration, outcome.outcomes.len());
    let mut outcomes = outcome.outcomes.into_iter();
    loop {
        let batch = outcomes
            .by_ref()
            .take(COMMIT_BATCH_FILES)
            .collect::<Vec<_>>();
        if batch.is_empty() {
            return Ok(());
        }
        throw_if_cancelled(context.signal.as_ref())?;
        let files = batch
            .iter()
            .map(|outcome| match outcome {
                EmbeddedFileOutcome::Success { file, .. }
                | EmbeddedFileOutcome::Failed { file, .. } => &file.file,
            })
            .collect::<Vec<_>>();
        let commit_started = Instant::now();
        context.storage.prepare_file_replacements(&files)?;
        timings.record("index_commit", commit_started.elapsed(), 0);
        apply_embedding_files(context, diff, progress_base, timings, stats, batch)?;
    }
}

fn apply_embedding_files(
    context: &IndexingContext<'_>,
    diff: &DiffPlan,
    progress_base: Option<ProgressBase>,
    timings: &mut TimingCollector,
    stats: &mut IndexStats,
    outcomes: Vec<EmbeddedFileOutcome>,
) -> Result<(), EngineError> {
    for outcome in outcomes {
        throw_if_cancelled(context.signal.as_ref())?;
        match outcome {
            EmbeddedFileOutcome::Success { file, vectors } => {
                let path = file.file.relative_path.clone();
                let commit_started = Instant::now();
                if let Err(error) = commit_file(context.storage, file, vectors, stats) {
                    let file = error.file;
                    let reason = mark_file_failed(context.storage, &file, "commit", &error.error)?;
                    record_file_failed(stats, &file, &reason);
                }
                timings.record("index_commit", commit_started.elapsed(), 1);
                report_indexing(
                    context,
                    stats,
                    diff,
                    progress_base,
                    Some(format!("indexed {}", path.display())),
                    None,
                );
            }
            EmbeddedFileOutcome::Failed { file, reason } => {
                let reason = mark_file_failed(
                    context.storage,
                    &file.file,
                    "embed",
                    &EngineError::internal(format!("embedding failed: {reason}")),
                )?;
                record_file_failed(stats, &file.file, &reason);
                report_indexing(
                    context,
                    stats,
                    diff,
                    progress_base,
                    Some(format!("failed {}", file.file.relative_path.display())),
                    None,
                );
            }
        }
    }
    Ok(())
}

struct CommitError {
    file: Box<FileRecord>,
    error: EngineError,
}

fn commit_file(
    storage: &dyn WorkspaceIndexStorage,
    file: PreparedFile,
    vectors: Vec<Vec<f32>>,
    stats: &mut IndexStats,
) -> Result<(), CommitError> {
    if file.fragments.len() != vectors.len() {
        return Err(CommitError {
            file: Box::new(file.file),
            error: EngineError::internal(format!(
                "entity/vector count mismatch: fragments={} vectors={}",
                file.fragments.len(),
                vectors.len()
            )),
        });
    }
    let public_entities = count_public_entities(&file.fragments);
    let entries = file
        .fragments
        .into_iter()
        .zip(vectors)
        .map(|(fragment, vector)| IndexedFragment {
            fragment: fragment.fragment,
            vector,
        })
        .collect::<Vec<_>>();
    storage
        .replace_file(&file.file, &entries)
        .map_err(|error| CommitError {
            file: Box::new(file.file.clone()),
            error,
        })?;
    stats.files_indexed += 1;
    stats.entities_created += public_entities;
    Ok(())
}

async fn prepare_candidate(
    context: &IndexingContext<'_>,
    control: &TaskControl,
    candidate: &IndexCandidate,
) -> Result<PreparedCandidate, EngineError> {
    let source = read_source(context.scanner, control, &candidate.discovered).await?;

    let mut file = candidate.file.clone();
    file.snapshot.size_bytes = u64::try_from(source.bytes.len())
        .map_err(|_| EngineError::invalid_argument("source byte length exceeds u64"))?;
    file.snapshot.content_hash = Some(sha256_hex(&source.bytes));
    if candidate.kind == CandidateKind::Modified
        && candidate.existing.as_ref().is_some_and(|existing| {
            existing.snapshot.size_bytes == file.snapshot.size_bytes
                && existing.snapshot.content_hash == file.snapshot.content_hash
        })
    {
        return Ok(PreparedCandidate::Unchanged);
    }

    let image_format = match source_kind(&file) {
        Some(SourceKind::Image(format)) => Some(format),
        Some(SourceKind::Text) => None,
        None => {
            return Err(EngineError::unsupported(format!(
                "no source reader is available for {}",
                file.relative_path.display()
            )));
        }
    };
    let source_text = if image_format.is_none() {
        Some(decode_text(&source.bytes, true).ok_or_else(|| {
            EngineError::invalid_argument(format!(
                "cannot extract text from {}: expected UTF-8 or BOM-marked UTF-16/32",
                file.relative_path.display()
            ))
        })?)
    } else {
        None
    };
    let chunk_options = index_chunk_options(
        context.embedding_model.info().limits.max_input_tokens,
        source_text.as_deref(),
    );
    let extracted = if let Some(format) = image_format {
        let image = ImageSource {
            file: file.clone(),
            content: ImageContent::new(source.bytes, format)?,
        };
        extract_for_indexing(&image, chunk_options)?
    } else {
        let text = TextSource {
            file: file.clone(),
            text: source_text
                .expect("text decoded for non-image source")
                .into_owned(),
        };
        extract_for_indexing(&text, chunk_options)?
    };
    validate_fragments(&file.id, extracted.iter().map(|item| &item.fragment))?;
    let fragments = prepare_fragments(extracted, chunk_options.max_chunk_chars);
    Ok(PreparedCandidate::File(Box::new(PreparedFile {
        file,
        fragments,
    })))
}

fn prepare_fragments(
    extracted: Vec<IndexingExtractionFragment>,
    max_chars: Option<usize>,
) -> Vec<PreparedFragment> {
    extracted
        .into_iter()
        .map(|item| PreparedFragment {
            embedding_content: EmbeddingInput::new(vector_content_for_fragment(
                &item.fragment,
                item.embedding_source.as_deref(),
                max_chars,
            )),
            fragment: item.fragment,
        })
        .collect()
}

fn count_public_entities(fragments: &[PreparedFragment]) -> usize {
    fragments
        .iter()
        .filter(|item| item.fragment.as_entity().is_some())
        .count()
}

fn mark_file_failed(
    storage: &dyn WorkspaceIndexStorage,
    file: &FileRecord,
    stage: &str,
    error: &EngineError,
) -> Result<String, EngineError> {
    let reason = collapse_whitespace(&format!("{stage}: {error}"));
    storage
        .mark_file_failed(file, &reason)
        .map_err(|mark_error| {
            EngineError::storage_failure(format!(
                "record failure for {}: {mark_error}; original={reason}",
                file.relative_path.display()
            ))
        })?;
    Ok(reason)
}

fn record_file_failed(stats: &mut IndexStats, file: &FileRecord, reason: &str) {
    stats.files_failed += 1;
    stats.failed_files.push(file.relative_path.clone());
    stats
        .failed_reasons
        .push(format!("{}: {reason}", file.relative_path.display()));
}

struct EmbeddingBatchOutcome {
    outcomes: Vec<EmbeddedFileOutcome>,
    duration: Duration,
}

enum EmbeddedFileOutcome {
    Success {
        file: PreparedFile,
        vectors: Vec<Vec<f32>>,
    },
    Failed {
        file: PreparedFile,
        reason: String,
    },
}

async fn embed_prepared_files(
    files: Vec<PreparedFile>,
    model: &dyn IndexEmbeddingRuntime,
    scheduler: Arc<EmbeddingScheduler>,
    signal: Option<CancellationToken>,
    progress: Option<IndexProgressReporter>,
) -> EmbeddingBatchOutcome {
    let started = Instant::now();
    if files.len() == 1 && files[0].fragments.len() > model.info().limits.max_batch_size {
        let file = files.into_iter().next().expect("one prepared file");
        let outcome = match embed_file(&file, model, &scheduler, signal.as_ref(), progress).await {
            Ok(embedding) => EmbeddedFileOutcome::Success {
                file,
                vectors: embedding.vectors,
            },
            Err(error) => EmbeddedFileOutcome::Failed {
                file,
                reason: model_error_text(&error),
            },
        };
        return EmbeddingBatchOutcome {
            outcomes: vec![outcome],
            duration: started.elapsed(),
        };
    }
    let contents = files
        .iter()
        .flat_map(|file| {
            file.fragments
                .iter()
                .map(|fragment| fragment.embedding_content.clone())
        })
        .collect::<Vec<_>>();
    report_embedding_progress(progress.as_ref(), &scheduler, describe_files(&files));
    let result = embed_with_retry(
        model,
        &contents,
        &scheduler,
        signal.as_ref(),
        progress.clone(),
    )
    .await;
    let outcomes = match result {
        Ok(embedding) => split_embedding(files, embedding),
        Err(error) if classify_embedding_retry(&error).retryable => {
            let reason = model_error_text(&error);
            files
                .into_iter()
                .map(|file| EmbeddedFileOutcome::Failed {
                    file,
                    reason: reason.clone(),
                })
                .collect()
        }
        Err(_) => {
            let mut outcomes = Vec::with_capacity(files.len());
            for file in files {
                match embed_file(&file, model, &scheduler, signal.as_ref(), progress.clone()).await
                {
                    Ok(embedding) => outcomes.push(EmbeddedFileOutcome::Success {
                        file,
                        vectors: embedding.vectors,
                    }),
                    Err(error) => outcomes.push(EmbeddedFileOutcome::Failed {
                        file,
                        reason: model_error_text(&error),
                    }),
                }
            }
            outcomes
        }
    };
    EmbeddingBatchOutcome {
        outcomes,
        duration: started.elapsed(),
    }
}

fn split_embedding(
    files: Vec<PreparedFile>,
    embedding: EmbeddingResult,
) -> Vec<EmbeddedFileOutcome> {
    if embedding.vectors.len() != files.iter().map(|file| file.fragments.len()).sum::<usize>() {
        let reason = format!(
            "embedding returned {} vectors for {} fragments",
            embedding.vectors.len(),
            files.iter().map(|file| file.fragments.len()).sum::<usize>()
        );
        return files
            .into_iter()
            .map(|file| EmbeddedFileOutcome::Failed {
                file,
                reason: reason.clone(),
            })
            .collect();
    }

    let mut vectors = embedding.vectors.into_iter();
    files
        .into_iter()
        .map(|file| {
            let count = file.fragments.len();
            let file_vectors = vectors.by_ref().take(count).collect::<Vec<_>>();
            EmbeddedFileOutcome::Success {
                file,
                vectors: file_vectors,
            }
        })
        .collect()
}

async fn embed_file(
    file: &PreparedFile,
    model: &dyn IndexEmbeddingRuntime,
    scheduler: &EmbeddingScheduler,
    signal: Option<&CancellationToken>,
    progress: Option<IndexProgressReporter>,
) -> Result<EmbeddingResult, ModelError> {
    let maximum = model.info().limits.max_batch_size;
    let mut running = FuturesUnordered::new();
    for (batch_index, fragments) in file.fragments.chunks(maximum).enumerate() {
        running.push(embed_fragment_batch(
            batch_index * maximum,
            fragments,
            model,
            scheduler,
            signal,
            progress.clone(),
        ));
    }

    let mut batches = Vec::new();
    while let Some(result) = running.next().await {
        batches.push(result?);
    }
    batches.sort_by_key(|batch| batch.start);
    let mut vectors = Vec::with_capacity(file.fragments.len());
    let mut truncated = Vec::new();
    for batch in batches {
        vectors.extend(batch.embedding.vectors);
        truncated.extend(
            batch
                .embedding
                .truncated
                .into_iter()
                .map(|index| batch.start + index),
        );
    }
    Ok(EmbeddingResult { vectors, truncated })
}

struct FragmentBatchResult {
    start: usize,
    embedding: EmbeddingResult,
}

async fn embed_fragment_batch(
    start: usize,
    fragments: &[PreparedFragment],
    model: &dyn IndexEmbeddingRuntime,
    scheduler: &EmbeddingScheduler,
    signal: Option<&CancellationToken>,
    progress: Option<IndexProgressReporter>,
) -> Result<FragmentBatchResult, ModelError> {
    let contents = fragments
        .iter()
        .map(|fragment| fragment.embedding_content.clone())
        .collect::<Vec<_>>();
    match embed_with_retry(model, &contents, scheduler, signal, progress.clone()).await {
        Ok(embedding) => Ok(FragmentBatchResult { start, embedding }),
        Err(error) if fragments.len() == 1 || classify_embedding_retry(&error).retryable => {
            Err(error)
        }
        Err(_) => {
            let mut vectors = Vec::with_capacity(fragments.len());
            let mut truncated = Vec::new();
            for (index, fragment) in fragments.iter().enumerate() {
                let embedding = embed_with_retry(
                    model,
                    std::slice::from_ref(&fragment.embedding_content),
                    scheduler,
                    signal,
                    progress.clone(),
                )
                .await
                .map_err(|error| {
                    ModelError::internal(format!(
                        "fragment {} failed after one-by-one fallback: {}",
                        fragment.fragment.document_id(),
                        model_error_text(&error)
                    ))
                })?;
                let Some(vector) = embedding.vectors.into_iter().next() else {
                    return Err(ModelError::internal(
                        "embedding returned no vector for a fragment",
                    ));
                };
                vectors.push(vector);
                if !embedding.truncated.is_empty() {
                    truncated.push(index);
                }
            }
            Ok(FragmentBatchResult {
                start,
                embedding: EmbeddingResult { vectors, truncated },
            })
        }
    }
}

async fn embed_with_retry(
    model: &dyn IndexEmbeddingRuntime,
    contents: &[EmbeddingInput],
    scheduler: &EmbeddingScheduler,
    signal: Option<&CancellationToken>,
    progress: Option<IndexProgressReporter>,
) -> Result<EmbeddingResult, ModelError> {
    let mut attempt = 0;
    loop {
        if signal.is_some_and(CancellationToken::is_cancelled) {
            return Err(ModelError::cancelled("embedding was cancelled"));
        }
        let permit = scheduler.acquire(signal).await?;
        let result = model
            .embed(
                contents,
                EmbeddingOptions {
                    purpose: Some(EmbeddingPurpose::Document),
                    signal: signal.cloned(),
                    ..EmbeddingOptions::default()
                },
                progress.clone(),
            )
            .await;
        drop(permit);
        match result {
            Ok(result) => {
                scheduler.record_success();
                return Ok(result);
            }
            Err(error) => {
                let retry = classify_embedding_retry(&error);
                if !retry.retryable || attempt >= maximum_retry_attempts(retry) {
                    return Err(error);
                }
                let delay = retry_delay(attempt, retry);
                scheduler.record_retryable_failure(retry.rate_limited, delay);
                abortable_delay(delay, signal).await?;
                attempt += 1;
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RetryClassification {
    retryable: bool,
    rate_limited: bool,
    retry_after: Option<Duration>,
}

fn classify_embedding_retry(error: &ModelError) -> RetryClassification {
    let text = model_error_text(error);
    let normalized = text.to_ascii_lowercase();
    let status = number_after(&normalized, "status=");
    let rate_limited = status == Some(429)
        || normalized.contains("rate limit")
        || normalized.contains("quota exceeded")
        || normalized.contains("too many requests")
        || normalized.contains("request rate increased too quickly");
    let server_error = status.is_some_and(|status| (500..=599).contains(&status));
    let retry_after = number_after(&normalized, "retryafterms=")
        .map(Duration::from_millis)
        .or_else(|| {
            float_after(&normalized, "retryafter=")
                .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                .map(Duration::from_secs_f64)
        });
    RetryClassification {
        retryable: rate_limited || server_error,
        rate_limited,
        retry_after,
    }
}

fn maximum_retry_attempts(retry: RetryClassification) -> usize {
    if retry.rate_limited {
        EMBEDDING_RATE_LIMIT_MAX_RETRIES
    } else {
        EMBEDDING_TRANSIENT_MAX_RETRIES
    }
}

fn retry_delay(attempt: usize, retry: RetryClassification) -> Duration {
    if let Some(retry_after) = retry.retry_after {
        return retry_after;
    }
    let (base, maximum) = if retry.rate_limited {
        (
            EMBEDDING_RATE_LIMIT_RETRY_BASE_DELAY,
            EMBEDDING_RATE_LIMIT_RETRY_MAX_DELAY,
        )
    } else {
        (
            EMBEDDING_TRANSIENT_RETRY_BASE_DELAY,
            EMBEDDING_TRANSIENT_RETRY_MAX_DELAY,
        )
    };
    let multiplier = 1_u32
        .checked_shl(attempt.try_into().unwrap_or(u32::MAX))
        .unwrap_or(u32::MAX);
    let exponential = base.saturating_mul(multiplier);
    exponential
        .saturating_add(Duration::from_millis(pseudo_jitter()))
        .min(maximum)
}

async fn abortable_delay(
    duration: Duration,
    signal: Option<&CancellationToken>,
) -> Result<(), ModelError> {
    if duration.is_zero() {
        return Ok(());
    }
    if let Some(signal) = signal {
        tokio::select! {
            () = tokio::time::sleep(duration) => Ok(()),
            () = signal.cancelled() => Err(ModelError::cancelled("embedding was cancelled")),
        }
    } else {
        tokio::time::sleep(duration).await;
        Ok(())
    }
}

fn model_error_text(error: &ModelError) -> String {
    let mut parts = vec![error.code().to_owned()];
    parts.push(error.to_string());
    if let Some(context) = error.context() {
        parts.push(context.to_owned());
    }
    if let Some(cause) = error.cause() {
        parts.push(cause.to_owned());
    }
    parts.join(": ")
}

fn number_after(text: &str, marker: &str) -> Option<u64> {
    let start = text.find(marker)? + marker.len();
    let digits = text[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

fn float_after(text: &str, marker: &str) -> Option<f64> {
    let start = text.find(marker)? + marker.len();
    let value = text[start..]
        .chars()
        .take_while(|character| character.is_ascii_digit() || *character == '.')
        .collect::<String>();
    (!value.is_empty()).then(|| value.parse().ok()).flatten()
}

fn pseudo_jitter() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::from(duration.subsec_nanos()) % EMBEDDING_RETRY_JITTER_MILLIS
        })
}

#[derive(Clone, Copy)]
struct EmbeddingConcurrencyPolicy {
    initial: usize,
    minimum: usize,
    maximum: usize,
    adaptive: bool,
}

fn resolve_embedding_policy(
    requested: Option<usize>,
    model: &EmbeddingModelInfo,
) -> Result<EmbeddingConcurrencyPolicy, EngineError> {
    if requested == Some(0) {
        return Err(EngineError::invalid_argument(
            "embedding_concurrency must be greater than zero",
        ));
    }
    if let Some(requested) = requested {
        return Ok(EmbeddingConcurrencyPolicy {
            initial: requested,
            minimum: 1,
            maximum: requested,
            adaptive: requested > 1,
        });
    }

    let remote = model.provider == "qwen";
    let multimodal = model.input_kinds.contains(&EmbeddingInputKind::Image);
    let local_default = model.default_concurrency.unwrap_or(1).max(1);
    let initial = if remote {
        if multimodal { 4 } else { 8 }
    } else {
        local_default
    };
    let maximum = if remote {
        if multimodal { 8 } else { 12 }
    } else {
        local_default
    };
    Ok(EmbeddingConcurrencyPolicy {
        initial,
        minimum: initial.min(4),
        maximum: maximum.max(initial),
        adaptive: maximum > 1,
    })
}

struct EmbeddingScheduler {
    policy: EmbeddingConcurrencyPolicy,
    state: Mutex<SchedulerState>,
    notify: Notify,
}

struct SchedulerState {
    active: usize,
    current: usize,
    cooldown_until: Option<Instant>,
    retryable_failures: usize,
    success_streak: usize,
}

impl EmbeddingScheduler {
    fn new(policy: EmbeddingConcurrencyPolicy) -> Self {
        Self {
            policy,
            state: Mutex::new(SchedulerState {
                active: 0,
                current: policy.initial,
                cooldown_until: None,
                retryable_failures: 0,
                success_streak: 0,
            }),
            notify: Notify::new(),
        }
    }

    const fn task_concurrency(&self) -> usize {
        self.policy.maximum
    }

    async fn acquire(
        &self,
        signal: Option<&CancellationToken>,
    ) -> Result<SchedulerPermit<'_>, ModelError> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if signal.is_some_and(CancellationToken::is_cancelled) {
                return Err(ModelError::cancelled(
                    "embedding was cancelled while waiting for capacity",
                ));
            }
            let cooldown = {
                let state = self.lock_state();
                state
                    .cooldown_until
                    .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
            };
            if let Some(duration) = cooldown {
                abortable_delay(duration, signal).await?;
                continue;
            }

            {
                let mut state = self.lock_state();
                if state.active < state.current {
                    state.active += 1;
                    return Ok(SchedulerPermit { scheduler: self });
                }
            }
            if let Some(signal) = signal {
                tokio::select! {
                    () = notified.as_mut() => {}
                    () = signal.cancelled() => {
                        return Err(ModelError::cancelled(
                            "embedding was cancelled while waiting for capacity",
                        ));
                    }
                }
            } else {
                notified.await;
            }
        }
    }

    fn release(&self) {
        let mut state = self.lock_state();
        state.active = state.active.saturating_sub(1);
        drop(state);
        self.notify.notify_waiters();
    }

    fn record_success(&self) {
        let mut state = self.lock_state();
        if !self.policy.adaptive || state.current >= self.policy.maximum {
            return;
        }
        state.success_streak += 1;
        if state.success_streak < EMBEDDING_SUCCESS_STREAK_MIN.max(state.current * 2) {
            return;
        }
        state.current += 1;
        state.success_streak = 0;
        drop(state);
        self.notify.notify_waiters();
    }

    fn record_retryable_failure(&self, rate_limited: bool, delay: Duration) {
        let mut state = self.lock_state();
        state.retryable_failures += 1;
        if rate_limited && !delay.is_zero() {
            let deadline = Instant::now() + delay;
            state.cooldown_until = Some(
                state
                    .cooldown_until
                    .map_or(deadline, |current| current.max(deadline)),
            );
        }
        if self.policy.adaptive {
            state.current = (state.current / 2).max(self.policy.minimum);
            state.success_streak = 0;
        }
    }

    fn snapshot(&self) -> IndexEmbeddingProgress {
        let state = self.lock_state();
        IndexEmbeddingProgress {
            concurrency: Some(state.current),
            max_concurrency: Some(self.policy.maximum),
            retryable_failures: Some(state.retryable_failures),
            ..IndexEmbeddingProgress::default()
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, SchedulerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct SchedulerPermit<'scheduler> {
    scheduler: &'scheduler EmbeddingScheduler,
}

impl Drop for SchedulerPermit<'_> {
    fn drop(&mut self) {
        self.scheduler.release();
    }
}

fn report_embedding_progress(
    reporter: Option<&IndexProgressReporter>,
    scheduler: &EmbeddingScheduler,
    detail: String,
) {
    if let Some(reporter) = reporter {
        reporter.report(IndexProgress {
            phase: IndexProgressPhase::Indexing,
            files_total: None,
            files_indexed: None,
            files_failed: None,
            detail: Some(detail),
            embedding: Some(scheduler.snapshot()),
        });
    }
}

fn report_indexing(
    context: &IndexingContext<'_>,
    stats: &IndexStats,
    diff: &DiffPlan,
    progress_base: Option<ProgressBase>,
    detail: Option<String>,
    embedding: Option<IndexEmbeddingProgress>,
) {
    report(
        context,
        IndexProgress {
            phase: IndexProgressPhase::Indexing,
            files_total: Some(progress_base.map_or(diff.pending_count(), |base| base.files_total)),
            files_indexed: Some(
                progress_base.map_or(0, |base| base.files_succeeded)
                    + stats.files_indexed
                    + stats.files_failed,
            ),
            files_failed: Some(stats.files_failed),
            detail,
            embedding,
        },
    );
}

fn report(context: &IndexingContext<'_>, progress: IndexProgress) {
    if let Some(reporter) = &context.on_progress {
        reporter.report(progress);
    }
}

fn describe_files(files: &[PreparedFile]) -> String {
    match files {
        [] => "embedding 0 files".to_owned(),
        [file] => format!("embedding {}", file.file.relative_path.display()),
        [first, ..] => format!(
            "embedding {} files, starting with {}",
            files.len(),
            first.file.relative_path.display()
        ),
    }
}

fn validate_context(context: &IndexingContext<'_>) -> Result<(), EngineError> {
    if context.storage.is_read_only() {
        return Err(EngineError::invalid_argument(
            "indexing requires writable workspace storage",
        ));
    }
    if context.workspace_index.id.trim().is_empty() {
        return Err(EngineError::invalid_argument(
            "indexing requires a workspace index id",
        ));
    }
    if !context.workspace_index.root.is_absolute() {
        return Err(EngineError::invalid_argument(
            "indexing requires an absolute workspace root",
        ));
    }
    if context.workspace_index.policy != WorkspaceIndexPolicy::Enabled {
        return Err(EngineError::invalid_argument(
            "indexing requires an enabled workspace",
        ));
    }
    if context.embedding_model.info().limits.max_batch_size == 0 {
        return Err(EngineError::internal(
            "embedding model max_batch_size must be greater than zero",
        ));
    }
    if let Some(schema) = &context.workspace_index.embedding {
        let model = context.embedding_model.info();
        if schema.provider != model.provider
            || schema.model != model.name
            || schema.dimension != model.dimension
            || schema.metric != metric_name(model.metric)
        {
            return Err(EngineError::invalid_argument(format!(
                "workspace embedding schema does not match model {}",
                model.reference
            )));
        }
    }
    let _ = resolve_embedding_policy(
        context.embedding_concurrency,
        context.embedding_model.info(),
    )?;
    Ok(())
}

const fn metric_name(metric: crate::models::EmbeddingMetric) -> &'static str {
    match metric {
        crate::models::EmbeddingMetric::Cosine => "cosine",
        crate::models::EmbeddingMetric::DotProduct => "dot",
        crate::models::EmbeddingMetric::Euclidean => "euclidean",
    }
}

fn host_root(workspace: &WorkspaceIndexInfo) -> RootSpec {
    let discovery = &workspace.discovery;
    RootSpec {
        path: workspace.root.clone(),
        recursive: true,
        discovery: HostDiscoveryOptions {
            include_paths: discovery.include_paths.clone(),
            exclude_paths: discovery.exclude_paths.clone(),
            globs: discovery.globs.clone(),
            insensitive_globs: discovery.insensitive_globs.clone(),
            file_types: discovery.file_types.clone(),
            excluded_file_types: discovery.excluded_file_types.clone(),
            hidden: discovery.hidden,
            no_ignore: discovery.no_ignore,
            ignore_files: discovery.ignore_files.clone(),
            max_depth: discovery.max_depth,
            max_file_size_bytes: discovery.max_file_size_bytes,
            follow: discovery.follow,
        },
    }
}

async fn classify_files(
    workspace: &WorkspaceIndexInfo,
    files: Vec<DiscoveredFile>,
    stored: &[FileRecord],
    control: &TaskControl,
) -> Result<(Vec<ScannedFile>, Vec<SkippedFile>), EngineError> {
    let workspace = workspace.clone();
    let stored = stored.to_vec();
    let signal = control.cancellation.clone();
    tokio::task::spawn_blocking(move || {
        let mut skipped = Vec::new();
        let files = scanned_files(&workspace, files, &stored, &mut skipped, &signal)?;
        Ok((files, skipped))
    })
    .await
    .map_err(|error| EngineError::internal(format!("file classification worker failed: {error}")))?
}

fn scanned_files(
    workspace: &WorkspaceIndexInfo,
    files: Vec<DiscoveredFile>,
    stored: &[FileRecord],
    skipped: &mut Vec<SkippedFile>,
    signal: &CancellationToken,
) -> Result<Vec<ScannedFile>, EngineError> {
    let existing = stored
        .iter()
        .map(|file| (&file.relative_path, file))
        .collect::<HashMap<_, _>>();
    let mut scanned = Vec::with_capacity(files.len());
    throw_if_cancelled(Some(signal))?;
    for discovered in files {
        throw_if_cancelled(Some(signal))?;
        if discovered.root != workspace.root {
            return Err(EngineError::invalid_argument(
                "discovered file must belong to the workspace root",
            ));
        }
        let id = FileId::for_path(&workspace.id, &discovered.relative_path)?;
        let absolute_path = workspace.root.join(&discovered.relative_path);
        let detected = match existing.get(&discovered.relative_path) {
            Some(file)
                if file.snapshot.modified_epoch_ms.is_some()
                    && file.snapshot.size_bytes == discovered.size_bytes
                    && file.snapshot.modified_epoch_ms == discovered.modified_epoch_ms =>
            {
                Ok(file.formats.clone())
            }
            _ => FileFormat::from_path(&absolute_path),
        };
        let (formats, detection_error) = match detected {
            Ok(formats) => (formats, None),
            Err(error) => (
                existing
                    .get(&discovered.relative_path)
                    .map_or_else(|| vec![FileFormat::Unknown], |file| file.formats.clone()),
                Some(error),
            ),
        };
        let source = FileRecord {
            id,
            relative_path: discovered.relative_path.clone(),
            formats,
            snapshot: FileSnapshot {
                size_bytes: discovered.size_bytes,
                modified_epoch_ms: discovered.modified_epoch_ms,
                content_hash: None,
            },
            index_status: FileIndexStatus::NotIndexed,
        };
        source.validate()?;
        let supported_category = [
            FileCategory::Code,
            FileCategory::Data,
            FileCategory::Document,
            FileCategory::Image,
            FileCategory::Audio,
            FileCategory::Video,
        ]
        .into_iter()
        .any(|category| source.has_category(category));
        let explicit_maximum = workspace.discovery.max_file_size_bytes;
        let maximum = explicit_maximum.unwrap_or_else(|| default_file_size_limit(&source));
        let reason = if !supported_category {
            Some(if source.has_category(FileCategory::Binary) {
                SkippedFileReason::Binary
            } else {
                SkippedFileReason::Unsupported
            })
        } else if source_kind(&source).is_none() {
            Some(SkippedFileReason::Unsupported)
        } else if source.snapshot.size_bytes > maximum {
            Some(SkippedFileReason::TooLarge)
        } else {
            None
        };
        if let Some(reason) = reason.filter(|_| detection_error.is_none()) {
            if skipped.len() < MAX_SKIPPED_FILE_SAMPLES {
                skipped.push(SkippedFile {
                    path: absolute_path,
                    reason,
                    size_bytes: Some(source.snapshot.size_bytes),
                    limit_bytes: (reason == SkippedFileReason::TooLarge).then_some(maximum),
                });
            }
            continue;
        }
        scanned.push(ScannedFile {
            file: source,
            discovered,
            detection_error,
        });
    }
    Ok(scanned)
}

fn default_file_size_limit(file: &FileRecord) -> u64 {
    if file.has_category(FileCategory::Image) {
        10 * 1024 * 1024
    } else if file.has_category(FileCategory::Data) {
        16 * 1024 * 1024
    } else if file.has_category(FileCategory::Code) {
        1024 * 1024
    } else {
        256 * 1024 * 1024
    }
}

async fn read_source(
    scanner: &dyn WorkspaceScannerPort,
    control: &TaskControl,
    file: &DiscoveredFile,
) -> Result<HostSource, EngineError> {
    let mut sources = scanner
        .read_batch(
            &ReadBatchRequest {
                files: vec![file.clone()],
            },
            control,
        )
        .await
        .map_err(map_host_error)?;
    if sources.len() != 1 {
        return Err(EngineError::internal(format!(
            "native scanner returned {} sources for one requested file",
            sources.len()
        )));
    }
    let source = sources.remove(0);
    if source.root != file.root || source.relative_path != file.relative_path {
        return Err(EngineError::internal(
            "native scanner returned bytes for a different source file",
        ));
    }
    if source.source_fingerprint != file.source_fingerprint
        || u64::try_from(source.bytes.len()).ok() != Some(file.size_bytes)
    {
        return Err(EngineError::resource_busy(format!(
            "source changed while being read: {}",
            file.relative_path.display()
        )));
    }
    Ok(source)
}

fn skipped_files(snapshot: &ScanSnapshot) -> Vec<SkippedFile> {
    snapshot
        .diagnostics
        .skipped_samples
        .iter()
        .map(|skipped| SkippedFile {
            path: skipped.path.clone(),
            reason: match skipped.reason {
                zg_host_native::SkippedFileReason::Empty => SkippedFileReason::Empty,
                zg_host_native::SkippedFileReason::TooLarge => SkippedFileReason::TooLarge,
                zg_host_native::SkippedFileReason::Unsupported => SkippedFileReason::Unsupported,
                zg_host_native::SkippedFileReason::Binary => SkippedFileReason::Binary,
            },
            size_bytes: skipped.size_bytes,
            limit_bytes: skipped.limit_bytes,
        })
        .collect()
}

#[derive(Debug)]
enum ChangeScope {
    All,
    Paths(Vec<PathBuf>),
}

impl ChangeScope {
    fn from_changes(
        workspace_root: &Path,
        changes: &[WorkspaceChange],
    ) -> Result<Self, EngineError> {
        if changes.is_empty()
            || changes
                .iter()
                .any(|change| matches!(change, WorkspaceChange::Rescan))
        {
            return Ok(Self::All);
        }
        let mut paths = Vec::new();
        for change in changes {
            let path = match change {
                WorkspaceChange::Upsert(path)
                | WorkspaceChange::Delete(path)
                | WorkspaceChange::RescanDirectory(path)
                | WorkspaceChange::DeletePrefix(path) => path,
                WorkspaceChange::Rescan => continue,
            };
            let relative = if path.is_absolute() {
                path.strip_prefix(workspace_root).map_err(|_| {
                    EngineError::invalid_argument(
                        "changed path must stay within the workspace root",
                    )
                })?
            } else {
                path.as_path()
            };
            if relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
            {
                return Err(EngineError::invalid_argument(
                    "changed path must stay within the workspace root",
                ));
            }
            paths.push(workspace_root.join(relative));
        }
        paths.sort();
        paths.dedup();
        Ok(Self::Paths(paths))
    }

    fn contains(&self, path: &Path) -> bool {
        match self {
            Self::All => true,
            Self::Paths(paths) => paths
                .iter()
                .any(|scope| path == scope || path.starts_with(scope)),
        }
    }

    fn scan_paths(&self) -> Vec<PathBuf> {
        match self {
            Self::All => Vec::new(),
            Self::Paths(paths) => paths.clone(),
        }
    }

    fn filter_stored(&self, workspace_root: &Path, files: &[FileRecord]) -> Vec<FileRecord> {
        files
            .iter()
            .filter(|file| self.contains(&workspace_root.join(&file.relative_path)))
            .cloned()
            .collect()
    }

    fn filter_scanned(&self, workspace_root: &Path, files: Vec<ScannedFile>) -> Vec<ScannedFile> {
        files
            .into_iter()
            .filter(|file| self.contains(&workspace_root.join(&file.file.relative_path)))
            .collect()
    }
}

fn task_control(signal: Option<CancellationToken>) -> TaskControl {
    TaskControl::new(signal.unwrap_or_default())
}

fn throw_if_cancelled(signal: Option<&CancellationToken>) -> Result<(), EngineError> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        Err(EngineError::cancelled("indexing was cancelled"))
    } else {
        Ok(())
    }
}

fn map_host_error(error: HostError) -> EngineError {
    match error {
        HostError::InvalidArgument { message, origin } => {
            host_engine_error(EngineError::INVALID_ARGUMENT, message, origin)
        }
        HostError::StorageFailure {
            component,
            message,
            origin,
        } => host_engine_error(
            EngineError::STORAGE_FAILURE,
            format!("{component} failed: {message}"),
            origin,
        ),
        HostError::Cancelled { message, origin } => {
            host_engine_error(EngineError::CANCELLED, message, origin)
        }
        HostError::DeadlineExceeded { message, origin } => {
            host_engine_error(EngineError::DEADLINE_EXCEEDED, message, origin)
        }
        HostError::ResourceClosed { message, origin } => {
            host_engine_error(EngineError::RESOURCE_CLOSED, message, origin)
        }
        HostError::Internal { message, origin } => {
            host_engine_error(EngineError::INTERNAL, message, origin)
        }
    }
}

fn host_engine_error(code: &'static str, message: String, origin: HostErrorSite) -> EngineError {
    EngineError::new_at(
        code,
        message,
        ErrorSite::new(origin.file(), origin.line(), origin.column()),
    )
}

fn build_index_result(
    context: &IndexingContext<'_>,
    passes: &[IndexPassResult],
    duration: Duration,
    timings: TimingCollector,
) -> IndexResult {
    let first = &passes[0];
    let final_pass = passes.last().expect("an index pass is always present");
    IndexResult {
        generation: context
            .workspace_index
            .generation
            .unwrap_or(0)
            .saturating_add(1),
        files_scanned: final_pass.files_scanned,
        files_added: passes.iter().map(|pass| pass.diff.added).sum(),
        files_modified: passes.iter().map(|pass| pass.diff.modified).sum(),
        files_pending: passes.iter().map(|pass| pass.diff.pending).sum(),
        files_deleted: passes.iter().map(|pass| pass.diff.deleted.len()).sum(),
        files_unchanged: first.diff.unchanged,
        files_failed: final_pass.stats.files_failed,
        entities_created: passes.iter().map(|pass| pass.stats.entities_created).sum(),
        duration_micros: duration.as_micros().try_into().unwrap_or(u64::MAX),
        timings: timings.entries,
        skipped: final_pass.skipped.clone(),
    }
}

#[derive(Default)]
struct TimingCollector {
    entries: Vec<TimingEntry>,
}

impl TimingCollector {
    fn record(&mut self, name: &str, duration: Duration, count: usize) {
        let duration_micros = duration.as_micros().try_into().unwrap_or(u64::MAX);
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.name == name) {
            entry.duration_micros = entry.duration_micros.saturating_add(duration_micros);
            entry.count = Some(entry.count.unwrap_or(0).saturating_add(count as u64));
        } else {
            self.entries.push(TimingEntry {
                name: name.to_owned(),
                duration_micros,
                count: Some(count.try_into().unwrap_or(u64::MAX)),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tempfile::tempdir;
    use tokio::time::sleep;
    use zg_host_native::NativeScanner;

    use crate::{
        api::{
            index::{options::DiscoveryOptions, progress::IndexProgressPhase},
            info::result::WorkspaceIndexEmbedding,
        },
        models::{EmbeddingMetric, EmbeddingModelLimits},
        storage::spi::{StorageResult, StorageSearchFilter, StorageSearchHit, StoredEntity},
    };

    use super::*;

    #[derive(Default)]
    struct MemoryStorage {
        files: Mutex<Vec<FileRecord>>,
        finalized: AtomicUsize,
        prepared_batches: Mutex<Vec<Vec<FileId>>>,
    }

    #[async_trait]
    impl WorkspaceIndexStorage for MemoryStorage {
        fn is_read_only(&self) -> bool {
            false
        }

        fn list_files(&self) -> StorageResult<Vec<FileRecord>> {
            Ok(self
                .files
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone())
        }

        fn get_entity(
            &self,
            _entity_id: &crate::domain::EntityId,
        ) -> StorageResult<Option<StoredEntity>> {
            Ok(None)
        }

        fn search_fts(
            &self,
            _query: &str,
            _limit: usize,
            _filter: Option<&StorageSearchFilter>,
        ) -> StorageResult<Vec<StorageSearchHit>> {
            Ok(Vec::new())
        }

        fn search_vector(
            &self,
            _vector: &[f32],
            _limit: usize,
            _filter: Option<&StorageSearchFilter>,
        ) -> StorageResult<Vec<StorageSearchHit>> {
            Ok(Vec::new())
        }

        fn replace_file(
            &self,
            file: &FileRecord,
            entries: &[IndexedFragment],
        ) -> StorageResult<()> {
            let mut stored = file.clone();
            stored.index_status = FileIndexStatus::Indexed {
                indexed_epoch_ms: 1,
                entity_count: entries
                    .iter()
                    .filter(|entry| entry.fragment.as_entity().is_some())
                    .count() as u64,
            };
            stored.validate()?;
            let mut files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(existing) = files.iter_mut().find(|existing| existing.id == stored.id) {
                *existing = stored;
            } else {
                files.push(stored);
            }
            Ok(())
        }

        fn prepare_file_replacements(&self, files: &[&FileRecord]) -> StorageResult<()> {
            self.prepared_batches
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(files.iter().map(|file| file.id.clone()).collect());
            Ok(())
        }

        fn mark_file_failed(&self, file: &FileRecord, error: &str) -> StorageResult<()> {
            let mut stored = file.clone();
            stored.index_status = FileIndexStatus::Failed {
                error: error.to_owned(),
            };
            let mut files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(existing) = files.iter_mut().find(|existing| existing.id == stored.id) {
                *existing = stored;
            } else {
                files.push(stored);
            }
            Ok(())
        }

        fn delete_file(&self, file_id: &FileId) -> StorageResult<()> {
            self.files
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|file| &file.id != file_id);
            Ok(())
        }

        async fn finalize_writes(&self) -> StorageResult<()> {
            self.finalized.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }

        fn close(&self) -> StorageResult<()> {
            Ok(())
        }
    }

    struct RecordingScanner {
        inner: NativeScanner,
        requests: Mutex<Vec<ScanRequest>>,
    }

    impl RecordingScanner {
        fn new() -> Self {
            Self {
                inner: NativeScanner::default(),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl WorkspaceScannerPort for RecordingScanner {
        async fn discover(
            &self,
            request: &ScanRequest,
            control: &TaskControl,
        ) -> Result<ScanSnapshot, HostError> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request.clone());
            self.inner.discover(request, control).await
        }

        async fn read_batch(
            &self,
            request: &ReadBatchRequest,
            control: &TaskControl,
        ) -> Result<Vec<HostSource>, HostError> {
            self.inner.read_batch(request, control).await
        }
    }

    struct ConcurrentModel {
        info: EmbeddingModelInfo,
        calls: AtomicUsize,
        active: AtomicUsize,
        maximum_active: AtomicUsize,
    }

    struct UnknownModifiedScanner(NativeScanner);

    #[async_trait]
    impl WorkspaceScannerPort for UnknownModifiedScanner {
        async fn discover(
            &self,
            request: &ScanRequest,
            control: &TaskControl,
        ) -> Result<ScanSnapshot, HostError> {
            let mut snapshot = self.0.discover(request, control).await?;
            for file in &mut snapshot.files {
                file.modified_epoch_ms = None;
                file.source_fingerprint = format!("unknown-mtime:{}", file.size_bytes);
            }
            Ok(snapshot)
        }

        async fn read_batch(
            &self,
            request: &ReadBatchRequest,
            control: &TaskControl,
        ) -> Result<Vec<HostSource>, HostError> {
            let mut sources = self.0.read_batch(request, control).await?;
            for file in &mut sources {
                file.source_fingerprint = format!("unknown-mtime:{}", file.bytes.len());
            }
            Ok(sources)
        }
    }

    impl ConcurrentModel {
        fn new() -> Self {
            Self {
                info: EmbeddingModelInfo {
                    reference: "local/test".to_owned(),
                    provider: "local".to_owned(),
                    name: "test".to_owned(),
                    dimension: 2,
                    metric: EmbeddingMetric::Cosine,
                    endpoint: None,
                    default_concurrency: Some(2),
                    input_kinds: vec![EmbeddingInputKind::Text],
                    limits: EmbeddingModelLimits {
                        max_batch_size: 1,
                        max_input_tokens: Some(64),
                        max_image_bytes: None,
                    },
                },
                calls: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                maximum_active: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl IndexEmbeddingRuntime for ConcurrentModel {
        fn info(&self) -> &EmbeddingModelInfo {
            &self.info
        }

        async fn embed(
            &self,
            contents: &[EmbeddingInput],
            _options: EmbeddingOptions,
            _progress: Option<IndexProgressReporter>,
        ) -> Result<EmbeddingResult, ModelError> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.maximum_active.fetch_max(active, Ordering::AcqRel);
            sleep(Duration::from_millis(10)).await;
            self.active.fetch_sub(1, Ordering::AcqRel);
            Ok(EmbeddingResult {
                vectors: contents.iter().map(|_| vec![1.0, 0.0]).collect(),
                truncated: Vec::new(),
            })
        }
    }

    fn workspace(root: &Path) -> WorkspaceIndexInfo {
        WorkspaceIndexInfo {
            id: "workspace-id".to_owned(),
            name: "fixture".to_owned(),
            path: root.join(".zvec-grep"),
            root: root.to_path_buf(),
            discovery: DiscoveryOptions {
                no_ignore: true,
                ..DiscoveryOptions::default()
            },
            policy: WorkspaceIndexPolicy::Enabled,
            embedding: Some(WorkspaceIndexEmbedding {
                provider: "local".to_owned(),
                model: "test".to_owned(),
                dimension: 2,
                metric: "cosine".to_owned(),
            }),
            index_version: Some(1),
            generation: None,
            created_epoch_ms: 1,
            updated_epoch_ms: 1,
        }
    }

    #[tokio::test]
    async fn unknown_mtime_uses_content_hash_and_reports_indexed_snapshot_bytes() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("file.txt");
        std::fs::write(&path, "alpha").expect("source");
        let workspace = workspace(directory.path());
        let scanner = UnknownModifiedScanner(NativeScanner::default());
        let storage = MemoryStorage::default();
        let model = ConcurrentModel::new();
        let context = IndexingContext {
            workspace_index: &workspace,
            storage: &storage,
            scanner: &scanner,
            embedding_model: &model,
            embedding_concurrency: None,
            on_progress: None,
            signal: None,
            changes: &[],
        };
        index_workspace(&context).await.expect("initial index");
        let calls = model.calls.load(Ordering::Acquire);
        let unchanged = index_workspace(&context).await.expect("same bytes");
        assert_eq!(unchanged.files_unchanged, 1);
        assert_eq!(model.calls.load(Ordering::Acquire), calls);

        std::fs::write(&path, "bravo").expect("same size, different bytes");
        let status = get_workspace_index_status(&workspace, &storage, &scanner, None)
            .await
            .expect("status");
        assert_eq!(status.files_modified, 1);
        assert_eq!(status.indexed_size_bytes, 5);
        let changed = index_workspace(&context).await.expect("changed bytes");
        assert_eq!(changed.files_modified, 1);
        assert!(model.calls.load(Ordering::Acquire) > calls);
        let files = storage.list_files().expect("stored files");
        assert_eq!(files[0].snapshot.modified_epoch_ms, None);
        assert_eq!(files[0].snapshot.size_bytes, 5);
        assert_eq!(files[0].snapshot.content_hash, Some(sha256_hex(b"bravo")));
        assert!(files[0].index_status.is_indexed());

        storage
            .mark_file_failed(&files[0], "test failure")
            .expect("failed state");
        let status = get_workspace_index_status(&workspace, &storage, &scanner, None)
            .await
            .expect("failed status");
        assert_eq!(status.files_failed, 1);
        assert_eq!(status.indexed_size_bytes, 0);
        assert_eq!(status.entities_indexed, 0);
    }

    #[tokio::test]
    async fn prepares_bounded_recovery_batches_for_multi_file_embedding_results() {
        let directory = tempdir().expect("temporary directory");
        for index in 0..=COMMIT_BATCH_FILES {
            std::fs::write(
                directory.path().join(format!("file-{index}.txt")),
                "orchard",
            )
            .expect("fixture file");
        }
        let workspace = workspace(directory.path());
        let scanner = NativeScanner::default();
        let storage = MemoryStorage::default();
        let mut model = ConcurrentModel::new();
        model.info.limits.max_batch_size = COMMIT_BATCH_FILES * 2;
        let result = index_workspace(&IndexingContext {
            workspace_index: &workspace,
            storage: &storage,
            scanner: &scanner,
            embedding_model: &model,
            embedding_concurrency: None,
            on_progress: None,
            signal: None,
            changes: &[],
        })
        .await
        .expect("index batch");
        assert_eq!(result.files_added, COMMIT_BATCH_FILES + 1);
        assert_eq!(result.entities_created, COMMIT_BATCH_FILES + 1);
        assert_eq!(model.calls.load(Ordering::Acquire), 1);
        let batches = storage
            .prepared_batches
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert_eq!(
            batches.iter().map(Vec::len).collect::<Vec<_>>(),
            [COMMIT_BATCH_FILES, 1]
        );
        assert_eq!(
            batches.iter().flatten().collect::<HashSet<_>>().len(),
            COMMIT_BATCH_FILES + 1
        );
    }

    #[tokio::test]
    async fn indexes_incrementally_reuses_unchanged_files_and_honors_concurrency() {
        let directory = tempdir().expect("temporary directory");
        for index in 0..4 {
            std::fs::write(
                directory.path().join(format!("file-{index}.txt")),
                format!("content {index}"),
            )
            .expect("fixture file");
        }
        let workspace = workspace(directory.path());
        let scanner = NativeScanner::default();
        let storage = MemoryStorage::default();
        let model = ConcurrentModel::new();
        let progress = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&progress);
        let reporter = IndexProgressReporter::new(move |value| {
            captured
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(value);
        });
        let context = IndexingContext {
            workspace_index: &workspace,
            storage: &storage,
            scanner: &scanner,
            embedding_model: &model,
            embedding_concurrency: Some(2),
            on_progress: Some(reporter),
            signal: None,
            changes: &[],
        };

        let first = index_workspace(&context).await.expect("initial index");
        assert_eq!(first.files_added, 4);
        assert_eq!(first.entities_created, 4);
        assert_eq!(storage.list_files().expect("stored files").len(), 4);
        assert_eq!(model.maximum_active.load(Ordering::Acquire), 2);
        assert!(
            progress
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .any(|item| item.phase == IndexProgressPhase::Done)
        );

        let calls = model.calls.load(Ordering::Acquire);
        let second = index_workspace(&context).await.expect("unchanged index");
        assert_eq!(second.files_unchanged, 4);
        assert_eq!(second.entities_created, 0);
        assert_eq!(model.calls.load(Ordering::Acquire), calls);

        std::fs::write(directory.path().join("file-0.txt"), "changed and longer")
            .expect("modified file");
        let third = index_workspace(&context).await.expect("modified index");
        assert_eq!(third.files_modified, 1);
        assert_eq!(third.files_unchanged, 3);

        std::fs::remove_file(directory.path().join("file-3.txt")).expect("deleted file");
        let fourth = index_workspace(&context).await.expect("deleted index");
        assert_eq!(fourth.files_deleted, 1);
        assert_eq!(storage.list_files().expect("stored files").len(), 3);
        assert_eq!(storage.finalized.load(Ordering::Acquire), 4);
    }

    #[tokio::test]
    async fn changed_paths_limit_diff_and_storage_mutation_scope() {
        let directory = tempdir().expect("temporary directory");
        let first_path = directory.path().join("first.txt");
        let second_path = directory.path().join("second.txt");
        std::fs::write(&first_path, "first").expect("first fixture");
        std::fs::write(&second_path, "second").expect("second fixture");
        let workspace = workspace(directory.path());
        let scanner = RecordingScanner::new();
        let storage = MemoryStorage::default();
        let model = ConcurrentModel::new();
        index_workspace(&IndexingContext {
            workspace_index: &workspace,
            storage: &storage,
            scanner: &scanner,
            embedding_model: &model,
            embedding_concurrency: Some(2),
            on_progress: None,
            signal: None,
            changes: &[],
        })
        .await
        .expect("initial index");

        std::fs::write(&first_path, "first changed").expect("first modified");
        std::fs::write(&second_path, "second changed").expect("second modified");
        let changes = [WorkspaceChange::Upsert(first_path.clone())];
        let result = index_workspace(&IndexingContext {
            workspace_index: &workspace,
            storage: &storage,
            scanner: &scanner,
            embedding_model: &model,
            embedding_concurrency: Some(2),
            on_progress: None,
            signal: None,
            changes: &changes,
        })
        .await
        .expect("narrow index");

        let requests = scanner
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert!(requests[0].scope_paths.is_empty());
        assert_eq!(
            requests[1].scope_paths.as_slice(),
            std::slice::from_ref(&first_path)
        );
        assert_eq!(result.files_scanned, 1);
        assert_eq!(result.files_modified, 1);
        let stored = storage.list_files().expect("stored files");
        let untouched = stored
            .iter()
            .find(|file| file.relative_path == Path::new("second.txt"))
            .expect("second stored file");
        let changed_hash = sha256_hex(b"second changed");
        assert_ne!(
            untouched.snapshot.content_hash.as_deref(),
            Some(changed_hash.as_str())
        );
    }

    #[test]
    fn changed_paths_share_the_workspace_base_and_reject_outside_paths() {
        let directory = tempdir().expect("temporary directory");
        let root = directory.path().join("workspace");
        let absolute = root.join("nested/source.rs");
        let scope = ChangeScope::from_changes(
            &root,
            &[
                WorkspaceChange::Upsert(PathBuf::from("nested/source.rs")),
                WorkspaceChange::Upsert(absolute.clone()),
            ],
        )
        .expect("workspace-relative change scope");
        assert_eq!(scope.scan_paths(), std::slice::from_ref(&absolute));
        assert!(scope.contains(&absolute));
        assert!(!scope.contains(&root.join("source.rs")));

        for path in [
            PathBuf::from("../outside.rs"),
            directory.path().join("outside.rs"),
            root.join("../outside.rs"),
        ] {
            assert!(ChangeScope::from_changes(&root, &[WorkspaceChange::Upsert(path)]).is_err());
        }
    }

    #[tokio::test]
    async fn indexes_readable_unknown_sources_and_skips_binary_or_unavailable_readers() {
        let directory = tempdir().expect("temporary directory");
        std::fs::write(directory.path().join("readable"), "ordinary text 中文 😀").expect("text");
        let mut utf16 = vec![0xff, 0xfe];
        utf16.extend("UTF-16 文本 😀".encode_utf16().flat_map(u16::to_le_bytes));
        std::fs::write(directory.path().join("encoded"), utf16).expect("UTF-16");
        std::fs::write(
            directory.path().join("database"),
            b"SQLite format 3\0payload",
        )
        .expect("database");
        std::fs::write(directory.path().join("unrecognized"), [0, 1, 2, 3]).expect("binary");
        std::fs::write(
            directory.path().join("document.pdf"),
            b"%PDF-1.7\nASCII-only syntax",
        )
        .expect("PDF");
        let workspace = workspace(directory.path());
        let scanner = NativeScanner::default();
        let storage = MemoryStorage::default();
        let model = ConcurrentModel::new();
        let result = index_workspace(&IndexingContext {
            workspace_index: &workspace,
            storage: &storage,
            scanner: &scanner,
            embedding_model: &model,
            embedding_concurrency: Some(1),
            on_progress: None,
            signal: None,
            changes: &[],
        })
        .await
        .expect("index supported sources");
        assert_eq!(result.files_added, 2);
        let files = storage.list_files().expect("files");
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|file| file.formats == [FileFormat::Text]));
        assert_eq!(result.skipped.len(), 3);
    }

    #[tokio::test]
    async fn classification_errors_stay_with_files_and_missing_timestamps_are_not_cached() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("fixture");
        std::fs::write(&path, "text").expect("fixture");
        let workspace = workspace(directory.path());
        let discovered = DiscoveredFile {
            root: directory.path().to_path_buf(),
            relative_path: PathBuf::from("fixture"),
            size_bytes: 4,
            modified_epoch_ms: None,
            source_fingerprint: "metadata-v1:4:unknown".to_owned(),
        };
        let control = TaskControl::default();
        let (first, _) = classify_files(&workspace, vec![discovered.clone()], &[], &control)
            .await
            .expect("classify");
        let mut stored = first[0].file.clone();
        stored.formats = vec![FileFormat::Rust];
        stored.snapshot.content_hash = Some("previous hash".to_owned());
        stored.index_status = FileIndexStatus::Indexed {
            indexed_epoch_ms: 1,
            entity_count: 1,
        };
        let (second, _) = classify_files(
            &workspace,
            vec![discovered.clone()],
            &[stored.clone()],
            &control,
        )
        .await
        .expect("reclassify");
        assert_eq!(second[0].file.formats, [FileFormat::Text]);
        assert_eq!(compute_diff(second, &[stored]).modified, 1);
        std::fs::remove_file(path).expect("remove source during scan");
        let (missing, _) = classify_files(&workspace, vec![discovered], &[], &control)
            .await
            .expect("per-file failure");
        assert_eq!(missing.len(), 1);
        assert_eq!(
            missing[0].detection_error.as_ref().expect("error").code(),
            EngineError::NOT_FOUND
        );
        control.cancellation.cancel();
        assert!(
            classify_files(&workspace, Vec::new(), &[], &control)
                .await
                .is_err()
        );
    }

    #[test]
    fn keeps_host_errors_outside_the_engine_api_boundary() {
        let origin_line = line!() + 1;
        let error = map_host_error(HostError::cancelled("native host operation was cancelled"));
        assert_eq!(error.code(), EngineError::CANCELLED);
        assert!(
            error
                .origin()
                .file
                .ends_with("src/pipelines/indexing/pipeline.rs")
        );
        assert_eq!(error.origin().line, origin_line);
        let invalid = map_host_error(HostError::invalid_argument("bad root"));
        assert_eq!(invalid.code(), EngineError::INVALID_ARGUMENT);
    }
}
