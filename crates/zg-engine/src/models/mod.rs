//! Private embedding model implementations matching the TypeScript engine.

// Model definitions and selection.
mod catalog;
mod error;
mod factory;
mod resolution;
mod spi;

// Runtime and artifact management.
mod artifacts;
mod compute;
mod download_progress;
mod runtime;

// Embedding backends.
mod llama_cpp;
mod model2vec;
mod qwen;
mod transformers;

// Model catalog and reference resolution.
pub(crate) use catalog::{EmbeddingCatalogEntry, get_embedding_model_catalog_entry};
pub(crate) use resolution::{ResolveEmbeddingReferenceOptions, resolve_embedding_reference};

// Runtime lifecycle.
pub(crate) use runtime::ModelRuntimeManager;
pub(crate) type ModelRuntimeLease = runtime::ModelRuntimeLease;
pub(crate) type ModelRuntimeRequest = runtime::ModelRuntimeRequest;
pub(crate) type ModelRuntimeSnapshot = runtime::ModelRuntimeSnapshot;

// Model configuration and capabilities. Backend traits, factories, and
// validation helpers remain private to `models`.
pub use spi::Device;
pub(crate) use spi::{EmbeddingModelProgress, ModelProgressReporter};
pub(crate) type CreateEmbeddingModelOptions = spi::CreateEmbeddingModelOptions;
pub(crate) type EmbeddingMetric = spi::EmbeddingMetric;
pub(crate) type EmbeddingModelInfo = spi::EmbeddingModelInfo;
#[cfg(test)]
pub(crate) type EmbeddingModelLimits = spi::EmbeddingModelLimits;

// Embedding requests, results, and errors.
pub(crate) type EmbeddingInput = spi::EmbeddingInput;
pub(crate) type EmbeddingInputKind = spi::EmbeddingInputKind;
pub(crate) type EmbeddingOptions = spi::EmbeddingOptions;
pub(crate) type EmbeddingPurpose = spi::EmbeddingPurpose;
pub(crate) type EmbeddingResult = spi::EmbeddingResult;
pub(crate) type ModelError = spi::ModelError;

impl runtime::ModelRuntimeManager {
    pub(crate) fn new() -> Self {
        Self::new_impl()
    }

    /// Returns a counted lease, reusing an existing runtime with the same key.
    pub(crate) fn acquire(
        &self,
        request: ModelRuntimeRequest,
    ) -> Result<ModelRuntimeLease, ModelError> {
        self.acquire_impl(request)
    }

    /// Stops new acquisitions and retires runtimes without active leases.
    pub(crate) fn close(&self) {
        self.close_impl();
    }

    pub(crate) fn snapshot(&self) -> ModelRuntimeSnapshot {
        self.snapshot_impl()
    }
}

impl runtime::ModelRuntimeRequest {
    pub(crate) fn new(
        reference: impl Into<String>,
        options: CreateEmbeddingModelOptions,
        embedding_concurrency: Option<usize>,
    ) -> Self {
        Self::new_impl(reference, options, embedding_concurrency)
    }
}

impl runtime::ModelRuntimeLease {
    pub(crate) fn info(&self) -> &EmbeddingModelInfo {
        self.info_impl()
    }

    pub(crate) async fn embed(
        &self,
        inputs: &[EmbeddingInput],
        options: EmbeddingOptions,
        progress: Option<ModelProgressReporter>,
    ) -> Result<EmbeddingResult, ModelError> {
        self.embed_impl(inputs, options, progress).await
    }
}

#[cfg(test)]
mod tests;
