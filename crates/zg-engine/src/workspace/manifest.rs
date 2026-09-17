use crate::{
    EngineError,
    domain::{
        EmbeddingMetric, EmbeddingSchema, FileSelection, IndexDescriptor, IndexState, Workspace,
    },
    models::Device,
    utils::{atomic_write, sync_directory},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub(crate) const WORKSPACE_MANIFEST_FILE: &str = "manifest.json";
pub(crate) const CURRENT_MANIFEST_VERSION: u32 = 4;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EmbeddingRuntimeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<Device>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,
}

/// Disk metadata owns layout/versioning; domain workspace owns its logical state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "ManifestData", into = "ManifestData")]
pub(crate) struct WorkspaceManifest {
    pub manifest_version: u32,
    pub workspace: Workspace,
    /// Root recorded on disk before resolving a moved workspace.
    pub recorded_root: PathBuf,
    pub path: PathBuf,
    pub index_version: Option<u32>,
    pub storage_generation: Option<String>,
    pub embedding_runtime: EmbeddingRuntimeConfig,
}

// Retain the existing flat disk representation, while retiring its UUID identity.
// Unknown legacy `id` fields are ignored; the next write stores only the name.
// Serialized policy is a disk concern; domain enabled state always has a descriptor.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum IndexPolicy {
    Uninitialized,
    Enabled,
    Disabled,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestData {
    manifest_version: u32,
    name: String,
    path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<PathBuf>,
    #[serde(default)]
    discovery: ManifestDiscovery,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_paths: Option<Vec<LegacyRootPath>>,
    index_policy: IndexPolicy,
    embedding: Option<StoredEmbeddingSchema>,
    index_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    storage_generation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    generation: Option<u64>,
    created_time: u64,
    updated_time: u64,
    embedding_runtime: EmbeddingRuntimeConfig,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LegacyRootPath {
    absolute_path: PathBuf,
    recursive: bool,
    #[serde(flatten)]
    discovery: ManifestDiscovery,
}

#[derive(Deserialize, Serialize)]
struct StoredEmbeddingSchema {
    provider: String,
    model: String,
    dimension: usize,
    metric: String,
}

impl TryFrom<StoredEmbeddingSchema> for EmbeddingSchema {
    type Error = String;
    fn try_from(value: StoredEmbeddingSchema) -> Result<Self, Self::Error> {
        let metric = match value.metric.as_str() {
            "cosine" => EmbeddingMetric::Cosine,
            "dot" => EmbeddingMetric::DotProduct,
            "euclidean" => EmbeddingMetric::Euclidean,
            _ => return Err("embedding metric is invalid".into()),
        };
        Ok(Self {
            provider: value.provider,
            model: value.model,
            dimension: value.dimension,
            metric,
        })
    }
}

impl From<EmbeddingSchema> for StoredEmbeddingSchema {
    fn from(value: EmbeddingSchema) -> Self {
        Self {
            provider: value.provider,
            model: value.model,
            dimension: value.dimension,
            metric: match value.metric {
                EmbeddingMetric::Cosine => "cosine",
                EmbeddingMetric::DotProduct => "dot",
                EmbeddingMetric::Euclidean => "euclidean",
            }
            .into(),
        }
    }
}

impl TryFrom<ManifestData> for WorkspaceManifest {
    type Error = String;
    fn try_from(input: ManifestData) -> Result<Self, Self::Error> {
        let (root, discovery) = match (input.root, input.root_paths) {
            (Some(root), None) => (root, input.discovery),
            (None, Some(mut roots)) => {
                if roots.len() != 1 {
                    return Err("legacy rootPaths must contain exactly one workspace root; multiple-root workspaces are unsupported".into());
                }
                let mut root = roots.remove(0);
                if input.path.parent() != Some(root.absolute_path.as_path()) {
                    return Err("legacy source root differs from the workspace directory; recreate the index with one workspace root".into());
                }
                if !root.recursive {
                    root.discovery.max_depth = Some(root.discovery.max_depth.unwrap_or(1).min(1));
                }
                (root.absolute_path, root.discovery)
            }
            (Some(_), Some(_)) => return Err("specify root or legacy rootPaths, not both".into()),
            (None, None) => return Err("workspace root is missing".into()),
        };
        let index = input
            .embedding
            .map(|embedding| -> Result<_, String> {
                Ok(IndexDescriptor {
                    embedding: embedding.try_into()?,
                    // Zero records an unknown legacy revision or an unpublished first build.
                    // It is never exposed as a known API generation.
                    revision: input.generation.unwrap_or(0),
                })
            })
            .transpose()?;
        let index = match input.index_policy {
            IndexPolicy::Disabled => IndexState::Disabled,
            IndexPolicy::Uninitialized => IndexState::Uninitialized,
            IndexPolicy::Enabled => {
                IndexState::Enabled(index.ok_or("enabled workspace requires an index descriptor")?)
            }
        };
        let manifest = Self {
            manifest_version: input.manifest_version,
            recorded_root: root.clone(),
            workspace: Workspace {
                name: input.name,
                root,
                file_selection: discovery.into(),
                index,
                created_epoch_ms: input.created_time,
                updated_epoch_ms: input.updated_time,
            },
            path: input.path,
            index_version: input.index_version,
            storage_generation: input.storage_generation,
            embedding_runtime: input.embedding_runtime,
        };
        manifest.validate().map_err(|error| error.to_string())?;
        Ok(manifest)
    }
}

impl From<WorkspaceManifest> for ManifestData {
    fn from(manifest: WorkspaceManifest) -> Self {
        let generation = manifest.revision();
        let workspace = manifest.workspace;
        Self {
            manifest_version: manifest.manifest_version,
            name: workspace.name,
            path: manifest.path,
            root: Some(workspace.root),
            discovery: workspace.file_selection.into(),
            root_paths: None,
            index_policy: match &workspace.index {
                IndexState::Uninitialized => IndexPolicy::Uninitialized,
                IndexState::Disabled => IndexPolicy::Disabled,
                IndexState::Enabled(_) => IndexPolicy::Enabled,
            },
            embedding: workspace
                .index
                .descriptor()
                .map(|index| index.embedding.clone().into()),
            index_version: manifest.index_version,
            storage_generation: manifest.storage_generation,
            generation,
            created_time: workspace.created_epoch_ms,
            updated_time: workspace.updated_epoch_ms,
            embedding_runtime: manifest.embedding_runtime,
        }
    }
}

impl WorkspaceManifest {
    pub(crate) fn new(
        workspace: Workspace,
        home: PathBuf,
        index_version: Option<u32>,
        embedding_runtime: EmbeddingRuntimeConfig,
    ) -> Result<Self, EngineError> {
        let manifest = Self {
            manifest_version: CURRENT_MANIFEST_VERSION,
            path: home,
            recorded_root: workspace.root.clone(),
            workspace,
            index_version,
            storage_generation: None,
            embedding_runtime,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub(crate) fn storage_home(&self) -> PathBuf {
        self.storage_generation.as_ref().map_or_else(
            || self.path.clone(),
            |generation| self.path.join("generations").join(generation),
        )
    }

    pub(crate) fn embedding(&self) -> Option<&EmbeddingSchema> {
        self.workspace
            .index
            .descriptor()
            .map(|index| &index.embedding)
    }

    pub(crate) fn revision(&self) -> Option<u64> {
        self.workspace
            .index
            .descriptor()
            .and_then(|index| (index.revision != 0).then_some(index.revision))
    }

    pub(crate) fn record_revision(&mut self, revision: u64, updated_epoch_ms: u64) {
        if let IndexState::Enabled(index) = &mut self.workspace.index {
            index.revision = revision;
        }
        self.workspace.updated_epoch_ms = updated_epoch_ms;
        self.manifest_version = CURRENT_MANIFEST_VERSION;
    }

    pub(crate) fn same_index_settings(&self, other: &Self) -> bool {
        self.workspace.name == other.workspace.name
            && self.index_version == other.index_version
            && self.embedding() == other.embedding()
            && self.embedding_runtime == other.embedding_runtime
            && self.workspace.file_selection == other.workspace.file_selection
    }

    pub(crate) fn validate(&self) -> Result<(), EngineError> {
        if !matches!(self.manifest_version, 1 | 2 | 3 | CURRENT_MANIFEST_VERSION) {
            return Err(invalid_manifest(format!(
                "unsupported manifestVersion {}",
                self.manifest_version
            )));
        }
        if let Some(generation) = &self.storage_generation
            && uuid::Uuid::parse_str(generation).is_err()
        {
            return Err(invalid_manifest("storageGeneration must be a UUID"));
        }
        if !self.path.is_absolute() {
            return Err(invalid_manifest("path must be an absolute workspace home"));
        }
        self.workspace
            .validate()
            .map_err(|error| invalid_manifest(error.message()))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ManifestDiscovery {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub globs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub insensitive_globs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_types: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_file_types: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub no_ignore: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore_files: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub follow: bool,
}

impl From<FileSelection> for ManifestDiscovery {
    fn from(discovery: FileSelection) -> Self {
        Self {
            include: discovery.include_paths,
            exclude: discovery.exclude_paths,
            globs: discovery.globs,
            insensitive_globs: discovery.insensitive_globs,
            file_types: discovery.file_types,
            excluded_file_types: discovery.excluded_file_types,
            hidden: discovery.hidden,
            no_ignore: discovery.no_ignore,
            ignore_files: discovery.ignore_files,
            max_depth: discovery.max_depth,
            max_file_size_bytes: discovery.max_file_size_bytes,
            follow: discovery.follow,
        }
    }
}

impl From<ManifestDiscovery> for FileSelection {
    fn from(discovery: ManifestDiscovery) -> Self {
        Self {
            include_paths: discovery.include,
            exclude_paths: discovery.exclude,
            globs: discovery.globs,
            insensitive_globs: discovery.insensitive_globs,
            file_types: discovery.file_types,
            excluded_file_types: discovery.excluded_file_types,
            hidden: discovery.hidden,
            no_ignore: discovery.no_ignore,
            ignore_files: discovery.ignore_files,
            max_depth: discovery.max_depth,
            max_file_size_bytes: discovery.max_file_size_bytes,
            follow: discovery.follow,
        }
    }
}

pub(crate) fn workspace_manifest_path(home: &Path) -> PathBuf {
    home.join(WORKSPACE_MANIFEST_FILE)
}

pub(crate) fn read_workspace_manifest(
    home: &Path,
) -> Result<Option<WorkspaceManifest>, EngineError> {
    let path = workspace_manifest_path(home);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(manifest_io("read", &path, &error)),
    };
    let mut manifest: WorkspaceManifest = serde_json::from_str(&text)
        .map_err(|error| invalid_manifest(format!("path={} cause={error}", path.display())))?;
    manifest.validate()?;
    // The directory containing the manifest defines the workspace location.
    // Persisted absolute paths may refer to where the workspace lived before a move.
    manifest.path = std::path::absolute(home)
        .map_err(|error| manifest_io("resolve directory for", home, &error))?;
    manifest.workspace.root = manifest
        .path
        .parent()
        .ok_or_else(|| invalid_manifest("workspace home has no parent directory"))?
        .to_path_buf();
    Ok(Some(manifest))
}

pub(crate) fn write_workspace_manifest(
    home: &Path,
    manifest: &WorkspaceManifest,
) -> Result<(), EngineError> {
    manifest.validate()?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    if let Err(error) = builder.create(home)
        && !(error.kind() == std::io::ErrorKind::AlreadyExists && home.is_dir())
    {
        return Err(manifest_io("create directory for", home, &error));
    }
    let path = workspace_manifest_path(home);
    let mut bytes = serde_json::to_vec_pretty(manifest).map_err(|error| {
        EngineError::internal(format!("failed to encode workspace manifest: {error}"))
    })?;
    bytes.push(b'\n');
    atomic_write(&path, &bytes)?;
    if home.file_name().is_some() {
        sync_directory(home.parent().unwrap_or(home))?;
    }
    Ok(())
}

pub(crate) fn delete_workspace_manifest(home: &Path) -> Result<(), EngineError> {
    let path = workspace_manifest_path(home);
    match fs::remove_file(&path) {
        Ok(()) => sync_directory(home),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(manifest_io("delete", &path, &error)),
    }
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if predicates receive references"
)]
const fn is_false(value: &bool) -> bool {
    !*value
}

#[track_caller]
fn invalid_manifest(message: impl Into<String>) -> EngineError {
    EngineError::storage_failure(format!("invalid workspace manifest: {}", message.into()))
}

#[track_caller]
fn manifest_io(operation: &str, path: &Path, error: &std::io::Error) -> EngineError {
    EngineError::from_io(
        format!(
            "failed to {operation} workspace manifest {}",
            path.display()
        ),
        error,
    )
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn fixture_manifest(home: &Path) -> WorkspaceManifest {
        WorkspaceManifest::new(
            Workspace {
                name: "fixture".to_owned(),
                root: home.parent().expect("workspace root").to_path_buf(),
                file_selection: FileSelection {
                    globs: vec!["*.rs".into()],
                    hidden: true,
                    ..FileSelection::default()
                },
                index: IndexState::Enabled(IndexDescriptor {
                    embedding: EmbeddingSchema {
                        provider: "local".into(),
                        model: "minilm".into(),
                        dimension: 384,
                        metric: EmbeddingMetric::Cosine,
                    },
                    revision: 7,
                }),
                created_epoch_ms: 10,
                updated_epoch_ms: 20,
            },
            home.to_path_buf(),
            Some(1),
            EmbeddingRuntimeConfig {
                device: Some(Device::Cpu),
                cache_dir: Some(home.join("models")),
                ..EmbeddingRuntimeConfig::default()
            },
        )
        .expect("fixture manifest")
    }

    #[test]
    fn enabled_manifest_requires_a_descriptor_and_does_not_persist_runtime_status() {
        let directory = tempdir().expect("workspace");
        let mut manifest = fixture_manifest(&directory.path().join(".zvec-grep"));
        let mut json = serde_json::to_value(&manifest).expect("serialize");
        json["embedding"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<WorkspaceManifest>(json).is_err());
        for index in [
            IndexState::Uninitialized,
            IndexState::Disabled,
            manifest.workspace.index.clone(),
        ] {
            manifest.workspace.index = index;
            let json = serde_json::to_value(&manifest).expect("serialize");
            assert!(json.get("status").is_none());
            assert!(json.get("indexStatus").is_none());
            assert_eq!(
                serde_json::from_value::<WorkspaceManifest>(json).expect("deserialize"),
                manifest
            );
        }
    }

    #[test]
    fn preserves_the_resolved_workspace_home() {
        let directory = tempdir().expect("workspace");
        let home = directory.path().join("custom-index");
        let mut manifest = fixture_manifest(&home);

        assert_eq!(manifest.path, home);
        assert_eq!(manifest.storage_home(), home);
        assert_eq!(manifest.workspace.root, directory.path());
        let generation = uuid::Uuid::new_v4().to_string();
        manifest.storage_generation = Some(generation.clone());
        assert_eq!(
            manifest.storage_home(),
            home.join("generations").join(generation)
        );

        write_workspace_manifest(&home, &manifest).expect("persist at resolved home");
        assert_eq!(
            read_workspace_manifest(&home).expect("read resolved home"),
            Some(manifest)
        );
        assert!(!directory.path().join(".zvec-grep").exists());
    }

    #[test]
    fn writes_and_reads_a_single_workspace_root() {
        let directory = tempdir().expect("temporary directory");
        let home = directory.path().join(".zvec-grep");
        let manifest = fixture_manifest(&home);

        write_workspace_manifest(&home, &manifest).expect("write manifest");
        let text = fs::read_to_string(workspace_manifest_path(&home)).expect("manifest text");
        let json: serde_json::Value = serde_json::from_str(&text).expect("manifest json");

        assert_eq!(json["manifestVersion"], CURRENT_MANIFEST_VERSION);
        assert!(json.get("id").is_none());
        assert!(json.get("rootPaths").is_none());
        assert_eq!(json["root"], directory.path().to_string_lossy().as_ref());
        assert_eq!(json["discovery"]["globs"][0], "*.rs");
        assert_eq!(json["embeddingRuntime"]["device"], "cpu");
        assert_eq!(json["generation"], 7);
        assert_eq!(
            json["embeddingRuntime"]["cacheDir"],
            home.join("models").to_string_lossy().as_ref()
        );
        assert_eq!(manifest.revision(), Some(7));
        assert_eq!(
            read_workspace_manifest(&home).expect("read manifest"),
            Some(manifest)
        );
        let mut legacy = json;
        legacy
            .as_object_mut()
            .expect("manifest object")
            .remove("generation");
        legacy["embeddingRuntime"]
            .as_object_mut()
            .expect("runtime object")
            .remove("cacheDir");
        let legacy: WorkspaceManifest = serde_json::from_value(legacy).expect("legacy manifest");
        assert_eq!(legacy.revision(), None);
        assert_eq!(legacy.embedding_runtime.cache_dir, None);
    }

    #[test]
    fn legacy_single_root_keeps_metadata_available_for_rebuild() {
        let directory = tempdir().expect("temporary directory");
        let home = directory.path().join(".zvec-grep");
        let mut manifest = fixture_manifest(&home);
        manifest.manifest_version = 1;
        let mut json = serde_json::to_value(&manifest).expect("manifest json");
        let root = json
            .as_object_mut()
            .expect("manifest object")
            .remove("root")
            .expect("root");
        let mut discovery = json
            .as_object_mut()
            .expect("manifest object")
            .remove("discovery")
            .expect("discovery");
        discovery["absolutePath"] = root;
        discovery["recursive"] = true.into();
        json["rootPaths"] = serde_json::json!([discovery.clone()]);
        fs::create_dir(&home).expect("workspace home");
        fs::write(
            workspace_manifest_path(&home),
            serde_json::to_vec(&json).expect("legacy json"),
        )
        .expect("legacy manifest");
        let legacy = read_workspace_manifest(&home)
            .expect("legacy read")
            .expect("manifest");
        assert_eq!(legacy, manifest);

        let mut subtree = json.clone();
        subtree["rootPaths"][0]["absolutePath"] = serde_json::json!(directory.path().join("src"));
        let error = serde_json::from_value::<WorkspaceManifest>(subtree)
            .expect_err("legacy source scope cannot be widened to the workspace");
        assert!(error.to_string().contains("legacy source root differs"));

        let mut shallow = json.clone();
        shallow["rootPaths"][0]["recursive"] = false.into();
        let shallow: WorkspaceManifest =
            serde_json::from_value(shallow).expect("nonrecursive legacy workspace");
        assert_eq!(shallow.workspace.file_selection.max_depth, Some(1));

        json["rootPaths"] = serde_json::json!([discovery.clone(), discovery]);
        fs::write(
            workspace_manifest_path(&home),
            serde_json::to_vec(&json).expect("legacy json"),
        )
        .expect("multiple legacy roots");
        let error = read_workspace_manifest(&home).expect_err("multiple roots are unsupported");
        assert!(error.message().contains("exactly one workspace root"));
    }

    #[test]
    fn reading_a_moved_workspace_rebases_only_its_location() {
        let directory = tempdir().expect("temporary directory");
        let original = directory.path().join("original");
        let moved = directory.path().join("moved");
        fs::create_dir(&original).expect("original workspace");
        let mut manifest = fixture_manifest(&original.join(".zvec-grep"));
        write_workspace_manifest(&manifest.path, &manifest).expect("original manifest");
        fs::rename(&original, &moved).expect("move workspace");
        let relocated = read_workspace_manifest(&moved.join(".zvec-grep"))
            .expect("relocated manifest read")
            .expect("manifest");
        manifest.workspace.root = moved.clone();
        manifest.path = moved.join(".zvec-grep");
        assert_eq!(relocated, manifest);
    }

    #[test]
    fn string_names_are_validated_on_manifest_read_and_write() {
        let directory = tempdir().expect("workspace");
        let home = directory.path().join(".zvec-grep");
        let mut manifest = fixture_manifest(&home);
        write_workspace_manifest(&home, &manifest).expect("valid manifest");
        let original = fs::read(workspace_manifest_path(&home)).expect("original manifest");
        for name in ["", " ", " project", ".", "..", "a/b", "a\\b", "a\nb"] {
            manifest.workspace.name = name.to_owned();
            assert!(write_workspace_manifest(&home, &manifest).is_err());
            assert_eq!(
                fs::read(workspace_manifest_path(&home)).expect("preserved"),
                original
            );
            // Raw JSON can bypass in-memory validation; reads must still reject it.
            let json = serde_json::to_value(&manifest).expect("raw manifest");
            assert!(serde_json::from_value::<WorkspaceManifest>(json).is_err());
        }
    }

    #[test]
    fn rejects_invalid_or_unsupported_manifests() {
        let directory = tempdir().expect("temporary directory");
        let home = directory.path().join(".zvec-grep");
        fs::create_dir_all(&home).expect("workspace home");
        fs::write(workspace_manifest_path(&home), r#"{"manifestVersion":2}"#)
            .expect("invalid manifest");
        assert!(read_workspace_manifest(&home).is_err());
        let mut unsupported = fixture_manifest(&home);
        unsupported.manifest_version = CURRENT_MANIFEST_VERSION + 1;
        fs::write(
            workspace_manifest_path(&home),
            serde_json::to_vec(&unsupported).expect("unsupported manifest json"),
        )
        .expect("unsupported manifest");
        let error = read_workspace_manifest(&home).expect_err("unsupported manifest version");
        assert!(error.message().contains("unsupported manifestVersion"));
    }

    #[test]
    fn deleting_a_missing_manifest_is_idempotent() {
        let directory = tempdir().expect("temporary directory");
        assert!(delete_workspace_manifest(directory.path()).is_ok());
        assert!(delete_workspace_manifest(&directory.path().join("missing")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn manifest_and_workspace_permissions_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().expect("temporary directory");
        let home = directory.path().join(".zvec-grep");
        write_workspace_manifest(&home, &fixture_manifest(&home)).expect("write manifest");

        let directory_mode = fs::metadata(&home)
            .expect("workspace metadata")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(workspace_manifest_path(&home))
            .expect("manifest metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn replacing_manifest_preserves_file_and_directory_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().expect("temporary directory");
        let home = directory.path().join(".zvec-grep");
        let mut manifest = fixture_manifest(&home);
        write_workspace_manifest(&home, &manifest).expect("initial manifest");
        let path = workspace_manifest_path(&home);
        fs::set_permissions(&home, fs::Permissions::from_mode(0o750))
            .expect("custom workspace permissions");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
            .expect("custom manifest permissions");

        manifest.record_revision(8, 30);
        write_workspace_manifest(&home, &manifest).expect("replace manifest");

        assert_eq!(
            fs::metadata(&home)
                .expect("workspace metadata")
                .permissions()
                .mode()
                & 0o777,
            0o750
        );
        assert_eq!(
            fs::metadata(&path)
                .expect("manifest metadata")
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(
            read_workspace_manifest(&home).expect("updated manifest"),
            Some(manifest)
        );
    }
}
