# zg-engine

`zg-engine` is the in-process Rust library for indexing files and retrieving relevant content with lexical and vector search.

One `ZvecGrep` instance can serve multiple workspaces and reuse embedding models across requests. Each workspace stores its manifest and persistent Zvec index under `.zvec-grep`.

Each workspace has one root directory. File records store paths relative to that root; scanning a subdirectory does not change the path base. Workspace-local zvec collections under `.zvec-grep/catalog` allocate numeric file and directory IDs. `file_identities` and `directories` retain path identities; `metadata` stores their allocation counters. A durable batch journal makes allocation recoverable before IDs are returned. Legacy `identity.json` catalogs are imported once and removed after publication. These IDs are independent of the workspace name and absolute root. Moving a workspace together with `.zvec-grep`, renaming the workspace, and rebuilding its index preserve those IDs. Unicode query paths use UTF-8 components separated by `/`; identity keys and stored source paths retain a lossless native encoding, including non-Unicode names. This does not promise that native index files or arbitrary filenames can be moved between operating systems. Reads resolve absolute paths against the workspace's current location.

## Workspace names

A workspace's name is its sole identity in the per-user registry. Names are globally unique within that registry and case-sensitive: `backend` and `Backend` are different names. New workspaces default to the root directory's basename. If that name is already registered, choose another with `IndexOptions::name`, `zg index --name NAME`, or the MCP index tool's `name` field.

Setting a different name on an existing workspace explicitly renames it. Renaming preserves file IDs and active storage; it does not require a rebuild. The registry is authoritative, so read-only `info` and `context` return its name even if a previous operation stopped before updating the manifest. Reads do not create registry files or register workspaces.

The registry lives at `~/.zvec-grep/workspaces.json`. `ZVEC_GREP_WORKSPACE_REGISTRY` selects another registry using an absolute file path. An index operation reserves the name before building. Failed or interrupted first builds retain that reservation; `drop_index` or `zg index --drop` removes it along with workspace index state, allowing the name to be reused.

After moving a workspace, its next index operation updates the registered root when the original workspace no longer exists. Copying a live workspace requires a different name for the copy. File identity is scoped to each workspace, so a copy may retain its local file catalog while using its own workspace name.

See [workspace ownership and persistence](src/workspace/README.md) for manifest migration and recovery details.

## Usage

```rust,no_run
use zg_engine::{
    EngineResult, ZvecGrep,
    api::{context::ContextOptions, index::IndexOptions},
};

async fn search_workspace() -> EngineResult<()> {
    let engine = ZvecGrep::new();
    let root = std::path::PathBuf::from("/workspace");

    engine.index(IndexOptions {
        root: Some(root.clone()),
        ..IndexOptions::default()
    }).await?;

    let result = engine.context(ContextOptions {
        root: Some(root),
        query: Some("authentication policy".to_owned()),
        ..ContextOptions::default()
    }).await?;

    println!("{} results", result.items.len());
    engine.close();
    Ok(())
}
```

The default embedding model runs locally and downloads its assets on first use. `IndexOptions::embedding` selects a model, device, endpoint, or cache directory. Index updates reuse unchanged files and remove deleted files. Existing indexes remain available after the engine closes.

## Public API

- `ZvecGrep::index` builds or updates a workspace index and applies an explicitly requested workspace name.
- `ZvecGrep::context` retrieves content through indexed search or embedded grep. Indexed search can refresh changed files before querying.
- `ZvecGrep::info` reports workspace configuration and optional file status.
- `ZvecGrep::drop_index` removes the index, manifest, file identity catalog, and workspace name reservation while preserving source files.
- `ZvecGrep::close` releases model resources and rejects subsequent operations.

Request and result types live in `api::{index, context, info}`. Shared types have one owner and are re-exported through their existing API paths: `DiscoveryOptions` names the domain's `FileSelection`, `SymbolType` belongs to domain entities, `Device` belongs to models, and search routes and evidence belong to the search pipeline. The application boundary builds API result views from internal state; workspace persistence and model runtimes do not depend on API result types. `EngineError::code()` supports programmatic error handling; `EngineError::report()` includes diagnostic context and source locations for logging. Other modules are private implementation details.

## Storage

The index keeps files, entities, fragments, and vectors in separate collections. Zvec's WAL handles native document recovery. Before each mutation, the engine persists a pending record containing only source metadata or a deletion intent. It flushes all collections and clears the records after 64 operations, an estimated 16 MiB of source and vector data, finalization, or a healthy writer close.

The shared `FileRecord` contains identity, relative path, detected formats, a `FileSnapshot`, and a `FileIndexStatus`. Each physical index generation stores its own file records. `DirectoryRecord` contains only an ID and relative path and lives in the workspace catalog. Historical directory identities do not imply that the directory currently has indexed files. The generation file table stores derived filename and ancestor-directory indexes and a lossless path projection; path-only filtering enumerates that projection without decoding every file snapshot. Snapshots contain source size in bytes, optional modification time in unsigned Unix epoch milliseconds, and an optional content hash. Successfully indexed records require the hash of the exact bytes used during extraction. Modification time is a fast comparison hint; when it is unavailable, updates compare content hashes. Source reads check one file handle before and after reading, including its identity against the current path, to detect observable concurrent changes.

