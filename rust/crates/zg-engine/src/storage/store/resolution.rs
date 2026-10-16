//! Restricted resolver access to a coordinated, locked storage snapshot.
use super::*;
use crate::storage::graph::{
    NameIndexEntry, PendingRefPage, Resolution, ResolutionStats, StoredPendingRef,
};

/// The resolver can inspect references/candidates and submit decisions, but
/// cannot replace file snapshots, access database handles, or close storage.
pub(crate) trait ResolutionSession {
    fn list_pending_refs(&mut self, limit: usize, cursor: i64) -> EngineResult<PendingRefPage>;
    /// None defers the name because some indexed candidates are not yet visible.
    fn lookup_candidates(&self, name: &str) -> EngineResult<Option<Vec<NameIndexEntry>>>;
    fn file_path(&self, file_id: u32) -> EngineResult<Option<String>>;
    fn file_id_by_path(&self, path: &str) -> EngineResult<Option<u32>>;
    /// Existence evidence only, including unfinished modules: temporarily
    /// unavailable local modules must not be classified as external failures.
    fn path_has_module(&self, name: &str, extensions: &[&str]) -> EngineResult<bool>;
    fn imported_file_ids(&self, file_id: u32) -> EngineResult<Vec<u32>>;
    fn apply_resolutions(&mut self, resolutions: &[Resolution]) -> EngineResult<ResolutionStats>;
    fn apply_failures(&mut self, refs: &[i64]) -> EngineResult<usize>;
    fn record_candidates(&mut self, candidates: &[(i64, Vec<String>)]) -> EngineResult<usize>;
}

pub(super) struct StoreResolutionSession<'a> {
    state: &'a mut StoreState,
    issued: HashMap<i64, StoredPendingRef>,
}

impl<'a> StoreResolutionSession<'a> {
    pub(super) fn new(state: &'a mut StoreState) -> Self {
        Self {
            state,
            issued: HashMap::new(),
        }
    }

    fn graph(&self) -> &SqliteGraphStorage {
        self.state.graph.as_ref().expect("writable graph")
    }

    fn indexed_files(&self, ids: &[FileId]) -> EngineResult<HashMap<FileId, FileRecord>> {
        Ok(self
            .state
            .files
            .fetch(ids)?
            .into_iter()
            .filter(|(_, file)| file.index_status.is_indexed())
            .collect())
    }

    fn endpoints(&self, ids: &[String]) -> EngineResult<HashMap<String, FileId>> {
        let entity_ids = ids
            .iter()
            .cloned()
            .map(EntityId::from_string)
            .collect::<Vec<_>>();
        let mut visible: HashMap<_, _> = get_entities(self.state, &entity_ids)?
            .into_iter()
            .map(|(id, entity)| (id.as_str().to_owned(), entity.file.id))
            .collect();
        let files: Vec<_> = ids
            .iter()
            .filter_map(|id| id.strip_prefix('f')?.parse::<u32>().ok().map(FileId::new))
            .collect();
        for file in self.indexed_files(&files)?.into_values() {
            visible.insert(format!("f{}", file.id.get()), file.id);
        }
        Ok(visible)
    }

    fn eligible_refs(&self, ids: impl IntoIterator<Item = i64>) -> EngineResult<HashSet<i64>> {
        let refs: Vec<_> = ids
            .into_iter()
            .filter_map(|id| self.issued.get(&id))
            .collect();
        let sources: Vec<_> = refs
            .iter()
            .map(|r| r.reference.from_node_id.clone())
            .collect();
        let visible = self.endpoints(&sources)?;
        Ok(refs
            .into_iter()
            .filter(|r| {
                visible
                    .get(&r.reference.from_node_id)
                    .is_some_and(|file| file.get() == r.file_id)
            })
            .map(|r| r.id)
            .collect())
    }
}

