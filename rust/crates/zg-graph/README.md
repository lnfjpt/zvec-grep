# Graph

The `zg_graph::persistence` module ports the TypeScript graph persistence protocol
to Rust. The `zg-graph` crate is
an independent Cargo workspace member so its SQLite behavior can be validated
without loading embeddings, zvec, or a daemon. `rusqlite` owns the connection and
builds a bundled SQLite; Node.js and a system SQLite installation are not needed.

This is the storage-only migration. The engine does not use the crate yet.
Extraction, indexing orchestration, zvec symbol lookup, FTS fallback, resolvers,
MCP/CLI tools, and engine API methods belong in subsequent integration changes.

## Structure

```text
src/
├── lib.rs
└── persistence/
    ├── mod.rs
    ├── types.rs
    ├── schema.rs
    ├── writer.rs
    ├── reader.rs
    └── pending.rs
```

Storage types are exposed under `zg_graph::persistence`.

## Ownership

Only one table is stored: `edges`. An unresolved reference has a NULL target
and provenance; resolution fills those columns on the same row. Original
reference details remain available after resolution. There is no separate
reference table, foreign key or status column. Entity content, names,
paths, ranges and other node metadata remain in zvec. `FileGraph.entity_ids`
contains the current file's entity IDs for ownership validation; these IDs are
not persisted in a second entity/file table. IDs are opaque strings at this
boundary; integration must encode each engine file ID consistently for both
ownership and file-level endpoints, using the existing entity IDs for nodes.

`SqliteGraphStorage::open(path, OpenMode::ReadWrite)` initializes an empty graph
and enables WAL and a busy timeout. Read-only opening neither
creates a missing database nor migrates its schema. Both modes validate the
application ID and schema version. An unrelated or unsupported database is
rejected. Schema version 2 replaces the earlier two-table layout; existing
graph databases must be rebuilt, with no migration provided. Drop closes a connection; `close(self)` also reports close failures.
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
- Invalidation clears the target and provenance of incoming reference rows,
  preserving their IDs and original reference details for another resolution pass.

Transactions cover SQLite only. The indexer must coordinate graph writes with
zvec's file-update journal/checkpoints and use the workspace write lock. Do not
publish an index if only one store has committed. This crate deliberately does
not add another workspace recovery journal or a ready marker.

## Reference resolution protocol

1. `list_pending_refs(limit, cursor)` returns pending refs and their database IDs. Use cursor `0` initially; limits are `1..=1000`. `next_cursor` is only
   present when another page exists. Restart pagination after file changes.
2. A language-aware resolver selects and validates a target in the entity store.
3. `apply_resolutions(&results)` updates target and provenance on the existing rows
   atomically. Missing or already-resolved references count as `stale`;
   local edges are preserved. Malformed inputs or SQL errors roll back the batch.

The coordinator must hold the workspace write lock across the entire reference
read, target lookup/validation and writeback cycle, and prevent file mutations
through the same writer during that cycle. Storage does not detect outdated
results for reused or invalidated reference IDs. Graph storage is not yet
connected to the engine's workspace lock.
Symbol-index changes that introduce ambiguity may also require re-resolving
references; the indexing/resolver integration must define that policy.

## Queries

`get_callers` and `get_callees` return incoming and outgoing call edges in
insertion order. Other edge kinds, pending refs and node metadata are excluded.
`neighborhood(id, direction, kinds)` returns one-hop edges for `Direction::In`,
`Out`, or `Both`. Pass `None` for all kinds or `Some(&[])` for none. Self-loops
appear once, and distinct stored call sites are preserved. All three queries
have no limit parameter and return all matching edges in insertion order.
Fuzzy lookup, ranking, result formatting, per-symbol limits and totals belong
in the relationship pipeline.

## Checks

Run from `rust/`:

```sh
cargo fmt -p zg-graph --check
cargo check -p zg-graph --all-targets
cargo clippy -p zg-graph --all-targets -- -D warnings
cargo test -p zg-graph
RUSTDOCFLAGS="-D warnings" cargo doc -p zg-graph --no-deps
```

Tests exercise real SQLite connections: read-only opening, schema guards,
replacement/deletion, reverse invalidation, missing/already-resolved references, keyset
pagination, SQL-trigger-injected transaction failures, multi-connection
visibility, large files, metadata round trips and both call relationship queries.
