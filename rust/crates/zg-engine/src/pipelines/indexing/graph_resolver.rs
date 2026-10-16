//! Cross-file resolution of pending references via the graph's own indexes.
//!
//! The resolver consumes the graph storage's own pending queue, name index,
//! and file-path index, so it needs no extraction state:
//! [`ResolutionSession::list_pending_refs`] pages the backlog, relative
//! import specifiers resolve through [`ResolutionSession::file_id_by_path`],
//! [`ResolutionSession::lookup_candidates`] narrows name candidates, ambiguous
//! bare names resolve through [`ResolutionSession::imported_file_ids`] when
//! the file's imports pin them to one module, `self`/`this` receiver calls
//! bind to the one same-file declaration of the invoked member, qualified
//! `Class::method` receivers bind to the entity whose `qualified_name`
//! spells the same path, export/arity evidence narrows the remaining
//! ambiguities, external import specifiers terminate as failed, and
//! [`ResolutionSession::apply_resolutions`] writes each batch atomically.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use crate::domain::{FileFormat, formats_for_path, reference_compatible_with};
use crate::storage::ResolutionSession;
use crate::storage::graph::{
    EdgeKind, NameIndexEntry, PendingRef, Provenance, Resolution, StoredPendingRef,
};

/// Outcome of one resolution sweep.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ResolutionSweep {
    /// Pending references examined this sweep.
    pub scanned: usize,
    /// References resolved by `apply_resolutions`.
    pub resolved: usize,
    /// Proposals that no longer matched a pending row.
    pub stale: usize,
    /// Ambiguous references resolved through import scoping this sweep.
    pub import_bound: usize,
    /// Receiver-qualified calls bound to a same-file declaration this sweep.
    pub file_scoped: usize,
    /// Qualified references bound through `qualified_name` this sweep.
    pub preferred_file: usize,
    /// Ambiguous references narrowed to the one exported candidate.
    pub export_bound: usize,
    /// Ambiguous references narrowed to the one arity-matching candidate.
    pub arity_bound: usize,
    /// Import references terminated as unresolvable by `apply_failures`.
    pub failed: usize,
    /// Ambiguous references whose candidate sets `record_candidates` stored.
    pub candidates_recorded: usize,
}

/// Resolves pending references each sweep: relative import specifiers onto
/// file endpoints, workspace-unique names onto their single definition,
/// ambiguous bare names onto the one module their file imports,
/// `self`/`this` receiver calls onto the one same-file declaration, and
/// qualified `Class::method` receivers onto the entity declaring that
/// exact `qualified_name`. Ambiguities surviving these rules narrow through
/// export visibility and declared arity when the evidence picks exactly one
/// candidate, and import specifiers that address modules outside the
/// workspace terminate as failed.
///
/// Matching stays conservative. Same-file name hits stay pending — partition
/// already resolved unambiguous local names, so a survivor in the same file
/// is a receiver-typed call that needs type inference, except where the
/// receiver names the enclosing instance (`self`/`this`) and the file declares
/// the member exactly once — and names that stay ambiguous after every
/// evidence rule record their candidate set for consumers.
///
/// The entire read/resolve/writeback cycle runs under the caller's graph
/// storage lock, matching the serialization contract of `apply_resolutions`.
///
/// # Errors
/// Propagates storage failures. Completed writeback batches remain committed;
/// references not yet written stay pending for a later sweep.
pub(super) fn resolve_pending(
    graph: &mut dyn ResolutionSession,
) -> crate::EngineResult<ResolutionSweep> {
    let mut sweep = ResolutionSweep::default();
    let mut resolutions = Vec::new();
    let mut failures: Vec<i64> = Vec::new();
    let mut candidate_sets: Vec<(i64, Vec<String>)> = Vec::new();
    let mut importing_paths: HashMap<u32, Option<String>> = HashMap::new();
    let mut candidate_paths: HashMap<u32, Option<String>> = HashMap::new();
    let mut importing_files: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut module_paths: HashMap<(String, String), bool> = HashMap::new();
    let mut cursor = 0;
    loop {
        let page = graph.list_pending_refs(1000, cursor)?;
        sweep.scanned += page.refs.len();
        for stored in &page.refs {
            if is_unresolvable_import(graph, &stored.reference, &mut module_paths)? {
                failures.push(stored.id);
                continue;
            }
            if let Some(resolution) = resolve_import(graph, stored, &mut importing_paths)? {
                // The batch applies after the loop, so fold this sweep's fresh
                // import targets into the in-memory scope for bare names.
                if let Some(target_file) = resolution
                    .target_id
                    .strip_prefix('f')
                    .and_then(|digits| digits.parse::<u32>().ok())
                {
                    if !importing_files.contains_key(&stored.file_id) {
                        importing_files
                            .insert(stored.file_id, graph.imported_file_ids(stored.file_id)?);
                    }
                    let modules = importing_files
                        .get_mut(&stored.file_id)
                        .expect("import scope just ensured");
                    if !modules.contains(&target_file) {
                        modules.push(target_file);
                    }
                }
                resolutions.push(resolution);
                continue;
            }
            let hits = {
                let Some(raw) = graph.lookup_candidates(&stored.reference.name_tail)? else {
                    // Hidden candidates must not turn an ambiguous name into
                    // a seemingly unique match against an unfinished snapshot.
                    continue;
                };
                filter_compatible_hits(graph, stored, raw, &mut candidate_paths)?
            };
            if let Some(resolution) = resolve_receiver_local(stored, &hits) {
                resolutions.push(resolution);
                sweep.file_scoped += 1;
                continue;
            }
            if let Some(resolution) = resolve_qualified_name(stored, &hits) {
                resolutions.push(resolution);
                sweep.preferred_file += 1;
                continue;
            }
            if let [hit] = hits.as_slice() {
                if hit.file_id != stored.file_id {
                    resolutions.push(Resolution {
                        ref_id: stored.id,
                        target_id: hit.entity_id.clone(),
                        provenance: Provenance::WorkspaceUnique,
                    });
                }
            } else if hits.len() > 1 {
                if let Some(resolution) =
                    resolve_import_bound(graph, stored, &hits, &mut importing_files)?
                {
                    resolutions.push(resolution);
                    sweep.import_bound += 1;
                    continue;
                }
                if let Some(resolution) = resolve_export_bound(stored, &hits) {
                    resolutions.push(resolution);
                    sweep.export_bound += 1;
                    continue;
                }
                if let Some(resolution) = resolve_arity_bound(stored, &hits) {
                    resolutions.push(resolution);
                    sweep.arity_bound += 1;
                    continue;
                }
                candidate_sets.push((
                    stored.id,
                    hits.into_iter().map(|hit| hit.entity_id).collect(),
                ));
            }
        }
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = next;
    }
    let stats = graph.apply_resolutions(&resolutions)?;
    sweep.resolved = stats.resolved;
    sweep.stale = stats.stale;
    sweep.failed = graph.apply_failures(&failures)?;
    if !candidate_sets.is_empty() {
        sweep.candidates_recorded = graph.record_candidates(&candidate_sets)?;
    }
    Ok(sweep)
}