impl ResolutionSession for StoreResolutionSession<'_> {
    fn list_pending_refs(&mut self, limit: usize, cursor: i64) -> EngineResult<PendingRefPage> {
        let mut page = self
            .graph()
            .list_pending_refs(limit, cursor)
            .map_err(graph_error)?;
        let ids: Vec<_> = page
            .refs
            .iter()
            .map(|r| r.reference.from_node_id.clone())
            .collect();
        let visible = self.endpoints(&ids)?;
        page.refs.retain(|r| {
            visible
                .get(&r.reference.from_node_id)
                .is_some_and(|file| file.get() == r.file_id)
        });
        self.issued
            .extend(page.refs.iter().map(|r| (r.id, r.clone())));
        // Preserve the underlying cursor even when the entire page is hidden.
        Ok(page)
    }

    fn lookup_candidates(&self, name: &str) -> EngineResult<Option<Vec<NameIndexEntry>>> {
        let candidates = self.graph().lookup_by_name(name).map_err(graph_error)?;
        let ids: Vec<_> = candidates.iter().map(|c| c.entity_id.clone()).collect();
        let visible = self.endpoints(&ids)?;
        if candidates.iter().any(|c| {
            !visible
                .get(&c.entity_id)
                .is_some_and(|file| file.get() == c.file_id)
        }) {
            return Ok(None);
        }
        Ok(Some(candidates))
    }

    fn file_path(&self, file_id: u32) -> EngineResult<Option<String>> {
        Ok(self
            .indexed_files(&[FileId::new(file_id)])?
            .remove(&FileId::new(file_id))
            .map(|file| file.relative_path.to_string_lossy().replace('\\', "/")))
    }

    fn file_id_by_path(&self, path: &str) -> EngineResult<Option<u32>> {
        let Some(id) = self.graph().file_id_by_path(path).map_err(graph_error)? else {
            return Ok(None);
        };
        Ok((self.file_path(id)?.as_deref() == Some(path)).then_some(id))
    }

    fn path_has_module(&self, name: &str, extensions: &[&str]) -> EngineResult<bool> {
        self.graph()
            .path_has_module(name, extensions)
            .map_err(graph_error)
    }

    fn imported_file_ids(&self, file_id: u32) -> EngineResult<Vec<u32>> {
        if self.file_path(file_id)?.is_none() {
            return Ok(Vec::new());
        }
        let ids = self
            .graph()
            .imported_file_ids(file_id)
            .map_err(graph_error)?;
        let files: Vec<_> = ids.iter().copied().map(FileId::new).collect();
        let visible = self.indexed_files(&files)?;
        Ok(ids
            .into_iter()
            .filter(|id| visible.contains_key(&FileId::new(*id)))
            .collect())
    }

    fn apply_resolutions(&mut self, resolutions: &[Resolution]) -> EngineResult<ResolutionStats> {
        let refs = self.eligible_refs(resolutions.iter().map(|r| r.ref_id))?;
        let ids: Vec<_> = resolutions.iter().map(|r| r.target_id.clone()).collect();
        let targets = self.endpoints(&ids)?;
        let valid: Vec<_> = resolutions
            .iter()
            .filter(|r| refs.contains(&r.ref_id) && targets.contains_key(&r.target_id))
            .cloned()
            .collect();
        let mut result = self
            .state
            .graph
            .as_mut()
            .expect("writable graph")
            .apply_resolutions(&valid)
            .map_err(graph_error)?;
        result.stale += resolutions.len() - valid.len();
        Ok(result)
    }

    fn apply_failures(&mut self, ids: &[i64]) -> EngineResult<usize> {
        let refs = self.eligible_refs(ids.iter().copied())?;
        let valid: Vec<_> = ids.iter().copied().filter(|id| refs.contains(id)).collect();
        self.state
            .graph
            .as_mut()
            .expect("writable graph")
            .apply_failures(&valid)
            .map_err(graph_error)
    }

    fn record_candidates(&mut self, entries: &[(i64, Vec<String>)]) -> EngineResult<usize> {
        let refs = self.eligible_refs(entries.iter().map(|(id, _)| *id))?;
        let ids: Vec<_> = entries
            .iter()
            .flat_map(|(_, ids)| ids.iter().cloned())
            .collect();
        let visible = self.endpoints(&ids)?;
        let valid: Vec<_> = entries
            .iter()
            .filter(|(id, ids)| {
                refs.contains(id)
                    && !ids.is_empty()
                    && ids.iter().all(|id| visible.contains_key(id))
            })
            .cloned()
            .collect();
        self.state
            .graph
            .as_mut()
            .expect("writable graph")
            .record_candidates(&valid)
            .map_err(graph_error)
    }
}
