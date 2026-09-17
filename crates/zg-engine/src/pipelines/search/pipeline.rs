use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::Instant,
};

use async_trait::async_trait;
use globset::{GlobBuilder, GlobMatcher};
use ignore::types::{Types, TypesBuilder};

use crate::{
    EngineError,
    domain::{Entity, EntityFragment, EntityId, FileId, FileRecord, SymbolType},
    models::{
        EmbeddingInput, EmbeddingModelInfo, EmbeddingOptions, EmbeddingPurpose, ModelError,
        ModelRuntimeLease,
    },
    storage::spi::{
        StoragePathFilter, StorageSearchFilter, StorageSearchHit, StoredSearchData,
        WorkspaceIndexStorage,
    },
};

use super::{
    path_filter::GlobFilter,
    types::{
        MatchedBy, SearchFinalTrace, SearchFusionTrace, SearchHitTrace, SearchRecallTrace,
        SearchRoute, SearchRouteMode, TimingEntry,
    },
};

const DEFAULT_LIMIT: usize = 7;
const RRF_K: f64 = 60.0;
const RECALL_INITIAL_DEPTH: usize = 200;
const RECALL_MAX_DEPTH: usize = 2_000;
const RECALL_GROWTH_FACTOR: usize = 2;
const RECALL_TARGET_FACTOR: usize = 5;
const RECALL_MIN_TARGET_CANDIDATES: usize = 50;

