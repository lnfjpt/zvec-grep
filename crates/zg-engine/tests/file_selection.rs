mod support;

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

use serde_json::{Value, json};
use support::{
    EmbeddingServer, configure_remote_model, index_options, info_options, native_file_records,
};
use zg_engine::{
    ZvecGrep,
    api::{
        context::{
            ContextOptions,
            options::{ContextRoute, ContextRouteMode},
        },
        index::{
            IndexOptions,
            options::{
                FileCategory, FileFilter, FileFilterUpdate, FileFormat, GlobRule, ScanOptions,
                ScanOptionsUpdate,
            },
        },
    },
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn write_sources(root: &Path, sources: &[(&str, &str)]) -> std::io::Result<()> {
    for (relative, contents) in sources {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture parent"))?;
        fs::write(path, contents)?;
    }
    Ok(())
}

fn paths(names: &[&str]) -> BTreeSet<PathBuf> {
    names.iter().map(PathBuf::from).collect()
}

fn stored_paths(index_path: &Path) -> TestResult<BTreeSet<PathBuf>> {
    Ok(native_file_records(index_path)?
        .iter()
        .map(|file| {
            PathBuf::from(
                file["value"]["relative_path"]["value"]
                    .as_str()
                    .expect("UTF-8 fixture path"),
            )
        })
        .collect())
}

async fn search_paths(
    engine: &ZvecGrep,
    root: &Path,
    mode: ContextRouteMode,
    filter: FileFilter,
) -> TestResult<BTreeSet<PathBuf>> {
    let result = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode,
                query: "orchard".into(),
            }],
            filter,
            limit: Some(64),
            auto_update: false,
            allow_remote: true,
            ..ContextOptions::default()
        })
        .await?;
    Ok(result
        .items
        .into_iter()
        .map(|item| item.relative_path)
        .collect())
}

async fn assert_index_and_queries(
    engine: &ZvecGrep,
    root: &Path,
    filter: &FileFilter,
    expected: &BTreeSet<PathBuf>,
) -> TestResult {
    let info = engine.info(info_options(root)).await?;
    assert_eq!(
        &stored_paths(&info.index_path)?,
        expected,
        "stored file membership"
    );
    for mode in [ContextRouteMode::Fts, ContextRouteMode::Vector] {
        assert_eq!(
            &search_paths(engine, root, mode, FileFilter::default()).await?,
            expected,
            "unfiltered {mode:?}"
        );
        assert_eq!(
            &search_paths(engine, root, mode, filter.clone()).await?,
            expected,
            "same selection in {mode:?}"
        );
    }
    assert_eq!(
        engine
            .info(info_options(root))
            .await?
            .workspace_index
            .expect("workspace")
            .filter,
        *filter,
        "query filters do not alter workspace configuration"
    );
    Ok(())
}

