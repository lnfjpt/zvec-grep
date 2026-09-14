use std::{
    env, fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use uuid::Uuid;
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
            result::{
                InfoSource, WorkspaceIndexEmbedding, WorkspaceIndexInfo, WorkspaceIndexPolicy,
            },
        },
    },
    models::{
        CreateEmbeddingModelOptions, EmbeddingMetric, ModelError, ModelRuntimeLease,
        ModelRuntimeManager, ModelRuntimeRequest, ResolveEmbeddingReferenceOptions,
        resolve_embedding_reference,
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
    },
};

use super::pipeline::{IndexingContext, get_workspace_index_status, index_workspace};

const DEFAULT_LOCAL_EMBEDDING: &str = "local/potion-code-16m-v2";

#[derive(Clone)]
pub(crate) struct WorkspaceIndexService {
    scanner: NativeScanner,
    storage_factory: Arc<dyn WorkspaceIndexStorageFactory>,
}

impl WorkspaceIndexService {
    pub(crate) fn new() -> Self {
        Self {
            scanner: NativeScanner::default(),
            storage_factory: Arc::new(crate::storage::ZvecStorageFactory::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_storage_factory(
        storage_factory: Arc<dyn WorkspaceIndexStorageFactory>,
    ) -> Self {
        Self {
            scanner: NativeScanner::default(),
            storage_factory,
        }
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
        let (pending, recovered_publication) = recover_build(&location.home, factory.as_ref())?;
        if pending.is_some() && !options.rebuild && !options.changes.is_empty() {
            return Err(EngineError::resource_busy(
                "workspace rebuild is pending; run index to resume it before processing watcher changes",
            ));
        }
        let existing = read_workspace_manifest(&location.home)?;
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
                embedding: storage_embedding_schema(&model),
            })?;
        let result = index_workspace(&IndexingContext {
            workspace_index: &manifest.index_info(),
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
            manifest.updated_time = now;
            manifest.generation = Some(indexed.generation);
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
        let manifest = read_workspace_manifest(&location.home)?.ok_or_else(|| {
            workspace_index_unavailable(&location.root, "workspace manifest disappeared")
        })?;
        if manifest.index_policy == WorkspaceIndexPolicy::Disabled {
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
        })?;
        let result = context_from_index(
            &location.root,
            &manifest.index_info(),
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
        if !is_indexed(&manifest) || manifest.index_policy == WorkspaceIndexPolicy::Disabled {
            return Ok(false);
        }
        assert_index_version(manifest.index_version)?;
        let storage = factory.open(WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: manifest.storage_home(),
        })?;
        let status = get_workspace_index_status(
            &manifest.index_info(),
            storage.as_ref(),
            &self.scanner,
            None,
        )
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
        let Some(manifest) = read_workspace_manifest(&location.home)? else {
            return Ok(unindexed_info(location, WorkspaceIndexPolicy::Undecided));
        };
        let metadata_indexed = is_indexed(&manifest);
        let storage_exists = self.storage_factory.exists(&manifest.storage_home())?;
        let indexed = metadata_indexed && storage_exists;
        let status = if options.include_status && indexed {
            assert_index_version(manifest.index_version)?;
            let factory = &self.storage_factory;
            let storage = factory.open(WorkspaceIndexStorageOptions::ReadOnly {
                storage_path: manifest.storage_home(),
            })?;
            let status = get_workspace_index_status(
                &manifest.index_info(),
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
            index_policy: manifest.index_policy,
            home: location.home,
            index_path: manifest.storage_home().join("storage"),
            source: if indexed {
                InfoSource::Index
            } else {
                InfoSource::Unindexed
            },
            workspace_index: Some(manifest.index_info()),
            status,
            suggestion: workspace_suggestion(&manifest, indexed),
        })
    }

    pub(crate) fn drop_index(&self, options: &InfoOptions) -> Result<bool, EngineError> {
        let location = workspace_index_location_from_option(options.root.as_deref())?;
        let factory = &self.storage_factory;
        if !workspace_has_index_data(&location, factory.as_ref())? {
            return Ok(false);
        }
        let _lock = acquire_home_lock(&location.home, LockMode::Write, "index.drop")?;
        if !workspace_has_index_data(&location, factory.as_ref())? {
            return Ok(false);
        }
        reset_workspace_index(&location, factory.as_ref())?;
        Ok(true)
    }
}

fn workspace_has_index_data(
    location: &WorkspaceIndexLocation,
    factory: &dyn WorkspaceIndexStorageFactory,
) -> Result<bool, EngineError> {
    Ok(location.manifest_path.exists()
        || has_build(&location.home)
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
    let info = WorkspaceIndexInfo {
        id: identity.map_or_else(|| Uuid::new_v4().to_string(), |value| value.id.clone()),
        name: identity.map_or_else(
            || workspace_name(&location.root),
            |value| value.name.clone(),
        ),
        path: location.home.clone(),
        root: location.root.clone(),
        discovery: resolve_discovery(settings, options),
        policy: WorkspaceIndexPolicy::Enabled,
        embedding: Some(embedding_schema(model)),
        index_version: Some(CURRENT_INDEX_VERSION),
        generation: active.and_then(|value| value.generation),
        created_epoch_ms: identity.map_or(now, |value| value.created_time),
        updated_epoch_ms: now,
    };
    let runtime = embedding_runtime(settings, options, model)?;
    let mut manifest = WorkspaceManifest::new(info, runtime)?;
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
    let schema = manifest.embedding.as_ref().ok_or_else(|| {
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
            .and_then(|manifest| manifest.embedding.as_ref())
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
    let Some(schema) = &existing.embedding else {
        return Ok(());
    };
    let current = model.info();
    if schema.provider == current.provider
        && schema.model == current.name
        && schema.dimension == current.dimension
        && schema.metric == metric_name(current.metric)
    {
        return Ok(());
    }
    Err(EngineError::invalid_argument(
        "existing index uses a different embedding model; rebuild the index",
    ))
}

pub(crate) fn resolve_discovery(
    existing: Option<&WorkspaceManifest>,
    options: &IndexOptions,
) -> DiscoveryOptions {
    let mut discovery = if options.reset_paths {
        DiscoveryOptions::default()
    } else {
        existing.map_or_else(DiscoveryOptions::default, |manifest| {
            manifest.index_info().discovery
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

fn embedding_schema(model: &ModelRuntimeLease) -> WorkspaceIndexEmbedding {
    let info = model.info();
    WorkspaceIndexEmbedding {
        provider: info.provider.clone(),
        model: info.name.clone(),
        dimension: info.dimension,
        metric: metric_name(info.metric).to_owned(),
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

const fn metric_name(metric: EmbeddingMetric) -> &'static str {
    match metric {
        EmbeddingMetric::Cosine => "cosine",
        EmbeddingMetric::DotProduct => "dot",
        EmbeddingMetric::Euclidean => "euclidean",
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
    manifest.index_policy == WorkspaceIndexPolicy::Enabled
        && manifest.embedding.is_some()
        && manifest.index_version.is_some()
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
    if manifest.index_policy == WorkspaceIndexPolicy::Disabled {
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
        domain::FileRecord,
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

        fn delete_file(&self, _file_id: &crate::domain::FileId) -> StorageResult<()> {
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
            crate::api::info::result::WorkspaceIndexInfo {
                id: "workspace".into(),
                name: "workspace".into(),
                path: directory.path().join(".zvec-grep"),
                root: directory.path().to_path_buf(),
                discovery: DiscoveryOptions {
                    include_paths: vec!["src".into()],
                    globs: vec!["*.rs".into()],
                    hidden: true,
                    ..DiscoveryOptions::default()
                },
                policy: crate::api::info::result::WorkspaceIndexPolicy::Enabled,
                embedding: None,
                index_version: None,
                generation: None,
                created_epoch_ms: 0,
                updated_epoch_ms: 0,
            },
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
        assert_eq!(workspace.id, before.id);
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
        assert_eq!(after.id, before.id);
        assert_eq!(after.discovery, before.discovery);
        assert_eq!(after.root, moved);
        models.close();
    }

    #[test]
    fn dropping_a_missing_index_is_an_idempotent_no_op() {
        let directory = tempdir().expect("temporary directory");

        assert!(
            !WorkspaceIndexService::new()
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
        assert_eq!(published.id, active.id);
        assert_eq!(published.created_time, active.created_time);
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
        assert_eq!(active.id, pending.target.id);
        assert_eq!(active.generation, Some(1));
        assert_eq!(
            std::fs::canonicalize(&active.root).expect("active workspace root"),
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

        let result = service
            .index(
                &models,
                IndexOptions {
                    root: Some(directory.path().to_path_buf()),
                    discovery: DiscoveryOptions {
                        include_paths: vec!["sources".into()],
                        ..DiscoveryOptions::default()
                    },
                    embedding: Some(EmbeddingModelSpec {
                        reference: "local/potion-code-16m-v2".to_owned(),
                        revision: None,
                        cache_dir: Some(directory.path().join("model-cache")),
                        endpoint: None,
                        device: Device::Cpu,
                    }),
                    ..IndexOptions::default()
                },
            )
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
        assert_eq!(rebuilt.root, manifest.root);
        assert_eq!(rebuilt.discovery, manifest.discovery);
        assert_eq!(rebuilt.embedding_runtime, manifest.embedding_runtime);
        assert_eq!(rebuilt.generation, Some(2));
        assert_eq!(rebuilt.id, manifest.id);
        assert_eq!(rebuilt.created_time, manifest.created_time);
        assert_ne!(rebuilt.storage_generation, manifest.storage_generation);

        assert!(service.drop_index(&info_options).expect("drop index"));
        assert!(!factory.exists.load(Ordering::Acquire));
        models.close();
    }
}
