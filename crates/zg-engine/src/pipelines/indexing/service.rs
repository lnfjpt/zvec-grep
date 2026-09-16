use std::{
    env, fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use zg_host_native::NativeScanner;

use crate::{
    EngineError,
    api::{
        context::{ContextOptions, ContextResult},
        index::{
            IndexOptions, IndexResult,
            options::{Device, DiscoveryOptions, EmbeddingModelSpec},
        },
        info::{
            InfoOptions, InfoResult,
            result::{InfoSource, WorkspaceIndexInfo, WorkspaceIndexPolicy},
        },
    },
    domain::{EmbeddingSchema, IndexPolicy, Workspace, WorkspaceIndex, WorkspaceName},
    models::{
        CreateEmbeddingModelOptions, ModelError, ModelRuntimeLease, ModelRuntimeManager,
        ModelRuntimeRequest, ResolveEmbeddingReferenceOptions, resolve_embedding_reference,
    },
    pipelines::search::context::{NormalizedContextRequest, context_from_index},
    storage::spi::{
        WorkspaceIndexEmbeddingSchema, WorkspaceIndexStorageFactory, WorkspaceIndexStorageOptions,
    },
    workspace::{
        CURRENT_INDEX_VERSION,
        build::{
            WorkspaceBuild, has_build, has_generation_storage, prepare_build, publish_build,
            recover_build,
        },
        layout::{
            WorkspaceIndexLocation, find_nearest_workspace, reset_workspace_index,
            workspace_index_location,
        },
        lock::{LockMode, acquire_home_lock},
        manifest::{
            EmbeddingRuntimeConfig, WorkspaceManifest, read_workspace_manifest,
            write_workspace_manifest,
        },
        registry::WorkspaceRegistry,
    },
};

use super::pipeline::{IndexingContext, get_workspace_index_status, index_workspace};

const DEFAULT_LOCAL_EMBEDDING: &str = "local/potion-code-16m-v2";

#[derive(Clone)]
pub(crate) struct WorkspaceIndexService {
    scanner: NativeScanner,
    storage_factory: Arc<dyn WorkspaceIndexStorageFactory>,
    registry: Option<WorkspaceRegistry>,
    #[cfg(test)]
    _registry_directory: Option<Arc<tempfile::TempDir>>,
}

impl WorkspaceIndexService {
    pub(crate) fn new() -> Self {
        Self {
            scanner: NativeScanner::default(),
            storage_factory: Arc::new(crate::storage::ZvecStorageFactory::new()),
            registry: None,
            #[cfg(test)]
            _registry_directory: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_storage_factory(
        storage_factory: Arc<dyn WorkspaceIndexStorageFactory>,
    ) -> Self {
        let directory = Arc::new(tempfile::tempdir().expect("test workspace registry"));
        Self {
            scanner: NativeScanner::default(),
            storage_factory,
            registry: Some(
                WorkspaceRegistry::at(directory.path().join("workspaces.json"))
                    .expect("registry path"),
            ),
            _registry_directory: Some(directory),
        }
    }

    fn registry(&self) -> Result<WorkspaceRegistry, EngineError> {
        self.registry
            .clone()
            .map_or_else(WorkspaceRegistry::global, Ok)
    }

    /// A registry rename is authoritative; replay it into metadata after a crash.
    fn reconcile_name(&self, manifest: &mut WorkspaceManifest) -> Result<(), EngineError> {
        let registry = self.registry()?;
        if let Some(name) = registry.name_for_root(&manifest.workspace.root)? {
            manifest.workspace.name = name;
        } else if let Some((name, _)) = moved_registration(&registry, manifest)? {
            manifest.workspace.name = name;
        } else if let Some(previous) = registry.root_for_name(&manifest.workspace.name)? {
            return Err(EngineError::invalid_argument(format!(
                "workspace name '{}' is already registered at {}; use index --name to choose another name",
                manifest.workspace.name,
                previous.display()
            )));
        }
        Ok(())
    }

    fn register_name(
        &self,
        root: &Path,
        existing: Option<&WorkspaceManifest>,
        requested: Option<&str>,
    ) -> Result<WorkspaceName, EngineError> {
        let registry = self.registry()?;
        let requested = requested.map(WorkspaceName::new).transpose()?;
        if let Some(current) = registry.name_for_root(root)? {
            let name = requested.unwrap_or_else(|| current.clone());
            registry.rename(&current, &name, root)?;
            return Ok(name);
        }
        if let Some(existing) = existing
            && let Some((previous_name, previous_root)) = moved_registration(&registry, existing)?
        {
            registry.relocate(&previous_name, &previous_root, root)?;
            let name = requested.unwrap_or_else(|| previous_name.clone());
            registry.rename(&previous_name, &name, root)?;
            return Ok(name);
        }
        let name = requested
            .or_else(|| existing.map(|manifest| manifest.workspace.name.clone()))
            .map_or_else(|| WorkspaceName::new(workspace_name(root)), Ok)?;
        registry.register(&name, root)?;
        Ok(name)
    }

    pub(crate) async fn index(
        &self,
        models: &ModelRuntimeManager,
        mut options: IndexOptions,
    ) -> Result<IndexResult, EngineError> {
        if let Some(cache_dir) = options
            .embedding
            .as_mut()
            .and_then(|embedding| embedding.cache_dir.as_mut())
        {
            *cache_dir = std::path::absolute(&*cache_dir).map_err(|error| {
                EngineError::from_io("failed to resolve model cache directory", &error)
            })?;
        }
        let factory = &self.storage_factory;
        let requested_root = resolve_root(options.root.as_deref())?;
        validate_workspace_root(&requested_root)?;
        let location = find_nearest_workspace(&requested_root)?
            .map_or_else(|| workspace_index_location(&requested_root), Ok)?;
        options.root = Some(location.root.clone());
        let _lock = acquire_home_lock(
            &location.home,
            LockMode::Write,
            if options.rebuild {
                "index.rebuild"
            } else {
                "index"
            },
        )?;
        let (mut pending, recovered_publication) = recover_build(&location.home, factory.as_ref())?;
        if pending.is_some() && !options.rebuild && !options.changes.is_empty() {
            return Err(EngineError::resource_busy(
                "workspace rebuild is pending; run index to resume it before processing watcher changes",
            ));
        }
        let mut existing = read_workspace_manifest(&location.home)?;
        let name = self.register_name(
            &location.root,
            existing
                .as_ref()
                .or_else(|| pending.as_ref().map(|build| &build.target)),
            options.name.as_deref(),
        )?;
        options.name = Some(name.to_string());
        if let Some(active) = &mut existing {
            active.workspace.name = name.clone();
        }
        if let Some(build) = &mut pending {
            build.target.workspace.name = name;
        }
        let mut rebuilding = options.rebuild
            || pending.is_some()
            || existing
                .as_ref()
                .is_none_or(|manifest| !is_indexed(manifest));
        if !rebuilding {
            assert_index_version(
                existing
                    .as_ref()
                    .and_then(|manifest| manifest.index_version),
            )?;
        }
        let settings = pending
            .as_ref()
            .map(|build| &build.target)
            .or(existing.as_ref());
        let model = acquire_model(models, settings, &options)?;
        if !rebuilding {
            assert_embedding_compatible(existing.as_ref(), &model)?;
        }
        let mut manifest =
            index_manifest(&location, existing.as_ref(), settings, &options, &model)?;
        if recovered_publication
            && existing
                .as_ref()
                .is_some_and(|active| active.same_index_settings(&manifest))
        {
            // A repeated request after a crash in cleanup continues from the
            // published generation, unless it requests different build settings.
            rebuilding = false;
        }
        let build = if rebuilding {
            // A new generation must cover the complete configured workspace,
            // including when the triggering request came from a narrow watcher event.
            options.changes.clear();
            let build = prepare_build(manifest, existing.as_ref(), pending, factory.as_ref())?;
            manifest = build.target.clone();
            Some(build)
        } else {
            None
        };
        self.run_index(manifest, build, model, options).await
    }

    async fn run_index(
        &self,
        mut manifest: WorkspaceManifest,
        build: Option<WorkspaceBuild>,
        model: ModelRuntimeLease,
        options: IndexOptions,
    ) -> Result<IndexResult, EngineError> {
        let storage = self
            .storage_factory
            .open(WorkspaceIndexStorageOptions::ReadWrite {
                storage_path: manifest.storage_home(),
                workspace_path: manifest.path.clone(),
                embedding: storage_embedding_schema(&model),
            })?;
        let result = index_workspace(&IndexingContext {
            workspace_index: &manifest.workspace,
            storage: storage.as_ref(),
            scanner: &self.scanner,
            embedding_model: &model,
            embedding_concurrency: options.embedding_concurrency,
            on_progress: options.on_progress,
            signal: options.signal.clone(),
            changes: &options.changes,
        })
        .await;
        // Closing precedes publication: all checkpoints and native handles belong
        // to the completed generation before the active manifest can select it.
        let close_result = storage.close();
        let indexed = result?;
        close_result?;
        if options
            .signal
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            return Err(EngineError::cancelled(
                "indexing was cancelled before publication",
            ));
        }
        if indexed.files_failed > 0 {
            return Err(EngineError::storage_failure(
                "indexing has failed files; build remains pending",
            ));
        }
        let now = epoch_millis();
        if let Some(build) = build {
            publish_build(
                build,
                indexed.generation,
                now,
                self.storage_factory.as_ref(),
            )?;
        } else {
            manifest.record_revision(indexed.generation, now);
            write_workspace_manifest(&manifest.path, &manifest)?;
        }
        Ok(indexed)
    }

    pub(crate) async fn context(
        &self,
        models: &ModelRuntimeManager,
        options: &ContextOptions,
        request: &NormalizedContextRequest,
    ) -> Result<ContextResult, EngineError> {
        let requested_root = resolve_root(options.root.as_deref())?;
        let Some(location) = find_nearest_workspace(&requested_root)? else {
            return Err(workspace_index_unavailable(
                &requested_root,
                "no workspace manifest was found",
            ));
        };
        let factory = &self.storage_factory;
        let initial_manifest = read_workspace_manifest(&location.home)?;
        if !initial_manifest
            .as_ref()
            .map(|manifest| factory.exists(&manifest.storage_home()))
            .transpose()?
            .unwrap_or(false)
        {
            return Err(workspace_index_unavailable(
                &location.root,
                "index storage is missing",
            ));
        }
        if !has_build(&location.home)
            && options.refresh.map_or(options.auto_update, |policy| {
                policy == crate::api::context::options::RefreshPolicy::Wait
            })
            && self
                .workspace_needs_refresh(&location, factory.as_ref())
                .await?
        {
            self.index(models, refresh_options(options, location.root.clone()))
                .await?;
        }
        let _lock = acquire_home_lock(&location.home, LockMode::Read, "context")?;
        let mut manifest = read_workspace_manifest(&location.home)?.ok_or_else(|| {
            workspace_index_unavailable(&location.root, "workspace manifest disappeared")
        })?;
        self.reconcile_name(&mut manifest)?;
        if manifest.workspace.index_policy == IndexPolicy::Disabled {
            return Err(EngineError::unsupported(format!(
                "workspace indexing is disabled at {}",
                location.root.display()
            )));
        }
        if !is_indexed(&manifest) {
            return Err(workspace_index_unavailable(
                &location.root,
                "workspace index has not been built",
            ));
        }
        assert_index_version(manifest.index_version)?;
        let model = request
            .routes
            .iter()
            .any(|route| route.mode == crate::api::context::options::ContextRouteMode::Vector)
            .then(|| {
                acquire_search_model(
                    models,
                    &manifest,
                    options.embedding_concurrency,
                    options,
                    &location.root,
                )
            })
            .transpose()?;
        if let Some(model) = &model {
            assert_embedding_compatible(Some(&manifest), model)?;
        }
        let storage = factory.open(WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: manifest.storage_home(),
            workspace_path: manifest.path.clone(),
        })?;
        let result = context_from_index(
            &location.root,
            &manifest.workspace,
            &manifest.path,
            storage.as_ref(),
            model
                .as_ref()
                .map(|model| crate::pipelines::search::RequestEmbeddingRuntime {
                    model,
                    signal: options.signal.clone(),
                })
                .as_ref()
                .map(|model| model as &dyn crate::pipelines::search::SearchEmbeddingRuntime),
            options,
            request,
        )
        .await;
        let close = storage.close();
        let result = result?;
        close?;
        Ok(result)
    }

    async fn workspace_needs_refresh(
        &self,
        location: &WorkspaceIndexLocation,
        factory: &dyn WorkspaceIndexStorageFactory,
    ) -> Result<bool, EngineError> {
        let _lock = acquire_home_lock(&location.home, LockMode::Read, "context.refresh")?;
        if has_build(&location.home) {
            return Ok(false);
        }
        let Some(manifest) = read_workspace_manifest(&location.home)? else {
            return Ok(false);
        };
        if !is_indexed(&manifest) || manifest.workspace.index_policy == IndexPolicy::Disabled {
            return Ok(false);
        }
        assert_index_version(manifest.index_version)?;
        let storage = factory.open(WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: manifest.storage_home(),
            workspace_path: manifest.path.clone(),
        })?;
        let status =
            get_workspace_index_status(&manifest.workspace, storage.as_ref(), &self.scanner, None)
                .await;
        let close = storage.close();
        let status = status?;
        close?;
        Ok(status.files_added > 0
            || status.files_modified > 0
            || status.files_deleted > 0
            || status.files_pending > 0
            || status.files_failed > 0)
    }

    pub(crate) async fn info(&self, options: InfoOptions) -> Result<InfoResult, EngineError> {
        let requested_root = resolve_root(options.root.as_deref())?;
        let requested_location = workspace_index_location(&requested_root)?;
        let Some(location) = find_nearest_workspace(&requested_root)? else {
            return Ok(unindexed_info(
                requested_location,
                WorkspaceIndexPolicy::Undecided,
            ));
        };
        let _lock = acquire_home_lock(&location.home, LockMode::Read, "info")?;
        let Some(mut manifest) = read_workspace_manifest(&location.home)? else {
            return Ok(unindexed_info(location, WorkspaceIndexPolicy::Undecided));
        };
        self.reconcile_name(&mut manifest)?;
        let metadata_indexed = is_indexed(&manifest);
        let storage_exists = self.storage_factory.exists(&manifest.storage_home())?;
        let indexed = metadata_indexed && storage_exists;
        let status = if options.include_status && indexed {
            assert_index_version(manifest.index_version)?;
            let factory = &self.storage_factory;
            let storage = factory.open(WorkspaceIndexStorageOptions::ReadOnly {
                storage_path: manifest.storage_home(),
                workspace_path: manifest.path.clone(),
            })?;
            let status = get_workspace_index_status(
                &manifest.workspace,
                storage.as_ref(),
                &self.scanner,
                None,
            )
            .await;
            let close = storage.close();
            let status = status?;
            close?;
            Some(status)
        } else {
            None
        };

        Ok(InfoResult {
            root: location.root,
            indexed,
            index_policy: manifest.workspace.index_policy.into(),
            home: location.home,
            index_path: manifest.storage_home().join("storage"),
            source: if indexed {
                InfoSource::Index
            } else {
                InfoSource::Unindexed
            },
            workspace_index: Some(workspace_info(&manifest)),
            status,
            suggestion: workspace_suggestion(&manifest, indexed),
        })
    }

    pub(crate) fn drop_index(&self, options: &InfoOptions) -> Result<bool, EngineError> {
        let location = workspace_index_location_from_option(options.root.as_deref())?;
        let registry = self.registry()?;
        let name = registry.name_for_root(&location.root)?;
        let factory = &self.storage_factory;
        if name.is_none() && !workspace_has_index_data(&location, factory.as_ref())? {
            return Ok(false);
        }
        if !location.root.try_exists().map_err(|error| {
            EngineError::from_io(
                format!("inspect workspace root {}", location.root.display()),
                &error,
            )
        })? {
            return registry.unregister_missing(&location.root);
        }
        let _lock = acquire_home_lock(&location.home, LockMode::Write, "index.drop")?;
        let registration = if let Some(name) = registry.name_for_root(&location.root)? {
            Some((name, location.root.clone()))
        } else {
            // Corrupt metadata must not prevent explicit cleanup. Valid metadata
            // also lets a moved workspace release its previous registry location.
            let manifest = read_workspace_manifest(&location.home)
                .ok()
                .flatten()
                .or_else(|| {
                    crate::workspace::build::read_build(&location.home)
                        .ok()
                        .flatten()
                        .map(|build| build.target)
                });
            manifest
                .as_ref()
                .map(|manifest| moved_registration(&registry, manifest))
                .transpose()?
                .flatten()
        };
        let has_data = workspace_has_index_data(&location, factory.as_ref())?;
        if has_data {
            reset_workspace_index(&location, factory.as_ref())?;
        }
        if let Some((name, registered_root)) = &registration {
            registry.unregister(name, registered_root)?;
        }
        Ok(has_data || registration.is_some())
    }
}

