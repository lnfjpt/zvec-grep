use super::{
    Direction, Edge, EdgeKind, Result, SqliteGraphStorage, decode_enum, decode_metadata, nonempty,
};
use rusqlite::{Row, params_from_iter};

impl SqliteGraphStorage {
    /// All resolved one-hop edges in insertion order, with no result limit.
    /// `None` selects all kinds; `Some(&[])` selects none.
    /// Self-loops appear once; distinct stored call sites are preserved.
    ///
    /// # Errors
    /// Rejects blank IDs, invalid stored data, and SQLite errors.
    pub(crate) fn neighborhood(
        &self,
        id: &str,
        direction: Direction,
        kinds: Option<&[EdgeKind]>,
    ) -> Result<Vec<Edge>> {
        nonempty(id)?;
        let mut filter = String::from("status = 'resolved'");
        let mut values = vec![id];
        if let Some(kinds) = kinds {
            if kinds.is_empty() {
                return Ok(Vec::new());
            }
            let mut names: Vec<&str> = kinds.iter().map(|kind| kind.as_str()).collect();
            names.sort_unstable();
            names.dedup();
            let parameters = (2..names.len() + 2)
                .map(|index| format!("?{index}"))
                .collect::<Vec<_>>()
                .join(", ");
            filter.push_str(" AND kind IN (");
            filter.push_str(&parameters);
            filter.push(')');
            values.extend(names);
        }
        let predicate = match direction {
            Direction::In => format!("target = ?1 AND {filter}"),
            Direction::Out => format!("source = ?1 AND {filter}"),
            // Separate endpoint lookups avoid scanning all resolved rows to
            // satisfy ORDER BY id. UNION deduplicates self-loops by row ID,
            // preserving distinct stored edges even when their fields match.
            Direction::Both => format!(
                "id IN (SELECT id FROM edges WHERE source = ?1 AND {filter}
                        UNION SELECT id FROM edges WHERE target = ?1 AND {filter})"
            ),
        };
        let sql = format!(
            "SELECT kind, source, target, line, col, provenance, metadata
             FROM edges WHERE {predicate} ORDER BY id"
        );
        let mut statement = self.connection.prepare(&sql)?;
        Ok(statement
            .query_map(params_from_iter(values), edge_from_row)?
            .collect::<rusqlite::Result<_>>()?)
    }
}

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