/// Drops name-index candidates whose file language cannot host the
/// reference. Every downstream rule — same-file, qualified, workspace
/// uniqueness, import/export/arity bounds, and the recorded candidate
/// sets — then reasons over same-language evidence only. The gate never
/// discards on missing information: unknown reference languages, missing
/// candidate paths, and unregistered extensions all keep their entries.
///
/// # Errors
/// Propagates graph storage failures.
fn filter_compatible_hits(
    graph: &dyn ResolutionSession,
    stored: &StoredPendingRef,
    hits: Vec<NameIndexEntry>,
    candidate_paths: &mut HashMap<u32, Option<String>>,
) -> crate::EngineResult<Vec<NameIndexEntry>> {
    let mut filtered = Vec::with_capacity(hits.len());
    for hit in hits {
        let path = match candidate_paths.entry(hit.file_id) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => entry.insert(graph.file_path(hit.file_id)?).clone(),
        };
        let keep = match (
            FileFormat::parse(&stored.reference.language),
            path.as_deref(),
        ) {
            (Some(reference), Some(path)) if !formats_for_path(path).is_empty() => {
                formats_for_path(path)
                    .iter()
                    .any(|candidate| reference_compatible_with(reference, *candidate))
            }
            _ => true,
        };
        if keep {
            filtered.push(hit);
        }
    }
    Ok(filtered)
}

/// Resolves a relative import specifier (`./x`, `../y`) onto the target
/// file's endpoint `f{file_id}`. Bare package names and alias roots stay
/// pending: they address modules outside the workspace snapshot.
///
/// # Errors
/// Propagates graph storage failures; an unmatched specifier is `None`.
fn resolve_import(
    graph: &dyn ResolutionSession,
    stored: &StoredPendingRef,
    importing_paths: &mut HashMap<u32, Option<String>>,
) -> crate::EngineResult<Option<Resolution>> {
    let reference = &stored.reference;
    if reference.reference_kind != EdgeKind::Imports
        || !(reference.reference_name.starts_with("./")
            || reference.reference_name.starts_with("../"))
    {
        return Ok(None);
    }
    let path = match importing_paths.entry(stored.file_id) {
        Entry::Occupied(entry) => entry.get().clone(),
        Entry::Vacant(entry) => entry.insert(graph.file_path(stored.file_id)?).clone(),
    };
    let Some(path) = path else {
        // A file without a persisted path cannot anchor a relative specifier.
        return Ok(None);
    };
    let Some(base) = join_specifier(importing_dir(&path), &reference.reference_name) else {
        return Ok(None);
    };
    for candidate in path_variants(&base, extensions_for(&reference.language)) {
        if let Some(file_id) = graph.file_id_by_path(&candidate)? {
            return Ok(Some(Resolution {
                ref_id: stored.id,
                target_id: format!("f{file_id}"),
                provenance: Provenance::ImportScoped,
            }));
        }
    }
    Ok(None)
}

/// Resolves an ambiguous bare-name reference through the file's imports:
/// when the imports pin the name to exactly one module and that module
/// declares it exactly once, the reference targets that definition.
/// Receiver-typed calls wait for receiver inference, and a local definition
/// shadows any import binding, so both stay with the general ambiguity path.
///
/// # Errors
/// Propagates graph storage failures; an unresolvable reference is `None`.
fn resolve_import_bound(
    graph: &dyn ResolutionSession,
    stored: &StoredPendingRef,
    hits: &[NameIndexEntry],
    importing_files: &mut HashMap<u32, Vec<u32>>,
) -> crate::EngineResult<Option<Resolution>> {
    let reference = &stored.reference;
    if reference.receiver_name.is_some()
        || !matches!(
            reference.reference_kind,
            EdgeKind::Calls | EdgeKind::Extends | EdgeKind::Implements
        )
        || hits.iter().any(|hit| hit.file_id == stored.file_id)
    {
        return Ok(None);
    }
    let modules = match importing_files.entry(stored.file_id) {
        Entry::Occupied(entry) => entry.get().clone(),
        Entry::Vacant(entry) => entry
            .insert(graph.imported_file_ids(stored.file_id)?)
            .clone(),
    };
    let scoped: Vec<&NameIndexEntry> = hits
        .iter()
        .filter(|hit| modules.contains(&hit.file_id))
        .collect();
    if let [hit] = scoped.as_slice() {
        return Ok(Some(Resolution {
            ref_id: stored.id,
            target_id: hit.entity_id.clone(),
            provenance: Provenance::ImportBound,
        }));
    }
    Ok(None)
}

/// Binds a `self.`/`this.` receiver call to the one entity declaring the
/// invoked member in the same file: the receiver names the enclosing
/// instance, so a unique same-file declaration is that member's definition.
/// Members inherited from base classes and members declared more than once
/// in the file stay pending.
fn resolve_receiver_local(
    stored: &StoredPendingRef,
    hits: &[NameIndexEntry],
) -> Option<Resolution> {
    if !matches!(
        stored.reference.receiver_name.as_deref(),
        Some("self" | "this")
    ) {
        return None;
    }
    let local: Vec<&NameIndexEntry> = hits
        .iter()
        .filter(|hit| hit.file_id == stored.file_id)
        .collect();
    if let [hit] = local.as_slice() {
        return Some(Resolution {
            ref_id: stored.id,
            target_id: hit.entity_id.clone(),
            provenance: Provenance::FileScoped,
        });
    }
    None
}

/// Binds a qualified call (`Class::method()`, including receivers spelled as
/// full `crate::path::Class` paths reduced to their last `::` segment) to the
/// entity whose `qualified_name` spells exactly that scope and member. The
/// qualified name carries the declaring scope, so a unique match resolves
/// even when the short member name is ambiguous workspace-wide. Duplicate
/// `qualified_name` declarations and instance-style dotted receivers, which
/// never spell a `::`-scoped path, stay pending.
fn resolve_qualified_name(
    stored: &StoredPendingRef,
    hits: &[NameIndexEntry],
) -> Option<Resolution> {
    let reference = &stored.reference;
    if reference.reference_kind != EdgeKind::Calls {
        return None;
    }
    let receiver = reference.receiver_name.as_deref()?;
    let scope = receiver.rsplit("::").next().unwrap_or(receiver);
    let expected = format!("{scope}::{}", reference.name_tail);
    let matches: Vec<&NameIndexEntry> = hits
        .iter()
        .filter(|hit| hit.qualified_name == expected)
        .collect();
    if let [hit] = matches.as_slice() {
        return Some(Resolution {
            ref_id: stored.id,
            target_id: hit.entity_id.clone(),
            provenance: Provenance::PreferredFile,
        });
    }
    None
}

