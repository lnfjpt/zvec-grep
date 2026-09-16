//! Compile common path globs into retrieval predicates, with ripgrep matching
//! as the fallback for syntax that cannot be represented exactly.

use std::path::Path;

use ignore::overrides::{Override, OverrideBuilder};

use crate::{
    EngineError,
    storage::spi::{StoragePathFilter, WorkspaceIndexStorage},
};

pub(super) struct GlobFilter<'a> {
    matcher: Override,
    sensitive: &'a [String],
    insensitive: &'a [String],
}

impl<'a> GlobFilter<'a> {
    pub(super) fn new(
        workspace_root: &Path,
        sensitive: &'a [String],
        insensitive: &'a [String],
    ) -> Result<Self, EngineError> {
        let mut builder = OverrideBuilder::new(workspace_root);
        // The current API keeps these in separate lists: insensitive rules
        // follow sensitive rules, preserving the order within each list.
        for (patterns, insensitive) in [(sensitive, false), (insensitive, true)] {
            builder
                .case_insensitive(insensitive)
                .map_err(|error| glob_error(&error))?;
            for pattern in patterns {
                builder.add(pattern).map_err(|error| glob_error(&error))?;
            }
        }
        Ok(Self {
            matcher: builder.build().map_err(|error| glob_error(&error))?,
            sensitive,
            insensitive,
        })
    }

    pub(super) fn is_match(&self, relative_path: &Path) -> bool {
        // A file cannot be reached by a ripgrep walk when one of its parent
        // directories is excluded, even if a later rule includes the file.
        !self.matcher.matched(relative_path, false).is_ignore()
            && relative_path
                .parent()
                .into_iter()
                .flat_map(Path::ancestors)
                .take_while(|parent| !parent.as_os_str().is_empty())
                .all(|parent| !self.matcher.matched(parent, true).is_ignore())
    }

    pub(super) fn pushdown(
        &self,
        storage: &dyn WorkspaceIndexStorage,
    ) -> Result<Option<StoragePathFilter>, EngineError> {
        if !storage.supports_path_filters() || !self.insensitive.is_empty() {
            return Ok(None);
        }
        let mut includes = Vec::new();
        let mut excludes = Vec::new();
        let mut has_exclusion = false;
        let mut uses_file_name = false;
        for pattern in self.sensitive {
            if let Some(pattern) = pattern.strip_prefix('!') {
                // Recursive directory exclusions at the end of the rule list
                // remove whole subtrees. Other exclusions can prune matching
                // directories by name and require the walk-aware fallback.
                let Some(directory) = recursive_directory(pattern) else {
                    return Ok(None);
                };
                excludes.push(directory_filter(storage, directory)?);
                has_exclusion = true;
            } else {
                // Re-inclusion depends on which parent directories a later
                // rule restores, so do not reduce it to Boolean file matches.
                if has_exclusion {
                    return Ok(None);
                }
                let Some((predicate, name)) = positive_filter(storage, pattern)? else {
                    return Ok(None);
                };
                uses_file_name |= name;
                includes.push(predicate);
            }
        }
        if uses_file_name && storage.has_non_unicode_file_names()? {
            // The catalog retains native paths. Lossy STRING metadata must
            // never determine the result for a non-Unicode basename.
            return Ok(None);
        }
        let include = if includes.is_empty() {
            StoragePathFilter::All
        } else {
            any(includes)
        };
        Ok(Some(if excludes.is_empty() {
            include
        } else {
            all(vec![include, negate(any(excludes))])
        }))
    }
}

fn glob_error(error: &ignore::Error) -> EngineError {
    EngineError::invalid_argument(format!("invalid ripgrep glob: {error}"))
}

fn positive_filter(
    storage: &dyn WorkspaceIndexStorage,
    pattern: &str,
) -> Result<Option<(StoragePathFilter, bool)>, EngineError> {
    if matches!(pattern, "*" | "**" | "**/*") {
        return Ok(Some((StoragePathFilter::All, false)));
    }
    if let Some(directory) = recursive_directory(pattern) {
        return Ok(Some((directory_filter(storage, directory)?, false)));
    }
    // **/foo and foo both match basenames at any depth.
    let unanchored = pattern.strip_prefix("**/").unwrap_or(pattern);
    if let Some(predicate) = file_name_filter(unanchored) {
        return Ok(Some((predicate, true)));
    }
    if let Some((directory, basename)) = pattern.split_once("/**/")
        && literal_directory(directory)
        && let Some(predicate) = file_name_filter(basename)
    {
        return Ok(Some((
            all(vec![directory_filter(storage, directory)?, predicate]),
            true,
        )));
    }
    Ok(None)
}

fn recursive_directory(pattern: &str) -> Option<&str> {
    let directory = pattern.strip_suffix("/**")?;
    literal_directory(directory).then_some(directory)
}

fn literal_directory(directory: &str) -> bool {
    let directory = directory.strip_prefix('/').unwrap_or(directory);
    !directory.is_empty()
        && directory
            .split('/')
            .all(|part| literal_name(part) && !matches!(part, "." | ".."))
}

fn directory_filter(
    storage: &dyn WorkspaceIndexStorage,
    directory: &str,
) -> Result<StoragePathFilter, EngineError> {
    let directory = directory.strip_prefix('/').unwrap_or(directory);
    Ok(storage
        .directory_id(Path::new(directory))?
        .map_or(StoragePathFilter::None, StoragePathFilter::Directory))
}

fn file_name_filter(pattern: &str) -> Option<StoragePathFilter> {
    if literal_name(pattern) {
        return Some(StoragePathFilter::FileNameExact(pattern.to_owned()));
    }
    if let Some(prefix) = pattern
        .strip_suffix('*')
        .filter(|prefix| literal_like(prefix))
    {
        return Some(StoragePathFilter::FileNamePrefix(prefix.to_owned()));
    }
    if let Some(suffix) = pattern
        .strip_prefix('*')
        .filter(|suffix| literal_like(suffix))
    {
        return Some(StoragePathFilter::FileNameSuffix(suffix.to_owned()));
    }
    None
}

fn literal_name(name: &str) -> bool {
    // A conservative alphabet avoids differences between glob escaping,
    // platform separators and SQL LIKE wildcard syntax.
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

fn literal_like(name: &str) -> bool {
    literal_name(name) && !name.contains('_')
}

fn any(predicates: Vec<StoragePathFilter>) -> StoragePathFilter {
    let mut result = Vec::new();
    for predicate in predicates {
        match predicate {
            StoragePathFilter::All => return StoragePathFilter::All,
            StoragePathFilter::None => {}
            predicate => result.push(predicate),
        }
    }
    match result.len() {
        0 => StoragePathFilter::None,
        1 => result.pop().expect("one predicate"),
        _ => StoragePathFilter::Or(result),
    }
}

fn all(predicates: Vec<StoragePathFilter>) -> StoragePathFilter {
    let mut result = Vec::new();
    for predicate in predicates {
        match predicate {
            StoragePathFilter::None => return StoragePathFilter::None,
            StoragePathFilter::All => {}
            predicate => result.push(predicate),
        }
    }
    match result.len() {
        0 => StoragePathFilter::All,
        1 => result.pop().expect("one predicate"),
        _ => StoragePathFilter::And(result),
    }
}

fn negate(predicate: StoragePathFilter) -> StoragePathFilter {
    match predicate {
        StoragePathFilter::All => StoragePathFilter::None,
        StoragePathFilter::None => StoragePathFilter::All,
        predicate => StoragePathFilter::Not(Box::new(predicate)),
    }
}
