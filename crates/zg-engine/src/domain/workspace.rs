use std::{fmt, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{EngineError, EngineResult};

use super::SourcePath;

/// The unique, case-sensitive name of a registered workspace.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct WorkspaceName(String);

impl WorkspaceName {
    pub(crate) fn new(value: impl Into<String>) -> EngineResult<Self> {
        let value = value.into();
        if value.is_empty()
            || value.trim() != value
            || matches!(value.as_str(), "." | "..")
            || value
                .chars()
                .any(|character| character.is_control() || matches!(character, '/' | '\\'))
        {
            return Err(EngineError::invalid_argument(
                "workspace name must be non-empty, have no surrounding whitespace, path separators, or control characters",
            ));
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkspaceName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl TryFrom<String> for WorkspaceName {
    type Error = EngineError;
    fn try_from(value: String) -> EngineResult<Self> {
        Self::new(value)
    }
}

impl From<WorkspaceName> for String {
    fn from(value: WorkspaceName) -> Self {
        value.0
    }
}

/// Saved source selection. Request overrides are resolved before execution.
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IndexPolicy {
    Enabled,
    Disabled,
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

/// Metadata of a committed index; revision is not an immutable data snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceIndex {
    pub embedding: EmbeddingSchema,
    pub revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Workspace {
    pub name: WorkspaceName,
    pub root: PathBuf,
    pub file_selection: FileSelection,
    pub index_policy: IndexPolicy,
    pub index: Option<WorkspaceIndex>,
    pub created_epoch_ms: u64,
    pub updated_epoch_ms: u64,
}

impl Workspace {
    pub(crate) fn validate(&self) -> EngineResult<()> {
        if !self.root.is_absolute() {
            return Err(EngineError::invalid_argument(
                "workspace root must be an absolute path",
            ));
        }
        if let Some(index) = &self.index {
            index.embedding.validate()?;
        }
        Ok(())
    }

    pub(crate) fn source_path(&self, relative: &SourcePath) -> PathBuf {
        self.root.join(relative)
    }

    pub(crate) fn indexed(&self) -> bool {
        self.index_policy == IndexPolicy::Enabled && self.index.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_have_one_exact_representation() {
        for invalid in ["", " x", "x ", ".", "..", "a/b", "a\\b", "a\nb"] {
            assert!(WorkspaceName::new(invalid).is_err(), "{invalid:?}");
        }
        assert_eq!(
            WorkspaceName::new("项目 backend")
                .expect("valid name")
                .as_str(),
            "项目 backend"
        );
        assert_ne!(
            WorkspaceName::new("Backend").expect("name"),
            WorkspaceName::new("backend").expect("name")
        );
        assert!(serde_json::from_str::<WorkspaceName>("\"../other\"").is_err());
    }

    #[test]
    fn policy_and_committed_index_are_independent() {
        let directory = tempfile::tempdir().expect("root");
        let mut workspace = Workspace {
            name: WorkspaceName::new("example").expect("name"),
            root: directory.path().to_path_buf(),
            file_selection: FileSelection::default(),
            index_policy: IndexPolicy::Enabled,
            index: None,
            created_epoch_ms: 1,
            updated_epoch_ms: 1,
        };
        assert!(!workspace.indexed());
        assert_eq!(
            workspace.source_path(&SourcePath::new("src/lib.rs").expect("source path")),
            directory.path().join("src/lib.rs")
        );
        workspace.index = Some(WorkspaceIndex {
            embedding: EmbeddingSchema {
                provider: "local".into(),
                model: "example".into(),
                dimension: 8,
                metric: EmbeddingMetric::Cosine,
            },
            revision: 1,
        });
        assert!(workspace.indexed());
        workspace.index_policy = IndexPolicy::Disabled;
        assert!(!workspace.indexed());
        workspace
            .validate()
            .expect("disabled index remains a valid state");
    }
}
