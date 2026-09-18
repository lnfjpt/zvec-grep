use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use crate::{
    EngineError,
    api::{
        context::{
            ContextOptions, ContextResult,
            result::{
                ContentRange, ContextContainer, ContextCoverage, ContextDiagnostics, ContextItem,
                ContextItemKind, ContextItemStatus, ContextSource, EmptyReason, MatchedBy,
                RgDiagnostics, TimingEntry,
            },
        },
        index::{IndexOptions, IndexResult},
        info::{InfoOptions, InfoResult},
    },
    lexical::{
        LexicalSearchService,
        structure::{
            EnrichedLexicalMatch, StructureEnrichmentResult, enrich_lexical_matches_with_structure,
        },
        types::{LexicalCoverage, LexicalOptions, LexicalSearchReply, LexicalSearchRequest},
    },
    models::ModelRuntimeManager,
    pipelines::{
        indexing::service::WorkspaceIndexService, search::context::normalize_context_request,
    },
};

const DEFAULT_MAX_CONCURRENT_LEXICAL_SEARCHES: usize = 2;

#[derive(Clone, Debug)]
pub(crate) struct EngineService {
    lexical: LexicalSearchService,
    indexing: WorkspaceIndexService,
    models: ModelRuntimeManager,
    closed: Arc<AtomicBool>,
}

impl EngineService {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            lexical: LexicalSearchService::default()
                .with_max_searches(DEFAULT_MAX_CONCURRENT_LEXICAL_SEARCHES),
            indexing: WorkspaceIndexService::new(),
            models: ModelRuntimeManager::new(),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Retrieves context from an index or, when `rg` is enabled, embedded ripgrep.
    ///
    /// # Errors
    ///
    /// Returns an engine error when the request is invalid or its selected
    /// retrieval mode is unavailable.
    pub(crate) async fn context(
        &self,
        options: ContextOptions,
    ) -> Result<ContextResult, EngineError> {
        self.ensure_open()?;
        options.validate_file_selection()?;
        let root = resolve_root(options.root.as_deref())?;
        let normalized = normalize_context_request(&options)?;
        if !options.rg {
            return self
                .indexing
                .context(&self.models, &options, &normalized)
                .await;
        }
        if !options.rg_options.extra_args.is_empty() {
            return Err(EngineError::unsupported(
                "context rg_options.extra_args is not supported by the embedded backend",
            ));
        }
        let structure_max_file_size_bytes = options.max_file_size_bytes;
        let request = LexicalSearchRequest {
            root: Some(root.clone()),
            patterns: normalized.rg_patterns,
            pattern_files: options.rg_options.pattern_files.clone(),
            paths: options.rg_paths,
            limit: options.limit,
            options: LexicalOptions {
                matching: options.rg_options,
                hidden: options.hidden,
                no_ignore: options.no_ignore,
                follow: options.follow,
                globs: options.globs,
                insensitive_globs: options.insensitive_globs,
                file_types: options.file_types,
                excluded_file_types: options.excluded_file_types,
                ignore_files: options.ignore_files,
                max_depth: options.max_depth,
                max_file_size_bytes: options.max_file_size_bytes,
                modified_after_epoch_ms: options.modified_after_epoch_ms,
                modified_before_epoch_ms: options.modified_before_epoch_ms,
            },
        };
        let mut reply = self.lexical.search(&root, &request).await?;
        let structure_started = Instant::now();
        let enrichment = enrich_lexical_matches_with_structure(
            &root,
            std::mem::take(&mut reply.matches),
            structure_max_file_size_bytes,
        );
        let structure_duration = structure_started.elapsed();
        let mut result = context_from_lexical(normalized.display_query, reply, enrichment);
        result.diagnostics.timings.push(TimingEntry {
            name: "structure_enrichment".to_owned(),
            duration_micros: structure_duration
                .as_micros()
                .try_into()
                .unwrap_or(u64::MAX),
            count: None,
        });
        Ok(result)
    }

    /// Creates or refreshes the workspace index.
    ///
    /// # Errors
    ///
    /// Returns an engine error when indexing fails or no storage backend is configured.
    pub(crate) async fn index(&self, options: IndexOptions) -> Result<IndexResult, EngineError> {
        self.ensure_open()?;
        self.indexing.index(&self.models, options).await
    }

