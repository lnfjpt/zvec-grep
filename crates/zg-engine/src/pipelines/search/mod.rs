//! Indexed workspace search and context-result assembly.

pub(crate) mod context;
mod path_filter;
mod pipeline;
pub(crate) mod types;

pub(crate) use pipeline::{RequestEmbeddingRuntime, SearchEmbeddingRuntime};