#[derive(Clone, Debug)]
pub(crate) struct SearchPlan {
    pub routes: Vec<SearchRoute>,
    pub limit: Option<usize>,
    pub trace: bool,
    pub prefer_symbol: bool,
    pub symbol_types: Vec<SymbolType>,
    pub include_paths: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub globs: Vec<String>,
    pub insensitive_globs: Vec<String>,
    pub file_types: Vec<String>,
    pub excluded_file_types: Vec<String>,
    pub modified_after_epoch_ms: Option<u64>,
    pub modified_before_epoch_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedSearchRoute {
    pub id: String,
    pub mode: SearchRouteMode,
    pub query: String,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchEvidence {
    pub fragment: EntityFragment,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchHit {
    pub entity: Entity,
    pub file: FileRecord,
    pub evidence: Vec<SearchEvidence>,
    pub rank: usize,
    pub score: f64,
    pub matched_by: MatchedBy,
    pub trace: Option<SearchHitTrace>,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchPlanResult {
    pub routes: Vec<ResolvedSearchRoute>,
    pub hits: Vec<SearchHit>,
    pub timings: Vec<TimingEntry>,
}

#[async_trait]
pub(crate) trait SearchEmbeddingRuntime: Send + Sync {
    fn info(&self) -> &EmbeddingModelInfo;

    async fn embed_queries(&self, queries: &[String]) -> Result<Vec<Vec<f32>>, ModelError>;
}

#[async_trait]
impl SearchEmbeddingRuntime for ModelRuntimeLease {
    fn info(&self) -> &EmbeddingModelInfo {
        self.info()
    }

    async fn embed_queries(&self, queries: &[String]) -> Result<Vec<Vec<f32>>, ModelError> {
        self.embed(
            &queries
                .iter()
                .cloned()
                .map(EmbeddingInput::text)
                .collect::<Vec<_>>(),
            EmbeddingOptions {
                purpose: Some(EmbeddingPurpose::Query),
                ..EmbeddingOptions::default()
            },
            None,
        )
        .await
        .map(|result| result.vectors)
    }
}

pub(crate) struct RequestEmbeddingRuntime<'a> {
    pub model: &'a ModelRuntimeLease,
    pub signal: Option<tokio_util::sync::CancellationToken>,
}

#[async_trait]
impl SearchEmbeddingRuntime for RequestEmbeddingRuntime<'_> {
    fn info(&self) -> &EmbeddingModelInfo {
        self.model.info()
    }

    async fn embed_queries(&self, queries: &[String]) -> Result<Vec<Vec<f32>>, ModelError> {
        self.model
            .embed(
                &queries
                    .iter()
                    .cloned()
                    .map(EmbeddingInput::text)
                    .collect::<Vec<_>>(),
                EmbeddingOptions {
                    purpose: Some(EmbeddingPurpose::Query),
                    signal: self.signal.clone(),
                    ..EmbeddingOptions::default()
                },
                None,
            )
            .await
            .map(|result| result.vectors)
    }
}

#[derive(Clone)]
struct RecallRoute {
    route: ResolvedSearchRoute,
    filter: Option<StorageSearchFilter>,
    vector_route_id: Option<String>,
}

struct Candidate {
    id: EntityId,
    file_id: FileId,
    sources: HashSet<SearchRouteMode>,
    recall: Vec<SearchRecallTrace>,
    evidence: Vec<InternalEvidence>,
    score: f64,
    rank: usize,
}

struct InternalEvidence {
    hit: StorageSearchHit,
    path: SearchRouteMode,
    route_id: String,
    rank: usize,
}

pub(crate) async fn search_workspace_index(
    workspace_root: &Path,
    plan: SearchPlan,
    storage: &dyn WorkspaceIndexStorage,
    embedding_model: Option<&dyn SearchEmbeddingRuntime>,
) -> Result<SearchPlanResult, EngineError> {
    let total_started = Instant::now();
    let plan_started = Instant::now();
    let routes = resolve_routes(&plan.routes)?;
    validate_modified_range(&plan)?;
    let limit = plan.limit.unwrap_or(DEFAULT_LIMIT);
    let plan_duration = plan_started.elapsed();

    let filter_started = Instant::now();
    let filter = search_plan_to_storage_filter(workspace_root, &plan, storage)?;
    let filter_duration = filter_started.elapsed();
    let has_searchable_files = !filter_matches_no_files(filter.as_ref());

    let embedding_started = Instant::now();
    let vectors = if has_searchable_files
        && routes
            .iter()
            .any(|route| route.mode == SearchRouteMode::Vector)
    {
        embed_vector_routes(&routes, require_embedding_model(embedding_model)?).await?
    } else {
        HashMap::new()
    };
    let embedding_duration = embedding_started.elapsed();

    let recall_started = Instant::now();
    let mut candidates = HashMap::new();
    if has_searchable_files && limit > 0 {
        collect_adaptive_recall(
            &routes,
            filter.as_ref(),
            plan.prefer_symbol,
            &vectors,
            limit,
            storage,
            &mut candidates,
        )?;
    }
    let recall_duration = recall_started.elapsed();

    let fusion_started = Instant::now();
    let fused = fuse_candidates(candidates);
    let selected = fused.into_iter().take(limit).collect::<Vec<_>>();
    let fusion_duration = fusion_started.elapsed();

    let load_started = Instant::now();
    let hits = load_candidates(selected, limit, plan.trace, storage)?;
    let load_duration = load_started.elapsed();

    Ok(SearchPlanResult {
        routes,
        hits,
        timings: vec![
            timing("search_plan", plan_duration),
            timing("search_filter", filter_duration),
            timing("query_embedding", embedding_duration),
            timing("recall", recall_duration),
            timing("fusion", fusion_duration),
            timing("load_results", load_duration),
            timing("search_total", total_started.elapsed()),
        ],
    })
}

fn resolve_routes(routes: &[SearchRoute]) -> Result<Vec<ResolvedSearchRoute>, EngineError> {
    if routes.is_empty() {
        return Err(EngineError::invalid_argument(
            "search plan requires at least one route",
        ));
    }
    let mut counts = HashMap::<SearchRouteMode, usize>::new();
    routes
        .iter()
        .enumerate()
        .map(|(index, route)| {
            let query = route.query.trim();
            if query.is_empty() {
                return Err(EngineError::invalid_argument(format!(
                    "search route {index} requires a non-empty query"
                )));
            }
            let count = counts.entry(route.mode).or_default();
            *count += 1;
            let base = match route.mode {
                SearchRouteMode::Fts => "fts",
                SearchRouteMode::Vector => "vector",
            };
            Ok(ResolvedSearchRoute {
                id: if *count == 1 {
                    base.to_owned()
                } else {
                    format!("{base}-{count}")
                },
                mode: route.mode,
                query: query.to_owned(),
            })
        })
        .collect()
}

fn validate_modified_range(plan: &SearchPlan) -> Result<(), EngineError> {
    if plan.modified_after_epoch_ms.is_some_and(|after| {
        plan.modified_before_epoch_ms
            .is_some_and(|before| after > before)
    }) {
        Err(EngineError::invalid_argument(
            "modified-after must not be later than modified-before",
        ))
    } else {
        Ok(())
    }
}

fn require_embedding_model(
    model: Option<&dyn SearchEmbeddingRuntime>,
) -> Result<&dyn SearchEmbeddingRuntime, EngineError> {
    model.ok_or_else(|| {
        EngineError::unsupported("vector search requires a configured embedding model")
    })
}

async fn embed_vector_routes(
    routes: &[ResolvedSearchRoute],
    model: &dyn SearchEmbeddingRuntime,
) -> Result<HashMap<String, Vec<f32>>, EngineError> {
    let vector_routes = routes
        .iter()
        .filter(|route| route.mode == SearchRouteMode::Vector)
        .collect::<Vec<_>>();
    let maximum = model.info().limits.max_batch_size;
    if maximum == 0 {
        return Err(EngineError::internal(
            "embedding model has a zero query batch limit",
        ));
    }
    let mut vectors = HashMap::new();
    for batch in vector_routes.chunks(maximum) {
        let queries = batch
            .iter()
            .map(|route| route.query.clone())
            .collect::<Vec<_>>();
        let embedded = model
            .embed_queries(&queries)
            .await
            .map_err(ModelError::into_engine_error)?;
        if embedded.len() != batch.len() {
            return Err(EngineError::internal(
                "embedding model returned the wrong number of query vectors",
            ));
        }
        for (route, vector) in batch.iter().zip(embedded) {
            vectors.insert(route.id.clone(), vector);
        }
    }
    Ok(vectors)
}

#[allow(clippy::too_many_arguments)]
fn collect_adaptive_recall(
    routes: &[ResolvedSearchRoute],
    filter: Option<&StorageSearchFilter>,
    prefer_symbol: bool,
    vectors: &HashMap<String, Vec<f32>>,
    limit: usize,
    storage: &dyn WorkspaceIndexStorage,
    candidates: &mut HashMap<EntityId, Candidate>,
) -> Result<(), EngineError> {
    let recall_routes = build_recall_routes(routes, filter, prefer_symbol);
    let target = (limit * RECALL_TARGET_FACTOR).max(RECALL_MIN_TARGET_CANDIDATES);
    let mut previous_depth = 0;
    let mut depth = RECALL_INITIAL_DEPTH;
    loop {
        let mut saturated = false;
        for route in &recall_routes {
            let hits = match route.route.mode {
                SearchRouteMode::Fts => {
                    storage.search_fts(&route.route.query, depth, route.filter.as_ref())?
                }
                SearchRouteMode::Vector => vectors
                    .get(route.vector_route_id.as_deref().unwrap_or(&route.route.id))
                    .map_or_else(
                        || Ok(Vec::new()),
                        |vector| storage.search_vector(vector, depth, route.filter.as_ref()),
                    )?,
            };
            saturated |= hits.len() >= depth;
            add_recall_hits(candidates, &hits, &route.route, previous_depth)?;
        }
        if candidates.len() >= target || !saturated || depth >= RECALL_MAX_DEPTH {
            return Ok(());
        }
        previous_depth = depth;
        depth = (depth * RECALL_GROWTH_FACTOR).min(RECALL_MAX_DEPTH);
    }
}

fn build_recall_routes(
    routes: &[ResolvedSearchRoute],
    filter: Option<&StorageSearchFilter>,
    prefer_symbol: bool,
) -> Vec<RecallRoute> {
    let mut output = routes
        .iter()
        .cloned()
        .map(|route| RecallRoute {
            vector_route_id: (route.mode == SearchRouteMode::Vector).then(|| route.id.clone()),
            route,
            filter: filter.cloned(),
        })
        .collect::<Vec<_>>();
    if prefer_symbol {
        for route in routes {
            let symbol_names = extract_symbol_names(&route.query);
            if symbol_names.is_empty() {
                continue;
            }
            let mut symbol_filter = filter.cloned().unwrap_or_default();
            symbol_filter.symbol_names = Some(symbol_names);
            output.push(RecallRoute {
                route: ResolvedSearchRoute {
                    id: format!("{}.prefer-symbol", route.id),
                    mode: SearchRouteMode::Fts,
                    query: route.query.clone(),
                },
                filter: Some(symbol_filter),
                vector_route_id: None,
            });
        }
    }
    output
}

fn add_recall_hits(
    candidates: &mut HashMap<EntityId, Candidate>,
    hits: &[StorageSearchHit],
    route: &ResolvedSearchRoute,
    start_index: usize,
) -> Result<(), EngineError> {
    for (index, hit) in hits.iter().enumerate().skip(start_index) {
        let rank = index + 1;
        let candidate = candidates
            .entry(hit.entity_id.clone())
            .or_insert_with(|| Candidate {
                id: hit.entity_id.clone(),
                file_id: hit.file_id,
                sources: HashSet::new(),
                recall: Vec::new(),
                evidence: Vec::new(),
                score: 0.0,
                rank: usize::MAX,
            });
        if candidate.file_id != hit.file_id {
            return Err(EngineError::storage_failure(
                "search hits disagree on entity file ownership",
            ));
        }
        candidate.sources.insert(route.mode);
        candidate.evidence.push(InternalEvidence {
            hit: hit.clone(),
            path: route.mode,
            route_id: route.id.clone(),
            rank,
        });
        add_or_update_recall(
            &mut candidate.recall,
            SearchRecallTrace {
                path: route.mode,
                route_id: route.id.clone(),
                query: route.query.clone(),
                found: true,
                rank: Some(rank),
                score: Some(hit.score),
                forced: false,
                reason: None,
            },
        );
    }
    Ok(())
}

fn add_or_update_recall(recall: &mut Vec<SearchRecallTrace>, next: SearchRecallTrace) {
    if let Some(existing) = recall
        .iter_mut()
        .find(|item| item.path == next.path && item.route_id == next.route_id)
    {
        if existing
            .rank
            .is_none_or(|rank| next.rank.is_some_and(|next_rank| next_rank < rank))
        {
            *existing = next;
        }
    } else {
        recall.push(next);
    }
}

fn fuse_candidates(candidates: HashMap<EntityId, Candidate>) -> Vec<Candidate> {
    let mut fused = candidates
        .into_values()
        .map(|mut candidate| {
            candidate.score = candidate
                .recall
                .iter()
                .filter_map(|trace| trace.rank)
                .map(|rank| 1.0 / (RRF_K + rank_as_f64(rank)))
                .sum();
            candidate
        })
        .collect::<Vec<_>>();
    fused.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.id.as_str().cmp(right.id.as_str()))
    });
    for (index, candidate) in fused.iter_mut().enumerate() {
        candidate.rank = index + 1;
    }
    fused
}

