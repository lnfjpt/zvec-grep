use crate::{Edge, Result, SqliteGraphStorage, decode_enum, decode_metadata, nonempty};
use rusqlite::Row;

impl SqliteGraphStorage {
    /// All incoming call edges, in insertion order; retains distinct call sites.
    /// # Errors
    /// Rejects blank IDs, invalid stored data, and SQLite errors.
    pub fn get_callers(&self, target_id: &str) -> Result<Vec<Edge>> {
        self.read_calls("target", target_id)
    }

    /// All outgoing call edges, in insertion order.
    /// # Errors
    /// Rejects blank IDs, invalid stored data, and SQLite errors.
    pub fn get_callees(&self, source_id: &str) -> Result<Vec<Edge>> {
        self.read_calls("source", source_id)
    }

    fn read_calls(&self, field: &str, id: &str) -> Result<Vec<Edge>> {
        nonempty(id)?;
        // `field` is a private constant selected above; endpoint values are bound.
        let mut statement = self.connection.prepare(&format!(
            "SELECT kind, source, target, line, column, provenance, metadata
             FROM edges WHERE {field} = ? AND kind = 'calls' ORDER BY id"
        ))?;
        Ok(statement
            .query_map([id], edge_from_row)?
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
