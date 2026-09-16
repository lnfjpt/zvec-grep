//! Types used by [`crate::ZvecGrep::context`].

pub use options::ContextOptions;
pub use result::ContextResult;

/// Options accepted by [`crate::ZvecGrep::context`].
pub mod options {
    pub use crate::domain::SymbolType;
    pub use crate::pipelines::search::types::{
        SearchRoute as ContextRoute, SearchRouteMode as ContextRouteMode,
    };

    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[allow(clippy::struct_excessive_bools)]
    pub struct ContextOptions {
        pub query: Option<String>,
        pub queries: Vec<String>,
        pub rg: bool,
        pub rg_options: RgOptions,
        pub rg_paths: Vec<PathBuf>,
        pub routes: Vec<ContextRoute>,
        pub fuse: bool,
        /// Workspace root. `None` uses the process working directory.
        pub root: Option<PathBuf>,
        pub limit: Option<usize>,
        pub auto_update: bool,
        /// Explicit refresh policy; absent preserves the legacy auto-update behavior.
        #[serde(default)]
        pub refresh: Option<RefreshPolicy>,
        pub trace: bool,
        pub prefer_symbol: bool,
        pub symbol_types: Vec<SymbolType>,
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
        pub modified_after_epoch_ms: Option<u64>,
        pub modified_before_epoch_ms: Option<u64>,
        pub embedding_concurrency: Option<usize>,
        /// Allows remote embedding for this operation without persisting a grant.
        #[serde(default)]
        pub allow_remote: bool,
        #[serde(default)]
        pub api_key: Option<String>,
        #[serde(default)]
        pub endpoint: Option<String>,
        /// Model disclosed by an interactive caller; reject a changed index model.
        #[serde(default)]
        pub authorization_model: Option<String>,
        #[serde(default)]
        pub device: Option<crate::api::index::options::Device>,
        #[serde(default)]
        pub model_cache: Option<PathBuf>,
        /// Runtime-only progress for synchronous refreshes.
        #[serde(skip)]
        pub on_progress: Option<crate::api::index::progress::IndexProgressReporter>,
        /// Runtime-only cooperative cancellation for this request.
        #[serde(skip)]
        pub signal: Option<tokio_util::sync::CancellationToken>,
    }

    impl Default for ContextOptions {
        fn default() -> Self {
            Self {
                query: None,
                queries: Vec::new(),
                rg: false,
                rg_options: RgOptions::default(),
                rg_paths: Vec::new(),
                routes: Vec::new(),
                fuse: false,
                root: None,
                limit: None,
                auto_update: true,
                refresh: None,
                trace: false,
                prefer_symbol: false,
                symbol_types: Vec::new(),
                include_paths: Vec::new(),
                exclude_paths: Vec::new(),
                globs: Vec::new(),
                insensitive_globs: Vec::new(),
                file_types: Vec::new(),
                excluded_file_types: Vec::new(),
                hidden: false,
                no_ignore: false,
                ignore_files: Vec::new(),
                max_depth: None,
                max_file_size_bytes: None,
                follow: false,
                modified_after_epoch_ms: None,
                modified_before_epoch_ms: None,
                embedding_concurrency: None,
                allow_remote: false,
                api_key: None,
                endpoint: None,
                authorization_model: None,
                device: None,
                model_cache: None,
                on_progress: None,
                signal: None,
            }
        }
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum RefreshPolicy {
        Background,
        Wait,
        Off,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub struct RgOptions {
        pub extra_args: Vec<String>,
        pub pattern_files: Vec<PathBuf>,
        pub fixed_strings: bool,
        pub ignore_case: bool,
        pub word_regexp: bool,
        pub before_context: usize,
        pub after_context: usize,
    }
}

/// Values returned by [`crate::ZvecGrep::context`].
pub mod result {
    pub use crate::lexical::structure::{
        StructureEnrichmentDiagnostics, StructureEnrichmentSource,
    };
    pub use crate::pipelines::search::types::{
        MatchedBy, SearchFinalTrace, SearchFusionTrace, SearchHitTrace, SearchRecallTrace,
        TimingEntry,
    };

    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};

    use super::options::SymbolType;

