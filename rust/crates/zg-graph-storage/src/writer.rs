use rusqlite::{TransactionBehavior, params, params_from_iter};
use std::collections::HashSet;

use crate::{Error, FileGraph, Provenance, Result, SqliteGraphStorage, nonempty};

impl SqliteGraphStorage {
    /// Atomically replaces one file's local graph and invalidates inbound edges.
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
        let mut targets: Vec<&str> = old_entity_ids.iter().map(String::as_str).collect();
        targets.push(file_id);
        targets.sort_unstable();
        targets.dedup();
        for chunk in targets.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            // Retain the original reference for the next resolution pass.
            tx.execute(
                &format!("UPDATE pending_refs SET status = 'pending'
                  WHERE id IN (SELECT ref_id FROM edges WHERE target IN ({placeholders}) AND file_id <> ?)"),
                params_from_iter(chunk.iter().copied().chain(std::iter::once(file_id))),
            )?;
            tx.execute(
                &format!("DELETE FROM edges WHERE target IN ({placeholders})"),
                params_from_iter(chunk.iter().copied()),
            )?;
        }
        tx.execute("DELETE FROM edges WHERE file_id = ?", [file_id])?;
        tx.execute("DELETE FROM pending_refs WHERE file_id = ?", [file_id])?;
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
            let mut insert = tx.prepare("INSERT INTO pending_refs
                (file_id, owner_id, ref_name, receiver_name, ref_kind, arity, line, column, status, metadata)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?)")?;
            for reference in &graph.pending_refs {
                insert.execute(params![
                    file_id,
                    reference.owner_id,
                    reference.ref_name,
                    reference.receiver_name,
                    crate::EdgeKind::from(reference.ref_kind).as_str(),
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

    /// Deletes owned rows and invalidates incoming references, including imports
    /// targeting the file itself. Repeating deletion is safe.
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
