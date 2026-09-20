use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{EngineError, EngineResult};

use super::{GlobRule, SourcePath, model::EmbeddingModelInfo};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Workspace {
    pub name: String,
    pub root: PathBuf,
    pub scan: ScanRules,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FtsConfig {
    pub tokenizer: &'static str,
    pub filters: &'static [&'static str],
}

/// Fixed FTS configuration for the current physical index format.
pub(crate) const FTS_CONFIG: FtsConfig = FtsConfig {
    tokenizer: "jieba",
    filters: &["lowercase"],
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IndexDescriptor {
    pub embedding: EmbeddingModelInfo,
    pub fts: FtsConfig,
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

/// Persistent rules for discovering and admitting workspace files to the index.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct ScanRules {
    /// Ordered path rules relative to the workspace root.
    pub globs: Vec<GlobRule>,
    pub hidden: bool,
    pub follow_symlinks: bool,
    pub max_depth: Option<usize>,
    pub max_file_size_bytes: Option<u64>,

    pub no_ignore: bool,
    pub ignore_files: Vec<PathBuf>,
    /// Traverse child Git repositories, including submodules and worktrees.
    pub nested_git: bool,
}

impl Default for ScanRules {
    fn default() -> Self {
        Self {
            globs: Vec::new(),
            hidden: false,
            follow_symlinks: false,
            max_depth: None,
            max_file_size_bytes: None,
            no_ignore: false,
            ignore_files: Vec::new(),
            nested_git: true,
        }
    }
}