    /// Returns workspace index metadata and status.
    ///
    /// # Errors
    ///
    /// Returns an engine error when workspace metadata or status cannot be read.
    pub(crate) async fn info(&self, options: InfoOptions) -> Result<InfoResult, EngineError> {
        self.ensure_open()?;
        self.indexing.info(options).await
    }

    /// Drops the persisted workspace index.
    ///
    /// # Errors
    ///
    /// Returns an engine error when index removal fails or no storage backend is configured.
    pub(crate) async fn drop_index(&self, options: InfoOptions) -> Result<bool, EngineError> {
        self.ensure_open()?;
        std::future::ready(self.indexing.drop_index(&options)).await
    }

    /// Closes this service and rejects subsequent requests.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.models.close();
    }

    pub(crate) fn runtime_snapshot(&self) -> crate::EngineRuntimeSnapshot {
        let models = self.models.snapshot();
        crate::EngineRuntimeSnapshot {
            loaded_models: models.cached_runtimes,
            active_model_leases: models.active_leases,
        }
    }

    fn ensure_open(&self) -> Result<(), EngineError> {
        if self.closed.load(Ordering::Acquire) {
            Err(
                EngineError::resource_closed("engine instance has been closed")
                    .with_help("Create a new ZvecGrep instance before sending another request."),
            )
        } else {
            Ok(())
        }
    }
}

fn context_from_lexical(
    query: String,
    reply: LexicalSearchReply,
    enrichment: StructureEnrichmentResult,
) -> ContextResult {
    let hits_returned = enrichment.items.len();
    let empty_reason = (hits_returned == 0).then_some({
        if !reply.diagnostics.missing_paths.is_empty()
            && reply.diagnostics.searched_paths.is_empty()
        {
            EmptyReason::NoSearchableFiles
        } else {
            EmptyReason::NoMatches
        }
    });
    ContextResult {
        freshness: None,
        background_refresh: None,
        query,
        root: reply.root,
        source: ContextSource::Rg,
        coverage: match reply.coverage {
            LexicalCoverage::Exhaustive => ContextCoverage::RgExhaustive,
            LexicalCoverage::Truncated => ContextCoverage::RgTruncated,
        },
        workspace_index: None,
        items: enrichment
            .items
            .into_iter()
            .map(
                |EnrichedLexicalMatch {
                     matched: item,
                     container,
                 }| ContextItem {
                    kind: ContextItemKind::LexicalMatch,
                    rank: item.rank,
                    absolute_path: item.absolute_path,
                    relative_path: item.relative_path,
                    range: lexical_range(item.range),
                    excerpt_range: item.excerpt_range.map(lexical_range),
                    content: item.content,
                    content_role: Some(crate::api::context::result::ContextContentRole::Source),
                    outline: None,
                    status: ContextItemStatus::Fresh,
                    score: None,
                    matched_by: MatchedBy::Lexical,
                    metadata: container
                        .as_ref()
                        .and_then(|value| value.metadata.as_ref())
                        .cloned(),
                    entity_id: None,
                    container: container.map(|value| ContextContainer {
                        entity_id: value.entity_id.as_str().to_owned(),
                        range: value.range.into(),
                        metadata: value.metadata,
                    }),
                    trace: None,
                    query_groups: Vec::new(),
                    selection_reason: None,
                    coverage_group: None,
                },
            )
            .collect(),
        group_results: Vec::new(),
        diagnostics: ContextDiagnostics {
            empty_reason,
            index: None,
            rg: Some(RgDiagnostics {
                backend: reply.diagnostics.backend,
                command: reply.diagnostics.command,
                args: reply.diagnostics.args,
                ignored_directories: reply.diagnostics.ignored_directories,
                missing_paths: reply.diagnostics.missing_paths,
                searched_paths: reply.diagnostics.searched_paths,
                limit: reply.diagnostics.limit,
                truncated: reply.diagnostics.truncated,
            }),
            structure: Some(enrichment.diagnostics),
            timings: Vec::new(),
        },
    }
}

fn lexical_range(range: crate::domain::TextRange) -> ContentRange {
    range.into()
}

fn resolve_root(root: Option<&Path>) -> Result<PathBuf, EngineError> {
    std::path::absolute(root.unwrap_or_else(|| Path::new(".")))
        .map_err(|error| EngineError::from_io("failed to resolve workspace root", &error))
}