fn workspace_has_index_data(
    location: &WorkspaceIndexLocation,
    factory: &dyn WorkspaceIndexStorageFactory,
) -> Result<bool, EngineError> {
    Ok(location.manifest_path.exists()
        || has_build(&location.home)
        || crate::storage::workspace_identities_exist(&location.home)?
        || factory.exists(&location.home)?
        || has_generation_storage(&location.home)?)
}

fn index_manifest(
    location: &WorkspaceIndexLocation,
    active: Option<&WorkspaceManifest>,
    settings: Option<&WorkspaceManifest>,
    options: &IndexOptions,
    model: &ModelRuntimeLease,
) -> Result<WorkspaceManifest, EngineError> {
    let identity = active.or(settings);
    let now = epoch_millis();
    let workspace = Workspace {
        name: options
            .name
            .as_deref()
            .map(WorkspaceName::new)
            .transpose()?
            .or_else(|| identity.map(|value| value.workspace.name.clone()))
            .map_or_else(|| WorkspaceName::new(workspace_name(&location.root)), Ok)?,
        root: location.root.clone(),
        file_selection: resolve_discovery(settings, options),
        index_policy: IndexPolicy::Enabled,
        index: Some(WorkspaceIndex {
            embedding: embedding_schema(model),
            revision: active.and_then(WorkspaceManifest::revision).unwrap_or(0),
        }),
        created_epoch_ms: identity.map_or(now, |value| value.workspace.created_epoch_ms),
        updated_epoch_ms: now,
    };
    let runtime = embedding_runtime(settings, options, model)?;
    let mut manifest = WorkspaceManifest::new(
        workspace,
        location.home.clone(),
        Some(CURRENT_INDEX_VERSION),
        runtime,
    )?;
    manifest.storage_generation = active.and_then(|value| value.storage_generation.clone());
    Ok(manifest)
}

