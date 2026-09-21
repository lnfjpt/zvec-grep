# Graph storage

`zg-graph-storage` ports the TypeScript graph persistence protocol to Rust. It is
an independent Cargo workspace member so its SQLite behavior can be validated
without loading embeddings, zvec, or a daemon. `rusqlite` owns the connection and
builds a bundled SQLite; Node.js and a system SQLite installation are not needed.

This is the storage-only migration. The engine does not use the crate yet.
Extraction, indexing orchestration, zvec symbol lookup, FTS fallback, resolvers,
MCP/CLI tools, and engine API methods belong in subsequent integration changes.

## Ownership

Only two tables are stored: `edges` and `pending_refs`. Entity content, names,
paths, ranges and other node metadata remain in zvec. `FileGraph.entity_ids`
contains the current file's entity IDs for ownership validation; these IDs are
not persisted in a second entity/file table. IDs are opaque strings at this
boundary; integration must encode each engine file ID consistently for both
ownership and file-level endpoints, using the existing entity IDs for nodes.

`SqliteGraphStorage::open(path, OpenMode::ReadWrite)` initializes an empty graph
and enables WAL, foreign keys, and a busy timeout. Read-only opening neither
creates a missing database nor migrates its schema. Both modes validate the
application ID and schema version. An unrelated or unsupported database is
rejected. Drop closes a connection; `close(self)` also reports close failures.
All operations are synchronous. Writes require `&mut self` and use SQLite
`BEGIN IMMEDIATE` transactions; the calling engine chooses its blocking boundary.

## File snapshots and invalidation

- `write_file_graph(file_id, graph, old_entity_ids)` atomically replaces owned
  edges and refs. Snapshots accept only local edges and locally owned pending
  references. The file ID itself is an implicit local endpoint.
- `delete_file_graph(file_id, old_entity_ids)` removes owned rows and invalidates
  incoming cross-file edges. Repeated deletion is safe.
- Both require **all pre-update entity IDs from zvec**, including removed
  definitions. Pass `[]` for a new file. Replacing a target with the same ID also
  invalidates its incoming resolved references.
- File-level import targets are invalidated even when the entity-ID list is empty.
  Large ID lists are processed in bounded SQL parameter batches within one transaction.
- Invalidation removes resolved edges, returns their references to pending, and
  rotates tokens so stale workers cannot recreate invalidated edges.

Transactions cover SQLite only. The indexer must coordinate graph writes with
zvec's file-update journal/checkpoints and use the workspace write lock. Do not
publish an index if only one store has committed. This crate deliberately does
not add another workspace recovery journal or a ready marker.

## Reference resolution protocol

1. `list_pending_refs(limit, cursor)` returns pending refs and their identity
   tokens. Use cursor `0` initially; limits are `1..=1000`. `next_cursor` is only
   present when another page exists. Restart pagination after file changes.
2. A language-aware resolver selects and validates a target in the entity store.
3. `apply_resolutions(&results)` inserts the resulting edges and updates statuses
   atomically. Missing, changed or already-resolved references count as `stale`;
   local edges are preserved. Malformed inputs or SQL errors roll back the batch.

Target existence/version validation and writeback must be serialized with file
updates/deletions by the coordinator. A reference token protects the reference's
state; it does not prove that an arbitrary target still exists. Symbol-index
changes that introduce ambiguity may also require re-resolving references; the
indexing/resolver integration must define that policy.

## Queries

`get_callers`, `get_callees`, `get_imports`, `get_inheritance`, `get_subclasses`,
and `get_implementations` return stored edges in insertion order. Imports are
filtered by the **owning file**, while other queries filter endpoints. Pending
refs and node metadata are excluded. Distinct stored call sites are preserved.
There is no neighborhood API, fuzzy lookup, ranking, or query limit here; result
formatting, per-symbol limits and totals belong in the relationship pipeline.

## Checks

Run from `rust/`:

```sh
cargo fmt -p zg-graph-storage --check
cargo check -p zg-graph-storage --all-targets
cargo clippy -p zg-graph-storage --all-targets -- -D warnings
cargo test -p zg-graph-storage
RUSTDOCFLAGS="-D warnings" cargo doc -p zg-graph-storage --no-deps
```

Tests exercise real SQLite connections: read-only opening, schema guards,
replacement/deletion, reverse invalidation, stale tokens/row-ID reuse, keyset
pagination, SQL-trigger-injected transaction failures, multi-connection
visibility, large files, metadata round trips and all six relationship queries.