fn load_candidates(
    candidates: Vec<Candidate>,
    limit: usize,
    trace: bool,
    storage: &dyn WorkspaceIndexStorage,
) -> Result<Vec<SearchHit>, EngineError> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let mut seen = HashSet::new();
    let hits = candidates
        .iter()
        .flat_map(|candidate| &candidate.evidence)
        .filter(|evidence| seen.insert(evidence.hit.document_id.clone()))
        .map(|evidence| evidence.hit.clone())
        .collect::<Vec<_>>();
    let mut data = storage.load_search_hits(&hits)?;
    candidates
        .into_iter()
        .map(|candidate| candidate_to_hit(candidate, limit, trace, &mut data))
        .collect()
}

fn candidate_to_hit(
    candidate: Candidate,
    limit: usize,
    trace: bool,
    data: &mut StoredSearchData,
) -> Result<SearchHit, EngineError> {
    let stored = data.entities.remove(&candidate.id).ok_or_else(|| {
        EngineError::storage_failure(format!("missing search entity {}", candidate.id.as_str()))
    })?;
    if stored.entity.id != candidate.id
        || stored.entity.file_id != candidate.file_id
        || stored.file.id != candidate.file_id
    {
        return Err(EngineError::storage_failure(
            "loaded search entity has inconsistent ownership",
        ));
    }
    let matched_by = derive_matched_by(&candidate.sources);
    let mut evidence = candidate.evidence;
    evidence.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then_with(|| route_mode_order(left.path).cmp(&route_mode_order(right.path)))
            .then_with(|| left.route_id.cmp(&right.route_id))
            .then_with(|| left.hit.document_id.cmp(&right.hit.document_id))
    });
    let evidence = evidence
        .into_iter()
        .map(|evidence| {
            let fragment = data
                .fragments
                .get(&evidence.hit.document_id)
                .ok_or_else(|| {
                    EngineError::storage_failure(format!(
                        "missing search fragment {}",
                        evidence.hit.document_id
                    ))
                })?;
            if fragment.document_id() != evidence.hit.document_id
                || fragment.entity_id() != &candidate.id
                || fragment.file_id() != &candidate.file_id
            {
                return Err(EngineError::storage_failure(
                    "loaded search fragment has inconsistent ownership",
                ));
            }
            Ok(SearchEvidence {
                fragment: fragment.clone(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SearchHit {
        entity: stored.entity,
        file: stored.file,
        evidence,
        rank: candidate.rank,
        score: candidate.score,
        matched_by,
        trace: trace.then_some(SearchHitTrace {
            recall: candidate.recall,
            fusion: SearchFusionTrace {
                rank: candidate.rank,
                score: candidate.score,
                forced: false,
            },
            final_selection: SearchFinalTrace {
                returned_by_limit: candidate.rank <= limit,
                cutoff_rank: limit,
            },
        }),
    })
}

fn derive_matched_by(sources: &HashSet<SearchRouteMode>) -> MatchedBy {
    match (
        sources.contains(&SearchRouteMode::Fts),
        sources.contains(&SearchRouteMode::Vector),
    ) {
        (true, true) => MatchedBy::FtsAndVector,
        (false, true) => MatchedBy::Vector,
        (true | false, false) => MatchedBy::Fts,
    }
}

fn route_mode_order(mode: SearchRouteMode) -> u8 {
    match mode {
        SearchRouteMode::Fts => 0,
        SearchRouteMode::Vector => 1,
    }
}

fn extract_symbol_names(query: &str) -> Vec<String> {
    const KEYWORDS: &[&str] = &[
        "class",
        "struct",
        "enum",
        "interface",
        "function",
        "method",
        "type",
        "const",
        "let",
        "var",
        "namespace",
        "where",
        "find",
        "explain",
    ];
    let mut names = HashSet::new();
    for token in query.split(|character: char| {
        !(character.is_ascii_alphanumeric() || matches!(character, '_' | ':' | '~'))
    }) {
        let token = token.trim_matches(':');
        if token.is_empty()
            || KEYWORDS.contains(&token.to_ascii_lowercase().as_str())
            || !token.chars().next().is_some_and(|character| {
                character.is_ascii_alphabetic() || matches!(character, '_' | '~')
            })
        {
            continue;
        }
        let parts = token
            .split("::")
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        let Some(name) = parts.last() else {
            continue;
        };
        let resolved = if parts.len() >= 2
            && parts[parts.len() - 2]
                .chars()
                .next()
                .is_some_and(|character| {
                    character.is_ascii_uppercase() || matches!(character, '_' | '~')
                }) {
            format!("{}::{name}", parts[parts.len() - 2])
        } else {
            (*name).to_owned()
        };
        names.insert(resolved);
    }
    let mut names = names.into_iter().collect::<Vec<_>>();
    names.sort();
    names
}

fn search_plan_to_storage_filter(
    workspace_root: &Path,
    plan: &SearchPlan,
    storage: &dyn WorkspaceIndexStorage,
) -> Result<Option<StorageSearchFilter>, EngineError> {
    let has_globs = !plan.globs.is_empty() || !plan.insensitive_globs.is_empty();
    let glob_filter = GlobFilter::new(workspace_root, &plan.globs, &plan.insensitive_globs)?;
    let path = if has_globs {
        glob_filter.pushdown(storage)?
    } else {
        None
    };
    let needs_file_filter = !plan.include_paths.is_empty()
        || !plan.exclude_paths.is_empty()
        || (has_globs && path.is_none())
        || !plan.file_types.is_empty()
        || !plan.excluded_file_types.is_empty()
        || plan.modified_after_epoch_ms.is_some()
        || plan.modified_before_epoch_ms.is_some();
    let file_ids = if !needs_file_filter {
        None
    } else if plan.modified_after_epoch_ms.is_some() || plan.modified_before_epoch_ms.is_some() {
        Some(resolve_filtered_file_ids(
            workspace_root,
            plan,
            &storage.list_files()?,
        )?)
    } else {
        let paths = storage.list_file_paths()?;
        Some(resolve_filtered_paths(
            workspace_root,
            plan,
            paths.iter().map(|(id, path)| (*id, path.as_path(), None)),
        )?)
    };
    let symbol_types = (!plan.symbol_types.is_empty()).then(|| plan.symbol_types.clone());
    if file_ids.is_none() && symbol_types.is_none() && path.is_none() {
        Ok(None)
    } else {
        Ok(Some(StorageSearchFilter {
            path,
            file_ids,
            symbol_types,
            ..StorageSearchFilter::default()
        }))
    }
}

fn resolve_filtered_file_ids(
    workspace_root: &Path,
    plan: &SearchPlan,
    files: &[FileRecord],
) -> Result<Vec<FileId>, EngineError> {
    resolve_filtered_paths(
        workspace_root,
        plan,
        files.iter().map(|file| {
            (
                file.id,
                file.relative_path.as_path(),
                file.snapshot.modified_epoch_ms,
            )
        }),
    )
}

fn resolve_filtered_paths<'a>(
    workspace_root: &Path,
    plan: &SearchPlan,
    files: impl IntoIterator<Item = (FileId, &'a Path, Option<u64>)>,
) -> Result<Vec<FileId>, EngineError> {
    let include = plan
        .include_paths
        .iter()
        .map(|pattern| PathMatcher::path(pattern))
        .collect::<Result<Vec<_>, _>>()?;
    let exclude = plan
        .exclude_paths
        .iter()
        .map(|pattern| PathMatcher::path(pattern))
        .collect::<Result<Vec<_>, _>>()?;
    let globs = GlobFilter::new(workspace_root, &plan.globs, &plan.insensitive_globs)?;
    let types = build_file_types(&plan.file_types, &plan.excluded_file_types)?;
    Ok(files
        .into_iter()
        .filter(|(_, relative, modified)| {
            let absolute = workspace_root.join(relative);
            (include.is_empty()
                || include.iter().any(|matcher| {
                    matcher.is_match(if matcher.absolute {
                        &absolute
                    } else {
                        relative
                    })
                }))
                && !exclude.iter().any(|matcher| {
                    matcher.is_match(if matcher.absolute {
                        &absolute
                    } else {
                        relative
                    })
                })
                && globs.is_match(relative)
                && !types.matched(relative, false).is_ignore()
                && plan
                    .modified_after_epoch_ms
                    .is_none_or(|after| modified.is_some_and(|modified| modified >= after))
                && plan
                    .modified_before_epoch_ms
                    .is_none_or(|before| modified.is_some_and(|modified| modified <= before))
        })
        .map(|(id, _, _)| id)
        .collect())
}

struct PathMatcher {
    normalized: String,
    matcher: Option<GlobMatcher>,
    absolute: bool,
}

impl PathMatcher {
    fn path(pattern: &str) -> Result<Self, EngineError> {
        let normalized = normalize_pattern(pattern);
        if normalized.is_empty() {
            return Err(EngineError::invalid_argument(
                "path filter must not be empty",
            ));
        }
        let absolute = is_absolute_pattern(&normalized);
        let matcher = has_glob(&normalized)
            .then(|| compile_glob(&normalized, false))
            .transpose()?;
        Ok(Self {
            normalized,
            matcher,
            absolute,
        })
    }

    fn is_match(&self, path: &Path) -> bool {
        if let Some(matcher) = &self.matcher {
            return matcher.is_match(path)
                || self
                    .normalized
                    .strip_suffix("/**")
                    .is_some_and(|directory| {
                        compile_glob(directory, false).is_ok_and(|matcher| matcher.is_match(path))
                    });
        }
        path == Path::new(&self.normalized)
            || path.starts_with(Path::new(self.normalized.trim_end_matches('/')))
    }
}

fn compile_glob(pattern: &str, case_insensitive: bool) -> Result<GlobMatcher, EngineError> {
    let normalized = normalize_pattern(pattern);
    let pattern = if !normalized.contains('/') && normalized != "**" {
        format!("**/{normalized}")
    } else {
        normalized
    };
    let mut builder = GlobBuilder::new(&pattern);
    builder
        .case_insensitive(case_insensitive)
        .literal_separator(true)
        .backslash_escape(false)
        .allow_unclosed_class(true);
    builder
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| {
            EngineError::invalid_argument(format!("invalid glob {pattern:?}: {error}"))
        })
}

fn build_file_types(included: &[String], excluded: &[String]) -> Result<Types, EngineError> {
    let mut builder = TypesBuilder::new();
    builder.add_defaults();
    for name in included {
        builder.select(&file_type_name(name));
    }
    for name in excluded {
        builder.negate(&file_type_name(name));
    }
    builder.build().map_err(|error| {
        EngineError::invalid_argument(format!("invalid ripgrep file type selection: {error}"))
    })
}

fn file_type_name(name: &str) -> String {
    let normalized = name.trim().trim_start_matches('.').to_ascii_lowercase();
    match normalized.as_str() {
        "bash" | "zsh" => "sh",
        "cjs" | "jsx" | "mjs" => "js",
        "cp" | "cc" | "cxx" => "cpp",
        "hpp" | "hxx" | "hh" => "h",
        "markdown" | "mdx" => "md",
        "pyi" => "py",
        "rb" => "ruby",
        "rs" => "rust",
        "tsx" => "ts",
        "yml" => "yaml",
        _ => &normalized,
    }
    .to_owned()
}

fn filter_matches_no_files(filter: Option<&StorageSearchFilter>) -> bool {
    filter.is_some_and(|filter| {
        filter.file_ids.as_ref().is_some_and(Vec::is_empty)
            || filter.path == Some(StoragePathFilter::None)
    })
}

fn normalize_pattern(pattern: &str) -> String {
    let mut normalized = pattern.trim().replace('\\', "/");
    while normalized.contains("//") {
        normalized = normalized.replace("//", "/");
    }
    if !is_absolute_pattern(&normalized) {
        while let Some(stripped) = normalized.strip_prefix("./") {
            normalized = stripped.to_owned();
        }
    }
    normalized
}

#[cfg(test)]
fn normalize_path(path: &Path) -> String {
    normalize_pattern(&path.to_string_lossy())
}

fn is_absolute_pattern(pattern: &str) -> bool {
    pattern.starts_with('/')
        || pattern.as_bytes().get(1) == Some(&b':') && pattern.as_bytes().get(2) == Some(&b'/')
}

fn has_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '['])
}

