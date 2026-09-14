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
        pub id: String,
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
        pub entities_indexed: usize,
        pub fragments_truncated: usize,
        pub files_pending: usize,
        pub files_failed: usize,
        pub files_added: usize,
        pub files_modified: usize,
        pub files_deleted: usize,
        pub files_unchanged: usize,
    }
}
