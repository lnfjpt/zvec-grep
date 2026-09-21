use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Extensible evidence attached to edges and unresolved references.
pub type Metadata = Map<String, Value>;

/// Direction of a one-hop query relative to its endpoint.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Direction {
    In,
    Out,
    #[default]
    Both,
}

/// Kinds of directed graph edges.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Contains,
    Calls,
    Imports,
    Extends,
    Implements,
}

impl EdgeKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Contains => "contains",
            Self::Calls => "calls",
            Self::Imports => "imports",
            Self::Extends => "extends",
            Self::Implements => "implements",
        }
    }
}

/// Name references cannot create structural containment edges.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    Calls,
    Imports,
    Extends,
    Implements,
}

impl From<RefKind> for EdgeKind {
    fn from(value: RefKind) -> Self {
        match value {
            RefKind::Calls => Self::Calls,
            RefKind::Imports => Self::Imports,
            RefKind::Extends => Self::Extends,
            RefKind::Implements => Self::Implements,
        }
    }
}

/// Evidence used to select an edge target.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    FileLocal,
    ImportScoped,
    PreferredFile,
    WorkspaceUnique,
}

impl Provenance {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::FileLocal => "file_local",
            Self::ImportScoped => "import_scoped",
            Self::PreferredFile => "preferred_file",
            Self::WorkspaceUnique => "workspace_unique",
        }
    }
}

/// A resolved directed edge. Entity/file identity is supplied by the indexer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Edge {
    pub kind: EdgeKind,
    pub source: String,
    pub target: String,
    /// One-based source line of the relationship, if known.
    pub line: Option<u32>,
    /// Zero-based source column, if known.
    pub column: Option<u32>,
    pub provenance: Provenance,
    pub metadata: Metadata,
}

/// Extraction output that still requires name resolution.
/// New snapshots insert unresolved references; storage fills their target.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingRef {
    pub owner_id: String,
    pub ref_name: String,
    pub receiver_name: Option<String>,
    pub ref_kind: RefKind,
    pub arity: Option<u32>,
    /// One-based source line.
    pub line: u32,
    /// Zero-based source column.
    pub column: u32,
    pub metadata: Metadata,
}

/// A complete per-file snapshot. Node metadata is deliberately not stored here.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileGraph {
    /// Complete current entity IDs; the file ID is an implicit local endpoint.
    pub entity_ids: Vec<String>,
    pub edges: Vec<Edge>,
    pub pending_refs: Vec<PendingRef>,
}

/// An unresolved edge with its database identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredPendingRef {
    pub id: i64,
    pub file_id: String,
    pub reference: PendingRef,
}

/// Page over pending references. Restart pagination after file mutations.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PendingRefPage {
    pub refs: Vec<StoredPendingRef>,
    pub next_cursor: Option<i64>,
}

/// A resolver's proposed edge. The caller must validate the target in zvec and
/// serialize the entire read, resolution and writeback cycle with workspace writes/deletions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resolution {
    /// ID of the unresolved row in the edges table.
    pub ref_id: i64,
    pub target_id: String,
    /// Must be a cross-file provenance; `FileLocal` is rejected.
    pub provenance: Provenance,
}

/// Applied proposals and missing/already resolved references in an atomic batch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResolutionStats {
    pub resolved: usize,
    pub stale: usize,
}
