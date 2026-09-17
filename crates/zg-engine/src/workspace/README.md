# Workspace persistence

`workspace` connects domain workspace state to the filesystem. It owns the name registry, manifest encoding, physical index locations, build publication and recovery, and workspace locks. The domain owns `Workspace`, `FileSelection`, `IndexState`, `IndexDescriptor`, and `EmbeddingSchema`; API result views are assembled at the application boundary.

## Names and the registry

The name is the sole per-user workspace identity. Names are stored as strings and validated by `Workspace::validate_name` at request, manifest, and registry boundaries. Names must be nonempty, must not be `.` or `..`, and must not contain surrounding whitespace, path separators, or control characters. Names are case-sensitive and unique across the registry, even when source directories are unrelated. A root directory can have one registered name. New workspaces use the root basename unless the caller supplies `IndexOptions::name`, CLI `--name`, or MCP `name`. A basename collision requires an explicit different name.

The default registry is `~/.zvec-grep/workspaces.json`. Set `ZVEC_GREP_WORKSPACE_REGISTRY` to an absolute file path to select another registry, including an isolated registry for tests. Registry mutations use a separate lock and atomic replacement. Lookup does not create the registry or its lock file.

Indexing reserves a name before the first build. A failed or interrupted build keeps the reservation so a retry retains its identity. Dropping the workspace index releases the reservation, including a reservation left by an unsuccessful first build, and preserves source files. If the source directory was deleted externally, an explicit drop at its previous path releases the name without recreating that directory.

## Renaming and relocation

An explicit new name on an existing workspace renames its registry entry:

```sh
zg index /path/to/workspace --name search-engine
```

The registry is authoritative. Read-only `info` and `context` report its current name without writing metadata. The next index operation reconciles a stale manifest or pending build with the registry. Renaming itself retains file IDs and active storage and does not require a rebuild.

Move the root together with its `.zvec-grep` directory to retain workspace state. Reads resolve source paths against the new root. The next index operation updates the registered location after checking that the original root directory no longer exists. A live copy cannot claim the original's name; index the copy with its own `--name`.

The name identifies the workspace across these operations. The root records its current location. File IDs belong to source records in one physical index generation. A writer loads a path-to-ID cache from these records once; unchanged files reuse their recorded IDs, and only previously unrecorded paths need allocation. Reservations stay in memory until source records enter the existing pending-write journal. There is no independent identity collection or allocation journal; directory IDs are allocated in memory. File and directory IDs are separate `u32` spaces; checked allocation rejects exhaustion without wrapping or partially reserving a batch.

File IDs remain unchanged while a record is updated, renamed at the workspace level, or relocated with its index. Rebuilds can assign different IDs. Deletion ends a record's identity; IDs may be reused after reopening once the old record is gone. Entity IDs derived from file IDs are therefore index-local references, not durable external handles. Unwritten reservations do not survive reopening. Directory filters use indexed arrays of 4-byte `u32` directory IDs on source and retrieval documents. The index-local directory map is derived from source paths and membership IDs, resolved in memory, and cached once per directory in `storage/directories.json`. The cache is checkpointed before clearing the file write journal; missing or invalid caches can be reconstructed, and recovery always derives the map from source records.

Old `catalog` / `identity.json` data is ignored by the new format and removed only after successful publication of a rebuilt index, or by explicit drop. Failed rebuilds retain the old index and its metadata. The global name registry does not store file or directory records.

## Manifest and physical index versions

`WorkspaceManifest` combines a domain `Workspace` with persistence-specific fields: manifest version, workspace home, physical index version, active storage generation, and embedding runtime configuration. It does not embed an API `WorkspaceIndexInfo`.

Manifest version 4 retains the existing flat JSON format. Loading an older compatible single-root manifest ignores its UUID `id`; subsequent writes preserve the name and omit the UUID. Names from old manifests must still satisfy registry uniqueness when indexing. The migration keeps embedding and source selection settings.

The unreleased physical index format is version 5; the previous released format is version 4. Manifest version, physical index version, logical index revision, and generation-directory UUID describe different things. A manifest migration does not by itself rebuild storage; a mismatched physical index version requires the user to run `index --rebuild`. Ordinary indexing and query refresh report the incompatibility without automatically rebuilding. The storage schema has its own revision for development-time layout changes. Logical index revisions can update the same physical generation; they do not create immutable per-file snapshots.

## Publication and recovery

First builds and rebuilds write under `.zvec-grep/generations`, with a durable `build.json` describing the pending work. A compatible retry resumes the build. An atomic manifest replacement selects the completed generation; cleanup removes the previous storage afterward. Interrupted cleanup is recovered by a subsequent index operation. Failed rebuilds keep the active index available.

The indexing application service acquires the workspace lock before mutating its registry entry and metadata. The registry's lock protects names across independent workspace roots. Storage owns collection recovery, pending file mutations, and the index-local file ID cache; these mechanisms remain separate from workspace naming.

## Persisted state and runtime observations

`Workspace.index` is `Uninitialized`, `Disabled`, or `Enabled(IndexDescriptor)`. Enabled always carries the embedding schema and logical revision; physical format and generation-directory layout remain owned by `WorkspaceManifest`. A first build or rebuild keeps its target in `WorkspaceBuild` until publication. The selected descriptor does not imply that files are up to date or that an indexing job succeeded.

The daemon's `WorkspaceRuntime` holds an optional `IndexStatusSnapshot`: observed health, file statistics, and inspection time. No snapshot means unchecked or invalidated. Watcher changes, indexing submissions and completions (including failure/cancellation), and drop invalidate it. Epoch and job checks reject scans overlapping these changes. Runtime snapshots can read this memory without scanning; explicit CLI/MCP status still reads disk and refreshes the observation. Watchers may miss external changes, so an observation is not a lasting freshness guarantee. Restart starts without an observation.

`IndexStats` contains counts. `IndexStatus` distinguishes unknown, uninitialized, disabled, missing, ready, stale, and failed. Queued/running/cancelled build states remain in the scheduler. CLI readiness requires a completed check with no failed, pending, added, modified, or deleted files.
