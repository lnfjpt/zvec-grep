//! Shared query admission: lease a compatible writer or acquire a workspace read lock.

use std::path::Path;

use crate::{
    EngineResult,
    workspace::{
        lock::{FileLock, HomeReadProbe, LockWait},
        manifest::read_workspace_manifest,
    },
};

use super::{
    indexed_search::writer::{WriterContexts, WriterLease, WriterSession},
    indexing::service::is_indexed,
};

pub(super) enum ReadSource<T> {
    Writer { lease: WriterLease, context: T },
    Read(FileLock),
}

pub(super) async fn acquire_read_source<T>(
    home: &Path,
    wait: &LockWait<'_>,
    operation: &str,
    writers: Option<&WriterContexts>,
    mut prepare: impl FnMut(&WriterSession) -> EngineResult<Option<T>>,
) -> EngineResult<ReadSource<T>> {
    let mut probe = HomeReadProbe::new(home, operation)?;
    loop {
        wait.check_cancelled()?;
        if let Some(writers) = writers
            && let Some(active) = read_workspace_manifest(home)?
            && is_indexed(&active)
            && let Some(lease) = writers.borrow(home, &active.storage_home())
            && let Some(context) = prepare(&lease.session)?
        {
            return Ok(ReadSource::Writer { lease, context });
        }
        if let Some(lock) = probe.try_acquire()? {
            return Ok(ReadSource::Read(lock));
        }
        // A writer can become available after indexing has opened its storage.
        wait.retry(home, operation).await?;
    }
}
