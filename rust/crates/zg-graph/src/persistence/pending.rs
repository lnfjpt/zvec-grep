use super::{
    Error, PendingRef, PendingRefPage, Provenance, Resolution, ResolutionStats, Result,
    SqliteGraphStorage, StoredPendingRef, decode_enum, decode_metadata, nonempty,
};
use rusqlite::{OptionalExtension, Row, TransactionBehavior, params};

const SELECT_REF: &str = "SELECT id, file_id, from_node_id, reference_name, receiver_name, reference_kind, arity, line, col, metadata, candidates, file_path, language, name_tail FROM unresolved_refs";

impl SqliteGraphStorage {
    /// Lists only pending refs using keyset pagination; no snapshot spans pages.
    /// Restart with cursor 0 after file mutations. Limit must be 1..=1000.
    /// # Errors
    /// Rejects invalid limits/cursors and malformed stored rows or SQLite errors.
    pub fn list_pending_refs(&self, limit: usize, cursor: i64) -> Result<PendingRefPage> {
        if !(1..=1000).contains(&limit) || cursor < 0 {
            return Err(Error::InvalidInput(
                "pending query requires limit 1..1000 and nonnegative cursor",
            ));
        }
        let mut statement = self.connection.prepare(&format!(
            "{SELECT_REF} WHERE status = 'pending' AND id > ? ORDER BY id LIMIT ?"
        ))?;
        let mut refs: Vec<_> = statement
            .query_map(params![cursor, limit + 1], ref_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        let more = refs.len() > limit;
        refs.truncate(limit);
        let next_cursor = if more {
            refs.last().map(|reference| reference.id)
        } else {
            None
        };
        Ok(PendingRefPage { refs, next_cursor })
    }

    /// Atomically applies proposals for existing pending refs, skipping missing
    /// or already resolved refs. The caller must serialize the entire read,
    /// resolution and writeback cycle with workspace writes/deletions.
    /// # Errors
    /// Rejects invalid proposals and SQLite failures; the entire batch rolls back.
    pub fn apply_resolutions(&mut self, resolutions: &[Resolution]) -> Result<ResolutionStats> {
        for resolution in resolutions {
            nonempty(&resolution.target_id)?;
            if resolution.ref_id < 1 || resolution.provenance == Provenance::FileLocal {
                return Err(Error::InvalidInput(
                    "resolution requires a positive ref ID and cross-file provenance",
                ));
            }
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut stats = ResolutionStats::default();
        {
            let mut find =
                tx.prepare(&format!("{SELECT_REF} WHERE id = ? AND status = 'pending'"))?;
            let mut insert = tx.prepare(
                "INSERT INTO edges
                (file_id, ref_id, kind, source, target, line, column, provenance, metadata)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )?;
            let mut mark =
                tx.prepare("UPDATE unresolved_refs SET status = 'resolved' WHERE id = ?")?;
            for resolution in resolutions {
                let reference = find
                    .query_row(params![resolution.ref_id], ref_from_row)
                    .optional()?;
                let Some(stored) = reference else {
                    stats.stale += 1;
                    continue;
                };
                let reference = stored.reference;
                insert.execute(params![
                    stored.file_id,
                    stored.id,
                    super::EdgeKind::from(reference.reference_kind).as_str(),
                    reference.from_node_id,
                    resolution.target_id,
                    reference.line,
                    reference.col,
                    resolution.provenance.as_str(),
                    serde_json::to_string(&reference.metadata)?
                ])?;
                mark.execute([stored.id])?;
                stats.resolved += 1;
            }
        }
        tx.commit()?;
        Ok(stats)
    }
}

fn ref_from_row(row: &Row<'_>) -> rusqlite::Result<StoredPendingRef> {
    Ok(StoredPendingRef {
        id: row.get(0)?,
        file_id: row.get(1)?,
        reference: PendingRef {
            from_node_id: row.get(2)?,
            reference_name: row.get(3)?,
            receiver_name: row.get(4)?,
            reference_kind: decode_enum(row.get(5)?)?,
            arity: row.get(6)?,
            line: row.get(7)?,
            col: row.get(8)?,
            metadata: decode_metadata(&row.get::<_, String>(9)?)?,
            candidates: row
                .get::<_, Option<String>>(10)?
                .map(|value| {
                    serde_json::from_str(&value).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            10,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                })
                .transpose()?,
            file_path: row.get(11)?,
            language: row.get(12)?,
            name_tail: row.get(13)?,
        },
    })
}
