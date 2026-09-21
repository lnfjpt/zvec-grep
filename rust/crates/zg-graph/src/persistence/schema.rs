use rusqlite::{Connection, TransactionBehavior};

use super::{Error, Result};

pub(crate) const VERSION: i64 = 6;
pub(crate) const APPLICATION_ID: i64 = 0x5a47_5250;

pub(crate) fn validate(connection: &Connection) -> Result<()> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let application_id: i64 =
        connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    if version != VERSION || application_id != APPLICATION_ID {
        return Err(Error::UnsupportedSchema {
            version,
            application_id,
        });
    }
    Ok(())
}

pub(crate) fn initialize(connection: &mut Connection) -> Result<()> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let application_id: i64 = tx.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    if version == 0 && application_id == 0 {
        let objects: i64 = tx.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        if objects != 0 {
            return Err(Error::ForeignDatabase);
        }
        tx.execute_batch(SCHEMA)?;
        tx.pragma_update(None, "user_version", VERSION)?;
        tx.pragma_update(None, "application_id", APPLICATION_ID)?;
    } else {
        validate(&tx)?;
    }
    tx.commit()?;
    Ok(())
}

const SCHEMA: &str = "
CREATE TABLE edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id TEXT NOT NULL,
    source TEXT NOT NULL,
    target TEXT,
    kind TEXT NOT NULL CHECK (kind IN ('contains', 'calls', 'imports', 'extends', 'implements')),
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'resolved', 'failed')),
    reference_name TEXT,
    receiver_name TEXT,
    arity INTEGER CHECK (arity >= 0),
    line INTEGER CHECK (line >= 1),
    col INTEGER CHECK (col >= 0),
    candidates TEXT CHECK (candidates IS NULL OR (json_valid(candidates) AND json_type(candidates) = 'array')),
    language TEXT NOT NULL DEFAULT 'unknown',
    name_tail TEXT NOT NULL DEFAULT '',
    provenance TEXT CHECK (provenance IN ('file_local', 'import_scoped', 'preferred_file', 'workspace_unique')),
    metadata TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata) AND json_type(metadata) = 'object'),
    CHECK ((status = 'resolved' AND target IS NOT NULL AND provenance IS NOT NULL)
        OR (status IN ('pending', 'failed') AND target IS NULL AND provenance IS NULL)),
    CHECK (
        (reference_name IS NULL AND status = 'resolved' AND provenance = 'file_local'
            AND receiver_name IS NULL AND arity IS NULL AND candidates IS NULL)
        OR
        (reference_name IS NOT NULL AND length(trim(reference_name)) > 0
            AND kind <> 'contains' AND line IS NOT NULL AND col IS NOT NULL
            AND (provenance IS NULL OR provenance <> 'file_local'))
    )
) STRICT;
CREATE INDEX edges_file ON edges(file_id);
CREATE INDEX edges_source_kind ON edges(source, kind);
CREATE INDEX edges_target_kind ON edges(target, kind);
CREATE INDEX edges_status_id ON edges(status, id);
CREATE INDEX edges_name_tail ON edges(name_tail);
";