File status is `NotIndexed`, `Indexed { indexed_epoch_ms, entity_count }`, or `Failed { error }`. A successful extraction may yield zero entities. Failed files retain no searchable artifacts; interrupted writes recover as `NotIndexed`. Entity counts are fixed-width `u64`. Token counts and embedding truncation counts are not persisted. Embedding backends may still truncate inputs that exceed their limits after fragment splitting. Workspace status reports `indexed_size_bytes`, the sum of successfully indexed source snapshots, separately from index storage size.

Reopening an interrupted batch removes its partial index data and marks affected files for reindexing before serving reads. The next indexing pass rereads those files and recomputes their embeddings, even if the source has not changed; completed checkpoints remain available. The versioned storage codec preserves numeric format IDs and encodes image bytes as base64.

Index version 5 uses the unified file record and requires a rebuild of earlier indexes (`IndexOptions::rebuild` or `zg index --rebuild`). Old single-root manifests can still supply names, embedding, and discovery settings for the rebuild. Manifest version 4 retains the flat JSON representation, ignores a legacy workspace UUID `id`, and writes only the workspace name. Manifest migration does not change physical index version 5.

Initial builds and rebuilds run in a generation directory under `.zvec-grep/generations`. A durable `build.json` identifies the target version and processing settings. Repeating `index` resumes a compatible interrupted build, reuses its completed checkpoints, and reconciles the full configured workspace. Failed or cancelled rebuilds leave the previous active index intact. After successful finalization and close, one atomic manifest update selects the new generation, then old storage is cleaned up. Interrupted publication and cleanup are recovered on the next index operation. There is no separate extraction version or per-file index version. Queries use the active generation; automatic refresh and watcher updates do not resume an unfinished rebuild.

Full-text search uses the Jieba dictionaries shipped and discovered by zvec-rust 0.7.1. Native distributions must include the SDK’s `data/jieba_dict` directory alongside its shared library. Storage schemas before version 8 require an index rebuild to remove their persisted dictionary paths. Files without a supported extractor are skipped; format recognition alone does not imply parsing support.

## Native runtime

The Zvec dependency obtains its native shared library during the build. Standalone applications must deploy that library and configure their platform's loader; Cargo's development environment is not a deployment bundle. `ZVEC_LIB_DIR` can select a library directory at build time.

- macOS: ship `lib/libzvec_c_api.dylib` beside the executable and link the final binary with `-Wl,-rpath,@executable_path/lib`.
- Linux: ship `lib/libzvec_c_api.so` beside the executable and set the final binary's rpath to `$ORIGIN/lib`.
- Windows: ship `zvec_c_api.dll` beside the executable.

Apply the target platform's signing requirements to distributed binaries and libraries.

## Modules

- `domain` defines workspace names and logical index metadata, file selection, file records and states, formats, ranges, atomic content, and entities.
- `extraction` turns supported text, code, Markdown, and image sources into content fragments and metadata. Callers attach file and entity identities. Recognizing a format does not imply that a decoder or model supports it.
- `models` owns embedding backends and reusable model runtimes.
- `storage` persists files, entities, fragments, and vectors with Zvec.
- `workspace` manages the per-user name registry, manifest encoding, physical index generations, index locations, and workspace locks.
- `pipelines::indexing` coordinates discovery, extraction, embedding, and incremental updates.
- `pipelines::search` plans indexed queries, combines results, and assembles context.
- `lexical` provides embedded grep retrieval.
- `service` composes these capabilities behind `ZvecGrep`.

## Source ranges

- `File` addresses the complete original file.
- `Byte` addresses a continuous span of original file bytes. The selected bytes need not form an independently decodable resource.
- `Text` addresses a continuous span of the complete decoded UTF-8 source. Each endpoint records a global byte offset, a line number, and a byte column. Coordinates are read through `TextRange` methods; its endpoints remain private.

Extractors choose the range that locates their output. Ranges describe positions; loading and parsing belong to the source-reading code. Range comparisons assume the same unchanged source file and coordinate space. Extraction and lexical search use the same text coordinates.

## Text conventions

- Internal text is UTF-8. Decode sources consistently, remove the encoding BOM, and preserve original line endings and whitespace.
- Text positions count UTF-8 bytes. Distinguish offsets in the full decoded source from columns within a line.
- Byte spans and columns are zero-based and half-open (`[start, end)`); empty spans are valid and line numbers are one-based. An endpoint immediately after a newline is column zero of the next line. Exact text reads reject offsets that are out of bounds or split a UTF-8 character.
- Source positions refer to the same file version and decoded text used during extraction. Outlines, normalized text, and other derived content retain their original source locations.
- Raw byte ranges and file content hashes refer to the original file bytes, independently of text decoding.
- Length limits state their units. Chunk budgets count UTF-16 code units; model limits count tokens. Truncation preserves character boundaries.
- Storage and public APIs preserve coordinate units and their reference points. Changes to these conventions require versioned compatibility handling.

## Development

Run from the repository root:

```sh
cargo check -p zg-engine --all-targets
cargo test -p zg-engine
RUSTDOCFLAGS="-D warnings" cargo doc -p zg-engine --no-deps
```
