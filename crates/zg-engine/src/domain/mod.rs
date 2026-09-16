mod content;
mod entity;
mod source;
mod workspace;

// Source files.
pub(crate) use source::{FileId, FileIndexStatus, FileRecord, FileSnapshot};

// Source directories and shared path invariants.
pub(crate) use source::{DirectoryId, DirectoryRecord, SourcePath};

// File formats.
pub(crate) use source::{FileCategory, FileFormat};

// Source ranges.
pub(crate) use source::{ByteRange, SourceRange, TextRange};

// Content.
pub(crate) use content::{Content, ImageContent, TableCell, TableCellRole, TableContent};

// Entities.
pub(crate) use entity::{Entity, EntityContent, EntityId, EntityMetadata};

// Fragments.
pub(crate) use entity::{EntityFragment, FragmentId, WindowFragment, validate_fragments};

pub use entity::SymbolType;
pub use workspace::FileSelection;
pub(crate) use workspace::{
    EmbeddingMetric, EmbeddingSchema, IndexPolicy, Workspace, WorkspaceIndex, WorkspaceName,
};
