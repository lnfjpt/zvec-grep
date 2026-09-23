//! Walk-time collectors for name-based graph references.
//!
//! These run inside entity bodies — the region the symbol walker deliberately
//! does not descend into — so call sites are attributed to their innermost
//! owning entity without a second full parse. Mirrors the TypeScript
//! `src/engine/extraction/graph/edge-collector.ts` implementation.

use tree_sitter::Node;

use crate::domain::{FileFormat, GraphRefKind};
use crate::utils::collapse_whitespace;

use super::vocabulary::{
    CALLEE_FIELDS, COMMON_CALL_TYPES, HeritageVocabulary, import_vocabulary_for,
    heritage_vocabulary_for,
};
use super::WalkContext;

const MAX_CALLEE_TEXT_CHARS: usize = 180;

// Local mirrors of the `code::adapter` node helpers, which are not visible
// from this module tree.
fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn node_text<'source>(node: Node<'_>, source: &'source [u8]) -> &'source str {
    node.utf8_text(source).unwrap_or_default()
}

/// True when the node is a call-ish expression per the shared vocabulary.
pub(crate) fn is_call_node(node: Node<'_>) -> bool {
    COMMON_CALL_TYPES.contains(&node.kind())
}

/// Extracts the callee expression text for a call node, if recoverable.
fn callee_text_of<'source>(node: Node<'_>, source: &'source [u8]) -> Option<&'source str> {
    for field in CALLEE_FIELDS {
        if let Some(target) = node.child_by_field_name(field) {
            return Some(node_text(target, source));
        }
    }
    node.named_child(0).map(|first| node_text(first, source))
}

/// Counts call arguments when the grammar exposes an `arguments` field.
fn call_arity_of(node: Node<'_>) -> Option<usize> {
    let arguments = node
        .child_by_field_name("arguments")
        .or_else(|| node.child_by_field_name("argument_list"))?;
    let mut cursor = arguments.walk();
    Some(
        arguments
            .named_children(&mut cursor)
            .filter(|child| child.kind() != "," && !child.kind().ends_with("_comment"))
            .count(),
    )
}

/// Splits a callee expression into its receiver and short member name.
fn split_callee_name(callee_text: &str) -> (String, Option<String>) {
    let collapsed = collapse_whitespace(callee_text);
    let cleaned = strip_new_prefix(&collapsed);
    let Some(separator) = cleaned.rfind(['.', ':']) else {
        return (cleaned.to_owned(), None);
    };
    if separator == 0 || separator + 1 >= cleaned.len() {
        return (cleaned.to_owned(), None);
    }
    (
        cleaned[separator + 1..].to_owned(),
        Some(cleaned[..separator].trim_end_matches(':').to_owned()),
    )
}

/// Strips a leading `new ` constructor prefix (`/^new\s+/`).
fn strip_new_prefix(cleaned: &str) -> &str {
    if let Some(rest) = cleaned.strip_prefix("new") {
        let after = rest.trim_start();
        if after.len() < rest.len() {
            return after;
        }
    }
    cleaned
}

/// Accepts plain and qualified identifiers (`dog`, `HashMap::new`, `a.b.c`)
/// in the ASCII shape tree-sitter grammars use for identifier tokens. The
/// TypeScript original expresses this as `/^[A-Za-z_$][A-Za-z0-9_$.:]*$/`.
fn is_identifier_like(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let first_ok = first.is_ascii_alphabetic() || matches!(first, '_' | '$');
    first_ok
        && chars
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '$' | '.' | ':'))
}

/// Scans an entity body for call sites and buffers one `calls` reference per
/// distinct callee. Nested call expressions inside an argument list are
/// skipped: the inner call is collected on its own visit when the walk
/// reaches it as a sibling — keeping attribution shallow matches how the
/// outline collector treats nested calls and avoids double-counting chains.
pub(crate) fn scan_call_edges(
    entity_node: Node<'_>,
    owner_index: usize,
    source: &[u8],
    context: &mut WalkContext,
) {
    visit_for_calls(entity_node, owner_index, source, context);
}

