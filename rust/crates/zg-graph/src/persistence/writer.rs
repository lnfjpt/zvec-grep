use rusqlite::{TransactionBehavior, params, params_from_iter};
use std::collections::HashSet;

use super::{Error, FileGraph, Provenance, RefDirection, Result, SqliteGraphStorage, nonempty};

impl SqliteGraphStorage {
    /// Atomically replaces one file's local graph and invalidates references to either endpoint.
    /// `old_entity_ids` must contain **all** pre-update entity IDs from zvec; pass
    /// an empty slice for a new file. Entity IDs in the new snapshot are not stored
    /// in a second node table. Cross-file edges must use `apply_resolutions`.
    ///
    /// # Errors
    /// Rejects invalid ownership, duplicate/empty IDs, non-local edges, invalid
    /// positions, read-only connections, and SQLite failures. Failures roll back.
    pub fn write_file_graph(
        &mut self,
        file_id: &str,
        graph: &FileGraph,
        old_entity_ids: &[String],
    ) -> Result<()> {
        validate(file_id, graph, old_entity_ids)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut endpoints: Vec<&str> = old_entity_ids.iter().map(String::as_str).collect();
        endpoints.push(file_id);
        endpoints.sort_unstable();
        endpoints.dedup();
        for chunk in endpoints.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            // Clear only the resolved endpoint; retain the owner and evidence.
            for (field, direction) in [("source", "in"), ("target", "out")] {
                tx.execute(
                    &format!(
                        "UPDATE edges SET {field} = NULL, provenance = NULL
                         WHERE {field} IN ({placeholders}) AND file_id <> ?
                         AND ref_direction = '{direction}'"
                    ),
                    params_from_iter(chunk.iter().copied().chain(std::iter::once(file_id))),
                )?;
            }
            // Remaining matches are direct edges or references whose owner was removed.
            for field in ["source", "target"] {
                tx.execute(
                    &format!("DELETE FROM edges WHERE {field} IN ({placeholders})"),
                    params_from_iter(chunk.iter().copied()),
                )?;
            }
        }
        tx.execute("DELETE FROM edges WHERE file_id = ?", [file_id])?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO edges
                (file_id, kind, source, target, line, column, provenance, metadata)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )?;
            for edge in &graph.edges {
                insert.execute(params![
                    file_id,
                    edge.kind.as_str(),
                    edge.source,
                    edge.target,
                    edge.line,
                    edge.column,
                    edge.provenance.as_str(),
                    serde_json::to_string(&edge.metadata)?
                ])?;
            }
            let mut insert = tx.prepare(
                "INSERT INTO edges
                (file_id, source, target, ref_direction, ref_name, receiver_name, kind, arity, line, column, metadata)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )?;
            for reference in &graph.pending_refs {
                let (source, target) = match reference.direction {
                    RefDirection::In => (None, Some(&reference.owner_id)),
                    RefDirection::Out => (Some(&reference.owner_id), None),
                };
                insert.execute(params![
                    file_id,
                    source,
                    target,
                    reference.direction.as_str(),
                    reference.ref_name,
                    reference.receiver_name,
                    super::EdgeKind::from(reference.ref_kind).as_str(),
                    reference.arity,
                    reference.line,
                    reference.column,
                    serde_json::to_string(&reference.metadata)?
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Deletes owned rows and invalidates references to either endpoint, including
    /// references to the file itself. Repeating deletion is safe.
    ///
    /// # Errors
    /// Rejects empty IDs and SQLite failures. The complete mutation is atomic.
    pub fn delete_file_graph(&mut self, file_id: &str, old_entity_ids: &[String]) -> Result<()> {
        self.write_file_graph(file_id, &FileGraph::default(), old_entity_ids)
    }
}

fn validate(file_id: &str, graph: &FileGraph, old_entity_ids: &[String]) -> Result<()> {
    nonempty(file_id)?;
    for id in old_entity_ids {
        nonempty(id)?;
    }
    let mut local = HashSet::new();
    for id in &graph.entity_ids {
        nonempty(id)?;
        if id == file_id || !local.insert(id.as_str()) {
            return Err(Error::InvalidInput(
                "entity IDs must be unique and distinct from the file ID",
            ));
        }
    }
    let owns = |id: &str| id == file_id || local.contains(id);
    for edge in &graph.edges {
        if edge.provenance != Provenance::FileLocal || !owns(&edge.source) || !owns(&edge.target) {
            return Err(Error::InvalidInput(
                "file snapshots require local edges; use apply_resolutions for cross-file edges",
            ));
        }
        if edge.line == Some(0) {
            return Err(Error::InvalidInput("edge lines must be one-based"));
        }
    }
    for reference in &graph.pending_refs {
        nonempty(&reference.ref_name)?;
        if !owns(&reference.owner_id) || reference.line == 0 {
            return Err(Error::InvalidInput(
                "pending refs require local ownership and one-based lines",
            ));
        }
    }
    Ok(())
}