impl Default for WorkspaceIndexService {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for WorkspaceIndexService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceIndexService")
            .field("scanner", &self.scanner)
            .finish_non_exhaustive()
    }
}

fn refresh_options(options: &ContextOptions, root: PathBuf) -> IndexOptions {
    IndexOptions {
        root: Some(root),
        // Keep automatic refresh distinct from an explicit resume request. The
        // write-locked pending-build check also covers a stage created after the
        // query's earlier read-locked freshness check.
        changes: vec![crate::api::index::options::WorkspaceChange::Rescan],
        on_progress: options.on_progress.clone(),
        signal: options.signal.clone(),
        allow_remote: options.allow_remote,
        api_key: options.api_key.clone(),
        endpoint: options.endpoint.clone(),
        embedding_concurrency: options.embedding_concurrency,
        device: options.device,
        model_cache: options.model_cache.clone(),
        embedding: options.authorization_model.as_ref().map(|reference| {
            crate::api::index::options::EmbeddingModelSpec {
                reference: reference.clone(),
                revision: None,
                cache_dir: options.model_cache.clone(),
                endpoint: options.endpoint.clone(),
                device: options
                    .device
                    .unwrap_or(crate::api::index::options::Device::Auto),
            }
        }),
        ..IndexOptions::default()
    }
}

fn acquire_model(
    models: &ModelRuntimeManager,
    existing: Option<&WorkspaceManifest>,
    options: &IndexOptions,
) -> Result<ModelRuntimeLease, EngineError> {
    if options
        .embedding
        .as_ref()
        .and_then(|embedding| embedding.revision.as_ref())
        .is_some()
    {
        return Err(EngineError::unsupported(
            "embedding revision overrides are not supported by the catalog-backed runtime",
        ));
    }
    let reference = embedding_reference(existing, options.embedding.as_ref())?;
    let config = crate::config::read()?;
    let local = reference.starts_with("local/");
    if (!local && options.device.is_some()) || (local && options.endpoint.is_some()) {
        return Err(EngineError::invalid_argument(
            "device requires a local model; endpoint requires a remote model",
        ));
    }
    let existing_runtime = existing.map(|manifest| &manifest.embedding_runtime);
    let api_key = if local {
        None
    } else {
        options
            .api_key
            .clone()
            .or_else(|| existing_runtime.and_then(|runtime| runtime.api_key.clone()))
            .or_else(|| {
                crate::config::string(
                    &config,
                    &[
                        "providers",
                        reference.split('/').next().unwrap_or_default(),
                        "apiKey",
                    ],
                )
            })
            .or_else(environment_api_key)
    };
    let endpoint = options.endpoint.clone().or_else(|| {
        options
            .embedding
            .as_ref()
            .and_then(|embedding| embedding.endpoint.clone())
            .or_else(|| existing_runtime.and_then(|runtime| runtime.endpoint.clone()))
    });
    let endpoint = if local {
        endpoint
    } else {
        let endpoint = crate::authorization::remote_endpoint(&reference, endpoint.as_deref())?;
        let root = resolve_root(options.root.as_deref())?;
        crate::authorization::require(&root, &reference, &endpoint, options.allow_remote)?;
        Some(endpoint)
    };
    let device = if local {
        crate::config::runtime_device(
            &config,
            &reference,
            options.device.or_else(|| {
                options
                    .embedding
                    .as_ref()
                    .map(|e| e.device)
                    .filter(|device| *device != Device::Auto)
            }),
            existing_runtime.and_then(|runtime| runtime.device),
        )?
    } else {
        None
    };
    models
        .acquire(ModelRuntimeRequest::new(
            reference.clone(),
            CreateEmbeddingModelOptions {
                api_key,
                endpoint,
                model_cache_dir: crate::config::model_cache(
                    &config,
                    options
                        .model_cache
                        .clone()
                        .or_else(|| options.embedding.as_ref().and_then(|e| e.cache_dir.clone())),
                    existing_runtime.and_then(|runtime| runtime.cache_dir.clone()),
                ),
                device,
                ..CreateEmbeddingModelOptions::default()
            },
            options.embedding_concurrency,
        ))
        .map_err(ModelError::into_engine_error)
}

fn acquire_search_model(
    models: &ModelRuntimeManager,
    manifest: &WorkspaceManifest,
    embedding_concurrency: Option<usize>,
    options: &ContextOptions,
    root: &Path,
) -> Result<ModelRuntimeLease, EngineError> {
    let schema = manifest.embedding().ok_or_else(|| {
        workspace_index_unavailable(&manifest.path, "embedding schema is missing")
    })?;
    let reference = format!("{}/{}", schema.provider, schema.model);
    if options
        .authorization_model
        .as_ref()
        .is_some_and(|expected| expected != &reference)
    {
        return Err(EngineError::permission_denied(
            "Workspace embedding model changed after authorization; retry the query",
        ));
    }
    let config = crate::config::read()?;
    let local = schema.provider == "local";
    if !local && options.device.is_some() {
        return Err(EngineError::invalid_argument(
            "--device is only supported for local embedding models",
        ));
    }
    let endpoint = if local {
        None
    } else {
        let endpoint = crate::authorization::remote_endpoint(
            &reference,
            options
                .endpoint
                .as_deref()
                .or(manifest.embedding_runtime.endpoint.as_deref()),
        )?;
        crate::authorization::require(root, &reference, &endpoint, options.allow_remote)?;
        Some(endpoint)
    };
    models
        .acquire(ModelRuntimeRequest::new(
            reference.clone(),
            CreateEmbeddingModelOptions {
                api_key: (!local)
                    .then(|| {
                        options
                            .api_key
                            .clone()
                            .or_else(|| manifest.embedding_runtime.api_key.clone())
                            .or_else(|| {
                                crate::config::string(
                                    &config,
                                    &["providers", &schema.provider, "apiKey"],
                                )
                            })
                            .or_else(environment_api_key)
                    })
                    .flatten(),
                endpoint,
                device: if local {
                    crate::config::runtime_device(
                        &config,
                        &reference,
                        options.device,
                        manifest.embedding_runtime.device,
                    )?
                } else {
                    None
                },
                model_cache_dir: crate::config::model_cache(
                    &config,
                    options.model_cache.clone(),
                    manifest.embedding_runtime.cache_dir.clone(),
                ),
                ..CreateEmbeddingModelOptions::default()
            },
            embedding_concurrency,
        ))
        .map_err(ModelError::into_engine_error)
}

pub(crate) fn embedding_reference(
    existing: Option<&WorkspaceManifest>,
    requested: Option<&EmbeddingModelSpec>,
) -> Result<String, EngineError> {
    resolve_embedding_reference(ResolveEmbeddingReferenceOptions {
        explicit: requested.map(|embedding| embedding.reference.clone()),
        existing: existing
            .and_then(|manifest| manifest.embedding())
            .map(|embedding| format!("{}/{}", embedding.provider, embedding.model)),
        global_default: crate::config::string(&crate::config::read()?, &["defaults", "embedding"]),
        fallback: Some(DEFAULT_LOCAL_EMBEDDING.to_owned()),
        ..ResolveEmbeddingReferenceOptions::default()
    })
    .map_err(ModelError::into_engine_error)?
    .ok_or_else(|| EngineError::internal("default embedding model is not configured"))
}