/// Reports import references whose specifier can never address a workspace
/// module: TS/JS bare package specifiers (anything not starting with `./`
/// or `../`, reduced to its first path segment) and Python's bare top-level
/// module imports (`import re`). A specifier whose leading segment matches
/// a workspace file or directory — a monorepo package, a top-level module —
/// stays in the queue for path-based rules instead of failing.
///
/// Scoped specifiers (`@scope/pkg`) and path aliases (`@/x`) always stay
/// queued: a workspace's own package name or `paths` mapping can route them
/// inside the workspace while no file or directory spells the scope, so
/// existence cannot be proven either way. They are safe to hold — their
/// name tails contain `/`, which entity names never do, so the uniqueness
/// rule cannot mis-bind them.
///
/// # Errors
/// Propagates graph storage failures; every other reference is `false`.
fn is_unresolvable_import(
    graph: &dyn ResolutionSession,
    reference: &PendingRef,
    module_paths: &mut HashMap<(String, String), bool>,
) -> crate::EngineResult<bool> {
    if reference.reference_kind != EdgeKind::Imports {
        return Ok(false);
    }
    let specifier = reference.reference_name.as_str();
    let root = match reference.language.as_str() {
        // Angle-bracket includes address the standard library and `-I`
        // include paths, which a workspace snapshot can never spell — the
        // extraction layer only emits this form for `#include <x>`. Quoted
        // forms arrive normalized to `./x` and flow through the relative
        // path below.
        "c" | "cpp" if specifier.starts_with('<') => return Ok(true),
        "typescript" | "javascript"
            if !specifier.starts_with("./") && !specifier.starts_with("../") =>
        {
            if specifier.starts_with('@') {
                return Ok(false);
            }
            specifier
                .split('/')
                .find(|segment| !segment.is_empty())
                .unwrap_or(specifier)
        }
        "python" if !specifier.starts_with('.') && !specifier.contains('.') => specifier,
        _ => return Ok(false),
    };
    let key = (reference.language.clone(), root.to_owned());
    let exists = match module_paths.entry(key) {
        Entry::Occupied(entry) => *entry.get(),
        Entry::Vacant(entry) => {
            let exists = graph.path_has_module(root, extensions_for(&reference.language))?;
            entry.insert(exists);
            exists
        }
    };
    Ok(!exists)
}

/// Narrows an ambiguous TS/JS reference to the one workspace-wide exported
/// declaration: cross-file lookups only reach exported symbols, so when
/// every candidate is declared in another file and exactly one is exported,
/// the rest are unreachable shadows. A same-file candidate (free local
/// access) or several exported declarations keeps the ambiguity path.
fn resolve_export_bound(stored: &StoredPendingRef, hits: &[NameIndexEntry]) -> Option<Resolution> {
    if !matches!(
        stored.reference.language.as_str(),
        "typescript" | "javascript"
    ) || !matches!(
        stored.reference.reference_kind,
        EdgeKind::Calls | EdgeKind::Extends | EdgeKind::Implements
    ) {
        return None;
    }
    let exported: Vec<&NameIndexEntry> = hits.iter().filter(|hit| hit.is_exported).collect();
    if exported.len() == 1 && hits.iter().all(|hit| hit.file_id != stored.file_id) {
        return Some(Resolution {
            ref_id: stored.id,
            target_id: exported[0].entity_id.clone(),
            provenance: Provenance::ExportBound,
        });
    }
    None
}

/// Narrows an ambiguous reference to the one candidate whose declared
/// parameter count equals the call's argument count: among candidates that
/// all declare their arity, a single match is the invoked overload.
/// Candidates with unknown arity cannot rule themselves out, so their
/// presence keeps the reference pending.
fn resolve_arity_bound(stored: &StoredPendingRef, hits: &[NameIndexEntry]) -> Option<Resolution> {
    let expected = stored.reference.arity?;
    let mut matched = None;
    for hit in hits {
        match hit.arity {
            Some(arity) if arity == expected => {
                if matched.is_some() {
                    return None;
                }
                matched = Some(hit);
            }
            Some(_) => {}
            None => return None,
        }
    }
    let hit = matched?;
    Some(Resolution {
        ref_id: stored.id,
        target_id: hit.entity_id.clone(),
        provenance: Provenance::ArityBound,
    })
}

/// Directory prefix of a forward-slash workspace path; the workspace root
/// itself is the empty string.
fn importing_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(index) => &path[..index],
        None => "",
    }
}

/// Joins a relative specifier onto the importing directory, folding `.` and
/// `..` segments. Returns `None` when the specifier names no file (`./`) or
/// the joined path escapes the root.
fn join_specifier(dir: &str, specifier: &str) -> Option<String> {
    let mut segments: Vec<&str> = Vec::new();
    for segment in dir.split('/').filter(|segment| !segment.is_empty()) {
        segments.push(segment);
    }
    let mut named = false;
    for segment in specifier.split('/').filter(|segment| !segment.is_empty()) {
        match segment {
            "." => continue,
            ".." => {
                segments.pop()?;
            }
            _ => {
                segments.push(segment);
                named = true;
            }
        }
    }
    if !named {
        return None;
    }
    Some(segments.join("/")).filter(|joined| !joined.is_empty())
}

/// Path candidates for one specifier: the literal base first (explicit
/// extensions win), then per-language extensions, then directory index files.
fn path_variants(base: &str, extensions: &[&str]) -> Vec<String> {
    let mut variants = vec![base.to_owned()];
    variants.extend(
        extensions
            .iter()
            .map(|extension| format!("{base}{extension}")),
    );
    variants.extend(
        extensions
            .iter()
            .map(|extension| format!("{base}/index{extension}")),
    );
    variants
}

