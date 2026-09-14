//! Indexed workspace search and context-result assembly.

pub(crate) mod context;
mod pipeline;

pub(crate) use pipeline::{RequestEmbeddingRuntime, SearchEmbeddingRuntime};
