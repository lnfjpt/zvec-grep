use super::{
    DeclaredBinding, Direction, Edge, EdgeKind, EntityRecord, ImportReference, Metadata,
    NameIndexEntry, Result, SqliteGraphStorage, decode_enum, decode_metadata, nonempty,
};
use rusqlite::{Row, params_from_iter};
use serde_json::Value;

impl SqliteGraphStorage {
    /// All resolved one-hop edges in insertion order, with no result limit.
    /// `None` selects all kinds; `Some(&[])` selects none.
    /// Self-loops appear once; distinct stored call sites are preserved.
    /// Awaits read-side consumers (query pipelines, MCP tools); tests use it.
    ///
    /// # Errors
    /// Rejects blank IDs, invalid stored data, and SQLite errors.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn neighborhood(
        &self,
        id: &str,
        direction: Direction,
        kinds: Option<&[EdgeKind]>,
    ) -> Result<Vec<Edge>> {
        nonempty(id)?;
        let endpoint = match direction {
            Direction::In => "target = ?1",
            Direction::Out => "source = ?1",
            Direction::Both => "(source = ?1 OR target = ?1)",
        };
        let mut sql = format!(
            "SELECT kind, source, target, line, col, provenance, metadata
             FROM edges WHERE {endpoint} AND status = 'resolved'"
        );
        let mut values = vec![id];
        if let Some(kinds) = kinds {
            if kinds.is_empty() {
                return Ok(Vec::new());
            }
            let mut names: Vec<&str> = kinds.iter().map(|kind| kind.as_str()).collect();
            names.sort_unstable();
            names.dedup();
            sql.push_str(" AND kind IN (");
            sql.push_str(&vec!["?"; names.len()].join(", "));
            sql.push(')');
            values.extend(names);
        }
        sql.push_str(" ORDER BY id");
        let mut statement = self.connection.prepare(&sql)?;
        Ok(statement
            .query_map(params_from_iter(values), edge_from_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Entity IDs owned by a file in the last persisted snapshot, recovered
    /// from its `contains` edges. This is not the complete canonical ID set;
    /// update coordination must read IDs from zvec instead.
    /// Returns an empty set for files that were never persisted.
    ///
    /// # Errors
    /// Propagates SQLite errors.
    #[cfg(test)]
    pub(crate) fn file_entity_ids(&self, file_id: u32) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT target FROM edges
             WHERE file_id = ?1 AND kind = 'contains' AND target IS NOT NULL
             ORDER BY target",
        )?;
        let rows = statement.query_map([file_id], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
    }

    /// Every workspace entity that declares `name`, ordered by entity ID.
    /// This is the resolver's name index; empty names never occur because
    /// unnamed entities are not persisted.
    ///
    /// When a definition exists for the name, pure declarations never
    /// surface: a prototype shadows nothing, but any definition beats every
    /// same-named declaration (C/C++ ODR-style preference), so the rules
    /// downstream never mis-bind a call to a bodyless header entity.
    ///
    /// # Errors
    /// Rejects blank names, invalid stored data, and SQLite errors.
    pub(crate) fn lookup_by_name(&self, name: &str) -> Result<Vec<NameIndexEntry>> {
        nonempty(name)?;
        let mut statement = self.connection.prepare(
            "SELECT entity_id, file_id, qualified_name, is_exported, arity, is_declaration
             FROM entities
             WHERE name = ?1 ORDER BY entity_id",
        )?;
        let mut rows = statement
            .query_map([name], |row| {
                Ok(NameIndexEntry {
                    entity_id: row.get(0)?,
                    file_id: row.get(1)?,
                    qualified_name: row.get(2)?,
                    is_exported: row.get::<_, i64>(3)? == 1,
                    arity: row
                        .get::<_, Option<i64>>(4)?
                        .and_then(|arity| u32::try_from(arity).ok()),
                    is_declaration: row.get::<_, i64>(5)? == 1,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if rows.iter().any(|row| !row.is_declaration) {
            rows.retain(|row| !row.is_declaration);
        }
        Ok(rows)
    }

    /// The single workspace entity declaring `name`, or `None` when absent or
    /// ambiguous. Awaits read-side consumers (query pipelines, MCP tools);
    /// tests use it.
    ///
    /// # Errors
    /// Rejects blank names, invalid stored data, and SQLite errors.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn lookup_unique(&self, name: &str) -> Result<Option<NameIndexEntry>> {
        let mut hits = self.lookup_by_name(name)?;
        Ok((hits.len() == 1).then(|| hits.pop().expect("single hit")))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    /// The workspace-relative path of one file, or `None` when it has no
    /// persisted snapshot. Anchors relative import specifiers for the resolver.
    ///
    /// # Errors
    /// Propagates SQLite errors.
    pub(crate) fn file_path(&self, file_id: u32) -> Result<Option<String>> {
        let mut statement = self
            .connection
            .prepare("SELECT relative_path FROM file_paths WHERE file_id = ?1")?;
        let mut rows = statement.query_map([file_id], |row| row.get::<_, String>(0))?;
        Ok(match rows.next() {
            Some(row) => Some(row?),
            None => None,
        })
    }

    /// The file owning `relative_path`, or `None` when no snapshot matches.
    /// Paths are workspace-relative with forward slashes.
    ///
    /// # Errors
    /// Rejects blank paths and SQLite errors.
    pub(crate) fn file_id_by_path(&self, relative_path: &str) -> Result<Option<u32>> {
        nonempty(relative_path)?;
        let mut statement = self
            .connection
            .prepare("SELECT file_id FROM file_paths WHERE relative_path = ?1")?;
        let mut rows = statement.query_map([relative_path], |row| row.get::<_, i64>(0))?;
        Ok(match rows.next() {
            Some(row) => u32::try_from(row?).ok(),
            None => None,
        })
    }

    /// Whether the workspace snapshot contains a module rooted at `name`:
    /// a file matching one of the dot-prefixed `extensions`, or any path
    /// inside a directory named `name`. Anchors the resolver's
    /// external-import detection; specifiers are identifiers, so no LIKE
    /// escaping is needed.
    ///
    /// # Errors
    /// Propagates SQLite errors.
    pub(crate) fn path_has_module(&self, name: &str, extensions: &[&str]) -> Result<bool> {
        for extension in extensions {
            let mut statement = self
                .connection
                .prepare("SELECT EXISTS(SELECT 1 FROM file_paths WHERE relative_path = ?1)")?;
            let path = format!("{name}{extension}");
            if statement.query_row([&path], |row| row.get::<_, i64>(0))? != 0 {
                return Ok(true);
            }
        }
        let mut statement = self.connection.prepare(
            "SELECT EXISTS(SELECT 1 FROM file_paths WHERE relative_path LIKE ?1 || '/%')",
        )?;
        Ok(statement.query_row([name], |row| row.get::<_, i64>(0))? != 0)
    }

    /// Files whose modules one file's resolved imports point at, deduplicated:
    /// `f{file_id}` endpoints from import-scoped resolution plus the owning
    /// files of name-unique entity targets. This is the import scope that
    /// anchors bare-name resolution.
    ///
    /// # Errors
    /// Propagates SQLite errors.
    pub(crate) fn imported_file_ids(&self, file_id: u32) -> Result<Vec<u32>> {
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT e.target, n.file_id FROM edges e
             LEFT JOIN entities n ON n.entity_id = e.target
             WHERE e.file_id = ?1 AND e.kind = 'imports' AND e.status = 'resolved'
               AND e.target IS NOT NULL",
        )?;
        let rows = statement.query_map([file_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
        })?;
        let mut ids = Vec::new();
        for row in rows {
            let (target, owner) = row?;
            if let Some(id) = parse_file_endpoint(&target) {
                ids.push(id);
            } else if let Some(owner) = owner.and_then(|owner| u32::try_from(owner).ok()) {
                ids.push(owner);
            }
        }
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    /// Import statements of a file in insertion order, both pending and
    /// resolved: the resolver parses alias bindings from their statement
    /// text regardless of whether the specifier has been resolved yet.
    ///
    /// # Errors
    /// Propagates SQLite errors and invalid stored metadata.
    pub(crate) fn import_references(&self, file_id: u32) -> Result<Vec<ImportReference>> {
        let mut statement = self.connection.prepare(
            "SELECT reference_name, metadata FROM edges
             WHERE file_id = ?1 AND kind = 'imports' ORDER BY id",
        )?;
        let rows = statement.query_map([file_id], |row| {
            let specifier: String = row.get(0)?;
            let metadata: Metadata = decode_metadata(&row.get::<_, String>(1)?)?;
            let raw_text = metadata.get("rawText").and_then(Value::as_str);
            Ok(ImportReference {
                specifier,
                raw_text: raw_text.map(str::to_owned),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The type bindings one entity declares (`var x = new X()` inside its
    /// body, typed parameters in its signature): each names the receiver
    /// type the resolver may infer when a call names the bound name.
    ///
    /// # Errors
    /// Propagates SQLite errors.
    pub(crate) fn declared_bindings(&self, owner_id: &str) -> Result<Vec<DeclaredBinding>> {
        nonempty(owner_id)?;
        let mut statement = self.connection.prepare(
            "SELECT reference_name, metadata, line FROM edges
             WHERE source = ?1 AND kind = 'declares' ORDER BY line, id",
        )?;
        let rows = statement.query_map([owner_id], |row| {
            let name: String = row.get(0)?;
            let metadata: Metadata = decode_metadata(&row.get::<_, String>(1)?)?;
            let bound_type = metadata
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let line: i64 = row.get(2)?;
            Ok(DeclaredBinding {
                name,
                bound_type: bound_type.to_owned(),
                line: u32::try_from(line).unwrap_or_default(),
            })
        })?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<DeclaredBinding>>>()?
            .into_iter()
            .filter(|binding| !binding.bound_type.is_empty())
            .collect())
    }

    /// The name-index record of one entity by ID, or `None` when the entity
    /// has no persisted record. Anchors the resolver's receiver hierarchy:
    /// a call site's owner spells the enclosing class as its qualified-name
    /// scope.
    ///
    /// # Errors
    /// Propagates SQLite errors.
    pub(crate) fn entity_record(&self, entity_id: &str) -> Result<Option<EntityRecord>> {
        nonempty(entity_id)?;
        let mut statement = self.connection.prepare(
            "SELECT entity_id, name, qualified_name, is_exported, arity, is_declaration
             FROM entities WHERE entity_id = ?1",
        )?;
        let mut rows = statement.query_map([entity_id], |row| {
            Ok(EntityRecord {
                entity_id: row.get(0)?,
                name: row.get(1)?,
                qualified_name: row.get(2)?,
                is_exported: row.get::<_, i64>(3)? == 1,
                arity: row
                    .get::<_, Option<i64>>(4)?
                    .and_then(|arity| u32::try_from(arity).ok()),
                is_declaration: row.get::<_, i64>(5)? == 1,
            })
        })?;
        Ok(match rows.next() {
            Some(row) => Some(row?),
            None => None,
        })
    }

    /// Inheritance evidence of one class entity: the base entity IDs whose
    /// extends/implements edges have resolved, plus the base names still
    /// pending. The resolver merges both views so a receiver call resolves
    /// in the same sweep that resolves the class's own inheritance edges.
    ///
    /// # Errors
    /// Propagates SQLite errors.
    pub(crate) fn inheritance_bases(&self, entity_id: &str) -> Result<(Vec<String>, Vec<String>)> {
        nonempty(entity_id)?;
        let mut statement = self.connection.prepare(
            "SELECT target, reference_name, status FROM edges
             WHERE source = ?1 AND kind IN ('extends', 'implements') ORDER BY id",
        )?;
        let rows = statement.query_map([entity_id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut resolved = Vec::new();
        let mut pending = Vec::new();
        for row in rows {
            // Same-file bases resolve in the partition, whose resolved edges
            // spell the base in `target` alone; cross-file bases stay pending
            // with their spelled `reference_name`.
            let (target, base_name, status) = row?;
            if status == "resolved" {
                if let Some(target) = target {
                    resolved.push(target);
                }
            } else if let Some(base_name) = base_name {
                pending.push(base_name);
            }
        }
        Ok((resolved, pending))
    }
}

/// A file endpoint `f{file_id}`. Entity IDs are long hex digests whose
/// digits-only tails of bounded length never collide with this shape.
fn parse_file_endpoint(target: &str) -> Option<u32> {
    let digits = target.strip_prefix('f')?;
    if digits.is_empty() || digits.len() > 10 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg_attr(not(test), allow(dead_code))]
fn edge_from_row(row: &Row<'_>) -> rusqlite::Result<Edge> {
    Ok(Edge {
        kind: decode_enum(row.get(0)?)?,
        source: row.get(1)?,
        target: row.get(2)?,
        line: row.get(3)?,
        column: row.get(4)?,
        provenance: decode_enum(row.get(5)?)?,
        metadata: decode_metadata(&row.get::<_, String>(6)?)?,
    })
}