#[tokio::test]
async fn ordered_globs_have_the_same_membership_during_indexing_and_both_query_routes() -> TestResult
{
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    write_sources(
        root,
        &[
            ("src/main.rs", "/// orchard\npub fn main() {}\n"),
            ("src/derived/keep.rs", "/// orchard\npub fn keep() {}\n"),
            (
                "src/derived/nested/keep.rs",
                "/// orchard\npub fn nested() {}\n",
            ),
            ("blocked/keep.rs", "/// orchard\npub fn blocked() {}\n"),
            ("readme.md", "# orchard\n"),
        ],
    )?;
    let filter = FileFilter {
        globs: vec![
            GlobRule {
                pattern: "*.RS".into(),
                case_insensitive: true,
            },
            "!src/derived/**".into(),
            "src/derived/keep.rs".into(),
            "!blocked".into(),
            "blocked/keep.rs".into(),
        ],
        ..FileFilter::default()
    };
    let engine = ZvecGrep::new();
    let indexed = engine
        .index(IndexOptions {
            filter: FileFilterUpdate {
                globs: Some(filter.globs.clone()),
                ..FileFilterUpdate::default()
            },
            ..index_options(root)
        })
        .await?;
    assert_eq!((indexed.files_added, indexed.files_failed), (2, 0));
    assert_index_and_queries(
        &engine,
        root,
        &filter,
        &paths(&["src/main.rs", "src/derived/keep.rs"]),
    )
    .await?;
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
async fn nested_git_roots_are_scanned_and_explicit_globs_override_ignore_rules() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    write_sources(
        root,
        &[
            ("plain.txt", "orchard plain source"),
            ("nested/child.txt", "orchard nested source"),
            ("nested/.git/objects/private.txt", "orchard git internals"),
            (".gitignore", "ignored/\nblocked.txt\n"),
            ("blocked.txt", "orchard ignored file"),
            ("ignored/keep.txt", "orchard explicitly included subtree"),
        ],
    )?;
    let engine = ZvecGrep::new();
    assert_eq!(engine.index(index_options(root)).await?.files_added, 2);
    assert_index_and_queries(
        &engine,
        root,
        &FileFilter::default(),
        &paths(&["plain.txt", "nested/child.txt"]),
    )
    .await?;
    let filter = FileFilter {
        // A directory ignored by .gitignore must itself be explicitly included.
        globs: vec!["*.txt".into(), "ignored".into(), "ignored/**".into()],
        ..FileFilter::default()
    };
    let indexed = engine
        .index(IndexOptions {
            filter: FileFilterUpdate {
                globs: Some(filter.globs.clone()),
                ..FileFilterUpdate::default()
            },
            ..index_options(root)
        })
        .await?;
    assert_eq!((indexed.files_added, indexed.files_failed), (2, 0));
    assert_index_and_queries(
        &engine,
        root,
        &filter,
        &paths(&[
            "plain.txt",
            "nested/child.txt",
            "blocked.txt",
            "ignored/keep.txt",
        ]),
    )
    .await?;
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
async fn nested_git_scan_option_controls_membership_and_survives_reopen_and_rebuild() -> TestResult
{
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    write_sources(
        root,
        &[
            ("plain.txt", "orchard root source"),
            ("ordinary/keep.txt", "orchard ordinary directory"),
            (".git/objects/private.txt", "orchard root git internals"),
            ("nested/keep.txt", "orchard nested repository"),
            ("nested/ignored.txt", "orchard ignored nested source"),
            ("nested/.gitignore", "ignored.txt\n"),
            (
                "nested/.git/objects/private.txt",
                "orchard nested git internals",
            ),
            ("submodule/keep.txt", "orchard submodule source"),
            ("submodule/ignored.txt", "orchard ignored submodule source"),
            ("submodule/.gitignore", "ignored.txt\n"),
            ("submodule/.git", "gitdir: ../.git/modules/submodule\n"),
        ],
    )?;
    let boundary_filter = FileFilter {
        globs: vec![
            "*.txt".into(),
            "nested".into(),
            "nested/**".into(),
            "submodule".into(),
            "submodule/**".into(),
        ],
        ..FileFilter::default()
    };
    let outer = paths(&["plain.txt", "ordinary/keep.txt"]);
    let including_nested = paths(&[
        "plain.txt",
        "ordinary/keep.txt",
        "nested/keep.txt",
        "submodule/keep.txt",
    ]);
    let engine = ZvecGrep::new();
    let indexed = engine
        .index(IndexOptions {
            filter: FileFilterUpdate {
                globs: Some(boundary_filter.globs.clone()),
                ..FileFilterUpdate::default()
            },
            scan: ScanOptionsUpdate {
                nested_git: Some(false),
                no_ignore: Some(true),
                ..ScanOptionsUpdate::default()
            },
            ..index_options(root)
        })
        .await?;
    assert_eq!((indexed.files_added, indexed.files_failed), (2, 0));
    // Neither explicit directory globs nor no_ignore may cross a disabled boundary.
    assert_index_and_queries(&engine, root, &boundary_filter, &outer).await?;

    let indexed = engine
        .index(IndexOptions {
            filter: FileFilterUpdate {
                globs: Some(Vec::new()),
                ..FileFilterUpdate::default()
            },
            scan: ScanOptionsUpdate {
                nested_git: Some(true),
                no_ignore: Some(false),
                ..ScanOptionsUpdate::default()
            },
            ..index_options(root)
        })
        .await?;
    assert_eq!((indexed.files_added, indexed.files_failed), (2, 0));
    // Entering a repository does not disable its ignore files or expose .git internals.
    assert_index_and_queries(&engine, root, &FileFilter::default(), &including_nested).await?;
    assert_eq!(engine.index(index_options(root)).await?.files_unchanged, 4);
    assert!(
        engine
            .info(info_options(root))
            .await?
            .workspace_index
            .expect("workspace after omitted update")
            .scan
            .nested_git
    );
    engine.close();

    let engine = ZvecGrep::new();
    let info = engine.info(info_options(root)).await?;
    assert!(
        info.workspace_index
            .expect("reopened workspace")
            .scan
            .nested_git
    );
    let manifest: Value = serde_json::from_slice(&fs::read(info.home.join("manifest.json"))?)?;
    assert_eq!(manifest["scan"]["nested_git"], true);
    let rebuilt = engine
        .index(IndexOptions {
            rebuild: true,
            ..index_options(root)
        })
        .await?;
    assert_eq!((rebuilt.files_added, rebuilt.files_failed), (4, 0));
    assert_index_and_queries(&engine, root, &FileFilter::default(), &including_nested).await?;

    let indexed = engine
        .index(IndexOptions {
            scan: ScanOptionsUpdate {
                nested_git: Some(false),
                ..ScanOptionsUpdate::default()
            },
            ..index_options(root)
        })
        .await?;
    assert_eq!((indexed.files_deleted, indexed.files_failed), (2, 0));
    assert_index_and_queries(&engine, root, &FileFilter::default(), &outer).await?;
    assert_eq!(engine.index(index_options(root)).await?.files_unchanged, 2);
    engine.close();

    let engine = ZvecGrep::new();
    let info = engine.info(info_options(root)).await?;
    assert!(
        !info
            .workspace_index
            .expect("reopened workspace")
            .scan
            .nested_git
    );
    let manifest: Value = serde_json::from_slice(&fs::read(info.home.join("manifest.json"))?)?;
    assert_eq!(manifest["scan"]["nested_git"], false);
    let rebuilt = engine
        .index(IndexOptions {
            rebuild: true,
            ..index_options(root)
        })
        .await?;
    assert_eq!((rebuilt.files_added, rebuilt.files_failed), (2, 0));
    assert_index_and_queries(&engine, root, &FileFilter::default(), &outer).await?;
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
async fn detected_formats_and_overlapping_categories_control_indexing_and_queries() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    write_sources(
        root,
        &[
            (
                "script",
                "#!/usr/bin/env python3\n# orchard\ndef orchard():\n    return 1\n",
            ),
            (
                "page.html",
                "<!doctype html><html><body>orchard</body></html>",
            ),
            ("note.md", "# orchard\n"),
            ("settings.json", "{\"orchard\":\"apple\"}"),
            ("source.rs", "/// orchard\npub fn source() {}\n"),
        ],
    )?;
    let filter = FileFilter {
        formats: vec![
            FileFormat::Python,
            FileFormat::Rust,
            FileFormat::Html,
            FileFormat::Json,
        ],
        categories: vec![FileCategory::Code, FileCategory::Data],
        excluded_categories: vec![FileCategory::Document],
        ..FileFilter::default()
    };
    let engine = ZvecGrep::new();
    let indexed = engine
        .index(IndexOptions {
            filter: FileFilterUpdate {
                formats: Some(filter.formats.clone()),
                categories: Some(filter.categories.clone()),
                excluded_categories: Some(filter.excluded_categories.clone()),
                ..FileFilterUpdate::default()
            },
            ..index_options(root)
        })
        .await?;
    assert_eq!((indexed.files_added, indexed.files_failed), (3, 0));
    assert_index_and_queries(
        &engine,
        root,
        &filter,
        &paths(&["script", "settings.json", "source.rs"]),
    )
    .await?;
    let python = FileFilter {
        formats: vec![FileFormat::Python],
        ..FileFilter::default()
    };
    for mode in [ContextRouteMode::Fts, ContextRouteMode::Vector] {
        assert_eq!(
            search_paths(&engine, root, mode, python.clone()).await?,
            paths(&["script"])
        );
        assert!(
            search_paths(
                &engine,
                root,
                mode,
                FileFilter {
                    categories: vec![FileCategory::Document],
                    ..FileFilter::default()
                }
            )
            .await?
            .is_empty()
        );
    }
    let info = engine.info(info_options(root)).await?;
    let files = native_file_records(&info.index_path)?;
    let script = files
        .iter()
        .find(|file| file["value"]["relative_path"]["value"] == "script")
        .expect("stored script");
    assert_eq!(
        script["value"]["formats"],
        json!([FileFormat::Python as u16])
    );

    fs::write(
        root.join("script"),
        "orchard is now a plain text document with no shebang",
    )?;
    let updated = engine.index(index_options(root)).await?;
    assert_eq!((updated.files_deleted, updated.files_failed), (1, 0));
    assert_index_and_queries(
        &engine,
        root,
        &filter,
        &paths(&["settings.json", "source.rs"]),
    )
    .await?;
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Exercise the complete persisted update and rebuild lifecycle through the public API"
)]
async fn explicit_empty_false_and_null_updates_persist_and_survive_rebuild() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    write_sources(
        root,
        &[
            ("normal.txt", "orchard normal source"),
            (".hidden.txt", "orchard hidden source"),
            ("skipped.txt", "orchard ignored source"),
            (".gitignore", "skipped.txt\n"),
            (".customignore", "nothing.matches\n"),
            ("deep/nested.md", "# orchard nested document\n"),
        ],
    )?;
    fs::write(root.join("large.txt"), "orchard large source ".repeat(20))?;
    let engine = ZvecGrep::new();
    let initial = engine
        .index(IndexOptions {
            filter: FileFilterUpdate {
                globs: Some(vec!["*.txt".into()]),
                formats: Some(vec![FileFormat::Text]),
                categories: Some(vec![FileCategory::Document]),
                ..FileFilterUpdate::default()
            },
            scan: ScanOptionsUpdate {
                hidden: Some(true),
                no_ignore: Some(true),
                follow: Some(true),
                ignore_files: Some(vec![PathBuf::from(".customignore")]),
                max_depth: Some(Some(1)),
                max_file_size_bytes: Some(Some(40)),
                ..ScanOptionsUpdate::default()
            },
            ..index_options(root)
        })
        .await?;
    assert_eq!((initial.files_added, initial.files_failed), (3, 0));
    let before = engine
        .info(info_options(root))
        .await?
        .workspace_index
        .expect("workspace");
    let requests = server.requests.load(Ordering::Acquire);
    let inputs = server.inputs.load(Ordering::Acquire);
    assert_eq!(engine.index(index_options(root)).await?.files_unchanged, 3);
    let unchanged = engine
        .info(info_options(root))
        .await?
        .workspace_index
        .expect("workspace");
    assert_eq!(unchanged.filter, before.filter);
    assert_eq!(unchanged.scan, before.scan);
    assert_eq!(server.requests.load(Ordering::Acquire), requests);
    assert_eq!(server.inputs.load(Ordering::Acquire), inputs);

    let filter: FileFilterUpdate = serde_json::from_value(json!({"globs": [], "formats": []}))?;
    let scan: ScanOptionsUpdate = serde_json::from_value(json!({
        "hidden": false, "no_ignore": false, "follow": false,
        "ignore_files": [], "max_depth": null, "max_file_size_bytes": null
    }))?;
    let updated = engine
        .index(IndexOptions {
            filter,
            scan,
            ..index_options(root)
        })
        .await?;
    assert_eq!(
        (
            updated.files_added,
            updated.files_deleted,
            updated.files_failed
        ),
        (2, 2, 0)
    );
    let expected_filter = FileFilter {
        categories: vec![FileCategory::Document],
        ..FileFilter::default()
    };
    let expected_paths = paths(&["normal.txt", "large.txt", "deep/nested.md"]);
    assert_index_and_queries(&engine, root, &expected_filter, &expected_paths).await?;
    engine.close();

    let engine = ZvecGrep::new();
    let info = engine.info(info_options(root)).await?;
    let reopened = info.workspace_index.expect("reopened workspace");
    assert_eq!(reopened.filter, expected_filter);
    assert_eq!(reopened.scan, ScanOptions::default());
    let manifest: Value = serde_json::from_slice(&fs::read(info.home.join("manifest.json"))?)?;
    assert_eq!(manifest["filter"], serde_json::to_value(&expected_filter)?);
    assert_eq!(
        manifest["scan"],
        serde_json::to_value(ScanOptions::default())?
    );
    let rebuilt = engine
        .index(IndexOptions {
            rebuild: true,
            ..index_options(root)
        })
        .await?;
    assert_eq!((rebuilt.files_added, rebuilt.files_failed), (3, 0));
    let rebuilt_info = engine.info(info_options(root)).await?;
    assert_ne!(rebuilt_info.index_path, info.index_path);
    assert_eq!(
        rebuilt_info
            .workspace_index
            .expect("rebuilt workspace")
            .scan,
        ScanOptions::default()
    );
    assert_index_and_queries(&engine, root, &expected_filter, &expected_paths).await?;
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}
