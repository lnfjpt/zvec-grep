mod content;
mod entity;
mod metadata;
pub(crate) mod model;
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
pub(crate) use entity::{Entity, EntityContent, EntityId};

// Metadata.
pub(crate) use metadata::IndexField;
pub use metadata::{CodeMetadata, EntityMetadata, MarkdownMetadata, SymbolType};

// Fragments.
pub(crate) use entity::{EntityFragment, FragmentId, WindowFragment, validate_fragments};

// Workspaces.
pub use workspace::FileSelection;
pub(crate) use workspace::{FTS_CONFIG, FtsConfig, IndexDescriptor, IndexState, Workspace};

// Models.
pub(crate) use model::{EmbeddingModelInfo, Metric};
