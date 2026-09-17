//! Embedding-specific metadata and values; no model execution resources.

use super::ModelInfo;
use crate::{EngineError, EngineResult};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EmbeddingModelInfo {
    pub model: ModelInfo,
    pub dimension: usize,
    pub metric: EmbeddingMetric,
    pub max_batch_size: usize,
    pub max_input_tokens: Option<usize>,
    pub max_image_bytes: Option<usize>,
}

impl EmbeddingModelInfo {
    pub(crate) fn schema(&self) -> EmbeddingSchema {
        EmbeddingSchema {
            provider: self.model.provider.clone(),
            model: self.model.name.clone(),
            dimension: self.dimension,
            metric: self.metric,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EmbeddingMetric {
    Cosine,
    DotProduct,
    Euclidean,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EmbeddingSchema {
    pub provider: String,
    pub model: String,
    pub dimension: usize,
    pub metric: EmbeddingMetric,
}

impl EmbeddingSchema {
    pub(crate) fn validate(&self) -> EngineResult<()> {
        if self.provider.trim().is_empty() || self.model.trim().is_empty() || self.dimension == 0 {
            return Err(EngineError::invalid_argument(
                "workspace embedding schema requires a provider, model, and nonzero dimension",
            ));
        }
        Ok(())
    }

    pub(crate) fn ensure_compatible(&self, other: &Self) -> EngineResult<()> {
        if self != other {
            return Err(EngineError::invalid_argument(
                "existing index uses a different embedding model; rebuild the index",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EmbeddingPurpose {
    #[default]
    Document,
    Query,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EmbeddingResult {
    pub vectors: Vec<Vec<f32>>,
    pub truncated: Vec<usize>,
}
