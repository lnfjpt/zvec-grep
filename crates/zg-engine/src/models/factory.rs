use std::sync::Arc;

use super::{
    catalog::{EmbeddingCatalogEntry, get_embedding_model_catalog_entry},
    compute::ModelComputeRuntime,
    llama_cpp::LlamaCppEmbeddingModel,
    model2vec::Model2VecEmbeddingModel,
    qwen::QwenEmbeddingModel,
    spi::{EmbeddingModel, ModelError},
    transformers::TransformersEmbeddingModel,
};
use crate::domain::model::ModelConfig;

/// Creates a catalog-backed embedding model.
///
/// `None` for `options` is the Rust equivalent of omitting the optional
/// TypeScript options object.
///
/// # Errors
///
/// Returns an error for unknown references or invalid backend options.
pub fn create_embedding_model(
    reference: &str,
    options: Option<ModelConfig>,
    compute_runtime: ModelComputeRuntime,
) -> Result<Arc<dyn EmbeddingModel>, ModelError> {
    let entry = get_embedding_model_catalog_entry(reference).ok_or_else(|| {
        ModelError::new(
            crate::EngineError::NOT_FOUND,
            "Embedding model is not in the zvec-grep catalog",
            Some(format!("embedding={reference}")),
        )
    })?;
    let options = options.unwrap_or_default();
    match entry {
        EmbeddingCatalogEntry::Model2Vec(config) => Ok(Arc::new(Model2VecEmbeddingModel::new(
            config,
            options,
            compute_runtime,
        ))),
        EmbeddingCatalogEntry::Qwen(config) => {
            Ok(Arc::new(QwenEmbeddingModel::new(config, options)?))
        }
        EmbeddingCatalogEntry::TransformersJs(config) => Ok(Arc::new(
            TransformersEmbeddingModel::new(config, options, compute_runtime),
        )),
        EmbeddingCatalogEntry::LlamaCpp(config) => Ok(Arc::new(LlamaCppEmbeddingModel::new(
            config,
            options,
            compute_runtime,
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::create_embedding_model;

    #[test]
    fn factory_exposes_implemented_backends() {
        let model = create_embedding_model(
            "local/potion-code-16m-v2",
            None,
            crate::models::compute::ModelComputeRuntime::shared(),
        )
        .expect("Model2Vec backend should be implemented");
        assert_eq!(model.info().model.reference(), "local/potion-code-16m-v2");

        let qwen = create_embedding_model(
            "qwen/text-embedding-v4",
            Some(super::ModelConfig {
                api_key: Some("test".to_owned()),
                ..super::ModelConfig::default()
            }),
            crate::models::compute::ModelComputeRuntime::shared(),
        )
        .expect("Qwen backend should be implemented");
        assert_eq!(qwen.info().model.reference(), "qwen/text-embedding-v4");

        let llama = create_embedding_model(
            "local/embeddinggemma-300m",
            None,
            crate::models::compute::ModelComputeRuntime::shared(),
        )
        .expect("llama.cpp backend should be implemented");
        assert_eq!(llama.info().model.reference(), "local/embeddinggemma-300m");

        let transformers = create_embedding_model(
            "local/all-minilm-l6-v2",
            None,
            crate::models::compute::ModelComputeRuntime::shared(),
        )
        .expect("Transformers backend should be implemented");
        assert_eq!(
            transformers.info().model.reference(),
            "local/all-minilm-l6-v2"
        );

        let unknown = create_embedding_model(
            "missing",
            None,
            crate::models::compute::ModelComputeRuntime::shared(),
        )
        .err()
        .expect("unknown model should fail catalog lookup");
        assert_eq!(unknown.code(), crate::EngineError::NOT_FOUND);
    }
}
