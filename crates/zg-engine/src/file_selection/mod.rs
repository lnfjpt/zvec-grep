//! Engine-owned file matching and filesystem selection policy.

mod matcher;
mod policy;

pub(crate) use matcher::FileMatcher;
pub(crate) use policy::ScanPolicy;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Resolved filesystem discovery settings, persisted separately from domain filters.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct ScanOptions {
    pub hidden: bool,
    pub no_ignore: bool,
    /// Traverse child directories containing a `.git` file or directory.
    pub nested_git: bool,
    pub ignore_files: Vec<PathBuf>,
    /// No additional depth limit when absent.
    pub max_depth: Option<usize>,
    /// The indexing pipeline applies format-specific defaults when absent.
    pub max_file_size_bytes: Option<u64>,
    pub follow: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            hidden: false,
            no_ignore: false,
            nested_git: true,
            ignore_files: Vec::new(),
            max_depth: None,
            max_file_size_bytes: None,
            follow: false,
        }
    }
}

#[cfg(test)]
mod tests;
