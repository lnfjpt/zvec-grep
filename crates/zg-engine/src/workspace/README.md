# Workspace persistence

`workspace` connects domain workspace state to the filesystem. It owns the name registry, manifest encoding, physical index locations, build publication and recovery, and workspace locks. The domain owns `Workspace`, `WorkspaceName`, `FileSelection`, `IndexPolicy`, and `EmbeddingSchema`; API result views are assembled at the application boundary.

## Names and the registry

The name is the sole per-user workspace identity. Names are case-sensitive and unique across the registry, even when source directories are unrelated. A root directory can have one registered name. New workspaces use the root basename unless the caller supplies `IndexOptions::name`, CLI `--name`, or MCP `name`. A basename collision requires an explicit different name.

The default registry is `~/.zvec-grep/workspaces.json`. Set `ZVEC_GREP_WORKSPACE_REGISTRY` to an absolute file path to select another registry, including an isolated registry for tests. Registry mutations use a separate lock and atomic replacement. Lookup does not create the registry or its lock file.

Indexing reserves a name before the first build. A failed or interrupted build keeps the reservation so a retry retains its identity. Dropping the workspace index releases the reservation, including a reservation left by an unsuccessful first build, and preserves source files. If the source directory was deleted externally, an explicit drop at its previous path releases the name without recreating that directory.

## Renaming and relocation

An explicit new name on an existing workspace renames its registry entry:

```sh
zg index /path/to/workspace --name search-engine
```

The registry is authoritative. Read-only `info` and `context` report its current name without writing metadata. The next index operation reconciles a stale manifest or pending build with the registry. Renaming itself retains file IDs and active storage and does not require a rebuild.

Move the root together with its `.zvec-grep` directory to retain workspace state. Reads resolve source paths against the new root. The next index operation updates the registered location after checking that the original root directory no longer exists. A live copy cannot claim the original's name; index the copy with its own `--name`.

The name identifies the workspace across these operations. The root records its current location. Numeric file and directory IDs belong to workspace-local zvec collections under the resolved workspace home: `catalog/file_identities`, `catalog/directories`, and `catalog/metadata`. Paths are relative to the workspace root. Rebuilding and workspace renaming do not replace these collections; explicit drop removes them. A copied workspace can reuse numeric values in its own independent catalog. Legacy `identity.json` is imported once before removal. The global name registry does not store file or directory records.

## Manifest and physical index versions

`WorkspaceManifest` combines a domain `Workspace` with persistence-specific fields: manifest version, workspace home, physical index version, active storage generation, and embedding runtime configuration. It does not embed an API `WorkspaceIndexInfo`.

Manifest version 4 retains the existing flat JSON format. Loading an older compatible single-root manifest ignores its UUID `id`; subsequent writes preserve the name and omit the UUID. Names from old manifests must still satisfy registry uniqueness when indexing. The migration keeps embedding and source selection settings.

The physical index format remains version 5. Manifest version, physical index version, logical index revision, and generation-directory UUID describe different things. A manifest migration does not by itself rebuild storage; physical indexes older than version 5 require `--rebuild`. The storage schema has its own revision for development-time layout changes. Logical index revisions can update the same physical generation; they do not create immutable per-file snapshots.

## Publication and recovery

First builds and rebuilds write under `.zvec-grep/generations`, with a durable `build.json` describing the pending work. A compatible retry resumes the build. An atomic manifest replacement selects the completed generation; cleanup removes the previous storage afterward. Interrupted cleanup is recovered by a subsequent index operation. Failed rebuilds keep the active index available.

The indexing application service acquires the workspace lock before mutating its registry entry and metadata. The registry's lock protects names across independent workspace roots. Storage owns collection recovery, pending file mutations, and the file identity catalog; these mechanisms remain separate from workspace naming.
