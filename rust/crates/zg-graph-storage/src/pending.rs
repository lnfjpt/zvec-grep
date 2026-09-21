use crate::{
    Error, PendingRef, PendingRefPage, Provenance, Resolution, ResolutionStats, Result,
    SqliteGraphStorage, StoredPendingRef, decode_enum, decode_metadata, nonempty,
};
use rusqlite::{OptionalExtension, Row, TransactionBehavior, params};

const SELECT_REF: &str = "SELECT id, token, file_id, owner_id, ref_name, receiver_name, ref_kind, arity, line, column, metadata FROM pending_refs";

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

    /// Atomically applies valid pending-ref tokens, skipping stale/already applied
    /// proposals. Local edges are retained. Target existence/version validation
    /// and serialization with workspace changes belong to the calling pipeline.
    /// # Errors
    /// Rejects invalid proposals and SQLite failures; the entire batch rolls back.
    pub fn apply_resolutions(&mut self, resolutions: &[Resolution]) -> Result<ResolutionStats> {
        for resolution in resolutions {
            nonempty(&resolution.ref_token)?;
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
            let mut find = tx.prepare(&format!(
                "{SELECT_REF} WHERE id = ? AND token = ? AND status = 'pending'"
            ))?;
            let mut insert = tx.prepare(
                "INSERT INTO edges
                (file_id, ref_id, kind, source, target, line, column, provenance, metadata)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )?;
            let mut mark =
                tx.prepare("UPDATE pending_refs SET status = 'resolved' WHERE id = ?")?;
            for resolution in resolutions {
                let reference = find
                    .query_row(
                        params![resolution.ref_id, resolution.ref_token],
                        ref_from_row,
                    )
                    .optional()?;
                let Some(stored) = reference else {
                    stats.stale += 1;
                    continue;
                };
                let reference = stored.reference;
                insert.execute(params![
                    stored.file_id,
                    stored.id,
                    crate::EdgeKind::from(reference.ref_kind).as_str(),
                    reference.owner_id,
                    resolution.target_id,
                    reference.line,
                    reference.column,
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
        token: row.get(1)?,
        file_id: row.get(2)?,
        reference: PendingRef {
            owner_id: row.get(3)?,
            ref_name: row.get(4)?,
            receiver_name: row.get(5)?,
            ref_kind: decode_enum(row.get(6)?)?,
            arity: row.get(7)?,
            line: row.get(8)?,
            column: row.get(9)?,
            metadata: decode_metadata(&row.get::<_, String>(10)?)?,
        },
    })
}
