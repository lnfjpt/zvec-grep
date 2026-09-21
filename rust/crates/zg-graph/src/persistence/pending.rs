use super::{
    Error, PendingRef, PendingRefPage, Provenance, Resolution, ResolutionStats, Result,
    SqliteGraphStorage, StoredPendingRef, decode_enum, decode_metadata, nonempty,
};
use rusqlite::{Row, TransactionBehavior, params};

const SELECT_REF: &str = "SELECT id, file_id, source, ref_name, receiver_name, kind, arity, line, column, metadata FROM edges";

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
            "{SELECT_REF} WHERE target IS NULL AND id > ? ORDER BY id LIMIT ?"
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
            let mut update = tx.prepare(
                "UPDATE edges SET target = ?, provenance = ?
                 WHERE id = ? AND target IS NULL",
            )?;
            for resolution in resolutions {
                let changed = update.execute(params![
                    resolution.target_id,
                    resolution.provenance.as_str(),
                    resolution.ref_id,
                ])?;
                if changed == 0 {
                    stats.stale += 1;
                } else {
                    stats.resolved += 1;
                }
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
            owner_id: row.get(2)?,
            ref_name: row.get(3)?,
            receiver_name: row.get(4)?,
            ref_kind: decode_enum(row.get(5)?)?,
            arity: row.get(6)?,
            line: row.get(7)?,
            column: row.get(8)?,
            metadata: decode_metadata(&row.get::<_, String>(9)?)?,
        },
    })
}
