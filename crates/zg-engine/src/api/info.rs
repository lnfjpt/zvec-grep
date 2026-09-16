//! Types used by `info` and `drop_index`.

pub use options::InfoOptions;
pub use result::InfoResult;

pub mod options {
    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub struct InfoOptions {
        /// Workspace to inspect or mutate. `None` uses the working directory.
        pub root: Option<PathBuf>,
        pub include_status: bool,
    }
}

pub mod result {
    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};

    use crate::api::index::options::DiscoveryOptions;

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct InfoResult {
        pub root: PathBuf,
        pub indexed: bool,
        pub index_policy: WorkspaceIndexPolicy,
        pub home: PathBuf,
        pub index_path: PathBuf,
        pub source: InfoSource,
        pub workspace_index: Option<WorkspaceIndexInfo>,
        pub status: Option<WorkspaceIndexStatus>,
        pub suggestion: Option<String>,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum InfoSource {
        Index,
        Unindexed,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum WorkspaceIndexPolicy {
        Enabled,
        Disabled,
        Undecided,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct WorkspaceIndexInfo {
        pub name: String,
        pub path: PathBuf,
        /// The single base directory for every source file in this workspace.
        pub root: PathBuf,
        pub discovery: DiscoveryOptions,
        pub policy: WorkspaceIndexPolicy,
        pub embedding: Option<WorkspaceIndexEmbedding>,
        pub index_version: Option<u32>,
        pub generation: Option<u64>,
        pub created_epoch_ms: u64,
        pub updated_epoch_ms: u64,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct WorkspaceIndexEmbedding {
        pub provider: String,
        pub model: String,
        pub dimension: usize,
        pub metric: String,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub struct WorkspaceIndexStatus {
        pub files_scanned: usize,
        pub files_stored: usize,
        pub files_indexed: usize,
        pub entities_indexed: u64,
        /// Total source snapshot bytes for successfully indexed files, excluding index storage.
        pub indexed_size_bytes: u64,
        pub files_pending: usize,
        pub files_failed: usize,
        pub files_added: usize,
        pub files_modified: usize,
        pub files_deleted: usize,
        pub files_unchanged: usize,
    }
}

impl From<crate::domain::IndexPolicy> for result::WorkspaceIndexPolicy {
    fn from(value: crate::domain::IndexPolicy) -> Self {
        match value {
            crate::domain::IndexPolicy::Enabled => Self::Enabled,
            crate::domain::IndexPolicy::Disabled => Self::Disabled,
        }
    }
}

impl result::WorkspaceIndexInfo {
    pub(crate) fn from_workspace(
        workspace: &crate::domain::Workspace,
        home: &std::path::Path,
        index_version: Option<u32>,
    ) -> Self {
        Self {
            name: workspace.name.to_string(),
            path: home.to_path_buf(),
            root: workspace.root.clone(),
            discovery: workspace.file_selection.clone(),
            policy: workspace.index_policy.into(),
            embedding: workspace
                .index
                .as_ref()
                .map(|index| (&index.embedding).into()),
            index_version,
            generation: workspace
                .index
                .as_ref()
                .and_then(|index| (index.revision != 0).then_some(index.revision)),
            created_epoch_ms: workspace.created_epoch_ms,
            updated_epoch_ms: workspace.updated_epoch_ms,
        }
    }
}

impl From<&crate::domain::EmbeddingSchema> for result::WorkspaceIndexEmbedding {
    fn from(value: &crate::domain::EmbeddingSchema) -> Self {
        Self {
            provider: value.provider.clone(),
            model: value.model.clone(),
            dimension: value.dimension,
            metric: match value.metric {
                crate::domain::EmbeddingMetric::Cosine => "cosine",
                crate::domain::EmbeddingMetric::DotProduct => "dot",
                crate::domain::EmbeddingMetric::Euclidean => "euclidean",
            }
            .to_owned(),
        }
    }
}
