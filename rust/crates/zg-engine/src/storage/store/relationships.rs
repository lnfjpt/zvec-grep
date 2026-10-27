//! Read-only relationship access under the store lock.
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use super::{IndexStore, StoreState, graph_error};
use crate::storage::{
    fragments::build_filter,
    graph::{Direction, SqliteGraphStorage},
    types::{StoragePathFilter, StorageSearchFilter, StoredEntity},
};
use crate::{
    EngineError, EngineResult,
    api::relationships::MAX_SYMBOL_MATCHES,
    domain::{
        Entity, EntityId, FileEdge, FileId, FileIndexStatus, FileRecord, GraphEdgeKind, SourcePath,
    },
};

const MAX_SYMBOL_SUGGESTIONS: usize = 5;
const ENDPOINT_BATCH_SIZE: usize = 256;

impl IndexStore {
    /// Keep symbol lookup, edge selection and endpoint hydration on one snapshot.
    /// In-process writers use this same mutex for file replacement and deletion.
    pub(crate) fn with_relationship_read<T>(
        &self,
        operation: impl FnOnce(&RelationshipReadSession<'_>) -> EngineResult<T>,
    ) -> EngineResult<T> {
        self.read(|state| {
            let graph = state.graph.as_ref().ok_or_else(|| {
                EngineError::not_found(
                    "workspace graph is missing; this index has no persisted relationship data",
                )
            })?;
            operation(&RelationshipReadSession { state, graph })
        })
    }

    #[cfg(test)]
    pub(crate) fn find_symbols(
        &self,
        name: &str,
        scope: Option<&str>,
    ) -> EngineResult<Vec<StoredEntity>> {
        self.with_relationship_read(|session| session.find_symbols(name, scope, None))
    }
}

/// Read-only access valid while the store mutex is held for the entire query.
pub(crate) struct RelationshipReadSession<'a> {
    state: &'a StoreState,
    graph: &'a SqliteGraphStorage,
}

