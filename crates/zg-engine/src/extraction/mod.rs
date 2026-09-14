//! Deterministic source extraction used by the indexing and lexical-enrichment paths.

mod chunking;
mod code;
mod image;
mod markdown;
mod service;
mod spi;
mod text;

// Extraction sources.
pub(crate) use spi::{ImageSource, Source, SourceKind, TextSource};

// Chunking options.
pub(crate) use spi::ChunkOptions;

// Indexing output.
pub(crate) use spi::IndexingExtractionFragment;

use crate::{
    EngineError,
    domain::{Content, EntityFragment, FileRecord, TextRange},
};

// Shared implementation helpers used by the format-specific extractors.
use service::{
    chunk_options_for_metadata, fit_text_to_chars, make_entity_id, symbol_type_name,
    validate_source_file,
};

#[cfg(test)]
use service::{test_content, test_file, test_source};

pub(crate) fn source_kind(file: &FileRecord) -> Option<SourceKind> {
    service::source_kind(file)
}

pub(crate) fn extract<'source>(
    source: impl Into<Source<'source>>,
    options: ChunkOptions,
) -> Result<Vec<EntityFragment>, EngineError> {
    service::extract(source, options)
}

pub(crate) fn extract_for_indexing<'source>(
    source: impl Into<Source<'source>>,
    options: ChunkOptions,
) -> Result<Vec<IndexingExtractionFragment>, EngineError> {
    service::extract_for_indexing(source, options)
}

pub(crate) fn vector_content_for_fragment(
    fragment: &EntityFragment,
    embedding_content: Option<&[Content]>,
    max_chars: Option<usize>,
) -> Vec<Content> {
    service::vector_content_for_fragment(fragment, embedding_content, max_chars)
}
