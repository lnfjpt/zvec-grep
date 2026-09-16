//! Durable workspace storage backed by zvec.
//!
//! Workspace-level catalog collections reserve file and directory identities
//! independently of physical index generations. Catalog redo records complete
//! interrupted identity allocations before any new IDs can be returned.
//!
//! Native WAL protects generation documents. Pending source records track writes
//! until a batch is flushed across the retrieval collections. Interrupted file
//! batches are invalidated before readers open and rebuilt by normal indexing.

mod backend;
mod catalog;
mod codec;
mod pending;
pub(crate) mod spi;
mod zvec;

pub(crate) use backend::ZvecStorageFactory;

pub(crate) fn workspace_identities_exist(home: &std::path::Path) -> crate::EngineResult<bool> {
    catalog::Catalog::exists(home)
}

pub(crate) fn delete_workspace_identities(home: &std::path::Path) -> crate::EngineResult<()> {
    catalog::Catalog::delete(home)
}