fn visit_for_calls(
    node: Node<'_>,
    owner_index: usize,
    source: &[u8],
    context: &mut WalkContext,
) {
    if is_call_node(node) {
        if let Some(callee) = callee_text_of(node, source) {
            if callee.len() <= MAX_CALLEE_TEXT_CHARS && is_identifier_like(callee) {
                let (ref_name, receiver_name) = split_callee_name(callee);
                context.add_call_edge(
                    owner_index,
                    &ref_name,
                    receiver_name.as_deref(),
                    Some(callee),
                    call_arity_of(node),
                    node.start_position().row + 1,
                    node.start_position().column,
                );
            }
        }
        // Do not descend into the call's own arguments; other collectors and
        // the walk revisit that region.
        return;
    }
    for child in named_children(node) {
        visit_for_calls(child, owner_index, source, context);
    }
}

/// Normalizes an import module path for buffering (strips quotes/brackets).
fn normalize_module_path(module_text: &str) -> String {
    let unquoted = module_text
        .trim()
        .trim_start_matches(['<', '"', '\'', '`'])
        .trim_end_matches(['>', '"', '\'', '`']);
    strip_trailing_semicolons(unquoted)
        .trim()
        .to_owned()
}

/// Removes a trailing semicolon run plus following whitespace (`/;+\s*$/`).
fn strip_trailing_semicolons(value: &str) -> &str {
    let without_whitespace = value.trim_end();
    let semicolons = without_whitespace
        .chars()
        .rev()
        .take_while(|&ch| ch == ';')
        .count();
    if semicolons == 0 {
        return value;
    }
    &without_whitespace[..without_whitespace.len() - semicolons]
}

/// Extracts the module path from an import node via grammar fields or text.
fn module_path_of(node: Node<'_>, source: &[u8], format: FileFormat) -> Option<String> {
    if let Some(vocabulary) = import_vocabulary_for(format) {
        for field in vocabulary.module_fields {
            if let Some(target) = node.child_by_field_name(field) {
                let path = normalize_module_path(node_text(target, source));
                if !path.is_empty() {
                    return Some(path);
                }
            }
            // Go: `import_declaration` carries no fields; the path lives on
            // the nested `import_spec` nodes (single and grouped imports).
            if format == FileFormat::Go {
                for descendant in descendants_within(node, 2) {
                    if let Some(target) = descendant.child_by_field_name(field) {
                        let path = normalize_module_path(node_text(target, source));
                        if !path.is_empty() {
                            return Some(path);
                        }
                    }
                }
            }
        }
        // Java: the declaration carries no path field; the module path is
        // the first identifier-like named child (`identifier` /
        // `scoped_identifier`).
        if format == FileFormat::Java {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                let path = collapse_whitespace(node_text(child, source));
                if is_identifier_like(&path) {
                    return Some(path);
                }
            }
        }
    }
    // Text fallback (Rust `use a::b;`, Go grouped imports, C includes whose
    // path field is nested in a string literal node).
    let node_path = normalize_module_path(node_text(node, source));
    if format == FileFormat::Rust {
        return strip_rust_use_path(&node_path);
    }
    if !node_path.is_empty() && !node_path.contains('\n') {
        return Some(node_path);
    }
    None
}

/// Named descendants within `max_depth` levels below `node`.
fn descendants_within(node: Node<'_>, max_depth: usize) -> Vec<Node<'_>> {
    let mut found = Vec::new();
    if max_depth == 0 {
        return found;
    }
    for child in named_children(node) {
        found.push(child);
        found.extend(descendants_within(child, max_depth - 1));
    }
    found
}

/// Reduces a normalized `use` declaration to its module path: drops a leading
/// `use`, a trailing `as <ident>` alias, brace groups, and all whitespace.
fn strip_rust_use_path(normalized: &str) -> Option<String> {
    let without_alias = strip_trailing_as_alias(strip_leading_use(normalized));
    let stripped: String = without_alias
        .chars()
        .filter(|ch| !matches!(ch, '{' | '}') && !ch.is_whitespace())
        .collect();
    let stripped = stripped.strip_suffix(';').unwrap_or(&stripped);
    (!stripped.is_empty()).then(|| stripped.to_owned())
}

/// Strips a leading `use` keyword (`/^use\s+/i`).
fn strip_leading_use(text: &str) -> &str {
    if text.as_bytes().get(..3).is_some_and(|prefix| {
        prefix.eq_ignore_ascii_case(b"use")
    }) {
        let rest = text[3..].trim_start();
        if rest.len() < text.len() - 3 {
            return rest;
        }
    }
    text
}

/// Drops a trailing `as <ident>` alias (`/\s+as\s+\w+$/i`). Whitespace inside
/// the kept prefix does not survive [`strip_rust_use_path`], so rejoining on
/// single spaces preserves the resulting path.
fn strip_trailing_as_alias(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() >= 3 {
        let alias = words[words.len() - 2];
        let last = words[words.len() - 1];
        if alias.eq_ignore_ascii_case("as")
            && last.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            return words[..words.len() - 2].join(" ");
        }
    }
    text.to_owned()
}

