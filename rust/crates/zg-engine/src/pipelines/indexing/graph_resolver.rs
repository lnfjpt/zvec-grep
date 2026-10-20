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
    EdgeKind, ImportReference, NameIndexEntry, PendingRef, Provenance, Resolution, StoredPendingRef,
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
    let mut alias_bindings: HashMap<u32, Vec<AliasBinding>> = HashMap::new();
    let mut alias_targets: HashMap<(String, String), Option<u32>> = HashMap::new();
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
            if let Some(resolution) = resolve_alias_bound(
                graph,
                stored,
                &mut alias_bindings,
                &mut importing_paths,
                &mut alias_targets,
            )? {
                // An `as` alias never names an indexed symbol itself, so the
                // bare-name lookup below would only park the call; the alias
                // binding pins its imported name's module directly.
                resolutions.push(resolution);
                sweep.import_bound += 1;
                continue;
            }
            if let Some(resolution) = resolve_namespace_bound(
                graph,
                stored,
                &mut alias_bindings,
                &mut importing_paths,
                &mut alias_targets,
            )? {
                // The receiver names a module object, so the bare-name lookup
                // below would only see workspace-wide ambiguity; the binding
                // scopes the member to its one module.
                resolutions.push(resolution);
                sweep.import_bound += 1;
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
                } else if stored.reference.receiver_name.is_none() {
                    // Bare name, same file, single hit: the in-file symbol table
                    // skips declaration entities, so a pending bare call can only
                    // mean this one indexed location. Receiver-qualified calls
                    // still stay pending — a same-file receiver bind would be a
                    // type-inference guess.
                    resolutions.push(Resolution {
                        ref_id: stored.id,
                        target_id: hit.entity_id.clone(),
                        provenance: Provenance::FileScoped,
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

/// Resolves an import specifier onto the target file's endpoint
/// `f{file_id}`: TS/JS spell file-relative specifiers (`./x`, `../y`),
/// while Python spells dotted module paths that address the workspace root
/// (`from declare import x` addresses `declare.py` there). Bare package
/// names and alias roots stay pending: they address modules outside the
/// workspace snapshot.
///
/// # Errors
/// Propagates graph storage failures; an unmatched specifier is `None`.
fn resolve_import(
    graph: &dyn ResolutionSession,
    stored: &StoredPendingRef,
    importing_paths: &mut HashMap<u32, Option<String>>,
) -> crate::EngineResult<Option<Resolution>> {
    let reference = &stored.reference;
    if reference.reference_kind != EdgeKind::Imports {
        return Ok(None);
    }
    let module = match reference.language.as_str() {
        "python" if !reference.reference_name.starts_with('.') => {
            reference.reference_name.replace('.', "/")
        }
        _ if reference.reference_name.starts_with("./")
            || reference.reference_name.starts_with("../") =>
        {
            reference.reference_name.clone()
        }
        _ => return Ok(None),
    };
    let path = match importing_paths.entry(stored.file_id) {
        Entry::Occupied(entry) => entry.get().clone(),
        Entry::Vacant(entry) => entry.insert(graph.file_path(stored.file_id)?).clone(),
    };
    let Some(path) = path else {
        // A file without a persisted path cannot anchor a relative specifier.
        return Ok(None);
    };
    let base = match reference.language.as_str() {
        "python" => join_specifier("", &module),
        _ => join_specifier(importing_dir(&path), &module),
    };
    let Some(base) = base else {
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

/// One `as` alias binding parsed from an import statement's raw text.
#[derive(Clone, Debug, Eq, PartialEq)]
struct AliasBinding {
    /// Local name the importing file binds (`leftWork`, `left_work`).
    local: String,
    /// Name the import addresses inside the specifier's module (`work`),
    /// or `None` for module-namespace bindings (`import left as left_api`),
    /// which bare-name resolution cannot follow without a receiver type.
    imported: Option<String>,
    /// Module specifier the statement imports from.
    specifier: String,
}

/// Binds a bare call whose name is an `as` alias of an import: the alias
/// never names an indexed symbol itself, and the binding pins which module
/// the imported name addresses, so a unique same-module declaration of the
/// imported name is the call's target. Receiver-typed calls wait for
/// receiver inference and namespace bindings stay with the namespace rule;
/// specifiers addressing no snapshot module keep the general rules.
///
/// # Errors
/// Propagates graph storage failures; an unresolvable reference is `None`.
fn resolve_alias_bound(
    graph: &dyn ResolutionSession,
    stored: &StoredPendingRef,
    bindings: &mut HashMap<u32, Vec<AliasBinding>>,
    importing_paths: &mut HashMap<u32, Option<String>>,
    targets: &mut HashMap<(String, String), Option<u32>>,
) -> crate::EngineResult<Option<Resolution>> {
    let reference = &stored.reference;
    if reference.receiver_name.is_some() || reference.reference_kind != EdgeKind::Calls {
        return Ok(None);
    }
    let file_bindings = load_bindings(graph, stored, bindings)?;
    let matched: Vec<&AliasBinding> = file_bindings
        .iter()
        .filter(|binding| binding.imported.is_some() && binding.local == reference.name_tail)
        .collect();
    let [binding] = matched.as_slice() else {
        return Ok(None);
    };
    bind_in_module(
        graph,
        stored,
        binding.imported.as_deref().expect("imported binding"),
        binding,
        importing_paths,
        targets,
    )
}

/// Binds a call whose receiver names a module-namespace binding
/// (`import left as left_api`, then `left_api.work()`): the receiver is
/// the module object itself, so a unique declaration of the invoked member
/// inside that one module is the call's target. Namespace imports of no
/// snapshot module and members the module declares ambiguously keep the
/// reference with the general rules.
///
/// # Errors
/// Propagates graph storage failures; an unresolvable reference is `None`.
fn resolve_namespace_bound(
    graph: &dyn ResolutionSession,
    stored: &StoredPendingRef,
    bindings: &mut HashMap<u32, Vec<AliasBinding>>,
    importing_paths: &mut HashMap<u32, Option<String>>,
    targets: &mut HashMap<(String, String), Option<u32>>,
) -> crate::EngineResult<Option<Resolution>> {
    let reference = &stored.reference;
    let Some(receiver) = reference.receiver_name.as_deref() else {
        return Ok(None);
    };
    if reference.reference_kind != EdgeKind::Calls {
        return Ok(None);
    }
    let file_bindings = load_bindings(graph, stored, bindings)?;
    let matched: Vec<&AliasBinding> = file_bindings
        .iter()
        .filter(|binding| binding.imported.is_none() && binding.local == receiver)
        .collect();
    let [binding] = matched.as_slice() else {
        return Ok(None);
    };
    bind_in_module(
        graph,
        stored,
        &reference.name_tail,
        binding,
        importing_paths,
        targets,
    )
}

/// Cached alias bindings of the reference's own file.
///
/// # Errors
/// Propagates graph storage failures.
fn load_bindings(
    graph: &dyn ResolutionSession,
    stored: &StoredPendingRef,
    bindings: &mut HashMap<u32, Vec<AliasBinding>>,
) -> crate::EngineResult<Vec<AliasBinding>> {
    Ok(match bindings.entry(stored.file_id) {
        Entry::Occupied(entry) => entry.get().clone(),
        Entry::Vacant(entry) => entry
            .insert(load_alias_bindings(graph, stored)?)
            .clone(),
    })
}

/// Binds the addressed member to the one declaration inside the binding's
/// module: the import pins the module, so a unique same-module hit is the
/// target. A specifier addressing no snapshot module and members the
/// module declares ambiguously keep the reference with the general rules.
///
/// # Errors
/// Propagates graph storage failures; an unresolvable reference is `None`.
fn bind_in_module(
    graph: &dyn ResolutionSession,
    stored: &StoredPendingRef,
    imported: &str,
    binding: &AliasBinding,
    importing_paths: &mut HashMap<u32, Option<String>>,
    targets: &mut HashMap<(String, String), Option<u32>>,
) -> crate::EngineResult<Option<Resolution>> {
    let Some(target_file) = alias_target_file(graph, stored, binding, importing_paths, targets)?
    else {
        return Ok(None);
    };
    let Some(raw_hits) = graph.lookup_candidates(imported)? else {
        // Hidden candidates must not turn the binding into a seemingly
        // unique match against an unfinished snapshot.
        return Ok(None);
    };
    let scoped: Vec<&NameIndexEntry> = raw_hits
        .iter()
        .filter(|hit| hit.file_id == target_file)
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

/// Loads the file's import edges and parses their statement text into `as`
/// alias bindings. Only statements spelling an explicit `as` produce
/// bindings: unaliased imports keep flowing through the bare-name and
/// import-bound rules, which already resolve them.
///
/// # Errors
/// Propagates graph storage failures.
fn load_alias_bindings(
    graph: &dyn ResolutionSession,
    stored: &StoredPendingRef,
) -> crate::EngineResult<Vec<AliasBinding>> {
    let pairs: fn(&str) -> Vec<(String, Option<String>)> = match stored.reference.language.as_str()
    {
        "typescript" | "javascript" => ts_alias_pairs,
        "python" => python_alias_pairs,
        "rust" => rust_alias_pairs,
        _ => return Ok(Vec::new()),
    };
    let mut bindings = Vec::new();
    for reference in graph.import_references(stored.file_id)? {
        let Some(raw_text) = reference.raw_text.as_deref() else {
            continue;
        };
        for (local, imported) in pairs(raw_text) {
            bindings.push(AliasBinding {
                local,
                imported,
                specifier: reference.specifier.clone(),
            });
        }
    }
    Ok(bindings)
}

/// Parses an import statement into aliased `(local, imported)` pairs and
/// namespace bindings: `import { work as leftWork } from './left'` pairs a
/// local alias with the imported name, `import * as leftApi from './left'`
/// binds the module itself (`imported` stays `None`). Default imports and
/// unaliased names never spell a binding.
fn ts_alias_pairs(statement: &str) -> Vec<(String, Option<String>)> {
    let Some(clause_end) = statement.rfind(" from ") else {
        return Vec::new();
    };
    let clause = statement[..clause_end].trim();
    let Some(open) = clause.find('{') else {
        let Some(rest) = clause.strip_prefix("import ") else {
            return Vec::new();
        };
        let rest = rest.trim();
        let Some(as_index) = rest.find(" as ") else {
            return Vec::new();
        };
        if rest[..as_index].trim() != "*" {
            return Vec::new();
        }
        let local = rest[as_index + " as ".len()..].trim();
        if local.is_empty() {
            return Vec::new();
        }
        return vec![(local.to_owned(), None)];
    };
    let Some(close) = clause.rfind('}') else {
        return Vec::new();
    };
    if close < open {
        return Vec::new();
    }
    let mut pairs = Vec::new();
    for entry in clause[open + 1..close].split(',') {
        let entry = entry.trim();
        let Some(as_index) = entry.find(" as ") else {
            continue;
        };
        let imported = entry[..as_index].trim();
        let local = entry[as_index + " as ".len()..].trim();
        // `default as x` addresses the module's default export, which no
        // named lookup can reach.
        if imported.is_empty() || local.is_empty() || imported == "default" {
            continue;
        }
        pairs.push((local.to_owned(), Some(imported.to_owned())));
    }
    pairs
}

/// Parses `from declare import func as foobar` statements into aliased
/// `(local, imported)` pairs, and `import left as left_api` statements into
/// namespace bindings whose imported half stays `None`. Unaliased forms
/// never spell an `as`, so they stay unparsed.
fn python_alias_pairs(statement: &str) -> Vec<(String, Option<String>)> {
    let trimmed = statement.trim_start();
    // `from m import ...` spells statement-level names; a leading `import`
    // binds the module itself as a namespace.
    if !trimmed.starts_with("from ") {
        let Some(names) = trimmed.strip_prefix("import ") else {
            return Vec::new();
        };
        return python_namespace_pairs(names);
    }
    let Some(import_index) = statement.find(" import ") else {
        return Vec::new();
    };
    let names = statement[import_index + " import ".len()..].trim();
    let names = names
        .strip_prefix('(')
        .map(|stripped| stripped.strip_suffix(')').unwrap_or(stripped))
        .unwrap_or(names);
    let mut pairs = Vec::new();
    for entry in names.split(',') {
        let entry = entry.trim();
        let Some(as_index) = entry.find(" as ") else {
            continue;
        };
        let imported = entry[..as_index].trim();
        let local = entry[as_index + " as ".len()..].trim();
        if imported.is_empty() || local.is_empty() {
            continue;
        }
        pairs.push((local.to_owned(), Some(imported.to_owned())));
    }
    pairs
}

/// Parses comma-separated `module as alias` entries of a plain `import`
/// statement into namespace bindings.
fn python_namespace_pairs(names: &str) -> Vec<(String, Option<String>)> {
    let mut pairs = Vec::new();
    for entry in names.trim().trim_end_matches(';').split(',') {
        let entry = entry.trim();
        let Some(as_index) = entry.find(" as ") else {
            continue;
        };
        let imported = entry[..as_index].trim();
        let local = entry[as_index + " as ".len()..].trim();
        if imported.is_empty() || local.is_empty() {
            continue;
        }
        pairs.push((local.to_owned(), None));
    }
    pairs
}

/// Parses `use left::work as left_work;` statements into aliased
/// `(local, imported)` pairs. Brace groups (`use a::{b as c}`) need
/// per-item module paths the statement text alone cannot pin, so they
/// stay unparsed.
fn rust_alias_pairs(statement: &str) -> Vec<(String, Option<String>)> {
    if statement.contains('{') {
        return Vec::new();
    }
    let Some(as_index) = statement.find(" as ") else {
        return Vec::new();
    };
    let Some(path) = statement[..as_index].strip_prefix("use ") else {
        return Vec::new();
    };
    let path = path.trim();
    let local = statement[as_index + " as ".len()..]
        .trim()
        .trim_end_matches(';')
        .trim();
    let Some((module, imported)) = path.rsplit_once("::") else {
        return Vec::new();
    };
    if module.is_empty() || imported.is_empty() || local.is_empty() {
        return Vec::new();
    }
    vec![(local.to_owned(), Some(imported.to_owned()))]
}

/// Resolves an alias binding's module specifier onto the file it imports:
/// TS/JS specifiers are file-relative, Python dotted paths are root
/// relative, and Rust use paths name their module with the first segment
/// (a leading `crate::` addresses the workspace root). `None` keeps the
/// reference with the general rules.
///
/// # Errors
/// Propagates graph storage failures.
fn alias_target_file(
    graph: &dyn ResolutionSession,
    stored: &StoredPendingRef,
    binding: &AliasBinding,
    importing_paths: &mut HashMap<u32, Option<String>>,
    targets: &mut HashMap<(String, String), Option<u32>>,
) -> crate::EngineResult<Option<u32>> {
    let language = stored.reference.language.as_str();
    let resolved = match targets.entry((language.to_owned(), binding.specifier.clone())) {
        Entry::Occupied(entry) => *entry.get(),
        Entry::Vacant(entry) => entry
            .insert(alias_target_file_uncached(graph, stored, binding, importing_paths)?)
            .clone(),
    };
    Ok(resolved)
}

/// Uncached specifier resolution behind [`alias_target_file`].
///
/// # Errors
/// Propagates graph storage failures.
fn alias_target_file_uncached(
    graph: &dyn ResolutionSession,
    stored: &StoredPendingRef,
    binding: &AliasBinding,
    importing_paths: &mut HashMap<u32, Option<String>>,
) -> crate::EngineResult<Option<u32>> {
    let language = stored.reference.language.as_str();
    let module: String = match language {
        "typescript" | "javascript" => {
            if !binding.specifier.starts_with("./") && !binding.specifier.starts_with("../") {
                return Ok(None);
            }
            binding.specifier.clone()
        }
        "python" => {
            if binding.specifier.starts_with('.') {
                return Ok(None);
            }
            binding.specifier.replace('.', "/")
        }
        "rust" => {
            let crate_path = binding
                .specifier
                .strip_prefix("crate::")
                .unwrap_or(&binding.specifier);
            let Some(first) = crate_path.split("::").find(|segment| !segment.is_empty()) else {
                return Ok(None);
            };
            first.to_owned()
        }
        _ => return Ok(None),
    };
    let base = match language {
        "typescript" | "javascript" => {
            let path = match importing_paths.entry(stored.file_id) {
                Entry::Occupied(entry) => entry.get().clone(),
                Entry::Vacant(entry) => entry.insert(graph.file_path(stored.file_id)?).clone(),
            };
            let Some(path) = path else {
                return Ok(None);
            };
            match join_specifier(importing_dir(&path), &module) {
                Some(base) => base,
                None => return Ok(None),
            }
        }
        // Python and Rust specifiers address the workspace root.
        _ => module,
    };
    let mut variants = path_variants(&base, extensions_for(language));
    if language == "rust" {
        // Rust modules live in `<base>.rs` or `<base>/mod.rs`; the index
        // variant path_variants emits covers neither.
        variants.push(format!("{base}/mod.rs"));
    }
    for candidate in variants {
        if let Some(file_id) = graph.file_id_by_path(&candidate)? {
            return Ok(Some(file_id));
        }
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
    // The receiver may spell the full declaring path (`ns::Waldo::find()`) or
    // only its innermost segment (`Waldo::find()`); accept either spelling,
    // and bind only when exactly one entity answers.
    let scope = receiver.rsplit("::").next().unwrap_or(receiver);
    let expected_full = format!("{receiver}::{}", reference.name_tail);
    let expected_tail = format!("{scope}::{}", reference.name_tail);
    let matches: Vec<&NameIndexEntry> = hits
        .iter()
        .filter(|hit| hit.qualified_name == expected_full || hit.qualified_name == expected_tail)
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
        fn import_references(
            &self,
            file_id: u32,
        ) -> crate::EngineResult<Vec<crate::storage::graph::ImportReference>> {
            SqliteGraphStorage::import_references(self, file_id)
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
            is_declaration: false,
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

    fn graph_with_alias_import_and_call(
        specifier: &str,
        raw_text: &str,
        call: &str,
        language: &str,
    ) -> FileGraph {
        let mut import = pending_reference(
            "f9",
            specifier,
            crate::storage::graph::EdgeKind::Imports,
            language,
        );
        import.metadata.insert(
            "rawText".to_owned(),
            serde_json::Value::String(raw_text.to_owned()),
        );
        FileGraph {
            pending_refs: vec![
                import,
                pending_reference(
                    "f9",
                    call,
                    crate::storage::graph::EdgeKind::Calls,
                    language,
                ),
            ],
            ..empty_graph(Vec::new())
        }
    }

    fn graph_with_namespace_import_and_call(
        specifier: &str,
        raw_text: &str,
        receiver: &str,
        call: &str,
        language: &str,
    ) -> FileGraph {
        let mut import = pending_reference(
            "f9",
            specifier,
            crate::storage::graph::EdgeKind::Imports,
            language,
        );
        import.metadata.insert(
            "rawText".to_owned(),
            serde_json::Value::String(raw_text.to_owned()),
        );
        let mut call_reference = pending_reference(
            "f9",
            call,
            crate::storage::graph::EdgeKind::Calls,
            language,
        );
        call_reference.receiver_name = Some(receiver.to_owned());
        FileGraph {
            pending_refs: vec![import, call_reference],
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
    fn same_file_bare_unique_binds_file_scoped() {
        // The in-file symbol table skips declaration entities, so a same-file
        // bare call with exactly one indexed name is unambiguous workspace-wide.
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
        assert_eq!(sweep.resolved, 1);
        assert_eq!(sweep.candidates_recorded, 0);
        assert!(
            graph
                .list_pending_refs(10, 0)
                .expect("pending page")
                .refs
                .is_empty()
        );
    }

    #[test]
    fn same_file_qualified_unique_stays_pending() {
        // Receiver-qualified calls need the receiver's type; binding on a
        // same-file name hit alone would be a coincidental guess.
        let mut graph = open_memory();
        let mut file_graph = graph_with_qualified_call("recv", "helper");
        file_graph.entities.push(record("local", "helper"));
        file_graph.entity_ids.push("local".to_owned());
        graph
            .write_file_graph(9, "run.ts", &file_graph, &[])
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
    fn ts_alias_pairs_parse_named_imports() {
        assert_eq!(
            ts_alias_pairs("import { work as leftWork } from './left';"),
            vec![("leftWork".to_owned(), Some("work".to_owned()))]
        );
        assert_eq!(
            ts_alias_pairs("import { work as a, rest as b } from './m';"),
            vec![
                ("a".to_owned(), Some("work".to_owned())),
                ("b".to_owned(), Some("rest".to_owned())),
            ]
        );
        // Namespace clauses, default imports, and unaliased names never
        // spell a named binding; `* as ns` binds the module itself.
        assert_eq!(
            ts_alias_pairs("import * as ns from './m';"),
            vec![("ns".to_owned(), None)]
        );
        assert!(ts_alias_pairs("import def from './m';").is_empty());
        assert!(ts_alias_pairs("import { plain } from './m';").is_empty());
    }

    #[test]
    fn python_alias_pairs_parse_from_imports() {
        assert_eq!(
            python_alias_pairs("from declare import func as foobar"),
            vec![("foobar".to_owned(), Some("func".to_owned()))]
        );
        assert_eq!(
            python_alias_pairs("from m import (a as x, b as y)"),
            vec![
                ("x".to_owned(), Some("a".to_owned())),
                ("y".to_owned(), Some("b".to_owned())),
            ]
        );
        // `import a as b` binds the module itself as a namespace.
        assert_eq!(
            python_alias_pairs("import left as left_api"),
            vec![("left_api".to_owned(), None)]
        );
        assert!(python_alias_pairs("from m import plain").is_empty());
    }

    #[test]
    fn rust_alias_pairs_parse_use_statements() {
        assert_eq!(
            rust_alias_pairs("use left::work as left_work;"),
            vec![("left_work".to_owned(), Some("work".to_owned()))]
        );
        assert_eq!(
            rust_alias_pairs("use crate::deep::item as short;"),
            vec![("short".to_owned(), Some("item".to_owned()))]
        );
        // Brace groups pin per-item paths the statement text alone cannot
        // spell, and unaliased uses flow through the general rules.
        assert!(rust_alias_pairs("use a::{b as c};").is_empty());
        assert!(rust_alias_pairs("use left::work;").is_empty());
    }

    #[test]
    fn aliased_import_call_binds_imported_symbol() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "left.ts",
                &empty_graph(vec![record("left-work", "work")]),
                &[],
            )
            .expect("left write");
        graph
            .write_file_graph(
                2,
                "right.ts",
                &empty_graph(vec![record("right-work", "work")]),
                &[],
            )
            .expect("right write");
        graph
            .write_file_graph(
                9,
                "caller.ts",
                &graph_with_alias_import_and_call(
                    "./left",
                    "import { work as leftWork } from './left';",
                    "leftWork",
                    "typescript",
                ),
                &[],
            )
            .expect("caller write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        // `leftWork` matches no indexed symbol, so only the alias binding
        // can pin it to left.ts's `work`.
        assert_eq!(sweep.scanned, 2);
        assert_eq!(sweep.resolved, 2);
        assert_eq!(sweep.import_bound, 1);
        assert!(
            graph
                .list_pending_refs(10, 0)
                .expect("pending page")
                .refs
                .is_empty()
        );
        let left_inbound = graph
            .neighborhood(
                "left-work",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Calls]),
            )
            .expect("left inbound");
        assert_eq!(left_inbound.len(), 1);
        assert_eq!(left_inbound[0].source, "f9");
        assert_eq!(left_inbound[0].provenance, Provenance::ImportBound);
        let right_inbound = graph
            .neighborhood(
                "right-work",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Calls]),
            )
            .expect("right inbound");
        assert!(right_inbound.is_empty());
    }

    #[test]
    fn python_alias_import_binds_imported_symbol() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "declare.py",
                &empty_graph(vec![record("declare-func", "func")]),
                &[],
            )
            .expect("declare write");
        graph
            .write_file_graph(
                9,
                "caller.py",
                &graph_with_alias_import_and_call(
                    "declare",
                    "from declare import func as foobar",
                    "foobar",
                    "python",
                ),
                &[],
            )
            .expect("caller write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        // The dotted module specifier resolves from the workspace root, and
        // the alias pins `foobar` to that module's `func`.
        assert_eq!(sweep.scanned, 2);
        assert_eq!(sweep.resolved, 2);
        assert_eq!(sweep.import_bound, 1);
        assert_eq!(sweep.failed, 0);
        let inbound = graph
            .neighborhood(
                "declare-func",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Calls]),
            )
            .expect("inbound");
        assert_eq!(inbound.len(), 1);
        assert_eq!(inbound[0].provenance, Provenance::ImportBound);
    }

    #[test]
    fn rust_alias_use_binds_imported_symbol() {
        let mut graph = open_memory();
        let mut work = record("left-work", "work");
        work.qualified_name = "left::work".to_owned();
        graph
            .write_file_graph(1, "left.rs", &empty_graph(vec![work]), &[])
            .expect("left write");
        graph
            .write_file_graph(
                9,
                "caller.rs",
                &graph_with_alias_import_and_call(
                    "left::work",
                    "use left::work as left_work;",
                    "left_work",
                    "rust",
                ),
                &[],
            )
            .expect("caller write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        // The use path's first segment names the module, and the alias pins
        // `left_work` to its `work`. The use edge itself binds through its
        // name tail's workspace uniqueness, as any Rust use edge does.
        assert_eq!(sweep.resolved, 2);
        assert_eq!(sweep.import_bound, 1);
        let inbound = graph
            .neighborhood(
                "left-work",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Calls]),
            )
            .expect("inbound");
        assert_eq!(inbound.len(), 1);
        assert_eq!(inbound[0].provenance, Provenance::ImportBound);
    }

    #[test]
    fn namespace_import_call_binds_module_member() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "left.py",
                &empty_graph(vec![record("left-work", "work"), record("left-stop", "stop")]),
                &[],
            )
            .expect("left write");
        graph
            .write_file_graph(2, "right.py", &empty_graph(vec![record("right-work", "work")]), &[])
            .expect("right write");
        graph
            .write_file_graph(
                9,
                "caller.py",
                &graph_with_namespace_import_and_call(
                    "left",
                    "import left as left_api",
                    "left_api",
                    "work",
                    "python",
                ),
                &[],
            )
            .expect("caller write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        // `left_api.work()` matches two workspace-wide `work` declarations;
        // the namespace binding scopes the member to left.py alone.
        assert_eq!(sweep.resolved, 2);
        assert_eq!(sweep.import_bound, 1);
        let inbound = graph
            .neighborhood(
                "left-work",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Calls]),
            )
            .expect("inbound");
        assert_eq!(inbound.len(), 1);
        assert_eq!(inbound[0].provenance, Provenance::ImportBound);
        let right_inbound = graph
            .neighborhood(
                "right-work",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Calls]),
            )
            .expect("right inbound");
        assert!(right_inbound.is_empty());
    }

    #[test]
    fn namespace_call_with_missing_member_stays_pending() {
        let mut graph = open_memory();
        // left.py never declares `work`; the workspace names it twice.
        graph
            .write_file_graph(1, "left.py", &empty_graph(vec![record("left-stop", "stop")]), &[])
            .expect("left write");
        graph
            .write_file_graph(2, "right.py", &empty_graph(vec![record("right-work", "work")]), &[])
            .expect("right write");
        graph
            .write_file_graph(3, "third.py", &empty_graph(vec![record("third-work", "work")]), &[])
            .expect("third write");
        graph
            .write_file_graph(
                9,
                "caller.py",
                &graph_with_namespace_import_and_call(
                    "left",
                    "import left as left_api",
                    "left_api",
                    "work",
                    "python",
                ),
                &[],
            )
            .expect("caller write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        // The namespace pins the member to left.py, which declares no
        // `work`; binding either workspace-wide declaration would be a
        // coincidence, so the call stays queued with its candidates.
        assert_eq!(sweep.resolved, 1);
        assert_eq!(sweep.import_bound, 0);
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
    fn ts_namespace_import_call_binds_module_member() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "left.ts",
                &empty_graph(vec![record("left-work", "work"), record("left-stop", "stop")]),
                &[],
            )
            .expect("left write");
        graph
            .write_file_graph(2, "right.ts", &empty_graph(vec![record("right-work", "work")]), &[])
            .expect("right write");
        graph
            .write_file_graph(
                9,
                "caller.ts",
                &graph_with_namespace_import_and_call(
                    "./left",
                    "import * as leftApi from './left';",
                    "leftApi",
                    "work",
                    "typescript",
                ),
                &[],
            )
            .expect("caller write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        // The `* as leftApi` clause binds the module object, and the member
        // call scopes to left.ts exactly like the Python form.
        assert_eq!(sweep.resolved, 2);
        assert_eq!(sweep.import_bound, 1);
        let inbound = graph
            .neighborhood(
                "left-work",
                crate::storage::graph::Direction::In,
                Some(&[crate::storage::graph::EdgeKind::Calls]),
            )
            .expect("inbound");
        assert_eq!(inbound.len(), 1);
        assert_eq!(inbound[0].provenance, Provenance::ImportBound);
    }

    #[test]
    fn alias_call_with_unresolvable_module_stays_pending() {
        let mut graph = open_memory();
        graph
            .write_file_graph(
                1,
                "other.ts",
                &empty_graph(vec![record("other-work", "work")]),
                &[],
            )
            .expect("other write");
        graph
            .write_file_graph(
                9,
                "caller.ts",
                &graph_with_alias_import_and_call(
                    "./missing",
                    "import { work as leftWork } from './missing';",
                    "leftWork",
                    "typescript",
                ),
                &[],
            )
            .expect("caller write");

        let sweep = resolve_pending(&mut graph).expect("sweep");

        // No snapshot module answers the specifier: binding the one
        // workspace-wide `work` would be a coincidence, not the alias's
        // target, so the call stays queued.
        assert_eq!(sweep.resolved, 0);
        assert_eq!(sweep.import_bound, 0);
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
            is_declaration: false,
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
            is_declaration: false,
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
                    is_declaration: false,
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
