//! Resumable workspace builds, published by a single atomic manifest update.

use std::{fs, path::Path};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    EngineError,
    storage::spi::WorkspaceIndexStorageFactory,
    utils::{atomic_write, sync_directory},
};

use super::manifest::{WorkspaceManifest, read_workspace_manifest, write_workspace_manifest};

const BUILD_FILE: &str = "build.json";
const BUILD_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WorkspaceBuild {
    version: u32,
    pub target: WorkspaceManifest,
    phase: BuildPhase,
    base_generation: Option<u64>,
    previous: Option<PreviousStorage>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum BuildPhase {
    Building,
    Publishing,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
enum PreviousStorage {
    Legacy,
    Generation { id: String },
}

impl WorkspaceBuild {
    /// A stage is reusable only for the same base generation and processing settings.
    fn matches(&self, target: &WorkspaceManifest) -> bool {
        self.target.same_index_settings(target) && self.base_generation == target.revision()
    }

    fn validate(&self) -> Result<(), EngineError> {
        self.target.validate()?;
        if self.version != BUILD_VERSION || self.target.storage_generation.is_none() {
            return Err(EngineError::storage_failure(
                "unsupported workspace build record",
            ));
        }
        if let Some(PreviousStorage::Generation { id }) = &self.previous
            && (Uuid::parse_str(id).is_err() || self.target.storage_generation.as_ref() == Some(id))
        {
            return Err(EngineError::storage_failure(
                "invalid previous build generation",
            ));
        }
        Ok(())
    }
}

pub(crate) fn has_build(home: &Path) -> bool {
    home.join(BUILD_FILE).is_file()
}

/// The manifest may have been lost while generation storage is still present.
/// An empty generations container is left behind by drop and is not an index.
pub(crate) fn has_generation_storage(home: &Path) -> Result<bool, EngineError> {
    let entries = match fs::read_dir(home.join("generations")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(build_io(error)),
    };
    for entry in entries {
        if entry
            .map_err(build_io)?
            .file_type()
            .map_err(build_io)?
            .is_dir()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Must run under the workspace write lock. A published stage only needs cleanup.
pub(crate) fn recover_build(
    home: &Path,
    factory: &dyn WorkspaceIndexStorageFactory,
) -> Result<(Option<WorkspaceBuild>, bool), EngineError> {
    let Some(build) = read_build(home)? else {
        return Ok((None, false));
    };
    let active = read_workspace_manifest(home)?;
    // The selected storage directory is the commit marker within this locked
    // workspace home. Its name may have changed after the manifest was published.
    if active
        .as_ref()
        .is_some_and(|manifest| manifest.storage_generation == build.target.storage_generation)
    {
        cleanup_previous(home, &build, factory)?;
        remove_build(home)?;
        return Ok((None, true));
    }
    Ok((Some(build), false))
}

pub(crate) fn prepare_build(
    mut target: WorkspaceManifest,
    active: Option<&WorkspaceManifest>,
    pending: Option<WorkspaceBuild>,
    factory: &dyn WorkspaceIndexStorageFactory,
) -> Result<WorkspaceBuild, EngineError> {
    if let Some(mut pending) = pending {
        if pending.matches(&target) {
            // Even a completed-but-unpublished stage is scanned again on resume:
            // source files may have changed since the previous process stopped.
            pending.phase = BuildPhase::Building;
            pending.target.record_revision(
                target.revision().unwrap_or(0),
                target.workspace.updated_epoch_ms,
            );
            write_build(&pending)?;
            ensure_stage(&pending)?;
            return Ok(pending);
        }
        remove_generation(&target.path, &pending.target, factory)?;
        remove_build(&target.path)?;
    }
    target.storage_generation = Some(Uuid::new_v4().to_string());
    let previous = active.map(|manifest| {
        manifest
            .storage_generation
            .as_ref()
            .map_or(PreviousStorage::Legacy, |id| PreviousStorage::Generation {
                id: id.clone(),
            })
    });
    let build = WorkspaceBuild {
        version: BUILD_VERSION,
        base_generation: target.revision(),
        target,
        phase: BuildPhase::Building,
        previous,
    };
    // Persist the owner/target before creating any stage data, so interrupted
    // construction never leaves an unidentified candidate for publication.
    write_build(&build)?;
    ensure_stage(&build)?;
    Ok(build)
}

/// The caller has completed and closed stage storage before publishing.
pub(crate) fn publish_build(
    mut build: WorkspaceBuild,
    generation: u64,
    updated_time: u64,
    factory: &dyn WorkspaceIndexStorageFactory,
) -> Result<(), EngineError> {
    build.target.record_revision(generation, updated_time);
    build.phase = BuildPhase::Publishing;
    write_build(&build)?;
    write_workspace_manifest(&build.target.path, &build.target)?;
    // The manifest is the commit point. A crash during cleanup is recovered by
    // comparing its selected generation with build.json, never by republishing.
    cleanup_previous(&build.target.path, &build, factory)?;
    remove_build(&build.target.path)
}

/// Drop also removes interrupted and superseded generations, under the home lock.
pub(crate) fn drop_build_storage(
    home: &Path,
    factory: &dyn WorkspaceIndexStorageFactory,
) -> Result<(), EngineError> {
    let generations = home.join("generations");
    match fs::read_dir(&generations) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(build_io)?;
                if !entry.file_type().map_err(build_io)?.is_dir() {
                    continue;
                }
                factory.delete(&entry.path())?;
                fs::remove_dir_all(entry.path()).map_err(build_io)?;
            }
            sync_directory(&generations)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(build_io(error)),
    }
    remove_build(home)
}

pub(crate) fn read_build(home: &Path) -> Result<Option<WorkspaceBuild>, EngineError> {
    let bytes = match fs::read(home.join(BUILD_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(build_io(error)),
    };
    let mut build: WorkspaceBuild = serde_json::from_slice(&bytes).map_err(|error| {
        EngineError::storage_failure(format!("invalid workspace build record: {error}"))
    })?;
    build.validate()?;
    build.target.path = std::path::absolute(home).map_err(build_io)?;
    build.target.workspace.root = build
        .target
        .path
        .parent()
        .ok_or_else(|| EngineError::storage_failure("workspace home has no parent"))?
        .to_path_buf();
    Ok(Some(build))
}

fn write_build(build: &WorkspaceBuild) -> Result<(), EngineError> {
    build.validate()?;
    let bytes = serde_json::to_vec_pretty(build)
        .map_err(|error| EngineError::internal(format!("encode workspace build: {error}")))?;
    atomic_write(&build.target.path.join(BUILD_FILE), &bytes)
}

fn ensure_stage(build: &WorkspaceBuild) -> Result<(), EngineError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(build.target.storage_home())
        .map_err(build_io)?;
    sync_directory(&build.target.path.join("generations"))?;
    sync_directory(&build.target.path)
}

fn cleanup_previous(
    home: &Path,
    build: &WorkspaceBuild,
    factory: &dyn WorkspaceIndexStorageFactory,
) -> Result<(), EngineError> {
    match &build.previous {
        None => Ok(()),
        Some(PreviousStorage::Legacy) => factory.delete(home),
        Some(PreviousStorage::Generation { id }) => {
            let mut previous = build.target.clone();
            previous.storage_generation = Some(id.clone());
            remove_generation(home, &previous, factory)
        }
    }?;
    crate::storage::delete_workspace_identities(home)
}

fn remove_generation(
    home: &Path,
    manifest: &WorkspaceManifest,
    factory: &dyn WorkspaceIndexStorageFactory,
) -> Result<(), EngineError> {
    let storage = manifest.storage_home();
    factory.delete(&storage)?;
    match fs::remove_dir_all(&storage) {
        Ok(()) => sync_directory(&home.join("generations")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(build_io(error)),
    }
}

fn remove_build(home: &Path) -> Result<(), EngineError> {
    match fs::remove_file(home.join(BUILD_FILE)) {
        Ok(()) => sync_directory(home),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(build_io(error)),
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "used as a Result::map_err callback"
)]
fn build_io(error: std::io::Error) -> EngineError {
    EngineError::from_io("access workspace build state", &error)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use tempfile::tempdir;

    use crate::{
        domain::{
            FileSelection, IndexDescriptor, IndexState, Workspace,
            model::{EmbeddingMetric, EmbeddingSchema, ModelConfig},
        },
        storage::spi::{StorageResult, WorkspaceIndexStorage, WorkspaceIndexStorageOptions},
    };

    use super::*;

    #[derive(Debug, Default)]
    struct TestFactory {
        fail_delete: AtomicBool,
    }

    impl WorkspaceIndexStorageFactory for TestFactory {
        fn open(
            &self,
            _options: WorkspaceIndexStorageOptions,
        ) -> StorageResult<Box<dyn WorkspaceIndexStorage>> {
            Err(EngineError::unsupported(
                "build protocol test does not open native storage",
            ))
        }

        fn exists(&self, home: &Path) -> StorageResult<bool> {
            Ok(home.join("storage").is_dir())
        }

        fn delete(&self, home: &Path) -> StorageResult<()> {
            if self.fail_delete.load(Ordering::Relaxed) {
                return Err(EngineError::storage_failure("injected cleanup failure"));
            }
            match fs::remove_dir_all(home.join("storage")) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(build_io(error)),
            }
        }
    }

    fn manifest(root: &Path) -> WorkspaceManifest {
        WorkspaceManifest::new(
            Workspace {
                name: "workspace".to_owned(),
                root: root.to_path_buf(),
                file_selection: FileSelection::default(),
                index: IndexState::Enabled(IndexDescriptor {
                    embedding: EmbeddingSchema {
                        provider: "local".into(),
                        model: "example".into(),
                        dimension: 8,
                        metric: EmbeddingMetric::Cosine,
                    },
                    revision: 7,
                }),
                created_epoch_ms: 1,
                updated_epoch_ms: 2,
            },
            root.join(".zvec-grep"),
            Some(5),
            ModelConfig::default(),
        )
        .expect("manifest")
    }

    fn write_active(manifest: &WorkspaceManifest) {
        fs::create_dir_all(manifest.storage_home().join("storage")).expect("active storage");
        fs::write(manifest.storage_home().join("storage/old"), "old data").expect("old data");
        write_workspace_manifest(&manifest.path, manifest).expect("active manifest");
    }

    #[test]
    fn interrupted_build_reuses_checkpoint_and_preserves_active() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let factory = TestFactory::default();
        let build = prepare_build(active.clone(), Some(&active), None, &factory).expect("stage");
        let checkpoint = build.target.storage_home().join("checkpoint");
        fs::write(&checkpoint, "committed file").expect("checkpoint");
        let (pending, published) = recover_build(&active.path, &factory).expect("recover");
        assert!(!published);
        let resumed =
            prepare_build(active.clone(), Some(&active), pending, &factory).expect("resume");
        assert_eq!(
            resumed.target.storage_generation,
            build.target.storage_generation
        );
        assert_eq!(
            fs::read(checkpoint).expect("retained checkpoint"),
            b"committed file"
        );
        assert_eq!(
            read_workspace_manifest(&active.path).expect("manifest"),
            Some(active.clone())
        );
        assert!(active.storage_home().join("storage/old").exists());
    }

    #[test]
    fn changed_build_settings_discard_only_the_incompatible_stage() {
        for changed_setting in 0..5 {
            let directory = tempdir().expect("workspace");
            let active = manifest(directory.path());
            write_active(&active);
            let factory = TestFactory::default();
            let build =
                prepare_build(active.clone(), Some(&active), None, &factory).expect("stage");
            let mut target = active.clone();
            match changed_setting {
                0 => target.workspace.file_selection.globs.push("*.rs".into()),
                1 => target.embedding_runtime.endpoint = Some("https://other.example".into()),
                2 => target.index_version = Some(6),
                3 => target.record_revision(8, 3),
                _ => target.workspace.name = "renamed".to_owned(),
            }
            let replaced = prepare_build(target, Some(&active), Some(build.clone()), &factory)
                .expect("new stage");
            assert_ne!(
                replaced.target.storage_generation,
                build.target.storage_generation
            );
            assert!(!build.target.storage_home().exists());
            assert!(active.storage_home().join("storage/old").exists());
            assert_eq!(
                read_workspace_manifest(&active.path).expect("manifest"),
                Some(active)
            );
        }
    }

    #[test]
    fn interrupted_publication_before_manifest_commit_can_resume_without_rebuilding() {
        let directory = tempdir().expect("workspace");
        let active = manifest(directory.path());
        write_active(&active);
        let factory = TestFactory::default();
        let mut build =
            prepare_build(active.clone(), Some(&active), None, &factory).expect("stage");
        build.phase = BuildPhase::Publishing;
        build.target.record_revision(8, 3);
        write_build(&build).expect("prepare publication");
        let (pending, published) = recover_build(&active.path, &factory).expect("recover");
        assert!(!published);
        let resumed =
            prepare_build(active.clone(), Some(&active), pending, &factory).expect("resume");
        assert_eq!(
            resumed.target.storage_generation,
            build.target.storage_generation
        );
        assert_eq!(resumed.target.revision(), Some(7));
        assert_eq!(resumed.phase, BuildPhase::Building);
        assert!(active.storage_home().join("storage/old").exists());
    }

    #[test]
    fn publication_cleanup_failure_keeps_new_active_and_recovers_after_rename() {
        let directory = tempdir().expect("workspace");
        let mut active = manifest(directory.path());
        active.storage_generation = Some(Uuid::new_v4().to_string());
        write_active(&active);
        fs::create_dir(active.path.join("catalog")).expect("legacy catalog");
        fs::write(active.path.join("identity.json"), b"legacy").expect("legacy identities");
        let factory = TestFactory::default();
        let build = prepare_build(active.clone(), Some(&active), None, &factory).expect("stage");
        fs::create_dir(build.target.storage_home().join("storage")).expect("completed new storage");
        factory.fail_delete.store(true, Ordering::Relaxed);
        let error = publish_build(build.clone(), 8, 3, &factory)
            .expect_err("cleanup fails after publication");
        assert!(error.message().contains("cleanup"));
        let mut committed = read_workspace_manifest(&active.path)
            .expect("manifest")
            .expect("active");
        assert_eq!(
            committed.storage_generation,
            build.target.storage_generation
        );
        assert_eq!(committed.revision(), Some(8));
        assert_eq!(committed.workspace.name, active.workspace.name);
        assert_eq!(
            committed.workspace.created_epoch_ms,
            active.workspace.created_epoch_ms
        );
        assert!(active.storage_home().exists());
        assert!(has_build(&active.path));
        assert!(active.path.join("catalog").exists());
        committed.workspace.name = "renamed".to_owned();
        write_workspace_manifest(&active.path, &committed).expect("rename after publication");
        factory.fail_delete.store(false, Ordering::Relaxed);
        let (pending, published) = recover_build(&active.path, &factory).expect("complete cleanup");
        assert!(pending.is_none());
        assert!(published);
        assert!(!active.storage_home().exists());
        assert!(committed.storage_home().join("storage").is_dir());
        assert!(!has_build(&active.path));
        assert!(!active.path.join("catalog").exists());
        assert!(!active.path.join("identity.json").exists());
        assert_eq!(
            read_workspace_manifest(&active.path).expect("active manifest"),
            Some(committed)
        );
    }

    #[test]
    fn completed_legacy_rebuild_switches_manifest_before_removing_old_storage() {
        let directory = tempdir().expect("workspace");
        let mut active = manifest(directory.path());
        active.index_version = Some(4);
        write_active(&active);
        let factory = TestFactory::default();
        let mut target = active.clone();
        target.index_version = Some(5);
        let build = prepare_build(target, Some(&active), None, &factory).expect("stage");
        fs::create_dir(build.target.storage_home().join("storage")).expect("new storage");
        publish_build(build, 8, 3, &factory).expect("publish");
        let committed = read_workspace_manifest(&active.path)
            .expect("manifest")
            .expect("active");
        assert_eq!(committed.index_version, Some(5));
        assert_eq!(committed.revision(), Some(8));
        assert!(committed.storage_home().join("storage").is_dir());
        assert!(!active.storage_home().join("storage").exists());
        assert!(!has_build(&active.path));
    }

    #[test]
    fn moving_workspace_rebases_pending_storage_and_retains_build_identity() {
        let directory = tempdir().expect("workspace");
        let original = directory.path().join("original");
        fs::create_dir(&original).expect("root");
        let active = manifest(&original);
        write_active(&active);
        let factory = TestFactory::default();
        let build = prepare_build(active.clone(), Some(&active), None, &factory).expect("stage");
        let moved = directory.path().join("moved");
        fs::rename(original, &moved).expect("move");
        let (pending, _) =
            recover_build(&moved.join(".zvec-grep"), &factory).expect("recover moved stage");
        let pending = pending.expect("pending");
        assert_eq!(
            pending.target.storage_generation,
            build.target.storage_generation
        );
        assert_eq!(pending.target.workspace.root, moved);
        assert!(pending.target.storage_home().is_dir());
        assert_eq!(pending.target.workspace.name, active.workspace.name);
    }

    #[test]
    fn drop_removes_both_active_and_interrupted_generations() {
        let directory = tempdir().expect("workspace");
        let mut active = manifest(directory.path());
        active.storage_generation = Some(Uuid::new_v4().to_string());
        write_active(&active);
        let factory = TestFactory::default();
        let build = prepare_build(active.clone(), Some(&active), None, &factory).expect("stage");
        assert!(has_generation_storage(&active.path).expect("generations present"));
        drop_build_storage(&active.path, &factory).expect("drop generations");
        assert!(!active.storage_home().exists());
        assert!(!build.target.storage_home().exists());
        assert!(!has_build(&active.path));
        assert!(!has_generation_storage(&active.path).expect("empty generations container"));
        drop_build_storage(&active.path, &factory).expect("idempotent drop");
    }

    #[test]
    fn generation_storage_detection_keeps_io_errors() {
        let directory = tempdir().expect("workspace home");
        assert!(!has_generation_storage(directory.path()).expect("missing generations"));
        fs::write(directory.path().join("generations"), b"not a directory")
            .expect("invalid layout");
        assert!(has_generation_storage(directory.path()).is_err());
    }
}