impl RelationshipReadSession<'_> {
    /// Bounded best-effort suggestions, never relationship query roots.
    /// Do not require the requested scope: a misspelled scope is a useful retry case.
    pub(crate) fn suggest_symbols(
        &self,
        name: &str,
        path: Option<&Path>,
    ) -> EngineResult<Vec<StoredEntity>> {
        let query = name.to_lowercase();
        let path_filter = self.native_path_filter(path)?;
        let hits = self
            .state
            .fragments
            .search_fts(&query, 100, path_filter.as_deref())?;
        let mut seen = HashSet::new();
        let ids: Vec<_> = hits
            .into_iter()
            .map(|hit| hit.entity_id)
            .filter(|id| seen.insert(id.clone()))
            .collect();
        let mut metadata = self.state.entities.fetch_code_metadata(&ids)?;
        let mut candidates: Vec<_> = ids
            .into_iter()
            .filter_map(|id| {
                let (file_id, metadata) = metadata.remove(&id)?;
                let rank = suggestion_rank(&query, metadata.symbol_name.as_deref()?)?;
                Some((id, file_id, rank))
            })
            .collect();
        // Stable sort preserves FTS relevance within each name-match category.
        // Rank before limiting, and hydrate only the selected visible definitions.
        candidates.sort_by_key(|(_, _, rank)| *rank);
        let file_ids: Vec<_> = candidates
            .iter()
            .map(|(_, file_id, _)| *file_id)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let files = self.state.files.fetch(&file_ids)?;
        let selected: Vec<_> = candidates
            .into_iter()
            .filter_map(|(id, file_id, _)| {
                let file = files.get(&file_id)?;
                visible_in_path(file, path).then(|| (id, file.clone()))
            })
            .take(MAX_SYMBOL_SUGGESTIONS)
            .collect();
        self.load_symbols(selected)
    }

    /// Resolve bounded roots and reuse their checked file records while hydrating.
    pub(crate) fn find_symbols(
        &self,
        name: &str,
        scope: Option<&str>,
        path: Option<&Path>,
    ) -> EngineResult<Vec<StoredEntity>> {
        let mut selected =
            self.select_symbols(name, ScopeMatch::Exact(scope), path, MAX_SYMBOL_MATCHES + 1)?;
        if let Some(scope) = scope.filter(|_| selected.is_empty()) {
            // Rust's module layout (`mod foo;` beside foo/mod.rs) lives in file
            // paths, so those top-level entities record no scope and a
            // scope-restricted lookup can never see them. Retry inside that
            // module's files only, so a wrong scope still finds nothing.
            let in_module = self.select_symbols(
                name,
                ScopeMatch::ModulePath(scope),
                path,
                MAX_SYMBOL_MATCHES + 1,
            )?;
            if in_module.len() <= MAX_SYMBOL_MATCHES {
                selected = in_module;
            }
        }
        if selected.len() > MAX_SYMBOL_MATCHES {
            return Err(EngineError::invalid_argument(format!(
                "symbol query matches more than {MAX_SYMBOL_MATCHES} indexed definitions; use a narrower scope::name or path filter",
            )));
        }
        self.load_symbols(selected)
    }

    /// Exact bare-name candidates for a wrong scope, bounded without an overflow error.
    pub(crate) fn suggest_exact_symbols(
        &self,
        name: &str,
        path: Option<&Path>,
    ) -> EngineResult<Vec<StoredEntity>> {
        let selected =
            self.select_symbols(name, ScopeMatch::Exact(None), path, MAX_SYMBOL_SUGGESTIONS)?;
        self.load_symbols(selected)
    }

    /// Narrow native retrieval before loading any symbol metadata or applying caps.
    /// Check the exact file first: directory identities can outlive their files.
    fn native_path_filter(&self, path: Option<&Path>) -> EngineResult<Option<String>> {
        let Some(path) = path else { return Ok(None) };
        let path = SourcePath::new(path)?;
        let filter = if let Some(file_id) = self.state.files.find_id(&path)? {
            StorageSearchFilter {
                file_ids: Some(vec![file_id]),
                ..StorageSearchFilter::default()
            }
        } else {
            StorageSearchFilter {
                path: Some(StoragePathFilter::Directory(path)),
                ..StorageSearchFilter::default()
            }
        };
        build_filter(Some(&filter), &|path| self.state.directories.get(path))
    }

    fn select_symbols(
        &self,
        name: &str,
        scope: ScopeMatch<'_>,
        path: Option<&Path>,
        limit: usize,
    ) -> EngineResult<Vec<(EntityId, FileRecord)>> {
        if name.is_empty() {
            return Err(EngineError::invalid_argument(
                "symbol name must not be empty",
            ));
        }
        let state = self.state;
        let path_filter = self.native_path_filter(path)?;
        let ids = state
            .fragments
            .find_symbol_entity_ids(name, path_filter.as_deref())?;
        let mut defined = Vec::new();
        let mut declarations = Vec::new();
        'batches: for batch in ids.chunks(128) {
            let mut owners: HashMap<_, _> = state
                .entities
                .fetch_code_metadata(batch)?
                .into_iter()
                .filter_map(|(id, (file_id, metadata))| {
                    (metadata.symbol_name.as_deref() == Some(name)
                        && scope.accepts(metadata.scope.as_deref()))
                    .then_some((id, (file_id, metadata.is_declaration)))
                })
                .collect();
            let file_ids: Vec<_> = owners
                .values()
                .map(|(file_id, _)| *file_id)
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let files = state.files.fetch(&file_ids)?;
            for id in batch {
                if let Some((file_id, is_declaration)) = owners.remove(id)
                    && let Some(file) = files.get(&file_id)
                    && visible_in_path(file, path)
                    && scope.accepts_file(file)
                {
                    let entry = (id.clone(), file.clone());
                    if is_declaration {
                        declarations.push(entry);
                    } else {
                        defined.push(entry);
                    }
                    if defined.len() + declarations.len() == limit {
                        break 'batches;
                    }
                }
            }
        }
        // A pure declaration (C/C++ prototype, in-class member declaration) only
        // names the same entity; prefer the definition whenever one exists.
        Ok(if defined.is_empty() {
            declarations
        } else {
            defined
        })
    }

    fn load_symbols(
        &self,
        selected: Vec<(EntityId, FileRecord)>,
    ) -> EngineResult<Vec<StoredEntity>> {
        let ids: Vec<_> = selected.iter().map(|(id, _)| id.clone()).collect();
        let mut entities = self.state.entities.fetch(&ids)?;
        // Selection already read and checked these records under the same store lock.
        Ok(selected
            .into_iter()
            .filter_map(|(id, file)| {
                let entity = entities.remove(&id)?;
                (entity.file_id() == file.id()).then_some(StoredEntity { entity, file })
            })
            .collect())
    }

    /// Fetch canonical entities and their indexed files without scanning either table.
    pub(crate) fn get_entities(
        &self,
        ids: &[EntityId],
    ) -> EngineResult<HashMap<EntityId, StoredEntity>> {
        get_entities(self.state, ids)
    }

    /// Filter resolved edges by endpoint visibility before applying the limit.
    pub(crate) fn neighborhood(
        &self,
        id: &str,
        direction: Direction,
        kinds: Option<&[GraphEdgeKind]>,
        limit: usize,
    ) -> EngineResult<Vec<(FileEdge, PathBuf)>> {
        let mut rows = self
            .graph
            .neighborhood(id, direction, kinds)
            .map_err(graph_error)?
            .into_iter();
        let mut edges = Vec::new();
        let mut hydration_size = limit.clamp(1, ENDPOINT_BATCH_SIZE);
        let mut checked = HashSet::new();
        let mut visible = HashMap::new();
        while edges.len() < limit {
            let batch: Vec<_> = rows.by_ref().take(hydration_size).collect();
            if batch.is_empty() {
                break;
            }
            // SQLite can be ahead of zvec after a failed write. Filter both
            // endpoints before counting a row toward the requested limit.
            let ids: Vec<_> = batch
                .iter()
                .flat_map(|edge| [&edge.source, &edge.target])
                .map(|id| EntityId::from_string(id.clone()))
                .filter(|id| checked.insert(id.clone()))
                .collect();
            visible.extend(endpoint_paths(self.state, &ids)?);
            let before = edges.len();
            let remaining = limit - before;
            edges.extend(
                batch
                    .into_iter()
                    .filter_map(|edge| {
                        let path = visible.get(&EntityId::from_string(edge.source.clone()))?;
                        visible.get(&EntityId::from_string(edge.target.clone()))?;
                        Some((edge, path.clone()))
                    })
                    .take(remaining),
            );
            // Avoid reading unused endpoints, but grow batches across invisible stretches.
            hydration_size = if edges.len() == before {
                hydration_size.saturating_mul(2).min(ENDPOINT_BATCH_SIZE)
            } else {
                (limit - edges.len()).clamp(1, ENDPOINT_BATCH_SIZE)
            };
        }
        Ok(edges)
    }
}

