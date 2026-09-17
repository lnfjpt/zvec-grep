# zg-engine

`zg-engine` is the Rust library for indexing files and retrieving content through lexical and vector search.

## Usage

Work in progress. Usage examples will be added once the engine is stable.

## Behavior

**Generated files.** Each workspace keeps its state under `.zvec-grep` in the source root:

- `manifest.json` stores workspace configuration and identifies the active index.
- `generations/` holds index data; `build.json` records an unfinished build.
- `authorization.json` stores saved consent for remote embedding services, when granted.

The engine also uses per-user files: `~/.zvec-grep/workspaces.json` registers workspace names and locations, and `~/.zvec-grep/config.json` stores settings when saved. Saved remote consent uses a signing key at `~/.zvec-grep/authorization.key` by default. Lock and recovery files are managed automatically.

**Models.** Local models download missing assets on first use and process content locally. On macOS and Linux, the default model cache is `~/.zvec-grep/models`. Set `ZVEC_GREP_MODEL_CACHE` to choose a cache directory. Remote models send content or queries to the configured embedding endpoint and require authorization.

**Index updates.** Repeated indexing reuses unchanged files, updates changed files, and removes deleted files from the index. Unsupported files are skipped. `drop_index` removes index data, workspace configuration, and the name reservation while preserving source files and shared model caches.

**Workspace names.** Names are case-sensitive and unique within the per-user registry; new workspaces default to the root directory's name. Use `IndexOptions::name` to choose or change a name. Move the source root together with `.zvec-grep`; the next index operation updates its registered location once the original root no longer exists. A copy of an existing workspace needs a different name.

**Rebuilds and recovery.** An incompatible index format requires an explicit rebuild with `IndexOptions::rebuild`. Repeating `index` resumes an interrupted build when its settings are compatible. Failed or cancelled rebuilds preserve the previous active index; the new index replaces it after successful completion.

## Modules

- **API** defines public request and result types.
- **Service** connects engine capabilities behind `ZvecGrep`.
- **Domain** defines shared data types for workspaces, sources, content, entities, and metadata.
- **Extraction** turns source files into content and metadata.
- **Models** provides embedding backends and manages model runtimes.
- **Storage** persists indexed data and supports lexical and vector search with Zvec.
- **Workspace** manages workspace names, configuration, index locations, and locks.
- **Pipelines** coordinates indexing and search.
- **Lexical** searches source files directly with embedded grep.
- **Authorization** manages consent for sending data to remote embedding services.
- **Config** manages global settings and resolves runtime configuration.
- **Error** defines engine errors and diagnostic reports.
- **Utils** provides shared helpers for text, encoding, hashing, and filesystem operations.