    #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
    pub struct ContextResult {
        pub query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub freshness: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub background_refresh: Option<String>,
        pub root: PathBuf,
        pub source: ContextSource,
        pub coverage: ContextCoverage,
        pub workspace_index: Option<ContextWorkspaceIndex>,
        pub items: Vec<ContextItem>,
        pub group_results: Vec<ContextGroupResult>,
        pub diagnostics: ContextDiagnostics,
    }

    #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
    pub struct ContextGroupResult {
        pub id: String,
        pub query: String,
        pub role: ContextQueryGroupRole,
        pub items: Vec<ContextItem>,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ContextSource {
        Index,
        Rg,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ContextCoverage {
        RankedSample,
        RgExhaustive,
        RgTruncated,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct ContextWorkspaceIndex {
        pub name: String,
        pub path: PathBuf,
        pub generation: Option<u64>,
    }

    #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
    pub struct ContextItem {
        pub kind: ContextItemKind,
        pub rank: usize,
        pub absolute_path: PathBuf,
        pub relative_path: PathBuf,
        pub range: ContentRange,
        pub excerpt_range: Option<ContentRange>,
        pub content: String,
        pub content_role: Option<ContextContentRole>,
        pub outline: Option<String>,
        pub status: ContextItemStatus,
        pub score: Option<f64>,
        pub matched_by: MatchedBy,
        pub metadata: Option<EntityMetadata>,
        pub entity_id: Option<String>,
        pub container: Option<ContextContainer>,
        pub trace: Option<SearchHitTrace>,
        pub query_groups: Vec<ContextQueryGroupMatch>,
        pub selection_reason: Option<ContextSelectionReason>,
        pub coverage_group: Option<String>,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ContextContentRole {
        Source,
        Outline,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct ContextContainer {
        pub entity_id: String,
        pub range: ContentRange,
        pub metadata: Option<EntityMetadata>,
    }

    #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
    pub struct ContextQueryGroupMatch {
        pub id: String,
        pub query: String,
        pub role: ContextQueryGroupRole,
        pub rank: usize,
        pub matched_by: MatchedBy,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ContextQueryGroupRole {
        Primary,
        Supplemental,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ContextSelectionReason {
        Coverage,
        GlobalFill,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ContextItemKind {
        IndexedEntity,
        LexicalMatch,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ContextItemStatus {
        Fresh,
        PossiblyStale,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub struct ContextDiagnostics {
        pub empty_reason: Option<EmptyReason>,
        pub index: Option<IndexDiagnostics>,
        pub rg: Option<RgDiagnostics>,
        pub structure: Option<StructureEnrichmentDiagnostics>,
        pub timings: Vec<TimingEntry>,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct IndexDiagnostics {
        pub hits_returned: usize,
        pub query_groups: Vec<IndexQueryGroupDiagnostics>,
        pub routes: Vec<IndexRouteDiagnostics>,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct IndexQueryGroupDiagnostics {
        pub id: String,
        pub query: String,
        pub role: ContextQueryGroupRole,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct IndexRouteDiagnostics {
        pub id: String,
        pub mode: super::options::ContextRouteMode,
        pub query: String,
    }

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum EmptyReason {
        NoMatches,
        NoSearchableFiles,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub struct RgDiagnostics {
        pub backend: String,
        pub command: PathBuf,
        pub args: Vec<String>,
        pub ignored_directories: Vec<PathBuf>,
        pub missing_paths: Vec<PathBuf>,
        pub searched_paths: Vec<PathBuf>,
        pub limit: Option<usize>,
        pub truncated: bool,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case", tag = "kind")]
    pub enum ContentRange {
        File,
        /// Half-open UTF-8 byte offsets and columns; endpoint lines are one-based.
        Text {
            start_line: usize,
            end_line: usize,
            start_byte_offset: usize,
            end_byte_offset: usize,
            start_byte_column: usize,
            end_byte_column: usize,
        },
        Byte {
            start_offset: u64,
            end_offset: u64,
        },
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case", tag = "kind")]
    pub enum EntityMetadata {
        Code {
            symbol_type: SymbolType,
            symbol_name: Option<String>,
            scope: Option<String>,
            node_type: Option<String>,
            signature: Option<String>,
            documentation: Option<String>,
            modifiers: Vec<String>,
        },
        Markdown {
            heading: Option<String>,
            level: Option<usize>,
            scope: Option<String>,
        },
    }
}

impl From<crate::domain::SourceRange> for result::ContentRange {
    fn from(range: crate::domain::SourceRange) -> Self {
        use crate::domain::SourceRange;
        match range {
            SourceRange::File => Self::File,
            SourceRange::Text(range) => Self::Text {
                start_line: range.start_line(),
                end_line: range.end_line(),
                start_byte_offset: range.start_byte_offset(),
                end_byte_offset: range.end_byte_offset(),
                start_byte_column: range.start_byte_column(),
                end_byte_column: range.end_byte_column(),
            },
            SourceRange::Byte(range) => Self::Byte {
                start_offset: range.start_offset,
                end_offset: range.end_offset,
            },
        }
    }
}

impl From<&crate::domain::SourceRange> for result::ContentRange {
    fn from(range: &crate::domain::SourceRange) -> Self {
        (*range).into()
    }
}

impl From<crate::domain::TextRange> for result::ContentRange {
    fn from(range: crate::domain::TextRange) -> Self {
        crate::domain::SourceRange::Text(range).into()
    }
}

impl From<&crate::domain::TextRange> for result::ContentRange {
    fn from(range: &crate::domain::TextRange) -> Self {
        (*range).into()
    }
}

impl From<crate::domain::EntityMetadata> for result::EntityMetadata {
    fn from(metadata: crate::domain::EntityMetadata) -> Self {
        match metadata {
            crate::domain::EntityMetadata::Code {
                symbol_type,
                symbol_name,
                scope,
                node_type,
                signature,
                documentation,
                modifiers,
            } => Self::Code {
                symbol_type,
                symbol_name,
                scope,
                node_type,
                signature,
                documentation,
                modifiers,
            },
            crate::domain::EntityMetadata::Markdown {
                heading,
                level,
                scope,
            } => Self::Markdown {
                heading,
                level,
                scope,
            },
        }
    }
}

impl From<&crate::domain::EntityMetadata> for result::EntityMetadata {
    fn from(metadata: &crate::domain::EntityMetadata) -> Self {
        metadata.clone().into()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::domain::{ByteRange, EntityMetadata, SourceRange, SymbolType, TextRange};

    use super::result;

    #[test]
    fn domain_values_preserve_public_wire_coordinates_and_metadata() {
        let file: result::ContentRange = SourceRange::File.into();
        assert_eq!(
            serde_json::to_value(file).expect("file range"),
            json!({ "kind": "file" })
        );
        let bytes: result::ContentRange = SourceRange::Byte(ByteRange {
            start_offset: 12,
            end_offset: 24,
        })
        .into();
        assert_eq!(
            serde_json::to_value(bytes).expect("byte range"),
            json!({ "kind": "byte", "start_offset": 12, "end_offset": 24 })
        );
        let indexed: result::ContentRange = TextRange::from_coordinates(6, 13, 2, 3, 0, 0)
            .expect("indexed range")
            .into();
        assert_eq!(
            serde_json::to_value(indexed).expect("indexed range"),
            json!({
                "kind": "text", "start_line": 2, "end_line": 3,
                "start_byte_offset": 6, "end_byte_offset": 13,
                "start_byte_column": 0, "end_byte_column": 0,
            })
        );
        let lexical: result::ContentRange = TextRange::from_coordinates(9, 12, 2, 2, 3, 6)
            .expect("lexical range")
            .into();
        assert_eq!(
            serde_json::to_value(lexical).expect("lexical range"),
            json!({
                "kind": "text", "start_line": 2, "end_line": 2,
                "start_byte_offset": 9, "end_byte_offset": 12,
                "start_byte_column": 3, "end_byte_column": 6,
            })
        );
        let metadata: result::EntityMetadata = EntityMetadata::Code {
            symbol_type: SymbolType::Function,
            symbol_name: Some("calculate".to_owned()),
            scope: None,
            node_type: None,
            signature: None,
            documentation: None,
            modifiers: vec!["public".to_owned()],
        }
        .into();
        assert_eq!(
            serde_json::to_value(metadata).expect("metadata"),
            json!({
                "kind": "code", "symbol_type": "function", "symbol_name": "calculate", "scope": null,
                "node_type": null, "signature": null, "documentation": null, "modifiers": ["public"],
            })
        );
    }
}
