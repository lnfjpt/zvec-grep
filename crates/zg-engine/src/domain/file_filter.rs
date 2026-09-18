use serde::{Deserialize, Serialize};

use super::{FileCategory, FileFormat};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileFilter {
    pub globs: Vec<GlobRule>,
    pub formats: Vec<FileFormat>,
    pub excluded_formats: Vec<FileFormat>,
    pub categories: Vec<FileCategory>,
    pub excluded_categories: Vec<FileCategory>,
}

impl FileFilter {
    #[must_use]
    pub fn has_format_constraints(&self) -> bool {
        !self.formats.is_empty()
            || !self.excluded_formats.is_empty()
            || !self.categories.is_empty()
            || !self.excluded_categories.is_empty()
    }
}

/// One ordered path rule. A leading `!` excludes matching paths.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GlobRule {
    pub pattern: String,
    #[serde(default)]
    pub case_insensitive: bool,
}

impl From<String> for GlobRule {
    fn from(pattern: String) -> Self {
        Self {
            pattern,
            case_insensitive: false,
        }
    }
}

impl From<&str> for GlobRule {
    fn from(pattern: &str) -> Self {
        pattern.to_owned().into()
    }
}