fn rank_as_f64(rank: usize) -> f64 {
    f64::from(u32::try_from(rank).unwrap_or(u32::MAX))
}

fn timing(name: &str, duration: std::time::Duration) -> TimingEntry {
    TimingEntry {
        name: name.to_owned(),
        duration_micros: duration.as_micros().try_into().unwrap_or(u64::MAX),
        count: None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;

    use crate::{
        domain::{
            Content, Entity, EntityContent, EntityFragment, EntityId, FileFormat, FileId,
            FileIndexStatus, FileRecord, FileSnapshot, FragmentId, SourceRange, TextRange,
            WindowFragment,
        },
        models::{
            EmbeddingInputKind, EmbeddingMetric, EmbeddingModelInfo, EmbeddingModelLimits,
            ModelError,
        },
        storage::spi::{
            IndexedFragment, StoragePathFilter, StorageResult, StorageSearchFilter,
            StorageSearchHit, StorageSearchPath, StoredEntity, StoredSearchData,
            WorkspaceIndexStorage,
        },
    };

    use super::{
        MatchedBy, SearchEmbeddingRuntime, SearchPlan, SearchRoute, SearchRouteMode,
        file_type_name, normalize_path, search_workspace_index,
    };

    #[test]
    fn normalizes_absolute_paths_and_ripgrep_file_type_aliases() {
        assert_eq!(
            normalize_path(Path::new("/workspace/src/lib.rs")),
            "/workspace/src/lib.rs"
        );
        assert_eq!(file_type_name(".RS"), "rust");
        assert_eq!(file_type_name("ALL"), "all");
    }

    struct FixtureModel {
        info: EmbeddingModelInfo,
        calls: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl FixtureModel {
        fn new() -> Self {
            Self {
                info: EmbeddingModelInfo {
                    reference: "local/fixture".to_owned(),
                    provider: "local".to_owned(),
                    name: "fixture".to_owned(),
                    dimension: 2,
                    metric: EmbeddingMetric::Cosine,
                    endpoint: None,
                    default_concurrency: Some(1),
                    input_kinds: vec![EmbeddingInputKind::Text],
                    limits: EmbeddingModelLimits {
                        max_batch_size: 8,
                        max_input_tokens: None,
                        max_image_bytes: None,
                    },
                },
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl SearchEmbeddingRuntime for FixtureModel {
        fn info(&self) -> &EmbeddingModelInfo {
            &self.info
        }

        async fn embed_queries(&self, queries: &[String]) -> Result<Vec<Vec<f32>>, ModelError> {
            self.calls
                .lock()
                .expect("query call mutex")
                .push(queries.to_vec());
            Ok(queries.iter().map(|_| vec![0.25, 0.75]).collect())
        }
    }

    struct FixtureStorage {
        paths_only: bool,
        path_pushdown: bool,
        files: Vec<FileRecord>,
        entities: HashMap<String, StoredEntity>,
        fts: HashMap<String, Vec<FixtureHit>>,
        vector: Vec<FixtureHit>,
        filters: Arc<Mutex<Vec<Option<StorageSearchFilter>>>>,
        load_batches: Mutex<Vec<Vec<StorageSearchHit>>>,
    }

    struct FixtureHit {
        hit: StorageSearchHit,
        fragment: EntityFragment,
    }

    impl FixtureStorage {
        fn hits_with_filter(
            &self,
            hits: &[FixtureHit],
            limit: usize,
            filter: Option<&StorageSearchFilter>,
        ) -> Vec<StorageSearchHit> {
            self.filters
                .lock()
                .expect("filter mutex")
                .push(filter.cloned());
            hits.iter()
                .filter(|hit| {
                    filter
                        .and_then(|filter| filter.file_ids.as_ref())
                        .is_none_or(|ids| ids.contains(&hit.hit.file_id))
                })
                .filter(|hit| {
                    filter
                        .and_then(|filter| filter.entity_ids.as_ref())
                        .is_none_or(|ids| ids.contains(&hit.hit.entity_id))
                })
                .take(limit)
                .map(|hit| hit.hit.clone())
                .collect()
        }
    }

    #[async_trait]
    impl WorkspaceIndexStorage for FixtureStorage {
        fn is_read_only(&self) -> bool {
            true
        }

        fn list_files(&self) -> StorageResult<Vec<FileRecord>> {
            assert!(
                !self.paths_only,
                "path filters must only read the path projection"
            );
            assert!(!self.path_pushdown, "pushdown must not scan file records");
            Ok(self.files.clone())
        }

        fn list_file_paths(&self) -> StorageResult<Vec<(FileId, PathBuf)>> {
            assert!(!self.path_pushdown, "pushdown must not scan paths");
            Ok(self
                .files
                .iter()
                .map(|file| (file.id, file.relative_path.to_path_buf()))
                .collect())
        }

        fn supports_path_filters(&self) -> bool {
            self.path_pushdown
        }

        fn has_non_unicode_file_names(&self) -> StorageResult<bool> {
            Ok(self.files.iter().any(|file| {
                file.relative_path
                    .file_name()
                    .is_some_and(|name| name.to_str().is_none())
            }))
        }

        fn load_search_hits(&self, hits: &[StorageSearchHit]) -> StorageResult<StoredSearchData> {
            self.load_batches
                .lock()
                .expect("load batches")
                .push(hits.to_vec());
            let mut data = StoredSearchData::default();
            for hit in hits {
                if let Some(entity) = self.entities.get(hit.entity_id.as_str()) {
                    data.entities.insert(hit.entity_id.clone(), entity.clone());
                }
                if let Some(found) = self
                    .fts
                    .values()
                    .flatten()
                    .chain(&self.vector)
                    .find(|found| found.hit.document_id == hit.document_id)
                {
                    data.fragments
                        .insert(hit.document_id.clone(), found.fragment.clone());
                }
            }
            Ok(data)
        }

        fn search_fts(
            &self,
            query: &str,
            limit: usize,
            filter: Option<&StorageSearchFilter>,
        ) -> StorageResult<Vec<StorageSearchHit>> {
            Ok(self.hits_with_filter(
                self.fts.get(query).map_or(&[], Vec::as_slice),
                limit,
                filter,
            ))
        }

        fn search_vector(
            &self,
            _vector: &[f32],
            limit: usize,
            filter: Option<&StorageSearchFilter>,
        ) -> StorageResult<Vec<StorageSearchHit>> {
            Ok(self.hits_with_filter(&self.vector, limit, filter))
        }

        fn replace_file(
            &self,
            _file: &FileRecord,
            _entries: &[IndexedFragment],
        ) -> StorageResult<()> {
            Ok(())
        }

        fn mark_file_failed(&self, _file: &FileRecord, _error: &str) -> StorageResult<()> {
            Ok(())
        }

        fn delete_file(&self, _file_id: FileId) -> StorageResult<()> {
            Ok(())
        }

        async fn finalize_writes(&self) -> StorageResult<()> {
            Ok(())
        }

        fn close(&self) -> StorageResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn fuses_fts_and_vector_routes_with_main_compatible_rrf() {
        let file = file(1, "src/lib.rs", "rust", 100);
        let entity_a = entity("a", &file, "alpha entity");
        let entity_b = entity("b", &file, "beta entity");
        let a_fts = hit(&entity_a, "a-fts", StorageSearchPath::Fts, 9.0);
        let b_fts = hit(&entity_b, "b-fts", StorageSearchPath::Fts, 8.0);
        let b_vector = hit(&entity_b, "b-vector", StorageSearchPath::Vector, 0.9);
        let a_vector = hit(&entity_a, "a-vector", StorageSearchPath::Vector, 0.8);
        let storage = FixtureStorage {
            paths_only: false,
            path_pushdown: false,
            files: vec![file],
            entities: HashMap::from([("a".to_owned(), entity_a), ("b".to_owned(), entity_b)]),
            fts: HashMap::from([("alpha".to_owned(), vec![a_fts, b_fts])]),
            vector: vec![b_vector, a_vector],
            filters: Arc::new(Mutex::new(Vec::new())),
            load_batches: Mutex::default(),
        };
        let model = FixtureModel::new();

        let result = search_workspace_index(
            Path::new("/workspace"),
            plan(vec![
                SearchRoute {
                    mode: SearchRouteMode::Fts,
                    query: "alpha".to_owned(),
                },
                SearchRoute {
                    mode: SearchRouteMode::Vector,
                    query: "alpha".to_owned(),
                },
            ]),
            &storage,
            Some(&model),
        )
        .await
        .expect("hybrid search");

        assert_eq!(result.hits.len(), 2);
        assert_eq!(result.hits[0].entity.id.as_str(), "a");
        assert_eq!(result.hits[1].entity.id.as_str(), "b");
        assert!(
            result
                .hits
                .iter()
                .all(|hit| hit.matched_by == MatchedBy::FtsAndVector)
        );
        assert_eq!(result.hits[0].evidence.len(), 2);
        assert_eq!(
            result.hits[0]
                .trace
                .as_ref()
                .map(|trace| trace.recall.len()),
            Some(2)
        );
        assert_eq!(
            model.calls.lock().expect("query calls").as_slice(),
            &[vec!["alpha".to_owned()]]
        );
    }

    #[tokio::test]
    async fn loads_only_selected_entities_once_after_adaptive_recall() {
        let source = file(1, "src/lib.rs", "rust", 100);
        let selected = entity("selected", &source, "selected outline");
        let discarded = entity("discarded", &source, "discarded outline");
        let mut fts = (0..205)
            .map(|index| {
                hit(
                    &selected,
                    &format!("window-{index:03}"),
                    StorageSearchPath::Fts,
                    1.0,
                )
            })
            .collect::<Vec<_>>();
        fts.push(hit(
            &discarded,
            "discarded-window",
            StorageSearchPath::Fts,
            0.5,
        ));
        let mut storage = pushdown_storage();
        storage
            .entities
            .insert("selected".to_owned(), selected.clone());
        storage.entities.insert("discarded".to_owned(), discarded);
        storage.fts.insert("query".to_owned(), fts);
        storage.vector = vec![hit(&selected, "window-000", StorageSearchPath::Vector, 0.9)];
        let mut plan = plan(vec![
            SearchRoute {
                mode: SearchRouteMode::Fts,
                query: "query".to_owned(),
            },
            SearchRoute {
                mode: SearchRouteMode::Vector,
                query: "query".to_owned(),
            },
        ]);
        plan.limit = Some(1);

        let result = search_workspace_index(
            Path::new("/workspace"),
            plan,
            &storage,
            Some(&FixtureModel::new()),
        )
        .await
        .expect("search");

        assert_eq!(result.hits.len(), 1);
        let selected_hit = &result.hits[0];
        assert_eq!(selected_hit.entity.id, selected.entity.id);
        assert_eq!(selected_hit.evidence.len(), 206);
        assert_eq!(
            selected_hit.evidence[0].fragment.document_id(),
            "window-000"
        );
        assert_eq!(
            selected_hit.evidence[1].fragment.document_id(),
            "window-000"
        );
        let trace = selected_hit.trace.as_ref().expect("trace");
        assert_eq!(trace.recall.len(), 2);
        assert!(trace.recall.iter().all(|recall| recall.rank == Some(1)));
        let batches = storage.load_batches.lock().expect("load batches");
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 205);
        assert!(
            batches[0]
                .iter()
                .all(|hit| hit.entity_id == selected.entity.id)
        );
        assert_eq!(storage.filters.lock().expect("filters").len(), 4);
    }

    #[tokio::test]
    async fn rejects_missing_entities_and_mismatched_fragment_ownership() {
        let source = file(1, "src/lib.rs", "rust", 100);
        let stored = entity("entity", &source, "source");
        for missing in [true, false] {
            let mut storage = pushdown_storage();
            let mut recalled = hit(&stored, "window", StorageSearchPath::Fts, 1.0);
            if !missing {
                storage.entities.insert("entity".to_owned(), stored.clone());
                let EntityFragment::Window(window) = &mut recalled.fragment else {
                    unreachable!("window fixture");
                };
                window.file_id = FileId::new(2);
            }
            storage.fts.insert("query".to_owned(), vec![recalled]);
            let plan = plan(vec![SearchRoute {
                mode: SearchRouteMode::Fts,
                query: "query".to_owned(),
            }]);
            let error = search_workspace_index(Path::new("/workspace"), plan, &storage, None)
                .await
                .expect_err("invalid stored result must fail");
            assert!(error.to_string().contains(if missing {
                "missing search entity"
            } else {
                "inconsistent ownership"
            }));
        }
    }

    #[tokio::test]
    async fn pushes_file_and_symbol_filters_into_storage() {
        let source = file(1, "src/service.rs", "rust", 200);
        let docs = file(2, "docs/service.md", "markdown", 200);
        let source_entity = entity("source-entity", &source, "Service implementation");
        let docs_entity = entity("docs-entity", &docs, "Service documentation");
        let filters = Arc::new(Mutex::new(Vec::new()));
        let storage = FixtureStorage {
            paths_only: false,
            path_pushdown: false,
            files: vec![source.clone(), docs.clone()],
            entities: HashMap::from([
                ("source-entity".to_owned(), source_entity.clone()),
                ("docs-entity".to_owned(), docs_entity.clone()),
            ]),
            fts: HashMap::from([(
                "Service".to_owned(),
                vec![
                    hit(&source_entity, "source-hit", StorageSearchPath::Fts, 2.0),
                    hit(&docs_entity, "docs-hit", StorageSearchPath::Fts, 1.0),
                ],
            )]),
            vector: Vec::new(),
            filters: Arc::clone(&filters),
            load_batches: Mutex::default(),
        };
        let mut plan = plan(vec![SearchRoute {
            mode: SearchRouteMode::Fts,
            query: "Service".to_owned(),
        }]);
        plan.include_paths = vec!["src".to_owned()];
        plan.file_types = vec!["rs".to_owned()];
        plan.prefer_symbol = true;

        let result = search_workspace_index(Path::new("/workspace"), plan, &storage, None)
            .await
            .expect("filtered search");

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].file.id, source.id);
        let filters = filters.lock().expect("captured filters");
        assert!(
            filters
                .iter()
                .flatten()
                .all(|filter| { filter.file_ids.as_deref() == Some(&[source.id]) })
        );
        assert!(filters.iter().flatten().any(|filter| {
            filter
                .symbol_names
                .as_ref()
                .is_some_and(|names| names.contains(&"Service".to_owned()))
        }));
    }

    #[test]
    fn resolves_absolute_and_relative_filters_against_the_current_workspace_root() {
        let source = file(1, "src/service.rs", "rust", 200);
        let files = [source.clone()];
        let first_root = Path::new("/original/workspace");
        let moved_root = Path::new("/moved/workspace");
        let mut plan = plan(Vec::new());

        plan.include_paths = vec!["src/**/*.rs".to_owned()];
        for root in [first_root, moved_root] {
            assert_eq!(
                super::resolve_filtered_file_ids(root, &plan, &files).expect("relative filter"),
                std::slice::from_ref(&source.id)
            );
        }

        plan.include_paths = vec!["/moved/workspace/src/**/*.rs".to_owned()];
        assert!(
            super::resolve_filtered_file_ids(first_root, &plan, &files)
                .expect("old root filter")
                .is_empty()
        );
        assert_eq!(
            super::resolve_filtered_file_ids(moved_root, &plan, &files).expect("moved root filter"),
            std::slice::from_ref(&source.id)
        );

        plan.include_paths.clear();
        plan.globs = vec!["src/**/*.rs".to_owned()];
        for root in [first_root, moved_root] {
            assert_eq!(
                super::resolve_filtered_file_ids(root, &plan, &files).expect("relative glob"),
                std::slice::from_ref(&source.id)
            );
        }
        plan.exclude_paths = vec!["/moved/workspace/src".to_owned()];
        assert!(
            super::resolve_filtered_file_ids(moved_root, &plan, &files)
                .expect("absolute exclusion")
                .is_empty()
        );
    }

    fn pushdown_storage() -> FixtureStorage {
        FixtureStorage {
            paths_only: false,
            path_pushdown: true,
            files: Vec::new(),
            entities: HashMap::new(),
            fts: HashMap::new(),
            vector: Vec::new(),
            filters: Arc::default(),
            load_batches: Mutex::default(),
        }
    }

    #[test]
    fn complex_globs_and_file_types_only_read_the_path_projection() {
        let mut storage = pushdown_storage();
        storage.path_pushdown = false;
        storage.paths_only = true;
        storage.files = vec![
            file(1, "src/engine/tests/a.rs", "rust", 10),
            file(2, "src/engine/internal/tests/b.rs", "rust", 10),
            file(3, "src/other/tests/a.md", "markdown", 10),
        ];
        let mut plan = plan(Vec::new());
        plan.globs = vec!["src/*/tests/**".into()];
        plan.file_types = vec!["rust".into()];
        let filter = super::search_plan_to_storage_filter(Path::new("/workspace"), &plan, &storage)
            .expect("path filter")
            .expect("filter present");
        assert_eq!(filter.file_ids, Some(vec![storage.files[0].id]));
    }

    #[test]
    fn common_globs_do_not_enumerate_file_ids() {
        let storage = pushdown_storage();
        let directory =
            StoragePathFilter::Directory(crate::domain::SourcePath::new("src").expect("directory"));
        let cases = [
            (vec!["src/**"], directory.clone()),
            (
                vec!["*.rs"],
                StoragePathFilter::FileNameSuffix(".rs".into()),
            ),
            (
                vec!["Cargo.toml"],
                StoragePathFilter::FileNameExact("Cargo.toml".into()),
            ),
            (
                vec!["test*"],
                StoragePathFilter::FileNamePrefix("test".into()),
            ),
            (
                vec!["src/**/*.rs"],
                StoragePathFilter::And(vec![
                    directory.clone(),
                    StoragePathFilter::FileNameSuffix(".rs".into()),
                ]),
            ),
            (
                vec!["src/**", "docs/**"],
                StoragePathFilter::Or(vec![
                    directory.clone(),
                    StoragePathFilter::Directory(
                        crate::domain::SourcePath::new("docs").expect("directory"),
                    ),
                ]),
            ),
            (
                vec!["src/**", "!src/generated/**"],
                StoragePathFilter::And(vec![
                    directory,
                    StoragePathFilter::Not(Box::new(StoragePathFilter::Directory(
                        crate::domain::SourcePath::new("src/generated").expect("directory"),
                    ))),
                ]),
            ),
            (
                vec!["missing/**"],
                StoragePathFilter::Directory(
                    crate::domain::SourcePath::new("missing").expect("directory"),
                ),
            ),
        ];
        for (patterns, expected) in cases {
            let mut plan = plan(Vec::new());
            plan.globs = patterns
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect();
            let result =
                super::search_plan_to_storage_filter(Path::new("/workspace"), &plan, &storage)
                    .expect("planned filter")
                    .expect("filter");
            assert_eq!(result.path, Some(expected), "{patterns:?}");
            assert!(result.file_ids.is_none(), "{patterns:?}");
        }
    }

    #[test]
    fn pushed_globs_match_ripgrep_overrides_for_files_and_parent_directories() {
        let storage = pushdown_storage();
        let paths = [
            "lib.rs",
            "Cargo.toml",
            "test.txt",
            "src/main.rs",
            "src/main.ts",
            "src/deep/test.rs",
            "src/generated/a.rs",
            "src/generated/deep/a.rs",
            "docs/a.rs",
            "docs/readme.md",
            "nested/src/main.rs",
            "folder.rs/readme.md",
        ];
        let cases = [
            vec!["*"],
            vec!["**"],
            vec!["**/*"],
            vec!["*.rs"],
            vec!["**/*.rs"],
            vec!["Cargo.toml"],
            vec!["test*"],
            vec!["src/**"],
            vec!["/src/**"],
            vec!["src/**/*.rs"],
            vec!["*.rs", "*.ts"],
            vec!["src/**", "docs/**"],
            vec!["!src/generated/**"],
            vec!["*.rs", "!src/generated/**"],
            vec!["src/**", "!src/generated/**", "!docs/**"],
        ];
        for patterns in cases {
            let patterns = patterns.into_iter().map(str::to_owned).collect::<Vec<_>>();
            let globs = super::GlobFilter::new(Path::new("/workspace"), &patterns, &[])
                .expect("valid fixture");
            let predicate = globs
                .pushdown(&storage)
                .expect("valid fixture")
                .expect("pushdown");
            for path in paths {
                assert_eq!(
                    predicate_matches(&predicate, Path::new(path)),
                    globs.is_match(Path::new(path)),
                    "patterns={patterns:?}, path={path}",
                );
            }
        }
    }

    #[test]
    fn optimized_globs_match_ripgrep_walk_across_rule_combinations() {
        use std::collections::BTreeSet;

        let root = tempfile::tempdir().expect("workspace fixture");
        let storage = pushdown_storage();
        let paths = [
            "lib.rs",
            "Cargo.toml",
            ".hidden.rs",
            "test.txt",
            "src/main.rs",
            "src/main.ts",
            "src/deep/test.rs",
            "src/generated/a.rs",
            "src/generated/deep/a.rs",
            "docs/a.rs",
            "docs/readme.md",
            "nested/src/main.rs",
            "folder.rs/readme.md",
            "docs/Cargo.toml",
            "src/folder.rs/README",
            "src/中文.rs",
        ];
        for path in paths {
            let absolute = root.path().join(path);
            std::fs::create_dir_all(absolute.parent().expect("parent")).expect("directory");
            std::fs::write(absolute, "fixture").expect("file");
        }
        let positives = [
            "*.rs",
            "*.ts",
            "Cargo.toml",
            "test*",
            "src/**",
            "docs/**",
            "src/**/*.rs",
        ];
        for first in positives {
            for second in positives {
                for exclusion in [None, Some("!src/generated/**"), Some("!docs/**")] {
                    let mut patterns = vec![first.to_owned(), second.to_owned()];
                    patterns.extend(exclusion.map(str::to_owned));
                    let globs = super::GlobFilter::new(root.path(), &patterns, &[]).expect("globs");
                    let predicate = globs
                        .pushdown(&storage)
                        .expect("planner")
                        .expect("pushdown");
                    let mut overrides = ignore::overrides::OverrideBuilder::new(root.path());
                    for pattern in &patterns {
                        overrides.add(pattern).expect("override");
                    }
                    let mut walker = ignore::WalkBuilder::new(root.path());
                    walker
                        .standard_filters(false)
                        .overrides(overrides.build().expect("overrides"));
                    let expected = walker
                        .build()
                        .map(|entry| entry.expect("walk entry"))
                        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
                        .map(|entry| {
                            entry
                                .path()
                                .strip_prefix(root.path())
                                .expect("relative path")
                                .to_path_buf()
                        })
                        .collect::<BTreeSet<_>>();
                    let selected = paths
                        .iter()
                        .filter(|path| predicate_matches(&predicate, Path::new(path)))
                        .map(PathBuf::from)
                        .collect::<BTreeSet<_>>();
                    assert_eq!(selected, expected, "{patterns:?}");
                    for path in paths {
                        assert_eq!(
                            globs.is_match(Path::new(path)),
                            expected.contains(Path::new(path)),
                            "{patterns:?}, {path}"
                        );
                    }
                }
            }
        }
    }

    fn predicate_matches(predicate: &StoragePathFilter, path: &Path) -> bool {
        let name = path
            .file_name()
            .expect("valid fixture")
            .to_str()
            .expect("valid fixture");
        match predicate {
            StoragePathFilter::All => true,
            StoragePathFilter::None => false,
            StoragePathFilter::Directory(directory) => path
                .parent()
                .is_some_and(|parent| parent.starts_with(directory.as_path())),
            StoragePathFilter::FileNameExact(value) => name == value,
            StoragePathFilter::FileNamePrefix(value) => name.starts_with(value),
            StoragePathFilter::FileNameSuffix(value) => name.ends_with(value),
            StoragePathFilter::And(predicates) => predicates
                .iter()
                .all(|predicate| predicate_matches(predicate, path)),
            StoragePathFilter::Or(predicates) => predicates
                .iter()
                .any(|predicate| predicate_matches(predicate, path)),
            StoragePathFilter::Not(predicate) => !predicate_matches(predicate, path),
        }
    }

    #[test]
    fn complex_globs_fall_back_with_ordered_reinclusion_and_directory_pruning() {
        let files = [
            file(1, "src/main.rs", "rust", 1),
            file(2, "src/generated/keep.rs", "rust", 1),
            file(3, "src/generated/nested/keep.rs", "rust", 1),
            file(4, "folder.rs/readme.md", "markdown", 1),
            file(5, "notes/keep.md", "markdown", 1),
        ];
        let cases = [
            (
                vec!["src/**/*.rs", "!src/generated/**", "src/generated/keep.rs"],
                vec![1, 2],
            ),
            (vec!["!src/generated", "src/generated/keep.rs"], vec![]),
            (
                vec!["!src/generated/**", "src/generated/nested/keep.rs"],
                vec![],
            ),
            (vec!["!*.rs"], vec![5]),
            (vec!["src/**/k?ep.[r]s"], vec![2, 3]),
        ];
        for (patterns, expected) in cases {
            let mut plan = plan(Vec::new());
            plan.globs = patterns
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect();
            let globs = super::GlobFilter::new(Path::new("/workspace"), &plan.globs, &[])
                .expect("valid fixture");
            assert!(
                globs
                    .pushdown(&pushdown_storage())
                    .expect("valid fixture")
                    .is_none(),
                "{patterns:?}"
            );
            let actual = super::resolve_filtered_file_ids(Path::new("/workspace"), &plan, &files)
                .expect("valid fixture");
            assert_eq!(
                actual.iter().map(|id| id.get()).collect::<Vec<_>>(),
                expected,
                "{patterns:?}"
            );
        }
    }

    #[test]
    fn fallback_preserves_glob_escaping_case_and_invalid_pattern_errors() {
        let patterns = [r"\!literal.rs".to_owned()];
        let globs =
            super::GlobFilter::new(Path::new("/workspace"), &patterns, &[]).expect("valid fixture");
        assert!(globs.is_match(Path::new("nested/!literal.rs")));
        assert!(!globs.is_match(Path::new("literal.rs")));

        let patterns = ["*.RS".to_owned()];
        let sensitive =
            super::GlobFilter::new(Path::new("/workspace"), &patterns, &[]).expect("valid fixture");
        assert!(!sensitive.is_match(Path::new("src/main.rs")));
        let insensitive =
            super::GlobFilter::new(Path::new("/workspace"), &[], &patterns).expect("valid fixture");
        assert!(insensitive.is_match(Path::new("src/main.rs")));
        assert!(
            insensitive
                .pushdown(&pushdown_storage())
                .expect("valid fixture")
                .is_none()
        );
        assert!(super::GlobFilter::new(Path::new("/workspace"), &["[".to_owned()], &[]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn fallback_matches_non_unicode_paths_without_lossy_conversion() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let mut source = file(1, "placeholder.rs", "rust", 1);
        source.relative_path =
            crate::domain::SourcePath::new(OsString::from_vec(b"src/\xff.rs".to_vec()))
                .expect("source path");
        let mut storage = pushdown_storage();
        storage.files = vec![source.clone()];
        let name_patterns = ["*.rs".to_owned()];
        let names =
            super::GlobFilter::new(Path::new("/workspace"), &name_patterns, &[]).expect("glob");
        assert!(names.pushdown(&storage).expect("planner").is_none());
        let directory_patterns = ["src/**".to_owned()];
        let directory = super::GlobFilter::new(Path::new("/workspace"), &directory_patterns, &[])
            .expect("glob");
        assert_eq!(
            directory.pushdown(&storage).expect("planner"),
            Some(StoragePathFilter::Directory(
                crate::domain::SourcePath::new("src").expect("directory")
            ))
        );
        let mut plan = plan(Vec::new());
        plan.globs = vec!["*.rs".into()];
        assert_eq!(
            super::resolve_filtered_file_ids(Path::new("/workspace"), &plan, &[source.clone()])
                .expect("valid fixture"),
            vec![source.id]
        );
        plan.globs = vec!["*\u{fffd}.rs".into()];
        assert!(
            super::resolve_filtered_file_ids(Path::new("/workspace"), &plan, &[source])
                .expect("valid fixture")
                .is_empty()
        );
    }

    fn plan(routes: Vec<SearchRoute>) -> SearchPlan {
        SearchPlan {
            routes,
            limit: Some(10),
            trace: true,
            prefer_symbol: false,
            symbol_types: Vec::new(),
            include_paths: Vec::new(),
            exclude_paths: Vec::new(),
            globs: Vec::new(),
            insensitive_globs: Vec::new(),
            file_types: Vec::new(),
            excluded_file_types: Vec::new(),
            modified_after_epoch_ms: None,
            modified_before_epoch_ms: None,
        }
    }

    fn file(id: u32, relative: &str, format: &str, modified: u64) -> FileRecord {
        FileRecord {
            id: FileId::new(id),
            relative_path: crate::domain::SourcePath::new(relative).expect("source path"),
            formats: vec![match format {
                "rust" => FileFormat::Rust,
                "markdown" => FileFormat::Markdown,
                _ => panic!("unexpected fixture format"),
            }],
            snapshot: FileSnapshot {
                size_bytes: 100,
                modified_epoch_ms: Some(modified),
                content_hash: Some(crate::utils::sha256_hex(&id.to_le_bytes())),
            },
            index_status: FileIndexStatus::Indexed {
                indexed_epoch_ms: modified,
                entity_count: 1,
            },
        }
    }

    fn entity(id: &str, file: &FileRecord, content: &str) -> StoredEntity {
        StoredEntity {
            entity: Entity {
                id: EntityId::new(id).expect("entity id"),
                file_id: file.id,
                range: text_range(1, 8),
                content: EntityContent::Source(vec![Content::Text(content.to_owned())]),
                metadata: None,
            },
            file: file.clone(),
        }
    }

    fn hit(
        stored: &StoredEntity,
        fragment_id: &str,
        path: StorageSearchPath,
        score: f64,
    ) -> FixtureHit {
        FixtureHit {
            hit: StorageSearchHit {
                document_id: fragment_id.to_owned(),
                entity_id: stored.entity.id.clone(),
                file_id: stored.file.id,
                path,
                score,
            },
            fragment: EntityFragment::Window(WindowFragment {
                id: FragmentId::new(fragment_id).expect("fragment id"),
                entity_id: stored.entity.id.clone(),
                file_id: stored.file.id,
                range: text_range(2, 3),
                contents: vec![Content::Text(format!(
                    "{} source",
                    stored.entity.id.as_str()
                ))],
            }),
        }
    }

    fn text_range(start_line: usize, end_line: usize) -> SourceRange {
        SourceRange::Text(
            TextRange::from_coordinates(
                (start_line - 1) * 10,
                end_line * 10 - 1,
                start_line,
                end_line,
                0,
                9,
            )
            .expect("valid range"),
        )
    }
}