fn environment_api_key() -> Option<String> {
    ["ZVEC_GREP_API_KEY", "DASHSCOPE_API_KEY", "QWEN_API_KEY"]
        .into_iter()
        .find_map(|name| {
            env::var(name)
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
}

fn assert_embedding_compatible(
    existing: Option<&WorkspaceManifest>,
    model: &ModelRuntimeLease,
) -> Result<(), EngineError> {
    let Some(existing) = existing.filter(|manifest| is_indexed(manifest)) else {
        return Ok(());
    };
    let Some(schema) = existing.embedding() else {
        return Ok(());
    };
    schema.ensure_compatible(&embedding_schema(model))
}

pub(crate) fn resolve_discovery(
    existing: Option<&WorkspaceManifest>,
    options: &IndexOptions,
) -> DiscoveryOptions {
    let mut discovery = if options.reset_paths {
        DiscoveryOptions::default()
    } else {
        existing.map_or_else(DiscoveryOptions::default, |manifest| {
            manifest.workspace.file_selection.clone()
        })
    };
    apply_discovery_overrides(&mut discovery, &options.discovery);
    discovery
}

fn apply_discovery_overrides(target: &mut DiscoveryOptions, overrides: &DiscoveryOptions) {
    if !overrides.include_paths.is_empty() {
        target.include_paths.clone_from(&overrides.include_paths);
    }
    if !overrides.exclude_paths.is_empty() {
        target.exclude_paths.clone_from(&overrides.exclude_paths);
    }
    if !overrides.globs.is_empty() {
        target.globs.clone_from(&overrides.globs);
    }
    if !overrides.insensitive_globs.is_empty() {
        target
            .insensitive_globs
            .clone_from(&overrides.insensitive_globs);
    }
    if !overrides.file_types.is_empty() {
        target.file_types.clone_from(&overrides.file_types);
    }
    if !overrides.excluded_file_types.is_empty() {
        target
            .excluded_file_types
            .clone_from(&overrides.excluded_file_types);
    }
    if !overrides.ignore_files.is_empty() {
        target.ignore_files.clone_from(&overrides.ignore_files);
    }
    target.hidden |= overrides.hidden;
    target.no_ignore |= overrides.no_ignore;
    target.follow |= overrides.follow;
    if overrides.max_depth.is_some() {
        target.max_depth = overrides.max_depth;
    }
    if overrides.max_file_size_bytes.is_some() {
        target.max_file_size_bytes = overrides.max_file_size_bytes;
    }
}

fn embedding_runtime(
    existing: Option<&WorkspaceManifest>,
    options: &IndexOptions,
    model: &ModelRuntimeLease,
) -> Result<EmbeddingRuntimeConfig, EngineError> {
    let current = existing
        .map(|manifest| manifest.embedding_runtime.clone())
        .unwrap_or_default();
    let config = crate::config::read()?;
    if model.info().provider == "local" {
        let reference = format!("{}/{}", model.info().provider, model.info().name);
        Ok(EmbeddingRuntimeConfig {
            cache_dir: crate::config::model_cache(
                &config,
                options
                    .model_cache
                    .clone()
                    .or_else(|| options.embedding.as_ref().and_then(|e| e.cache_dir.clone())),
                current.cache_dir,
            ),
            device: crate::config::runtime_device(
                &config,
                &reference,
                options.device.or_else(|| {
                    options
                        .embedding
                        .as_ref()
                        .map(|e| e.device)
                        .filter(|device| *device != Device::Auto)
                }),
                current.device,
            )?,
            ..EmbeddingRuntimeConfig::default()
        })
    } else {
        Ok(EmbeddingRuntimeConfig {
            api_key: current.api_key,
            endpoint: model.info().endpoint.clone().or(current.endpoint),
            device: None,
            cache_dir: None,
        })
    }
}

fn embedding_schema(model: &ModelRuntimeLease) -> EmbeddingSchema {
    let info = model.info();
    EmbeddingSchema {
        provider: info.provider.clone(),
        model: info.name.clone(),
        dimension: info.dimension,
        metric: info.metric,
    }
}

fn storage_embedding_schema(model: &ModelRuntimeLease) -> WorkspaceIndexEmbeddingSchema {
    let info = model.info();
    WorkspaceIndexEmbeddingSchema {
        provider: info.provider.clone(),
        model: info.name.clone(),
        dimension: info.dimension,
        metric: info.metric,
    }
}

fn assert_index_version(version: Option<u32>) -> Result<(), EngineError> {
    if let Some(version) = version
        && version != CURRENT_INDEX_VERSION
    {
        return Err(EngineError::storage_failure(format!(
            "unsupported index version {version}; expected {CURRENT_INDEX_VERSION}; rebuild the index"
        )));
    }
    Ok(())
}

fn is_indexed(manifest: &WorkspaceManifest) -> bool {
    manifest.workspace.indexed() && manifest.index_version.is_some()
}

fn validate_workspace_root(root: &Path) -> Result<(), EngineError> {
    if !root.is_dir() {
        return Err(EngineError::invalid_argument(format!(
            "workspace root must be an existing directory: {}",
            root.display()
        )));
    }
    Ok(())
}

fn workspace_index_location_from_option(
    root: Option<&Path>,
) -> Result<WorkspaceIndexLocation, EngineError> {
    workspace_index_location(&resolve_root(root)?)
}

fn resolve_root(root: Option<&Path>) -> Result<PathBuf, EngineError> {
    let root = root
        .map_or_else(env::current_dir, |root| Ok(root.to_path_buf()))
        .map_err(|error| EngineError::from_io("failed to resolve current directory", &error))?;
    if root.is_absolute() {
        Ok(root)
    } else {
        env::current_dir()
            .map(|current| current.join(root))
            .map_err(|error| EngineError::from_io("failed to resolve workspace root", &error))
    }
}

fn workspace_name(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace")
        .to_owned()
}

fn unindexed_info(location: WorkspaceIndexLocation, policy: WorkspaceIndexPolicy) -> InfoResult {
    InfoResult {
        root: location.root,
        indexed: false,
        index_policy: policy,
        home: location.home,
        index_path: location.index_path,
        source: InfoSource::Unindexed,
        workspace_index: None,
        status: None,
        suggestion: Some("run index to create a workspace index".to_owned()),
    }
}

fn workspace_suggestion(manifest: &WorkspaceManifest, indexed: bool) -> Option<String> {
    if manifest.workspace.index_policy == IndexPolicy::Disabled {
        Some("indexing is disabled for this workspace".to_owned())
    } else if !indexed {
        Some("workspace manifest exists but index storage is missing".to_owned())
    } else {
        None
    }
}

#[track_caller]
fn workspace_index_unavailable(root: &Path, reason: &str) -> EngineError {
    EngineError::not_found(format!("workspace index at {}: {reason}", root.display()))
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

fn workspace_info(manifest: &WorkspaceManifest) -> WorkspaceIndexInfo {
    WorkspaceIndexInfo::from_workspace(&manifest.workspace, &manifest.path, manifest.index_version)
}

/// Find the old registration of a physically moved workspace. The recorded root
/// also recovers a registry rename committed before the local manifest update.
fn moved_registration(
    registry: &WorkspaceRegistry,
    manifest: &WorkspaceManifest,
) -> Result<Option<(WorkspaceName, PathBuf)>, EngineError> {
    let absent = |root: &Path| {
        root.try_exists().map(|exists| !exists).map_err(|error| {
            EngineError::from_io(
                format!("inspect original workspace {}", root.display()),
                &error,
            )
        })
    };
    if manifest.recorded_root != manifest.workspace.root
        && absent(&manifest.recorded_root)?
        && let Some(name) = registry.name_for_root(&manifest.recorded_root)?
    {
        return Ok(Some((name, manifest.recorded_root.clone())));
    }
    if let Some(root) = registry.root_for_name(&manifest.workspace.name)?
        && absent(&root)?
    {
        return Ok(Some((manifest.workspace.name.clone(), root)));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::{
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    use tempfile::tempdir;

    use crate::{
        api::{
            index::{
                IndexOptions,
                options::{Device, DiscoveryOptions, EmbeddingModelSpec},
            },
            info::InfoOptions,
        },
        domain::{FileRecord, FileSelection, IndexPolicy, Workspace, WorkspaceName},
        storage::spi::{
            IndexedFragment, StorageResult, StorageSearchFilter, StorageSearchHit, StoredEntity,
            WorkspaceIndexStorage, WorkspaceIndexStorageFactory, WorkspaceIndexStorageOptions,
        },
    };

    use super::{ModelRuntimeManager, WorkspaceIndexService};

    #[derive(Debug, Default)]
    struct MemoryStorageFactory {
        exists: Arc<AtomicBool>,
        fail_finalize: Arc<AtomicBool>,
    }

    impl WorkspaceIndexStorageFactory for MemoryStorageFactory {
        fn open(
            &self,
            options: WorkspaceIndexStorageOptions,
        ) -> StorageResult<Box<dyn WorkspaceIndexStorage>> {
            if !options.is_read_only() {
                std::fs::create_dir_all(options.storage_path().join("storage"))
                    .expect("memory storage marker");
                self.exists.store(true, Ordering::Release);
            }
            Ok(Box::new(EmptyStorage {
                read_only: options.is_read_only(),
                fail_finalize: self.fail_finalize.clone(),
            }))
        }

        fn exists(&self, storage_path: &Path) -> StorageResult<bool> {
            Ok(storage_path.join("storage").is_dir())
        }

        fn delete(&self, storage_path: &Path) -> StorageResult<()> {
            self.exists.store(false, Ordering::Release);
            if storage_path.join("storage").exists() {
                std::fs::remove_dir_all(storage_path.join("storage"))
                    .expect("remove memory storage marker");
            }
            Ok(())
        }
    }

    #[derive(Debug)]
    struct EmptyStorage {
        read_only: bool,
        fail_finalize: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl WorkspaceIndexStorage for EmptyStorage {
        fn is_read_only(&self) -> bool {
            self.read_only
        }

        fn list_files(&self) -> StorageResult<Vec<FileRecord>> {
            Ok(Vec::new())
        }

        fn resolve_file_ids(
            &self,
            paths: &[std::path::PathBuf],
        ) -> StorageResult<Vec<crate::domain::FileId>> {
            assert!(
                paths.is_empty(),
                "this storage fixture only models empty workspaces"
            );
            Ok(Vec::new())
        }

        fn get_entity(
            &self,
            _entity_id: &crate::domain::EntityId,
        ) -> StorageResult<Option<StoredEntity>> {
            Ok(None)
        }

        fn search_fts(
            &self,
            _query: &str,
            _limit: usize,
            _filter: Option<&StorageSearchFilter>,
        ) -> StorageResult<Vec<StorageSearchHit>> {
            Ok(Vec::new())
        }

        fn search_vector(
            &self,
            _vector: &[f32],
            _limit: usize,
            _filter: Option<&StorageSearchFilter>,
        ) -> StorageResult<Vec<StorageSearchHit>> {
            Ok(Vec::new())
        }

        fn replace_file(
            &self,
            _file: &FileRecord,
            _entries: &[IndexedFragment],
        ) -> StorageResult<()> {
            Ok(())
        }

        fn mark_file_failed(&self, _file: &FileRecord, _error: &str) -> StorageResult<()> {
            Ok(())
        }

        fn delete_file(&self, _file_id: crate::domain::FileId) -> StorageResult<()> {
            Ok(())
        }

        async fn finalize_writes(&self) -> StorageResult<()> {
            if self.fail_finalize.load(Ordering::Relaxed) {
                return Err(crate::EngineError::storage_failure(
                    "injected checkpoint failure",
                ));
            }
            Ok(())
        }

        fn close(&self) -> StorageResult<()> {
            Ok(())
        }
    }

    fn write_legacy_manifest(
        home: &std::path::Path,
        manifest: &crate::workspace::manifest::WorkspaceManifest,
    ) {
        let mut legacy = serde_json::to_value(manifest).expect("legacy manifest value");
        legacy["manifestVersion"] = 1.into();
        legacy["indexVersion"] = 3.into();
        let root = legacy
            .as_object_mut()
            .expect("manifest object")
            .remove("root")
            .expect("workspace root");
        let mut discovery = legacy
            .as_object_mut()
            .expect("manifest object")
            .remove("discovery")
            .expect("discovery");
        discovery["absolutePath"] = root;
        discovery["recursive"] = true.into();
        legacy["rootPaths"] = serde_json::json!([discovery]);
        std::fs::write(
            home.join("manifest.json"),
            serde_json::to_vec(&legacy).expect("legacy manifest json"),
        )
        .expect("legacy manifest write");
    }

    #[test]
    fn rejects_incompatible_index_versions() {
        for version in [None, Some(super::CURRENT_INDEX_VERSION)] {
            super::assert_index_version(version).expect("supported or unbuilt index");
        }
        for version in [1, 2, 3, 4, super::CURRENT_INDEX_VERSION + 1] {
            let error = super::assert_index_version(Some(version))
                .expect_err("incompatible text coordinates");
            assert!(error.message().contains("rebuild the index"));
        }
    }

    #[test]
    fn discovery_overrides_preserve_saved_settings_until_reset() {
        let directory = tempdir().expect("workspace");
        let manifest = crate::workspace::manifest::WorkspaceManifest::new(
            Workspace {
                name: WorkspaceName::new("workspace").expect("workspace name"),
                root: directory.path().to_path_buf(),
                file_selection: FileSelection {
                    include_paths: vec!["src".into()],
                    globs: vec!["*.rs".into()],
                    hidden: true,
                    ..FileSelection::default()
                },
                index_policy: IndexPolicy::Enabled,
                index: None,
                created_epoch_ms: 0,
                updated_epoch_ms: 0,
            },
            directory.path().join(".zvec-grep"),
            None,
            crate::workspace::manifest::EmbeddingRuntimeConfig::default(),
        )
        .expect("manifest");
        let mut options = IndexOptions {
            discovery: DiscoveryOptions {
                globs: vec!["*.md".into()],
                ..DiscoveryOptions::default()
            },
            ..IndexOptions::default()
        };
        let resolved = super::resolve_discovery(Some(&manifest), &options);
        assert_eq!(resolved.include_paths, ["src"]);
        assert_eq!(resolved.globs, ["*.md"]);
        assert!(resolved.hidden);
        options.reset_paths = true;
        assert_eq!(
            super::resolve_discovery(Some(&manifest), &options),
            options.discovery
        );
    }

    #[tokio::test]
    async fn a_file_cannot_be_a_workspace_root() {
        let directory = tempdir().expect("workspace");
        let file = directory.path().join("file.txt");
        std::fs::write(&file, "text").expect("source file");
        let service =
            WorkspaceIndexService::with_storage_factory(Arc::new(MemoryStorageFactory::default()));
        let models = ModelRuntimeManager::new();
        let error = service
            .index(
                &models,
                IndexOptions {
                    root: Some(file),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect_err("workspace requires a directory");
        assert!(error.message().contains("existing directory"));
        assert_eq!(models.snapshot().cached_runtimes, 0);
    }

    #[tokio::test]
    async fn reopening_a_moved_workspace_uses_its_current_root() {
        let directory = tempdir().expect("temporary directory");
        let original = directory.path().join("original");
        let moved = directory.path().join("moved");
        std::fs::create_dir(&original).expect("workspace root");
        let service =
            WorkspaceIndexService::with_storage_factory(Arc::new(MemoryStorageFactory::default()));
        let models = ModelRuntimeManager::new();
        service
            .index(
                &models,
                IndexOptions {
                    root: Some(original.clone()),
                    discovery: DiscoveryOptions {
                        globs: vec!["*.rs".into()],
                        ..DiscoveryOptions::default()
                    },
                    embedding: Some(EmbeddingModelSpec {
                        reference: "local/potion-code-16m-v2".into(),
                        revision: None,
                        cache_dir: None,
                        endpoint: None,
                        device: Device::Cpu,
                    }),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect("empty workspace index");
        let before = super::read_workspace_manifest(&original.join(".zvec-grep"))
            .expect("manifest read")
            .expect("manifest");
        std::fs::rename(&original, &moved).expect("move workspace");
        let info = service
            .info(InfoOptions {
                root: Some(moved.clone()),
                include_status: true,
            })
            .await
            .expect("moved workspace info");
        let workspace = info.workspace_index.expect("workspace index");
        assert_eq!(workspace.name, before.workspace.name.as_str());
        assert_eq!(
            workspace.root,
            std::fs::canonicalize(&moved).expect("moved root")
        );
        assert_eq!(workspace.path, workspace.root.join(".zvec-grep"));
        assert_eq!(workspace.discovery.globs, ["*.rs"]);
        assert_eq!(info.status.expect("status").files_scanned, 0);
        service
            .index(
                &models,
                IndexOptions {
                    root: Some(moved.clone()),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect("update moved workspace");
        let after = super::read_workspace_manifest(&moved.join(".zvec-grep"))
            .expect("manifest read")
            .expect("manifest");
        assert_eq!(after.workspace.name, before.workspace.name);
        assert_eq!(
            after.workspace.file_selection,
            before.workspace.file_selection
        );
        assert_eq!(after.workspace.root, moved);
        assert_eq!(
            service
                .registry()
                .expect("registry")
                .root_for_name(&before.workspace.name)
                .expect("registered move"),
            Some(std::fs::canonicalize(&moved).expect("moved root"))
        );
        models.close();
    }

    #[test]
    fn dropping_a_missing_index_is_an_idempotent_no_op() {
        let directory = tempdir().expect("temporary directory");

        assert!(
            !WorkspaceIndexService::with_storage_factory(Arc::new(MemoryStorageFactory::default()))
                .drop_index(&InfoOptions {
                    root: Some(directory.path().to_path_buf()),
                    include_status: false,
                })
                .expect("missing index should be an idempotent no-op")
        );
    }

    #[test]
    fn dropping_orphaned_generation_storage_does_not_require_a_manifest() {
        let directory = tempdir().expect("workspace");
        let home = directory.path().join(".zvec-grep");
        let storage = home
            .join("generations")
            .join(uuid::Uuid::new_v4().to_string())
            .join("storage");
        std::fs::create_dir_all(&storage).expect("orphaned generation");
        let service =
            WorkspaceIndexService::with_storage_factory(Arc::new(MemoryStorageFactory::default()));
        let options = InfoOptions {
            root: Some(directory.path().to_path_buf()),
            include_status: false,
        };
        assert!(!home.join("manifest.json").exists());
        assert!(!home.join("build.json").exists());
        assert!(service.drop_index(&options).expect("drop orphaned storage"));
        assert!(!storage.exists());
        assert!(home.join("generations").is_dir());
        assert!(
            !service
                .drop_index(&options)
                .expect("empty container is not an index")
        );
    }

    #[test]
    fn dropping_an_orphaned_identity_catalog_cleans_its_paths_without_a_manifest() {
        let directory = tempdir().expect("workspace");
        let home = directory.path().join(".zvec-grep");
        std::fs::create_dir(&home).expect("workspace home");
        // Explicit drop must also clean damaged catalogs without decoding them.
        std::fs::write(home.join("identity.json"), b"broken identity catalog")
            .expect("orphaned catalog");
        let service =
            WorkspaceIndexService::with_storage_factory(Arc::new(MemoryStorageFactory::default()));
        let options = InfoOptions {
            root: Some(directory.path().to_path_buf()),
            include_status: false,
        };
        assert!(
            service
                .drop_index(&options)
                .expect("drop orphaned identities")
        );
        assert!(!home.join("identity.json").exists());
        assert!(home.join("identity.lock").is_file());
        assert!(
            !service
                .drop_index(&options)
                .expect("lock alone is not index data")
        );
    }

    #[tokio::test]
    async fn cancellation_retains_active_and_the_next_index_resumes_its_stage() {
        let directory = tempdir().expect("workspace");
        let factory = Arc::new(MemoryStorageFactory::default());
        let service = WorkspaceIndexService::with_storage_factory(factory);
        let models = ModelRuntimeManager::new();
        let options = empty_index_options(directory.path());
        service
            .index(&models, options.clone())
            .await
            .expect("initial index");
        let home = directory.path().join(".zvec-grep");
        let active = super::read_workspace_manifest(&home)
            .expect("manifest")
            .expect("active");
        let signal = tokio_util::sync::CancellationToken::new();
        signal.cancel();
        service
            .index(
                &models,
                IndexOptions {
                    rebuild: true,
                    signal: Some(signal),
                    ..options.clone()
                },
            )
            .await
            .expect_err("cancelled rebuild");
        assert_eq!(
            super::read_workspace_manifest(&home).expect("manifest"),
            Some(active.clone())
        );
        assert!(active.storage_home().join("storage").exists());
        let pending = crate::workspace::build::read_build(&home)
            .expect("build")
            .expect("pending");
        let refresh = super::refresh_options(
            &crate::api::context::ContextOptions::default(),
            directory.path().to_path_buf(),
        );
        assert_eq!(
            refresh.changes,
            [crate::api::index::options::WorkspaceChange::Rescan]
        );
        let refresh_error = service
            .index(&models, refresh)
            .await
            .expect_err("automatic refresh cannot implicitly publish the pending build");
        assert_eq!(refresh_error.code(), crate::EngineError::RESOURCE_BUSY);
        assert_eq!(
            crate::workspace::build::read_build(&home)
                .expect("build")
                .expect("pending")
                .target
                .storage_generation,
            pending.target.storage_generation,
        );
        let result = service
            .index(
                &models,
                IndexOptions {
                    root: options.root,
                    ..IndexOptions::default()
                },
            )
            .await
            .expect("ordinary index resumes rebuild");
        let published = super::read_workspace_manifest(&home)
            .expect("manifest")
            .expect("active");
        assert_eq!(
            published.storage_generation,
            pending.target.storage_generation
        );
        assert_eq!(published.workspace.name, active.workspace.name);
        assert_eq!(
            published.workspace.created_epoch_ms,
            active.workspace.created_epoch_ms
        );
        assert_eq!(result.generation, 2);
        assert!(!super::has_build(&home));
        assert!(!active.storage_home().exists());
        models.close();
    }

    #[tokio::test]
    async fn failed_checkpoint_does_not_publish_an_incomplete_rebuild() {
        let directory = tempdir().expect("workspace");
        let factory = Arc::new(MemoryStorageFactory::default());
        let service = WorkspaceIndexService::with_storage_factory(factory.clone());
        let models = ModelRuntimeManager::new();
        let options = empty_index_options(directory.path());
        service
            .index(&models, options.clone())
            .await
            .expect("initial index");
        let home = directory.path().join(".zvec-grep");
        let active = super::read_workspace_manifest(&home)
            .expect("manifest")
            .expect("active");
        factory.fail_finalize.store(true, Ordering::Relaxed);
        let error = service
            .index(
                &models,
                IndexOptions {
                    rebuild: true,
                    ..options
                },
            )
            .await
            .expect_err("checkpoint fails");
        assert!(error.message().contains("checkpoint"));
        assert_eq!(
            super::read_workspace_manifest(&home).expect("manifest"),
            Some(active.clone())
        );
        assert!(active.storage_home().join("storage").exists());
        assert!(super::has_build(&home));
        // Query-driven refresh must not resume a differently configured stage.
        let location = super::workspace_index_location(directory.path()).expect("location");
        assert!(
            !service
                .workspace_needs_refresh(&location, factory.as_ref())
                .await
                .expect("refresh decision")
        );
        assert!(
            service
                .drop_index(&InfoOptions {
                    root: Some(directory.path().to_path_buf()),
                    include_status: false
                })
                .expect("drop incomplete build")
        );
        assert!(!super::has_build(&home));
        assert!(!active.storage_home().exists());
        models.close();
    }

    fn empty_index_options(root: &Path) -> IndexOptions {
        IndexOptions {
            root: Some(root.to_path_buf()),
            embedding: Some(EmbeddingModelSpec {
                reference: "local/potion-code-16m-v2".into(),
                revision: None,
                cache_dir: None,
                endpoint: None,
                device: Device::Cpu,
            }),
            ..IndexOptions::default()
        }
    }

    #[tokio::test]
    async fn explicit_drop_releases_a_deleted_source_directory_without_recreating_it() {
        let directory = tempdir().expect("workspace roots");
        let deleted = directory.path().join("deleted");
        let replacement = directory.path().join("replacement");
        std::fs::create_dir(&deleted).expect("original root");
        std::fs::create_dir(&replacement).expect("replacement root");
        let service =
            WorkspaceIndexService::with_storage_factory(Arc::new(MemoryStorageFactory::default()));
        let models = ModelRuntimeManager::new();
        service
            .index(
                &models,
                IndexOptions {
                    name: Some("shared".into()),
                    ..empty_index_options(&deleted)
                },
            )
            .await
            .expect("original workspace owns name");
        std::fs::remove_dir_all(&deleted).expect("source directory deleted externally");
        let options = InfoOptions {
            root: Some(deleted.clone()),
            include_status: false,
        };
        assert!(
            service
                .drop_index(&options)
                .expect("drop deleted workspace")
        );
        assert!(!deleted.exists());
        assert!(!service.drop_index(&options).expect("idempotent cleanup"));
        let name = WorkspaceName::new("shared").expect("workspace name");
        let registry = service.registry().expect("registry");
        assert_eq!(registry.root_for_name(&name).expect("released name"), None);
        service
            .index(
                &models,
                IndexOptions {
                    name: Some(name.to_string()),
                    ..empty_index_options(&replacement)
                },
            )
            .await
            .expect("replacement workspace can reuse name");
        assert_eq!(
            registry.root_for_name(&name).expect("replacement owner"),
            Some(std::fs::canonicalize(&replacement).expect("replacement root"))
        );
        models.close();
    }

    #[tokio::test]
    async fn dropping_a_moved_workspace_releases_its_old_registration_before_reindexing() {
        let directory = tempdir().expect("workspace roots");
        let original = directory.path().join("original");
        let moved = directory.path().join("moved");
        let replacement = directory.path().join("replacement");
        std::fs::create_dir(&original).expect("original root");
        std::fs::create_dir(&replacement).expect("replacement root");
        let service =
            WorkspaceIndexService::with_storage_factory(Arc::new(MemoryStorageFactory::default()));
        let models = ModelRuntimeManager::new();
        service
            .index(
                &models,
                IndexOptions {
                    name: Some("shared".into()),
                    ..empty_index_options(&original)
                },
            )
            .await
            .expect("original workspace owns name");
        std::fs::rename(&original, &moved).expect("move without reopening index");
        assert!(
            service
                .drop_index(&InfoOptions {
                    root: Some(moved.clone()),
                    include_status: false,
                })
                .expect("drop moved workspace")
        );
        let name = WorkspaceName::new("shared").expect("workspace name");
        let registry = service.registry().expect("registry");
        assert_eq!(registry.root_for_name(&name).expect("released name"), None);
        assert!(!moved.join(".zvec-grep/manifest.json").exists());

        service
            .index(
                &models,
                IndexOptions {
                    name: Some("shared".into()),
                    ..empty_index_options(&replacement)
                },
            )
            .await
            .expect("another root can reuse the dropped name");
        assert_eq!(
            registry.root_for_name(&name).expect("new owner"),
            Some(std::fs::canonicalize(&replacement).expect("replacement root"))
        );
        models.close();
    }

    #[tokio::test]
    async fn copied_pending_first_build_cannot_steal_the_original_name() {
        let directory = tempdir().expect("workspace roots");
        let original = directory.path().join("original");
        let copy = directory.path().join("copy");
        std::fs::create_dir(&original).expect("original root");
        std::fs::create_dir_all(copy.join(".zvec-grep")).expect("copy home");
        let service =
            WorkspaceIndexService::with_storage_factory(Arc::new(MemoryStorageFactory::default()));
        let models = ModelRuntimeManager::new();
        let signal = tokio_util::sync::CancellationToken::new();
        signal.cancel();
        service
            .index(
                &models,
                IndexOptions {
                    name: Some("shared".into()),
                    signal: Some(signal),
                    ..empty_index_options(&original)
                },
            )
            .await
            .expect_err("interrupted initial build");
        let original_build = original.join(".zvec-grep/build.json");
        let original_bytes = std::fs::read(&original_build).expect("original pending build");
        assert!(!original.join(".zvec-grep/manifest.json").exists());
        std::fs::copy(&original_build, copy.join(".zvec-grep/build.json"))
            .expect("copy pending metadata");
        let error = service
            .index(
                &models,
                IndexOptions {
                    root: Some(copy.clone()),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect_err("copy cannot claim the original reservation");
        assert!(error.message().contains("already registered"));
        let registry = service.registry().expect("registry");
        assert_eq!(
            registry
                .root_for_name(&WorkspaceName::new("shared").expect("name"))
                .expect("original owner"),
            Some(std::fs::canonicalize(&original).expect("original root"))
        );
        assert_eq!(
            std::fs::read(original_build).expect("original build unchanged"),
            original_bytes
        );
        assert_eq!(
            registry.name_for_root(&copy).expect("copy registration"),
            None
        );
        assert!(!copy.join(".zvec-grep/manifest.json").exists());
        models.close();
    }

    #[tokio::test]
    async fn duplicate_names_are_rejected_before_model_acquisition() {
        let directory = tempdir().expect("workspace roots");
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        std::fs::create_dir(&first).expect("first workspace");
        std::fs::create_dir(&second).expect("second workspace");
        let service =
            WorkspaceIndexService::with_storage_factory(Arc::new(MemoryStorageFactory::default()));
        let first_models = ModelRuntimeManager::new();
        service
            .index(
                &first_models,
                IndexOptions {
                    name: Some("shared".into()),
                    ..empty_index_options(&first)
                },
            )
            .await
            .expect("first workspace owns name");

        let second_models = ModelRuntimeManager::new();
        let error = service
            .clone()
            .index(
                &second_models,
                IndexOptions {
                    name: Some("shared".into()),
                    ..empty_index_options(&second)
                },
            )
            .await
            .expect_err("second workspace cannot claim name");
        assert!(error.message().contains("shared"));
        assert!(error.message().contains("already registered"));
        assert_eq!(second_models.snapshot().cached_runtimes, 0);
        assert!(!second.join(".zvec-grep/manifest.json").exists());
        assert!(!second.join(".zvec-grep/build.json").exists());
        assert_eq!(
            service
                .registry()
                .expect("registry")
                .root_for_name(&WorkspaceName::new("shared").expect("name"))
                .expect("owner"),
            Some(std::fs::canonicalize(&first).expect("first root"))
        );
        first_models.close();
        second_models.close();
    }

    #[tokio::test]
    async fn explicit_rename_preserves_storage_and_file_identity_catalog() {
        let directory = tempdir().expect("workspace");
        let service =
            WorkspaceIndexService::with_storage_factory(Arc::new(MemoryStorageFactory::default()));
        let models = ModelRuntimeManager::new();
        service
            .index(
                &models,
                IndexOptions {
                    name: Some("before".into()),
                    ..empty_index_options(directory.path())
                },
            )
            .await
            .expect("initial index");
        let home = directory.path().join(".zvec-grep");
        let before = super::read_workspace_manifest(&home)
            .expect("manifest")
            .expect("active workspace");
        // Identities of previously deleted source files remain reserved across
        // ordinary updates. Renaming must preserve their allocation history.
        let identities = serde_json::to_vec(&serde_json::json!({
            "version": 1, "last_file_id": 7, "last_directory_id": 1,
            "files": [{"path": {"encoding": "utf8", "value": "src/deleted.rs"}, "id": 7}],
            "directories": [{"path": {"encoding": "utf8", "value": "src"}, "id": 1}]
        }))
        .expect("identity catalog");
        std::fs::write(home.join("identity.json"), &identities).expect("existing file identities");
        service
            .index(
                &models,
                IndexOptions {
                    name: Some("after".into()),
                    root: Some(directory.path().to_path_buf()),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect("rename workspace");
        let after = super::read_workspace_manifest(&home)
            .expect("manifest")
            .expect("renamed workspace");
        assert_eq!(after.workspace.name.as_str(), "after");
        assert_eq!(after.storage_generation, before.storage_generation);
        assert_eq!(
            after.workspace.created_epoch_ms,
            before.workspace.created_epoch_ms
        );
        assert_eq!(
            std::fs::read(home.join("identity.json")).expect("retained file IDs"),
            identities
        );
        let info = service
            .info(InfoOptions {
                root: Some(directory.path().to_path_buf()),
                include_status: false,
            })
            .await
            .expect("renamed info");
        assert_eq!(info.workspace_index.expect("workspace").name, "after");
        service
            .index(
                &models,
                IndexOptions {
                    root: Some(directory.path().to_path_buf()),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect("update keeps new name");
        let updated = super::read_workspace_manifest(&home)
            .expect("manifest")
            .expect("updated workspace");
        assert_eq!(updated.workspace.name, after.workspace.name);
        assert_eq!(updated.storage_generation, before.storage_generation);
        assert_eq!(
            std::fs::read(home.join("identity.json")).expect("retained file IDs"),
            identities
        );
        assert_eq!(
            service
                .registry()
                .expect("registry")
                .root_for_name(&WorkspaceName::new("before").expect("old name"))
                .expect("released name"),
            None
        );
        models.close();
    }

    #[tokio::test]
    async fn registry_rename_is_replayed_after_manifest_update_is_interrupted() {
        let directory = tempdir().expect("workspace");
        let service =
            WorkspaceIndexService::with_storage_factory(Arc::new(MemoryStorageFactory::default()));
        let models = ModelRuntimeManager::new();
        service
            .index(
                &models,
                IndexOptions {
                    name: Some("before".into()),
                    ..empty_index_options(directory.path())
                },
            )
            .await
            .expect("initial index");
        let home = directory.path().join(".zvec-grep");
        let before = super::read_workspace_manifest(&home)
            .expect("manifest")
            .expect("active workspace");
        service
            .registry()
            .expect("registry")
            .rename(
                &before.workspace.name,
                &WorkspaceName::new("replayed").expect("new name"),
                directory.path(),
            )
            .expect("commit registry rename before crash");
        let info = service
            .info(InfoOptions {
                root: Some(directory.path().to_path_buf()),
                include_status: false,
            })
            .await
            .expect("info reconciles name");
        assert_eq!(info.workspace_index.expect("workspace").name, "replayed");
        assert_eq!(
            super::read_workspace_manifest(&home).expect("read-only info retained manifest"),
            Some(before.clone())
        );
        service
            .index(
                &models,
                IndexOptions {
                    root: Some(directory.path().to_path_buf()),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect("index replays rename");
        let after = super::read_workspace_manifest(&home)
            .expect("manifest")
            .expect("reconciled workspace");
        assert_eq!(after.workspace.name.as_str(), "replayed");
        assert_eq!(after.storage_generation, before.storage_generation);
        models.close();
    }

    #[tokio::test]
    async fn moved_workspace_recovers_a_rename_committed_only_to_the_registry() {
        let directory = tempdir().expect("workspace roots");
        let original = directory.path().join("original");
        let moved = directory.path().join("moved");
        let service =
            WorkspaceIndexService::with_storage_factory(Arc::new(MemoryStorageFactory::default()));
        let models = ModelRuntimeManager::new();
        let before = move_after_interrupted_rename(&service, &models, &original, &moved).await;
        let info = service
            .info(InfoOptions {
                root: Some(moved.clone()),
                include_status: false,
            })
            .await
            .expect("info resolves the authoritative name through the recorded old root");
        assert_eq!(info.workspace_index.expect("workspace").name, "after");
        let registry = service.registry().expect("registry");
        let new_name = WorkspaceName::new("after").expect("new name");
        assert_eq!(
            registry.root_for_name(&new_name).expect("read-only info"),
            Some(before.workspace.root.clone())
        );
        let home = moved.join(".zvec-grep");
        assert_eq!(
            super::read_workspace_manifest(&home)
                .expect("manifest")
                .expect("unchanged persisted name")
                .workspace
                .name,
            before.workspace.name
        );
        service
            .index(
                &models,
                IndexOptions {
                    root: Some(moved.clone()),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect("index rebinds the renamed workspace after the move");
        let after = super::read_workspace_manifest(&home)
            .expect("manifest")
            .expect("reconciled workspace");
        assert_eq!(after.workspace.name, new_name);
        assert_eq!(after.storage_generation, before.storage_generation);
        assert_eq!(
            registry
                .root_for_name(&new_name)
                .expect("moved registration"),
            Some(std::fs::canonicalize(&moved).expect("moved root"))
        );
        assert_eq!(
            registry
                .root_for_name(&before.workspace.name)
                .expect("old name remains released"),
            None
        );
        models.close();
    }

    #[tokio::test]
    async fn drop_after_interrupted_rename_and_move_releases_the_authoritative_name() {
        let directory = tempdir().expect("workspace roots");
        let original = directory.path().join("original");
        let moved = directory.path().join("moved");
        let replacement = directory.path().join("replacement");
        std::fs::create_dir(&replacement).expect("replacement root");
        let service =
            WorkspaceIndexService::with_storage_factory(Arc::new(MemoryStorageFactory::default()));
        let models = ModelRuntimeManager::new();
        move_after_interrupted_rename(&service, &models, &original, &moved).await;
        assert!(
            service
                .drop_index(&InfoOptions {
                    root: Some(moved),
                    include_status: false,
                })
                .expect("drop the moved workspace before rename replay")
        );
        let registry = service.registry().expect("registry");
        let new_name = WorkspaceName::new("after").expect("new name");
        assert_eq!(
            registry.root_for_name(&new_name).expect("released name"),
            None
        );
        assert_eq!(
            registry
                .name_for_root(&original)
                .expect("released old root"),
            None
        );
        service
            .index(
                &models,
                IndexOptions {
                    name: Some(new_name.to_string()),
                    ..empty_index_options(&replacement)
                },
            )
            .await
            .expect("another workspace can reuse the authoritative name");
        assert_eq!(
            registry.root_for_name(&new_name).expect("new owner"),
            Some(std::fs::canonicalize(&replacement).expect("replacement root"))
        );
        models.close();
    }

    async fn move_after_interrupted_rename(
        service: &WorkspaceIndexService,
        models: &ModelRuntimeManager,
        original: &Path,
        moved: &Path,
    ) -> crate::workspace::manifest::WorkspaceManifest {
        std::fs::create_dir(original).expect("original root");
        let original = std::fs::canonicalize(original).expect("canonical original root");
        service
            .index(
                models,
                IndexOptions {
                    name: Some("before".into()),
                    ..empty_index_options(&original)
                },
            )
            .await
            .expect("initial index");
        let before = super::read_workspace_manifest(&original.join(".zvec-grep"))
            .expect("manifest")
            .expect("active workspace");
        service
            .registry()
            .expect("registry")
            .rename(
                &before.workspace.name,
                &WorkspaceName::new("after").expect("new name"),
                &original,
            )
            .expect("commit rename before manifest update is interrupted");
        std::fs::rename(original, moved).expect("move without replaying the rename");
        before
    }

    #[tokio::test]
    async fn drop_releases_name_reserved_before_initial_model_failure() {
        let directory = tempdir().expect("workspace");
        let service =
            WorkspaceIndexService::with_storage_factory(Arc::new(MemoryStorageFactory::default()));
        let models = ModelRuntimeManager::new();
        let mut options = empty_index_options(directory.path());
        options.name = Some("reserved".into());
        options.embedding.as_mut().expect("model spec").revision = Some("unsupported".into());
        let error = service
            .index(&models, options)
            .await
            .expect_err("model acquisition fails");
        assert_eq!(error.code(), crate::EngineError::UNSUPPORTED);
        assert_eq!(models.snapshot().cached_runtimes, 0);
        let home = directory.path().join(".zvec-grep");
        assert!(!home.join("manifest.json").exists());
        assert!(!home.join("build.json").exists());
        let registry = service.registry().expect("registry");
        let name = WorkspaceName::new("reserved").expect("name");
        assert_eq!(
            registry
                .name_for_root(directory.path())
                .expect("reservation"),
            Some(name.clone())
        );
        let info = InfoOptions {
            root: Some(directory.path().to_path_buf()),
            include_status: false,
        };
        assert!(
            service
                .drop_index(&info)
                .expect("drop reservation without manifest")
        );
        assert_eq!(registry.root_for_name(&name).expect("released name"), None);
        assert!(!service.drop_index(&info).expect("idempotent drop"));
        models.close();
    }

    #[tokio::test]
    async fn initial_build_cancelled_before_publication_resumes_from_a_subdirectory() {
        let directory = tempdir().expect("workspace");
        let nested = directory.path().join("src/nested");
        std::fs::create_dir_all(&nested).expect("nested directory");
        let service =
            WorkspaceIndexService::with_storage_factory(Arc::new(MemoryStorageFactory::default()));
        let models = ModelRuntimeManager::new();
        let signal = tokio_util::sync::CancellationToken::new();
        signal.cancel();
        service
            .index(
                &models,
                IndexOptions {
                    signal: Some(signal),
                    ..empty_index_options(directory.path())
                },
            )
            .await
            .expect_err("initial build cancelled");
        let home = directory.path().join(".zvec-grep");
        assert!(!home.join("manifest.json").exists());
        let pending = crate::workspace::build::read_build(&home)
            .expect("build")
            .expect("pending");
        service
            .index(
                &models,
                IndexOptions {
                    root: Some(nested.clone()),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect("resume parent workspace from nested directory");
        let active = super::read_workspace_manifest(&home)
            .expect("manifest")
            .expect("active");
        assert_eq!(active.storage_generation, pending.target.storage_generation);
        assert_eq!(active.workspace.name, pending.target.workspace.name);
        assert_eq!(active.revision(), Some(1));
        assert_eq!(
            std::fs::canonicalize(&active.workspace.root).expect("active workspace root"),
            std::fs::canonicalize(directory.path()).expect("workspace root")
        );
        assert!(!nested.join(".zvec-grep").exists());
        models.close();
    }

    #[tokio::test]
    async fn composes_workspace_lifecycle_around_the_indexing_pipeline() {
        let directory = tempdir().expect("temporary directory");
        let sources = directory.path().join("sources");
        std::fs::create_dir(&sources).expect("source directory");
        let factory = Arc::new(MemoryStorageFactory::default());
        let service = WorkspaceIndexService::with_storage_factory(factory.clone());
        let models = ModelRuntimeManager::new();
        let mut options = empty_index_options(directory.path());
        options.discovery.include_paths = vec!["sources".into()];
        options.embedding.as_mut().expect("local model").cache_dir =
            Some(directory.path().join("model-cache"));
        let result = service
            .index(&models, options)
            .await
            .expect("empty workspace should index");
        assert_eq!(result.files_scanned, 0);
        assert!(factory.exists.load(Ordering::Acquire));

        let info_options = InfoOptions {
            root: Some(directory.path().to_path_buf()),
            include_status: true,
        };
        let info = service
            .info(info_options.clone())
            .await
            .expect("workspace info");
        assert!(info.indexed);
        assert_eq!(info.status.expect("index status").files_stored, 0);
        assert_eq!(
            info.workspace_index.expect("workspace index").generation,
            Some(1)
        );
        let manifest = super::read_workspace_manifest(&info.home)
            .expect("manifest read")
            .expect("manifest");
        assert_eq!(
            manifest.embedding_runtime.cache_dir,
            Some(directory.path().join("model-cache"))
        );
        let lease = super::acquire_search_model(
            &models,
            &manifest,
            None,
            &crate::api::context::ContextOptions::default(),
            directory.path(),
        )
        .expect("search model");
        assert_eq!(
            models.snapshot().cached_runtimes,
            1,
            "search reuses the configured model cache"
        );
        drop(lease);

        // Legacy metadata remains readable long enough to request an explicit rebuild.
        write_legacy_manifest(&info.home, &manifest);
        let error = service
            .index(
                &models,
                IndexOptions {
                    root: Some(directory.path().to_path_buf()),
                    ..IndexOptions::default()
                },
            )
            .await
            .expect_err("legacy index needs rebuild");
        assert!(error.message().contains("rebuild the index"));

        service
            .index(
                &models,
                IndexOptions {
                    root: Some(directory.path().to_path_buf()),
                    rebuild: true,
                    ..IndexOptions::default()
                },
            )
            .await
            .expect("rebuild preserves discovery and model runtime");
        let rebuilt = super::read_workspace_manifest(&info.home)
            .expect("manifest read")
            .expect("manifest");
        assert_eq!(rebuilt.workspace.root, manifest.workspace.root);
        assert_eq!(
            rebuilt.workspace.file_selection,
            manifest.workspace.file_selection
        );
        assert_eq!(rebuilt.embedding_runtime, manifest.embedding_runtime);
        assert_eq!(rebuilt.revision(), Some(2));
        assert_eq!(rebuilt.workspace.name, manifest.workspace.name);
        assert_eq!(
            rebuilt.workspace.created_epoch_ms,
            manifest.workspace.created_epoch_ms
        );
        assert_ne!(rebuilt.storage_generation, manifest.storage_generation);

        assert!(service.drop_index(&info_options).expect("drop index"));
        assert!(!factory.exists.load(Ordering::Acquire));
        models.close();
    }
}
