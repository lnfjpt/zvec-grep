use rusqlite::{Connection, TransactionBehavior};

use super::{Error, Result};

pub(crate) const VERSION: i64 = 4;
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
CREATE TABLE unresolved_refs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id TEXT NOT NULL,
    from_node_id TEXT NOT NULL,
    reference_name TEXT NOT NULL,
    receiver_name TEXT,
    reference_kind TEXT NOT NULL CHECK (reference_kind IN ('calls', 'imports', 'extends', 'implements')),
    arity INTEGER CHECK (arity >= 0),
    line INTEGER NOT NULL CHECK (line >= 1),
    col INTEGER NOT NULL CHECK (col >= 0),
    candidates TEXT CHECK (candidates IS NULL OR (json_valid(candidates) AND json_type(candidates) = 'array')),
    file_path TEXT NOT NULL DEFAULT '',
    language TEXT NOT NULL DEFAULT 'unknown',
    name_tail TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'resolved', 'failed')),
    metadata TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata) AND json_type(metadata) = 'object')
) STRICT;
CREATE INDEX unresolved_refs_file ON unresolved_refs(file_id);
CREATE INDEX unresolved_refs_status_id ON unresolved_refs(status, id);
CREATE INDEX unresolved_refs_name_tail ON unresolved_refs(name_tail);
CREATE TABLE edges (
    id INTEGER PRIMARY KEY,
    file_id TEXT NOT NULL,
    ref_id INTEGER REFERENCES unresolved_refs(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('contains', 'calls', 'imports', 'extends', 'implements')),
    source TEXT NOT NULL,
    target TEXT NOT NULL,
    line INTEGER CHECK (line >= 1),
    column INTEGER CHECK (column >= 0),
    provenance TEXT NOT NULL CHECK (provenance IN ('file_local', 'import_scoped', 'preferred_file', 'workspace_unique')),
    metadata TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata) AND json_type(metadata) = 'object')
) STRICT;
CREATE INDEX edges_file ON edges(file_id);
CREATE INDEX edges_source_kind ON edges(source, kind);
CREATE INDEX edges_target_kind ON edges(target, kind);
CREATE UNIQUE INDEX edges_ref ON edges(ref_id) WHERE ref_id IS NOT NULL;
";