/// Extension preference per reference language, mirroring the TypeScript
/// extension table: source order over compiled siblings.
fn extensions_for(language: &str) -> &'static [&'static str] {
    match language {
        "typescript" => &[".ts", ".tsx", ".js", ".jsx"],
        "javascript" => &[".js", ".jsx"],
        "python" => &[".py"],
        "rust" => &[".rs"],
        "go" => &[".go"],
        "java" => &[".java"],
        "c" => &[".c", ".h"],
        "cpp" => &[".cpp", ".cc", ".cxx", ".hpp", ".h"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::graph::{EntityRecord, FileGraph, SqliteGraphStorage};

    // Algorithm fixtures use only the restricted interface. Canonical visibility
    // and writeback validation are exercised by IndexStore session tests.
    impl ResolutionSession for SqliteGraphStorage {
        fn list_pending_refs(
            &mut self,
            limit: usize,
            cursor: i64,
        ) -> crate::EngineResult<crate::storage::graph::PendingRefPage> {
            SqliteGraphStorage::list_pending_refs(self, limit, cursor)
                .map_err(|error| crate::EngineError::storage_failure(error.to_string()))
        }
        fn file_path(&self, file_id: u32) -> crate::EngineResult<Option<String>> {
            SqliteGraphStorage::file_path(self, file_id)
                .map_err(|error| crate::EngineError::storage_failure(error.to_string()))
        }
        fn file_id_by_path(&self, path: &str) -> crate::EngineResult<Option<u32>> {
            SqliteGraphStorage::file_id_by_path(self, path)
                .map_err(|error| crate::EngineError::storage_failure(error.to_string()))
        }
        fn path_has_module(&self, name: &str, extensions: &[&str]) -> crate::EngineResult<bool> {
            SqliteGraphStorage::path_has_module(self, name, extensions)
                .map_err(|error| crate::EngineError::storage_failure(error.to_string()))
        }
        fn imported_file_ids(&self, file_id: u32) -> crate::EngineResult<Vec<u32>> {
            SqliteGraphStorage::imported_file_ids(self, file_id)
                .map_err(|error| crate::EngineError::storage_failure(error.to_string()))
        }
        fn apply_resolutions(
            &mut self,
            resolutions: &[Resolution],
        ) -> crate::EngineResult<crate::storage::graph::ResolutionStats> {
            SqliteGraphStorage::apply_resolutions(self, resolutions)
                .map_err(|error| crate::EngineError::storage_failure(error.to_string()))
        }
        fn apply_failures(&mut self, ids: &[i64]) -> crate::EngineResult<usize> {
            SqliteGraphStorage::apply_failures(self, ids)
                .map_err(|error| crate::EngineError::storage_failure(error.to_string()))
        }
        fn record_candidates(
            &mut self,
            entries: &[(i64, Vec<String>)],
        ) -> crate::EngineResult<usize> {
            SqliteGraphStorage::record_candidates(self, entries)
                .map_err(|error| crate::EngineError::storage_failure(error.to_string()))
        }
        fn lookup_candidates(
            &self,
            name: &str,
        ) -> crate::EngineResult<Option<Vec<NameIndexEntry>>> {
            self.lookup_by_name(name)
                .map(Some)
                .map_err(|error| crate::EngineError::storage_failure(error.to_string()))
        }
    }

    fn record(entity_id: &str, name: &str) -> EntityRecord {
        EntityRecord {
            entity_id: entity_id.to_owned(),
            name: name.to_owned(),
            qualified_name: name.to_owned(),
            is_exported: true,
            arity: None,
        }
    }

    fn empty_graph(entities: Vec<EntityRecord>) -> FileGraph {
        FileGraph {
            entity_ids: entities
                .iter()
                .map(|record| record.entity_id.clone())
                .collect(),
            entities,
            edges: Vec::new(),
            pending_refs: Vec::new(),
        }
    }

    fn pending_reference(
        from_node_id: &str,
        reference_name: &str,
        kind: crate::storage::graph::EdgeKind,
        language: &str,
    ) -> crate::storage::graph::PendingRef {
        crate::storage::graph::PendingRef {
            from_node_id: from_node_id.to_owned(),
            reference_name: reference_name.to_owned(),
            receiver_name: None,
            reference_kind: kind,
            arity: None,
            line: 1,
            col: 0,
            metadata: serde_json::Map::new(),
            candidates: None,
            language: language.to_owned(),
            name_tail: reference_name
                .rsplit("::")
                .next()
                .unwrap_or_default()
                .to_owned(),
        }
    }

    fn graph_with_pending(entities: Vec<EntityRecord>, reference_name: &str) -> FileGraph {
        FileGraph {
            pending_refs: vec![pending_reference(
                "f9",
                reference_name,
                crate::storage::graph::EdgeKind::Calls,
                "typescript",
            )],
            ..empty_graph(entities)
        }
    }

    fn graph_with_import(from_node_id: &str, specifier: &str) -> FileGraph {
        FileGraph {
            pending_refs: vec![pending_reference(
                from_node_id,
                specifier,
                crate::storage::graph::EdgeKind::Imports,
                "typescript",
            )],
            ..empty_graph(Vec::new())
        }
    }

    fn graph_with_import_and_call(specifier: &str, call: &str) -> FileGraph {
        FileGraph {
            pending_refs: vec![
                pending_reference(
                    "f9",
                    specifier,
                    crate::storage::graph::EdgeKind::Imports,
                    "typescript",
                ),
                pending_reference(
                    "f9",
                    call,
                    crate::storage::graph::EdgeKind::Calls,
                    "typescript",
                ),
            ],
            ..empty_graph(Vec::new())
        }
    }

    fn open_memory() -> SqliteGraphStorage {
        SqliteGraphStorage::in_memory().expect("graph opens")
    }

    #[test]
    fn resolves_cross_file_unique_names() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "lib/helper.ts",
                &empty_graph(vec![record("entity-helper", "helper")]),
                &[],
            )
            .expect("definition write");
        graph
            .write_file_graph(9, "run.ts", &graph_with_pending(Vec::new(), "helper"), &[])
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(
            sweep,
            ResolutionSweep {
                scanned: 1,
                resolved: 1,
                stale: 0,
                import_bound: 0,
                file_scoped: 0,
                preferred_file: 0,
                export_bound: 0,
                arity_bound: 0,
                failed: 0,
                candidates_recorded: 0,
            }
        );
        assert!(
            graph
                .list_pending_refs(10, 0)
                .expect("pending page")
                .refs
                .is_empty()
        );
        let inbound = graph
            .neighborhood(
                "entity-helper",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Calls]),
            )
            .expect("inbound");
        assert_eq!(inbound.len(), 1);
        assert_eq!(inbound[0].source, "f9");
        assert_eq!(inbound[0].provenance, Provenance::WorkspaceUnique);
    }

    #[test]
    fn cross_language_unique_names_stay_pending() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "kernel/decode.ts",
                &empty_graph(vec![record("entity-str", "str")]),
                &[],
            )
            .expect("definition write");
        graph
            .write_file_graph(
                9,
                "run.py",
                &FileGraph {
                    pending_refs: vec![pending_reference(
                        "f9",
                        "str",
                        crate::storage::graph::EdgeKind::Calls,
                        "python",
                    )],
                    ..empty_graph(Vec::new())
                },
                &[],
            )
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        // The python builtin `str` matches exactly one entity, but it lives
        // in a TypeScript file: uniqueness across languages is coincidence,
        // not resolution.
        assert_eq!(sweep.resolved, 0);
        let page = graph.list_pending_refs(10, 0).expect("pending page");
        assert_eq!(page.refs.len(), 1);
    }

    #[test]
    fn same_language_unique_names_still_bind() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "lib/config.py",
                &empty_graph(vec![record("entity-parse", "parse_config")]),
                &[],
            )
            .expect("definition write");
        graph
            .write_file_graph(
                9,
                "run.py",
                &FileGraph {
                    pending_refs: vec![pending_reference(
                        "f9",
                        "parse_config",
                        crate::storage::graph::EdgeKind::Calls,
                        "python",
                    )],
                    ..empty_graph(Vec::new())
                },
                &[],
            )
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.resolved, 1);
        let inbound = graph
            .neighborhood(
                "entity-parse",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Calls]),
            )
            .expect("inbound");
        assert_eq!(inbound[0].provenance, Provenance::WorkspaceUnique);
    }

    #[test]
    fn cross_language_export_evidence_stays_pending() {
        let mut graph = open_memory();
        // Two same-named rust entities — exactly one exported — would hand
        // the export-bound rule a unique target if the gate let them in.
        let mut unexported = record("entity-set-inner", "set");
        unexported.is_exported = false;
        graph
            .write_file_graph(
                1,
                "kernel/buffers.rs",
                &empty_graph(vec![record("entity-set", "set"), unexported]),
                &[],
            )
            .expect("definition write");
        graph
            .write_file_graph(9, "run.ts", &graph_with_pending(Vec::new(), "set"), &[])
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.export_bound, 0);
        assert_eq!(sweep.resolved, 0);
        let page = graph.list_pending_refs(10, 0).expect("pending page");
        assert_eq!(page.refs.len(), 1);
    }

    #[test]
    fn language_bridges_still_bind_across_c_headers_and_ts_js() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "lib/parse.cpp",
                &empty_graph(vec![record("entity-token", "token")]),
                &[],
            )
            .expect("cpp write");
        graph
            .write_file_graph(
                9,
                "scan.c",
                &FileGraph {
                    pending_refs: vec![pending_reference(
                        "f9",
                        "token",
                        crate::storage::graph::EdgeKind::Calls,
                        "c",
                    )],
                    ..empty_graph(Vec::new())
                },
                &[],
            )
            .expect("c write");
        graph
            .write_file_graph(
                2,
                "lib/legacy.js",
                &empty_graph(vec![record("entity-legacy", "legacy")]),
                &[],
            )
            .expect("js write");
        graph
            .write_file_graph(
                10,
                "app.ts",
                &FileGraph {
                    pending_refs: vec![pending_reference(
                        "f10",
                        "legacy",
                        crate::storage::graph::EdgeKind::Calls,
                        "typescript",
                    )],
                    ..empty_graph(Vec::new())
                },
                &[],
            )
            .expect("ts write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        // C references bind through C++ definitions and TypeScript through
        // JavaScript sources: the registered bridges still resolve.
        assert_eq!(sweep.resolved, 2);
    }

    #[test]
    fn ambiguous_names_stay_pending_and_record_candidates() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "first.ts",
                &empty_graph(vec![record("first", "helper")]),
                &[],
            )
            .expect("first write");
        graph
            .write_file_graph(
                2,
                "second.ts",
                &empty_graph(vec![record("second", "helper")]),
                &[],
            )
            .expect("second write");
        graph
            .write_file_graph(9, "run.ts", &graph_with_pending(Vec::new(), "helper"), &[])
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.scanned, 1);
        assert_eq!(sweep.resolved, 0);
        assert_eq!(sweep.candidates_recorded, 1);
        let page = graph.list_pending_refs(10, 0).expect("pending page");
        let [stored] = page.refs.as_slice() else {
            panic!("expected one pending ref");
        };
        assert_eq!(
            stored.reference.candidates,
            Some(vec!["first".to_owned(), "second".to_owned()])
        );
    }

    #[test]
    fn same_file_hits_stay_pending() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                9,
                "run.ts",
                &graph_with_pending(vec![record("local", "helper")], "helper"),
                &[],
            )
            .expect("write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.scanned, 1);
        assert_eq!(sweep.resolved, 0);
        assert_eq!(sweep.candidates_recorded, 0);
        assert_eq!(
            graph
                .list_pending_refs(10, 0)
                .expect("pending page")
                .refs
                .len(),
            1
        );
    }

    #[test]
    fn ambiguous_bare_names_resolve_through_imported_modules() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "first.ts",
                &empty_graph(vec![record("first", "helper")]),
                &[],
            )
            .expect("first write");
        graph
            .write_file_graph(
                2,
                "second.ts",
                &empty_graph(vec![record("second", "helper")]),
                &[],
            )
            .expect("second write");
        graph
            .write_file_graph(
                9,
                "run.ts",
                &graph_with_import_and_call("./first", "helper"),
                &[],
            )
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.scanned, 2);
        assert_eq!(sweep.resolved, 2);
        assert_eq!(sweep.stale, 0);
        assert_eq!(sweep.import_bound, 1);
        assert_eq!(sweep.candidates_recorded, 0);
        assert!(
            graph
                .list_pending_refs(10, 0)
                .expect("pending page")
                .refs
                .is_empty()
        );
        let inbound = graph
            .neighborhood(
                "first",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Calls]),
            )
            .expect("inbound calls");
        assert_eq!(inbound.len(), 1);
        assert_eq!(inbound[0].source, "f9");
        assert_eq!(inbound[0].provenance, Provenance::ImportBound);
    }

    #[test]
    fn import_bound_requires_one_declaring_module() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "first.ts",
                &empty_graph(vec![record("first", "helper")]),
                &[],
            )
            .expect("first write");
        graph
            .write_file_graph(
                2,
                "second.ts",
                &empty_graph(vec![record("second", "helper")]),
                &[],
            )
            .expect("second write");
        graph
            .write_file_graph(
                9,
                "run.ts",
                &FileGraph {
                    pending_refs: vec![
                        pending_reference(
                            "f9",
                            "./first",
                            crate::storage::graph::EdgeKind::Imports,
                            "typescript",
                        ),
                        pending_reference(
                            "f9",
                            "./second",
                            crate::storage::graph::EdgeKind::Imports,
                            "typescript",
                        ),
                        pending_reference(
                            "f9",
                            "helper",
                            crate::storage::graph::EdgeKind::Calls,
                            "typescript",
                        ),
                    ],
                    ..empty_graph(Vec::new())
                },
                &[],
            )
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.resolved, 2);
        assert_eq!(sweep.import_bound, 0);
        assert_eq!(sweep.candidates_recorded, 1);
        let page = graph.list_pending_refs(10, 0).expect("pending page");
        let [stored] = page.refs.as_slice() else {
            panic!("expected one pending ref");
        };
        assert_eq!(
            stored.reference.candidates,
            Some(vec!["first".to_owned(), "second".to_owned()])
        );
    }

    #[test]
    fn local_definitions_shadow_imported_names() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "first.ts",
                &empty_graph(vec![record("first", "helper")]),
                &[],
            )
            .expect("first write");
        graph
            .write_file_graph(
                9,
                "run.ts",
                &FileGraph {
                    entity_ids: vec!["local".to_owned()],
                    entities: vec![record("local", "helper")],
                    ..graph_with_import_and_call("./first", "helper")
                },
                &[],
            )
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.import_bound, 0);
        let page = graph.list_pending_refs(10, 0).expect("pending page");
        let [stored] = page.refs.as_slice() else {
            panic!("expected the call to stay pending");
        };
        assert_eq!(stored.reference.name_tail, "helper");
    }

    #[test]
    fn receiver_calls_wait_for_receiver_inference() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "first.ts",
                &empty_graph(vec![record("first", "helper")]),
                &[],
            )
            .expect("first write");
        graph
            .write_file_graph(
                2,
                "second.ts",
                &empty_graph(vec![record("second", "helper")]),
                &[],
            )
            .expect("second write");
        let mut receiver_call = graph_with_import_and_call("./first", "helper");
        receiver_call.pending_refs[1].receiver_name = Some("utils".to_owned());
        graph
            .write_file_graph(9, "run.ts", &receiver_call, &[])
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.import_bound, 0);
        let page = graph.list_pending_refs(10, 0).expect("pending page");
        let [stored] = page.refs.as_slice() else {
            panic!("expected the receiver call to stay pending");
        };
        assert_eq!(stored.reference.receiver_name, Some("utils".to_owned()));
        assert_eq!(
            stored.reference.candidates,
            Some(vec!["first".to_owned(), "second".to_owned()])
        );
    }

    fn graph_with_self_call(member: &str, language: &str) -> FileGraph {
        let mut graph = empty_graph(Vec::new());
        let mut reference = pending_reference(
            "f9",
            member,
            crate::storage::graph::EdgeKind::Calls,
            language,
        );
        reference.receiver_name = Some("self".to_owned());
        graph.pending_refs.push(reference);
        graph
    }

    #[test]
    fn self_receiver_binds_to_unique_local_member() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                2,
                "sibling.py",
                &empty_graph(vec![record("other-run", "run")]),
                &[],
            )
            .expect("sibling write");
        let mut caller = empty_graph(vec![record("run-method", "run")]);
        caller.pending_refs = graph_with_self_call("run", "python").pending_refs;
        graph
            .write_file_graph(9, "counter.py", &caller, &[])
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.scanned, 1);
        assert_eq!(sweep.resolved, 1);
        assert_eq!(sweep.file_scoped, 1);
        assert_eq!(sweep.candidates_recorded, 0);
        assert!(
            graph
                .list_pending_refs(10, 0)
                .expect("pending page")
                .refs
                .is_empty()
        );
        let inbound = graph
            .neighborhood(
                "run-method",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Calls]),
            )
            .expect("inbound calls");
        assert_eq!(inbound.len(), 1);
        assert_eq!(inbound[0].source, "f9");
        assert_eq!(inbound[0].provenance, Provenance::FileScoped);
    }

    #[test]
    fn this_receiver_binds_to_unique_local_member() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                2,
                "sibling.ts",
                &empty_graph(vec![record("other-step", "step")]),
                &[],
            )
            .expect("sibling write");
        let mut caller = empty_graph(vec![record("step-method", "step")]);
        let mut reference = pending_reference(
            "f9",
            "step",
            crate::storage::graph::EdgeKind::Calls,
            "typescript",
        );
        reference.receiver_name = Some("this".to_owned());
        caller.pending_refs.push(reference);
        graph
            .write_file_graph(9, "counter.ts", &caller, &[])
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.resolved, 1);
        assert_eq!(sweep.file_scoped, 1);
        let inbound = graph
            .neighborhood(
                "step-method",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Calls]),
            )
            .expect("inbound calls");
        assert_eq!(inbound.len(), 1);
        assert_eq!(inbound[0].provenance, Provenance::FileScoped);
    }

    #[test]
    fn same_file_duplicate_members_stay_pending() {
        let mut graph = open_memory();
        let mut caller = empty_graph(vec![
            record("counter-run", "run"),
            record("gauge-run", "run"),
        ]);
        caller.pending_refs = graph_with_self_call("run", "python").pending_refs;
        graph
            .write_file_graph(9, "counter.py", &caller, &[])
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.resolved, 0);
        assert_eq!(sweep.file_scoped, 0);
        assert_eq!(sweep.candidates_recorded, 1);
        let page = graph.list_pending_refs(10, 0).expect("pending page");
        let [stored] = page.refs.as_slice() else {
            panic!("expected one pending ref");
        };
        let mut candidates = stored.reference.candidates.clone().unwrap_or_default();
        candidates.sort();
        assert_eq!(
            candidates,
            vec!["counter-run".to_owned(), "gauge-run".to_owned()]
        );
    }

    #[test]
    fn inherited_member_without_local_declaration_stays_pending() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "base.py",
                &empty_graph(vec![record("base-step", "step")]),
                &[],
            )
            .expect("base write");
        graph
            .write_file_graph(
                2,
                "mixin.py",
                &empty_graph(vec![record("mixin-step", "step")]),
                &[],
            )
            .expect("mixin write");
        graph
            .write_file_graph(
                9,
                "counter.py",
                &graph_with_self_call("step", "python"),
                &[],
            )
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.resolved, 0);
        assert_eq!(sweep.file_scoped, 0);
        assert_eq!(sweep.candidates_recorded, 1);
        let page = graph.list_pending_refs(10, 0).expect("pending page");
        let [stored] = page.refs.as_slice() else {
            panic!("expected the self call to stay pending");
        };
        assert_eq!(stored.reference.receiver_name, Some("self".to_owned()));
    }

    fn qualified_record(entity_id: &str, qualified_name: &str) -> EntityRecord {
        EntityRecord {
            entity_id: entity_id.to_owned(),
            name: qualified_name
                .rsplit("::")
                .next()
                .unwrap_or_default()
                .to_owned(),
            qualified_name: qualified_name.to_owned(),
            is_exported: true,
            arity: None,
        }
    }

    fn graph_with_qualified_call(receiver: &str, member: &str) -> FileGraph {
        let mut graph = empty_graph(Vec::new());
        let mut reference =
            pending_reference("f9", member, crate::storage::graph::EdgeKind::Calls, "rust");
        reference.receiver_name = Some(receiver.to_owned());
        graph.pending_refs.push(reference);
        graph
    }

    #[test]
    fn qualified_static_calls_bind_through_qualified_name() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "tables.rs",
                &empty_graph(vec![qualified_record("tables-default", "Tables::default")]),
                &[],
            )
            .expect("tables write");
        graph
            .write_file_graph(
                2,
                "gauges.rs",
                &empty_graph(vec![qualified_record("gauges-default", "Gauges::default")]),
                &[],
            )
            .expect("gauges write");
        graph
            .write_file_graph(
                9,
                "run.rs",
                &graph_with_qualified_call("Tables", "default"),
                &[],
            )
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.scanned, 1);
        assert_eq!(sweep.resolved, 1);
        assert_eq!(sweep.preferred_file, 1);
        assert_eq!(sweep.candidates_recorded, 0);
        assert!(
            graph
                .list_pending_refs(10, 0)
                .expect("pending page")
                .refs
                .is_empty()
        );
        let inbound = graph
            .neighborhood(
                "tables-default",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Calls]),
            )
            .expect("inbound calls");
        assert_eq!(inbound.len(), 1);
        assert_eq!(inbound[0].source, "f9");
        assert_eq!(inbound[0].provenance, Provenance::PreferredFile);
    }

    #[test]
    fn crate_path_receivers_match_the_last_segment() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "tables.rs",
                &empty_graph(vec![qualified_record("tables-default", "Tables::default")]),
                &[],
            )
            .expect("tables write");
        graph
            .write_file_graph(
                2,
                "gauges.rs",
                &empty_graph(vec![qualified_record("gauges-default", "Gauges::default")]),
                &[],
            )
            .expect("gauges write");
        graph
            .write_file_graph(
                9,
                "run.rs",
                &graph_with_qualified_call("crate::kernel::Tables", "default"),
                &[],
            )
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.resolved, 1);
        assert_eq!(sweep.preferred_file, 1);
        let inbound = graph
            .neighborhood(
                "tables-default",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Calls]),
            )
            .expect("inbound calls");
        assert_eq!(inbound.len(), 1);
        assert_eq!(inbound[0].provenance, Provenance::PreferredFile);
    }

    #[test]
    fn duplicate_qualified_names_stay_pending() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "first.rs",
                &empty_graph(vec![qualified_record("first-default", "Tables::default")]),
                &[],
            )
            .expect("first write");
        graph
            .write_file_graph(
                2,
                "second.rs",
                &empty_graph(vec![qualified_record("second-default", "Tables::default")]),
                &[],
            )
            .expect("second write");
        graph
            .write_file_graph(
                9,
                "run.rs",
                &graph_with_qualified_call("Tables", "default"),
                &[],
            )
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.resolved, 0);
        assert_eq!(sweep.preferred_file, 0);
        assert_eq!(sweep.candidates_recorded, 1);
        let page = graph.list_pending_refs(10, 0).expect("pending page");
        let [stored] = page.refs.as_slice() else {
            panic!("expected one pending ref");
        };
        let mut candidates = stored.reference.candidates.clone().unwrap_or_default();
        candidates.sort();
        assert_eq!(
            candidates,
            vec!["first-default".to_owned(), "second-default".to_owned()]
        );
    }

    #[test]
    fn dotted_instance_receivers_stay_pending() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "tables.ts",
                &empty_graph(vec![qualified_record("tables-default", "Tables::default")]),
                &[],
            )
            .expect("tables write");
        graph
            .write_file_graph(
                2,
                "gauges.ts",
                &empty_graph(vec![qualified_record("gauges-default", "Gauges::default")]),
                &[],
            )
            .expect("gauges write");
        let mut graph_with_call = graph_with_qualified_call("service.queue", "default");
        graph_with_call.pending_refs[0].language = "typescript".to_owned();
        graph
            .write_file_graph(9, "run.ts", &graph_with_call, &[])
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.resolved, 0);
        assert_eq!(sweep.preferred_file, 0);
        assert_eq!(sweep.candidates_recorded, 1);
        let page = graph.list_pending_refs(10, 0).expect("pending page");
        let [stored] = page.refs.as_slice() else {
            panic!("expected the dotted call to stay pending");
        };
        assert_eq!(
            stored.reference.receiver_name,
            Some("service.queue".to_owned())
        );
    }

    fn indexed_record(entity_id: &str, name: &str, is_exported: bool, arity: u32) -> EntityRecord {
        EntityRecord {
            entity_id: entity_id.to_owned(),
            name: name.to_owned(),
            qualified_name: format!("scope::{name}"),
            is_exported,
            arity: (arity > 0).then_some(arity),
        }
    }

    #[test]
    fn external_imports_terminate_as_failed() {
        let mut graph = open_memory();
        let mut references = graph_with_import("f9", "fs");
        references.pending_refs.push(pending_reference(
            "f9",
            "./util",
            crate::storage::graph::EdgeKind::Imports,
            "typescript",
        ));
        graph
            .write_file_graph(9, "run.ts", &references, &[])
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.failed, 1);
        assert_eq!(sweep.resolved, 0);
        let page = graph.list_pending_refs(10, 0).expect("pending page");
        let [stored] = page.refs.as_slice() else {
            panic!("the relative import stays pending");
        };
        assert_eq!(stored.reference.reference_name, "./util");
    }

    #[test]
    fn scoped_package_imports_stay_pending() {
        let mut graph = open_memory();
        let mut references = graph_with_import("f9", "@zvec/zvec");
        references.pending_refs.push(pending_reference(
            "f9",
            "@/utils",
            crate::storage::graph::EdgeKind::Imports,
            "typescript",
        ));
        graph
            .write_file_graph(9, "run.ts", &references, &[])
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        // A scoped specifier may address the workspace's own package (its
        // package.json name) or a `paths` alias, so existence is unprovable
        // either way and the refs hold instead of failing.
        assert_eq!(sweep.failed, 0);
        assert_eq!(sweep.resolved, 0);
        let page = graph.list_pending_refs(10, 0).expect("pending page");
        assert_eq!(page.refs.len(), 2);
        assert!(
            page.refs
                .iter()
                .all(|stored| stored.reference.reference_name.starts_with('@'))
        );
    }

    #[test]
    fn system_includes_fail_while_quoted_ones_flow_through_relative_paths() {
        let mut graph = open_memory();
        let mut references = graph_with_import("f9", "<string>");
        references.pending_refs[0].language = "cpp".to_owned();
        references.pending_refs.push(pending_reference(
            "f9",
            "./alloc.h",
            crate::storage::graph::EdgeKind::Imports,
            "cpp",
        ));
        graph
            .write_file_graph(9, "sample.cpp", &references, &[])
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        // The angle-bracket include terminates as external; the quoted
        // include keeps flowing through the relative-import rule (which
        // holds it — no sibling file exists).
        assert_eq!(sweep.failed, 1);
        assert_eq!(sweep.resolved, 0);
        let page = graph.list_pending_refs(10, 0).expect("pending page");
        let [stored] = page.refs.as_slice() else {
            panic!("only the quoted include stays pending");
        };
        assert_eq!(stored.reference.reference_name, "./alloc.h");
    }

    #[test]
    fn bare_imports_matching_workspace_modules_stay_pending() {
        let mut graph = open_memory();
        graph
            .write_file_graph(1, "graphify/mod.py", &empty_graph(Vec::new()), &[])
            .expect("module write");
        let mut reference = graph_with_import("f9", "graphify");
        reference.pending_refs[0].language = "python".to_owned();
        graph
            .write_file_graph(9, "run.py", &reference, &[])
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.failed, 0);
        assert_eq!(sweep.resolved, 0);
        assert_eq!(
            graph
                .list_pending_refs(10, 0)
                .expect("pending page")
                .refs
                .len(),
            1
        );
    }

    #[test]
    fn export_evidence_narrows_cross_file_ambiguity() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "first.ts",
                &empty_graph(vec![indexed_record("first", "helper", true, 0)]),
                &[],
            )
            .expect("first write");
        graph
            .write_file_graph(
                2,
                "second.ts",
                &empty_graph(vec![indexed_record("second", "helper", false, 0)]),
                &[],
            )
            .expect("second write");
        graph
            .write_file_graph(9, "run.ts", &graph_with_pending(Vec::new(), "helper"), &[])
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.export_bound, 1);
        let inbound = graph
            .neighborhood(
                "first",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Calls]),
            )
            .expect("inbound");
        assert_eq!(inbound[0].provenance, Provenance::ExportBound);
    }

    #[test]
    fn export_evidence_waits_for_unshadowed_candidates() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "first.ts",
                &empty_graph(vec![indexed_record("first", "helper", true, 0)]),
                &[],
            )
            .expect("first write");
        graph
            .write_file_graph(
                2,
                "second.ts",
                &empty_graph(vec![indexed_record("second", "helper", true, 0)]),
                &[],
            )
            .expect("second write");
        graph
            .write_file_graph(9, "run.ts", &graph_with_pending(Vec::new(), "helper"), &[])
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.export_bound, 0);
        assert_eq!(sweep.candidates_recorded, 1);
    }

    #[test]
    fn arity_evidence_narrows_fully_typed_candidates() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "unary.ts",
                &empty_graph(vec![indexed_record("unary", "handler", true, 1)]),
                &[],
            )
            .expect("unary write");
        graph
            .write_file_graph(
                2,
                "binary.ts",
                &empty_graph(vec![indexed_record("binary", "handler", true, 2)]),
                &[],
            )
            .expect("binary write");
        let mut reference = graph_with_pending(Vec::new(), "handler");
        reference.pending_refs[0].arity = Some(2);
        graph
            .write_file_graph(9, "run.ts", &reference, &[])
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.arity_bound, 1);
        let inbound = graph
            .neighborhood(
                "binary",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Calls]),
            )
            .expect("inbound");
        assert_eq!(inbound[0].provenance, Provenance::ArityBound);
    }

    #[test]
    fn arity_evidence_waits_for_untyped_candidates() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "typed.ts",
                &empty_graph(vec![indexed_record("typed", "handler", true, 2)]),
                &[],
            )
            .expect("typed write");
        graph
            .write_file_graph(
                2,
                "untyped.ts",
                &empty_graph(vec![EntityRecord {
                    entity_id: "untyped".to_owned(),
                    name: "handler".to_owned(),
                    qualified_name: "scope::handler".to_owned(),
                    is_exported: true,
                    arity: None,
                }]),
                &[],
            )
            .expect("untyped write");
        let mut reference = graph_with_pending(Vec::new(), "handler");
        reference.pending_refs[0].arity = Some(2);
        graph
            .write_file_graph(9, "run.ts", &reference, &[])
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.arity_bound, 0);
        assert_eq!(sweep.candidates_recorded, 1);
    }

    #[test]
    fn entity_targeted_imports_scope_bare_names() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "first.ts",
                &empty_graph(vec![record("first", "helper")]),
                &[],
            )
            .expect("first write");
        graph
            .write_file_graph(
                2,
                "second.ts",
                &empty_graph(vec![record("second", "helper")]),
                &[],
            )
            .expect("second write");
        graph
            .write_file_graph(
                9,
                "run.ts",
                &graph_with_import_and_call("first.helper", "helper"),
                &[],
            )
            .expect("reference write");
        // Name-unique import resolution targeted the entity, mirroring R1.
        let mut refs = graph.list_pending_refs(10, 0).expect("pending page").refs;
        let import = refs.remove(0);
        graph
            .apply_resolutions(&[Resolution {
                ref_id: import.id,
                target_id: "first".to_owned(),
                provenance: Provenance::WorkspaceUnique,
            }])
            .expect("entity-targeted import");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.resolved, 1);
        assert_eq!(sweep.import_bound, 1);
        let inbound = graph
            .neighborhood(
                "first",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Calls]),
            )
            .expect("inbound calls");
        assert_eq!(inbound.len(), 1);
        assert_eq!(inbound[0].provenance, Provenance::ImportBound);
    }

    #[test]
    fn import_bound_covers_extends_references() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "first.ts",
                &empty_graph(vec![record("first", "Base")]),
                &[],
            )
            .expect("first write");
        graph
            .write_file_graph(
                2,
                "second.ts",
                &empty_graph(vec![record("second", "Base")]),
                &[],
            )
            .expect("second write");
        let mut inheritance = graph_with_import_and_call("./first", "Base");
        inheritance.pending_refs[1].reference_kind = crate::storage::graph::EdgeKind::Extends;
        graph
            .write_file_graph(9, "run.ts", &inheritance, &[])
            .expect("reference write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.import_bound, 1);
        let inbound = graph
            .neighborhood(
                "first",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Extends]),
            )
            .expect("inbound extends");
        assert_eq!(inbound.len(), 1);
        assert_eq!(inbound[0].provenance, Provenance::ImportBound);
    }

    #[test]
    fn relative_imports_resolve_to_the_target_file() {
        let mut graph = open_memory();
        graph
            .write_file_graph(1, "lib/helper.ts", &empty_graph(vec![]), &[])
            .expect("definition write");
        graph
            .write_file_graph(9, "run.ts", &graph_with_import("f9", "./lib/helper"), &[])
            .expect("import write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.resolved, 1);
        assert!(
            graph
                .list_pending_refs(10, 0)
                .expect("pending page")
                .refs
                .is_empty()
        );
        let inbound = graph
            .neighborhood(
                "f1",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Imports]),
            )
            .expect("inbound imports");
        assert_eq!(inbound.len(), 1);
        assert_eq!(inbound[0].source, "f9");
        assert_eq!(inbound[0].target, "f1");
        assert_eq!(inbound[0].provenance, Provenance::ImportScoped);
    }

    #[test]
    fn imports_fall_through_extension_and_index_variants() {
        let mut graph = open_memory();
        graph
            .write_file_graph(4, "nested/utils/index.ts", &empty_graph(vec![]), &[])
            .expect("index file write");
        graph
            .write_file_graph(5, "values.ts", &empty_graph(vec![]), &[])
            .expect("values file write");
        graph
            .write_file_graph(
                9,
                "nested/run.ts",
                &FileGraph {
                    pending_refs: vec![
                        pending_reference(
                            "f9",
                            "./utils",
                            crate::storage::graph::EdgeKind::Imports,
                            "typescript",
                        ),
                        pending_reference(
                            "f9",
                            "../values",
                            crate::storage::graph::EdgeKind::Imports,
                            "typescript",
                        ),
                    ],
                    ..empty_graph(Vec::new())
                },
                &[],
            )
            .expect("import write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.resolved, 2);
        let resolved = graph
            .neighborhood("f9", crate::storage::graph::Direction::Out, None)
            .expect("outbound");
        let targets: Vec<_> = resolved
            .iter()
            .filter(|edge| edge.kind == crate::storage::graph::EdgeKind::Imports)
            .map(|edge| edge.target.as_str())
            .collect();
        assert_eq!(targets, ["f4", "f5"]);
    }

    #[test]
    fn bare_imports_fail_while_unmatched_relative_ones_stay_pending() {
        let mut graph = open_memory();
        graph
            .write_file_graph(9, "run.ts", &graph_with_import("f9", "react"), &[])
            .expect("bare write");
        graph
            .write_file_graph(
                10,
                "other.ts",
                &graph_with_import("f10", "./missing/module"),
                &[],
            )
            .expect("missing write");
        graph
            .write_file_graph(
                11,
                "deep/nested.ts",
                &graph_with_import("f11", "../../escape"),
                &[],
            )
            .expect("escape write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep.scanned, 3);
        assert_eq!(sweep.resolved, 0);
        assert_eq!(sweep.failed, 1);
        assert_eq!(
            graph
                .list_pending_refs(10, 0)
                .expect("pending page")
                .refs
                .len(),
            2
        );
    }

    #[test]
    fn an_empty_queue_is_a_noop() {
        let mut graph = open_memory();

        let sweep = resolve_pending(&mut graph).expect("sweep");

        assert_eq!(sweep, ResolutionSweep::default());
    }

    #[test]
    fn import_paths_join_and_fold_parent_segments() {
        assert_eq!(join_specifier("lib", "./helper"), Some("lib/helper".into()));
        assert_eq!(
            join_specifier("lib/nested", "../helper"),
            Some("lib/helper".into())
        );
        assert_eq!(join_specifier("", "./helper"), Some("helper".into()));
        assert_eq!(join_specifier("lib", "../escape"), Some("escape".into()));
        assert_eq!(join_specifier("lib", "../../escape"), None);
        assert_eq!(join_specifier("lib", "./"), None);
    }
}
