//! Open the indexed workspace and keep relationship reads in one read session.

use std::path::Path;

use crate::{
    EngineError, EngineResult,
    api::relationships::{RelationshipOptions, SymbolRelationships},
    pipelines::indexing::service::is_indexed,
    storage::{
        IndexStore,
        graph::Direction,
        read_session::{ReadSessionCache, ReadSessionLease},
    },
    workspace::{
        layout::find_nearest_workspace,
        lock::{LockMode, LockWait},
        manifest::read_workspace_manifest,
    },
};

use super::pipeline::{normalize, query_symbols};

pub(crate) async fn callers(
    options: &RelationshipOptions,
    read_sessions: Option<&ReadSessionCache>,
) -> EngineResult<Vec<SymbolRelationships>> {
    query(options, read_sessions, Direction::In).await
}

pub(crate) async fn callees(
    options: &RelationshipOptions,
    read_sessions: Option<&ReadSessionCache>,
) -> EngineResult<Vec<SymbolRelationships>> {
    query(options, read_sessions, Direction::Out).await
}

async fn query(
    options: &RelationshipOptions,
    read_sessions: Option<&ReadSessionCache>,
    direction: Direction,
) -> EngineResult<Vec<SymbolRelationships>> {
    let (name, scope, limit) = normalize(options)?;
    let wait = LockWait::new(options.signal.as_ref(), options.lock_timeout_ms)?;
    wait.check_cancelled()?;
    let requested = options.root.as_deref().unwrap_or_else(|| Path::new("."));
    let location = find_nearest_workspace(requested)?.ok_or_else(missing_index)?;
    // Keep the canonical entities and graph in the same workspace read scope.
    // Writers retire these same cached handles before replacing index data.
    let _lock = wait
        .acquire(&location.home, LockMode::Read, "relationships")
        .await?;
    let manifest = read_workspace_manifest(&location.home)?.ok_or_else(missing_index)?;
    if !is_indexed(&manifest) || !IndexStore::exists(&manifest.storage_home())? {
        return Err(missing_index());
    }
    let storage = match read_sessions {
        Some(cache) => cache.acquire(&location.home, &manifest.storage_home())?,
        None => ReadSessionLease::open(&manifest.storage_home())?,
    };
    let result = query_symbols(storage.storage(), name, scope, limit, direction);
    let close = storage.close();
    let result = result?;
    close?;
    wait.check_cancelled()?;
    Ok(result)
}

fn missing_index() -> EngineError {
    EngineError::not_found("workspace index is unavailable; run `zg index` first")
}