fn visible_in_path(file: &FileRecord, path: Option<&Path>) -> bool {
    file.index_status().is_indexed()
        && path.is_none_or(|path| file.relative_path().as_path().starts_with(path))
}

/// How a name query constrains the scopes recorded on candidate entities.
#[derive(Clone, Copy)]
enum ScopeMatch<'a> {
    /// Compare recorded scopes; `None` accepts any scope.
    Exact(Option<&'a str>),
    /// The qualified module recorded no scope: Rust keeps module identity in
    /// file paths (`foo/mod.rs`, `foo.rs`), so only that module's files may
    /// satisfy the query.
    ModulePath(&'a str),
}

impl ScopeMatch<'_> {
    fn accepts(self, recorded: Option<&str>) -> bool {
        match self {
            Self::Exact(scope) => {
                scope.is_none_or(|scope| recorded.unwrap_or("").replace('.', "::") == scope)
            }
            Self::ModulePath(_) => recorded.is_none_or(str::is_empty),
        }
    }

    fn accepts_file(self, file: &FileRecord) -> bool {
        match self {
            Self::Exact(_) => true,
            Self::ModulePath(scope) => module_path_matches(file, scope),
        }
    }
}

/// Rust maps module `foo` to `foo.rs` or `foo/mod.rs` and nested modules to
/// stacked directories; accept a file whose trailing layout spells the module.
fn module_path_matches(file: &FileRecord, scope: &str) -> bool {
    let modules: Vec<&str> = scope.split("::").filter(|part| !part.is_empty()).collect();
    if modules.is_empty() {
        return false;
    }
    let path = file.relative_path().as_path();
    let mut layout: Vec<&str> = Vec::new();
    if let Some(parent) = path.parent() {
        layout.extend(
            parent
                .components()
                .filter_map(|component| component.as_os_str().to_str()),
        );
    }
    if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
        && !matches!(stem, "mod" | "lib" | "main")
    {
        layout.push(stem);
    }
    layout.ends_with(&modules)
}

/// Smaller ranks are better; reverse containment needs a meaningful name length.
fn suggestion_rank(query: &str, candidate: &str) -> Option<u8> {
    let candidate = candidate.to_lowercase();
    if candidate.is_empty() {
        None
    } else if candidate == query {
        Some(0)
    } else if candidate.starts_with(query) {
        Some(1)
    } else if candidate.contains(query) {
        Some(2)
    } else if candidate.chars().count() >= 3 && query.contains(&candidate) {
        Some(3)
    } else {
        None
    }
}

fn get_entities(
    state: &StoreState,
    ids: &[EntityId],
) -> EngineResult<HashMap<EntityId, StoredEntity>> {
    let entities = state.entities.fetch(ids)?;
    let file_ids = entities
        .values()
        .map(Entity::file_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let files = state.files.fetch(&file_ids)?;
    Ok(entities
        .into_iter()
        .filter_map(|(id, entity)| {
            let file = files.get(&entity.file_id())?;
            if !matches!(file.index_status(), FileIndexStatus::Indexed { .. }) {
                return None;
            }
            Some((
                id,
                StoredEntity {
                    entity,
                    file: file.clone(),
                },
            ))
        })
        .collect())
}

fn endpoint_paths(
    state: &StoreState,
    ids: &[EntityId],
) -> EngineResult<HashMap<EntityId, PathBuf>> {
    let mut owners = state.entities.fetch_file_ids(ids)?;
    for id in ids {
        if let Some(file_id) = id
            .as_str()
            .strip_prefix('f')
            .and_then(|id| id.parse::<u32>().ok())
        {
            // Only canonical f{file_id} spellings represent implicit file endpoints.
            if id.as_str() == format!("f{file_id}") {
                owners.insert(id.clone(), FileId::new(file_id));
            }
        }
    }
    let file_ids: Vec<_> = owners
        .values()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let files = state.files.fetch(&file_ids)?;
    Ok(owners
        .into_iter()
        .filter_map(|(id, file_id)| {
            files
                .get(&file_id)
                .filter(|file| file.index_status().is_indexed())
                .map(|file| (id, file.relative_path().to_path_buf()))
        })
        .collect())
}
