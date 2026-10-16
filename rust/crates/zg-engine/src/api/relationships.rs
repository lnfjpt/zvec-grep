//! Symbol-based incoming and outgoing call queries.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Reads an existing workspace index without loading models or refreshing it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RelationshipOptions {
    /// Workspace location. `None` uses the working directory.
    pub root: Option<PathBuf>,
    /// Symbol name, optionally qualified as `scope::name`.
    pub symbol: String,
    /// Maximum returned call sites per matched symbol; defaults to 20.
    pub limit: Option<usize>,
    /// Maximum workspace lock wait in milliseconds; defaults to 30 seconds.
    pub lock_timeout_ms: Option<u64>,
    /// Runtime-only cooperative cancellation.
    #[serde(skip)]
    pub signal: Option<tokio_util::sync::CancellationToken>,
}

/// A definition in workspace-relative source coordinates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelationshipSymbol {
    pub name: String,
    pub file_path: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
}

/// One call site. Unavailable endpoint metadata is represented by `None`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelationshipEdge {
    pub symbol: Option<RelationshipSymbol>,
    /// One-based call-site line.
    pub line: Option<u32>,
    /// Zero-based byte column.
    pub column: Option<u32>,
}

/// One matched definition and its call sites, preserving repeated calls.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolRelationships {
    #[serde(flatten)]
    pub symbol: RelationshipSymbol,
    /// Number of resolved call sites before applying the per-symbol limit.
    pub total_edges: usize,
    pub edges: Vec<RelationshipEdge>,
}
