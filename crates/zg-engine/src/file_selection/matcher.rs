use std::path::Path;

use ignore::{
    Match,
    overrides::{Override, OverrideBuilder},
};

use crate::{
    EngineError, EngineResult,
    domain::{FileFilter, FileFormat},
};

const MAX_GLOB_RULES: usize = 1_024;
const MAX_GLOB_BYTES: usize = 4_096;
const MAX_TOTAL_GLOB_BYTES: usize = 1_048_576;

/// Compiled path rules and detected-format constraints; matching never reads disk.
#[derive(Clone, Debug)]
pub(crate) struct FileMatcher {
    paths: Override,
    filter: FileFilter,
}

impl FileMatcher {
    pub(crate) fn new(root: &Path, filter: &FileFilter) -> EngineResult<Self> {
        if filter.globs.len() > MAX_GLOB_RULES
            || filter
                .globs
                .iter()
                .map(|rule| rule.pattern.len())
                .sum::<usize>()
                > MAX_TOTAL_GLOB_BYTES
        {
            return Err(EngineError::invalid_argument(
                "file filter has too many glob rules or exceeds the total pattern size limit",
            ));
        }
        let mut builder = OverrideBuilder::new(root);
        for rule in &filter.globs {
            if rule.pattern.is_empty() || rule.pattern == "!" || rule.pattern.len() > MAX_GLOB_BYTES
            {
                return Err(EngineError::invalid_argument(format!(
                    "invalid glob {:?}: expected a non-empty pattern of at most {MAX_GLOB_BYTES} bytes",
                    rule.pattern
                )));
            }
            builder
                .case_insensitive(rule.case_insensitive)
                .map_err(|error| glob_error(&error))?;
            builder
                .add(&rule.pattern)
                .map_err(|error| glob_error(&error))?;
        }
        Ok(Self {
            paths: builder.build().map_err(|error| glob_error(&error))?,
            filter: filter.clone(),
        })
    }

    pub(crate) fn path_match(&self, path: &Path, is_directory: bool) -> Match<()> {
        self.paths.matched(path, is_directory).map(|_| ())
    }

    pub(crate) fn matches_path(&self, relative: &Path) -> bool {
        !self.path_match(relative, false).is_ignore()
            && relative
                .parent()
                .into_iter()
                .flat_map(Path::ancestors)
                .take_while(|parent| !parent.as_os_str().is_empty())
                .all(|parent| !self.path_match(parent, true).is_ignore())
    }

    pub(crate) fn matches_formats(&self, formats: &[FileFormat]) -> bool {
        (self.filter.formats.is_empty()
            || formats
                .iter()
                .any(|format| self.filter.formats.contains(format)))
            && !formats
                .iter()
                .any(|format| self.filter.excluded_formats.contains(format))
            && (self.filter.categories.is_empty()
                || formats
                    .iter()
                    .flat_map(|format| format.categories())
                    .any(|category| self.filter.categories.contains(category)))
            && !formats
                .iter()
                .flat_map(|format| format.categories())
                .any(|category| self.filter.excluded_categories.contains(category))
    }
}

fn glob_error(error: &ignore::Error) -> EngineError {
    EngineError::invalid_argument(format!("invalid file glob: {error}"))
}