/// True when the node type belongs to the language's import vocabulary.
pub(crate) fn is_import_node(node: Node<'_>, format: FileFormat) -> bool {
    import_vocabulary_for(format)
        .is_some_and(|vocabulary| vocabulary.import_types.contains(&node.kind()))
}

/// Buffers an `imports` reference for an import declaration node. The owner
/// is the file scope: imports belong to the file, not to any entity.
pub(crate) fn collect_import_edge(
    node: Node<'_>,
    source: &[u8],
    format: FileFormat,
    context: &mut WalkContext,
) {
    let Some(path) = module_path_of(node, source, format) else {
        return;
    };
    context.add_import_edge(
        &path,
        Some(node_text(node, source)),
        node.start_position().row + 1,
        node.start_position().column,
    );
}

/// Buffers `extends`/`implements` references for a class-like entity node,
/// reading the per-language heritage vocabulary.
pub(crate) fn collect_inheritance_edges(
    entity_node: Node<'_>,
    source: &[u8],
    format: FileFormat,
    owner_index: usize,
    context: &mut WalkContext,
) {
    let Some(vocabulary) = heritage_vocabulary_for(format) else {
        return;
    };
    // (kind, clause node) pairs; the clause node feeds type-name extraction.
    let clauses = match vocabulary {
        HeritageVocabulary::Nested {
            extends_types,
            implements_types,
        } => descendants_within(entity_node, 2)
            .into_iter()
            .filter_map(|descendant| {
                if extends_types.contains(&descendant.kind()) {
                    Some((GraphRefKind::Extends, descendant))
                } else if implements_types.contains(&descendant.kind()) {
                    Some((GraphRefKind::Implements, descendant))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>(),
        HeritageVocabulary::Fields { extends, implements } => extends
            .iter()
            .filter_map(|field| {
                entity_node
                    .child_by_field_name(field)
                    .map(|clause| (GraphRefKind::Extends, clause))
            })
            .chain(implements.iter().filter_map(|field| {
                entity_node
                    .child_by_field_name(field)
                    .map(|clause| (GraphRefKind::Implements, clause))
            }))
            .collect(),
    };
    for (ref_kind, clause) in clauses {
        for name in type_names_from_clause(clause, source) {
            context.add_inheritance_edge(
                ref_kind,
                owner_index,
                &name,
                Some(node_text(clause, source)),
                clause.start_position().row + 1,
                clause.start_position().column,
            );
        }
    }
}

/// C++ base-clause decoration kinds that never name an inherited type.
const CLAUSE_DECORATION_KINDS: &[&str] = &[
    "access_specifier",
    "alignas_qualifier",
    "attribute_declaration",
    "ms_declspec_modifier",
    "virtual_specifier",
];

/// Splits an inheritance clause into type names. Handles the common shapes:
/// a single type node, a comma-separated list, and C++ base specifiers with
/// access labels (`public Base`).
fn type_names_from_clause(clause: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    collect_type_names(clause, source, &mut names);
    names
}

fn collect_type_names(node: Node<'_>, source: &[u8], names: &mut Vec<String>) {
    if CLAUSE_DECORATION_KINDS.contains(&node.kind()) {
        return;
    }
    let text = collapse_whitespace(node_text(node, source));
    if text.is_empty() || text.len() > MAX_CALLEE_TEXT_CHARS {
        return;
    }
    if node.named_child_count() == 0 && is_identifier_like(&text) {
        names.push(text);
        return;
    }
    for child in named_children(node) {
        collect_type_names(child, source, names);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_callee_names_into_receiver_and_member() {
        assert_eq!(
            split_callee_name("helper"),
            ("helper".to_owned(), None)
        );
        assert_eq!(
            split_callee_name("dog.bark"),
            ("bark".to_owned(), Some("dog".to_owned()))
        );
        assert_eq!(
            split_callee_name("HashMap::new"),
            ("new".to_owned(), Some("HashMap".to_owned()))
        );
        // Receiver runs of `::` collapse to a single trailing run.
        assert_eq!(
            split_callee_name("a::b::c"),
            ("c".to_owned(), Some("a::b".to_owned()))
        );
        // `new` prefix is stripped; whitespace runs collapse.
        assert_eq!(
            split_callee_name("new   Widget"),
            ("Widget".to_owned(), None)
        );
        // A leading or trailing separator disables the split.
        assert_eq!(split_callee_name(".foo"), (".foo".to_owned(), None));
        assert_eq!(split_callee_name("foo."), ("foo.".to_owned(), None));
    }

    #[test]
    fn filters_identifier_like_texts() {
        assert!(is_identifier_like("helper"));
        assert!(is_identifier_like("$scope"));
        assert!(is_identifier_like("a.b.c"));
        assert!(is_identifier_like("HashMap::new"));
        assert!(!is_identifier_like(""));
        assert!(!is_identifier_like("1helper"));
        assert!(!is_identifier_like("foo bar"));
        assert!(!is_identifier_like("foo(3)"));
        assert!(!is_identifier_like("hélder"));
    }

    #[test]
    fn normalizes_module_paths() {
        assert_eq!(normalize_module_path("\"./utils.js\""), "./utils.js");
        assert_eq!(normalize_module_path("'./utils.js'"), "./utils.js");
        assert_eq!(normalize_module_path("<stdio.h>"), "stdio.h");
        // Quoted path with a trailing semicolon: the quote pair is stripped
        // before the semicolon, leaving the inner quote — matching the
        // TypeScript normalization order. Real grammar paths never combine
        // both shapes (fields exclude the semicolon; text fallbacks for Rust
        // and Go strip it separately).
        assert_eq!(normalize_module_path("  \"foo\";  "), "foo\"");
        assert_eq!(normalize_module_path(""), "");
    }

    #[test]
    fn strips_rust_use_paths() {
        assert_eq!(
            strip_rust_use_path("use std::collections::HashMap"),
            Some("std::collections::HashMap".to_owned())
        );
        // Trailing alias and grouped imports survive the text fallback.
        assert_eq!(
            strip_rust_use_path("use std::fmt::Write as W"),
            Some("std::fmt::Write".to_owned())
        );
        assert_eq!(
            strip_rust_use_path("use std::fmt::{Display, Write}"),
            Some("std::fmt::Display,Write".to_owned())
        );
        assert_eq!(strip_rust_use_path("use {}"), None);
    }

    #[test]
    fn reads_module_paths_from_grammar_nodes() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("rust grammar loads");
        let source = "use std::collections::HashMap;\n";
        let tree = parser
            .parse(source, None)
            .expect("rust source parses");
        let use_node = tree
            .root_node()
            .named_child(0)
            .expect("use declaration exists");
        assert_eq!(use_node.kind(), "use_declaration");
        assert_eq!(
            module_path_of(use_node, source.as_bytes(), FileFormat::Rust),
            Some("std::collections::HashMap".to_owned())
        );

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .expect("javascript grammar loads");
        let source = "import { helper } from \"./utils.js\";\n";
        let tree = parser
            .parse(source, None)
            .expect("javascript source parses");
        let import_node = tree
            .root_node()
            .named_child(0)
            .expect("import statement exists");
        assert_eq!(import_node.kind(), "import_statement");
        assert_eq!(
            module_path_of(import_node, source.as_bytes(), FileFormat::JavaScript),
            Some("./utils.js".to_owned())
        );
    }
}
