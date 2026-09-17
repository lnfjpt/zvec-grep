use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{EngineError, EngineResult};

use super::{SourcePath, model::EmbeddingSchema};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Workspace {
    pub name: String,
    pub root: PathBuf,
    pub file_selection: FileSelection,
    pub index: IndexState,
    pub created_epoch_ms: u64,
    pub updated_epoch_ms: u64,
}

impl Workspace {
    /// Validate names without normalizing their case or whitespace.
    pub(crate) fn validate_name(name: &str) -> EngineResult<()> {
        if name.is_empty()
            || name.trim() != name
            || matches!(name, "." | "..")
            || name
                .chars()
                .any(|character| character.is_control() || matches!(character, '/' | '\\'))
        {
            return Err(EngineError::invalid_argument(format!(
                "invalid workspace name {name:?}",
            )));
        }
        Ok(())
    }

    #[allow(clippy::unnecessary_debug_formatting)] // Keep control characters in paths escaped.
    pub(crate) fn validate(&self) -> EngineResult<()> {
        Self::validate_name(&self.name)?;
        if !self.root.is_absolute() {
            return Err(EngineError::invalid_argument(format!(
                "workspace root {:?} must be an absolute path",
                self.root,
            )));
        }
        if let IndexState::Enabled(index) = &self.index {
            index.embedding.validate()?;
        }
        Ok(())
    }

    pub(crate) fn source_path(&self, relative: &SourcePath) -> PathBuf {
        self.root.join(relative)
    }

    pub(crate) fn index_enabled(&self) -> bool {
        matches!(self.index, IndexState::Enabled(_))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct FileSelection {
    pub include_paths: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub globs: Vec<String>,
    pub insensitive_globs: Vec<String>,
    pub file_types: Vec<String>,
    pub excluded_file_types: Vec<String>,
    pub hidden: bool,
    pub no_ignore: bool,
    pub ignore_files: Vec<PathBuf>,
    pub max_depth: Option<usize>,
    pub max_file_size_bytes: Option<u64>,
    pub follow: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum IndexState {
    Uninitialized,
    Disabled,
    Enabled(IndexDescriptor),
}

impl IndexState {
    pub(crate) fn descriptor(&self) -> Option<&IndexDescriptor> {
        match self {
            Self::Enabled(index) => Some(index),
            Self::Uninitialized | Self::Disabled => None,
        }
    }
}

/// Metadata of a committed index; revision is not an immutable data snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IndexDescriptor {
    pub embedding: EmbeddingSchema,
    pub revision: u64,
}
