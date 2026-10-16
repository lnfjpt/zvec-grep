use super::{
    Direction, Edge, EdgeKind, NameIndexEntry, Result, SqliteGraphStorage, decode_enum,
    decode_metadata, nonempty,
};
use rusqlite::{Row, params_from_iter};

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
    /// # Errors
    /// Rejects blank names, invalid stored data, and SQLite errors.
    pub(crate) fn lookup_by_name(&self, name: &str) -> Result<Vec<NameIndexEntry>> {
        nonempty(name)?;
        let mut statement = self.connection.prepare(
            "SELECT entity_id, file_id, qualified_name, is_exported, arity FROM entities
             WHERE name = ?1 ORDER BY entity_id",
        )?;
        let rows = statement.query_map([name], |row| {
            Ok(NameIndexEntry {
                entity_id: row.get(0)?,
                file_id: row.get(1)?,
                qualified_name: row.get(2)?,
                is_exported: row.get::<_, i64>(3)? == 1,
                arity: row
                    .get::<_, Option<i64>>(4)?
                    .and_then(|arity| u32::try_from(arity).ok()),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
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
    /// a file matching one of `extensions`, or any path inside a directory
    /// named `name`. Anchors the resolver's external-import detection;
    /// specifiers are identifiers, so no LIKE escaping is needed.
    ///
    /// # Errors
    /// Propagates SQLite errors.
    pub(crate) fn path_has_module(&self, name: &str, extensions: &[&str]) -> Result<bool> {
        for extension in extensions {
            let mut statement = self
                .connection
                .prepare("SELECT EXISTS(SELECT 1 FROM file_paths WHERE relative_path = ?1)")?;
            let path = format!("{name}.{extension}");
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
