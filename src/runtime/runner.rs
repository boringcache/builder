use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tempfile::TempDir;

use super::step_slices::{
    PrefetchedSlice, StepSliceStats, compute_step_state, emit_slice_debug,
    explain_step_state_mismatch, initial_step_state_key, prefetch_step_slices,
    resolve_container_path, restore_prefetched_slice, step_slice_label,
};
use crate::backend::{
    CachedOperationPrime, ExecutionBackend, OperationCacheHooks, RunOptions, RunSummary, RunTimings,
};
use crate::cache::setup_snapshot::{
    SetupSnapshotMetadata, restore_setup_snapshot, save_setup_snapshot,
};
use crate::cache::stage_digest::digest_stage_rootfs;
use crate::cache::{CacheStore, CacheStoreConfig, open_cache_store};
use crate::schema::{Input, MultiTargetRecipe, Operation, Pipeline};
use crate::ui;
use crate::util::fs::copy_tree;

pub type RunConfig = CacheStoreConfig;

pub fn run_pipeline(
    backend: &dyn ExecutionBackend,
    pipeline: &Pipeline,
    options: &RunOptions,
    config: &RunConfig,
) -> Result<RunSummary> {
    let use_step_slices = should_use_step_slices(backend, pipeline);
    let use_setup_snapshot = pipeline.setup_snapshot.is_some();
    if backend.name() == "macos-container" {
        return backend.run(pipeline, options, config, None);
    }
    if !use_step_slices && !use_setup_snapshot {
        return backend.run(pipeline, options, config, None);
    }

    let store = open_cache_store(config)?;
    println!(
        "{} {} {}",
        ui::prefix(),
        ui::accent("cache-store:"),
        format_args!("{} ({})", store.kind(), store.detail())
    );

    let mut prepared = PreparedCaches::new(
        pipeline,
        store.as_ref(),
        options.no_cache,
        options.cache_explain,
        use_step_slices,
    )?;
    let run_pipeline = prepared.pipeline.clone();
    let mut result = backend.run(&run_pipeline, options, config, Some(&mut prepared));
    if let Ok(summary) = result.as_mut() {
        merge_step_slice_metrics(&mut summary.timings, &prepared.step_slice_stats);
    }
    prepared.emit_step_slice_summary();
    result
}

fn merge_step_slice_metrics(timings: &mut RunTimings, stats: &StepSliceStats) {
    timings.cache.hit_count += stats.restore_hits;
    timings.cache.miss_count +=
        stats.restore_misses + stats.restore_mismatches + stats.restore_failures;
    timings.cache.save_count += stats.saves;
}

fn should_use_step_slices(backend: &dyn ExecutionBackend, pipeline: &Pipeline) -> bool {
    if !pipeline_uses_step_slices(pipeline) {
        return false;
    }

    if host_backend_has_writable_mounts(backend.name(), pipeline) {
        ui::print_detail(
            "step slices disabled: host runtime does not yet restore writable mount outputs",
        );
        return false;
    }

    true
}

fn pipeline_uses_step_slices(pipeline: &Pipeline) -> bool {
    pipeline.operations.iter().any(
        |operation| matches!(operation, Operation::Exec(step) if step.build_cache != Some(false)),
    )
}

fn host_backend_has_writable_mounts(backend_name: &str, pipeline: &Pipeline) -> bool {
    matches!(backend_name, "linux-host" | "macos-host")
        && pipeline.inputs.iter().any(|input| !input.readonly)
}

pub fn run_multi_target_recipe(
    backend: &dyn ExecutionBackend,
    multi: &MultiTargetRecipe,
    options: &RunOptions,
    config: &RunConfig,
) -> Result<RunSummary> {
    // Track rootfs paths and keep-alive guards for non-final stages so
    // dependent stages can bind-mount them at /boringbuilder-stages/<name>/.
    let mut stage_rootfs: HashMap<String, PathBuf> = HashMap::new();
    let mut stage_digests: HashMap<String, String> = HashMap::new();
    let mut _stage_guards: Vec<Arc<dyn Send + Sync>> = Vec::new();
    let mut last_summary: Option<RunSummary> = None;
    let mut combined_timings = RunTimings::default();

    for (idx, target_name) in multi.order.iter().enumerate() {
        let pipeline = &multi.targets[target_name];
        let is_final = idx == multi.order.len() - 1;
        ui::print_step(idx + 1, multi.order.len(), target_name, false);

        // For non-final stages, keep the rootfs alive so dependent stages
        // can mount it at /boringbuilder-stages/<name>/.
        let mut stage_options = options.clone();
        if !is_final {
            stage_options.keep_rootfs = true;
            if pipeline.export.is_none() {
                stage_options.output_path_override = None;
                stage_options.export_format_override = None;
            }
        }

        // If this stage depends on previous stages, inject their rootfs as
        // read-only inputs.
        let mut runtime_pipeline = pipeline.clone();
        if !is_final {
            let snapshot_paths = stage_snapshot_paths(multi, target_name);
            if !snapshot_paths.is_empty() {
                runtime_pipeline.outputs = snapshot_paths.keys().cloned().collect();
                runtime_pipeline.stage_snapshot_follow_symlinks = snapshot_paths
                    .into_iter()
                    .filter_map(|(path, follow)| follow.then_some(path))
                    .collect();
            }
        }
        for dep in &pipeline.needs {
            if let Some(rootfs) = stage_rootfs.get(dep) {
                runtime_pipeline.inputs.push(Input {
                    source: rootfs.clone(),
                    dest: format!("/boringbuilder-stages/{}", dep),
                    readonly: true,
                });
            }
            if let Some(digest) = stage_digests.get(dep) {
                runtime_pipeline
                    .stage_dependency_digests
                    .insert(dep.clone(), digest.clone());
            }
        }

        let _workspace_copy = isolate_implicit_workspace(&mut runtime_pipeline)?;
        let summary = run_pipeline(backend, &runtime_pipeline, &stage_options, config)?;

        // Stash rootfs info for dependent stages.
        if let Some(rootfs) = &summary.rootfs_dir {
            let digest = digest_stage_rootfs(rootfs).with_context(|| {
                format!(
                    "failed to compute dependency digest for stage '{}' rootfs {}",
                    target_name,
                    rootfs.display()
                )
            })?;
            stage_rootfs.insert(target_name.clone(), rootfs.clone());
            stage_digests.insert(target_name.clone(), digest);
        }
        if let Some(guard) = &summary._keep_alive {
            _stage_guards.push(Arc::clone(guard));
        }

        combined_timings.merge(&summary.timings);
        last_summary = Some(summary);
    }

    let mut summary = last_summary.unwrap_or(RunSummary {
        container_name: None,
        export_path: None,
        rootfs_dir: None,
        _keep_alive: None,
        timings: RunTimings::default(),
    });
    summary.timings = combined_timings;
    Ok(summary)
}

fn isolate_implicit_workspace(pipeline: &mut Pipeline) -> Result<Option<TempDir>> {
    let Some(index) = implicit_workspace_input_index(pipeline)? else {
        return Ok(None);
    };

    let source = pipeline.inputs[index].source.clone();
    let temp_dir = tempfile::Builder::new()
        .prefix("boringbuilder-workspace-")
        .tempdir()
        .context("failed to create isolated target workspace")?;
    copy_tree(&source, temp_dir.path()).with_context(|| {
        format!(
            "failed to copy implicit workspace {} into isolated target workspace {}",
            source.display(),
            temp_dir.path().display()
        )
    })?;
    pipeline.inputs[index].source = temp_dir.path().to_path_buf();
    Ok(Some(temp_dir))
}

fn implicit_workspace_input_index(pipeline: &Pipeline) -> Result<Option<usize>> {
    let expected_source = pipeline.base_dir.canonicalize().with_context(|| {
        format!(
            "failed to resolve pipeline base dir {}",
            pipeline.base_dir.display()
        )
    })?;

    let writable = pipeline
        .inputs
        .iter()
        .enumerate()
        .filter(|(_, input)| !input.readonly && !input.dest.starts_with("/boringbuilder-stages/"))
        .collect::<Vec<_>>();

    if writable.len() != 1 {
        return Ok(None);
    }

    let (index, input) = writable[0];
    let input_source = input.source.canonicalize().with_context(|| {
        format!(
            "failed to resolve workspace input {}",
            input.source.display()
        )
    })?;

    if input.dest != pipeline.workdir || input_source != expected_source {
        return Ok(None);
    }

    Ok(Some(index))
}

struct PreparedCaches<'a> {
    _temp_dir: TempDir,
    pipeline: Pipeline,
    store: &'a dyn CacheStore,
    no_cache: bool,
    explain: bool,
    enable_step_slices: bool,
    /// Per-step slice state: snapshot taken before each step for delta computation.
    pre_step_snapshot: Option<crate::cache::slice::FsSnapshot>,
    /// Current step index for slice tag generation.
    current_step_index: usize,
    /// Current operation index for setup snapshot restore/save timing.
    current_operation_index: usize,
    /// Semantic identity of the current rootfs state after the previous exec step.
    current_state_key: String,
    /// Whether non-exec operations changed the rootfs since the previous exec step.
    rootfs_state_dirty: bool,
    /// Semantic identity of the current exec step result.
    pending_step_state_key: Option<String>,
    /// Debug metadata describing declared input hashes for the pending exec step.
    pending_step_state_debug: Option<crate::cache::slice::StepStateDebugMetadata>,
    /// Restore setup snapshots only once at the start of a pipeline run.
    setup_snapshot_restored: bool,
    /// Pre-fetched slice manifests (step_index → cached blob candidate).
    /// Blobs are fetched only after the current step state key matches.
    prefetched_slices: HashMap<usize, PrefetchedSlice>,
    /// Compact step-slice counters for the final run summary.
    step_slice_stats: StepSliceStats,
}

impl<'a> PreparedCaches<'a> {
    fn new(
        pipeline: &Pipeline,
        store: &'a dyn CacheStore,
        no_cache: bool,
        explain: bool,
        enable_step_slices: bool,
    ) -> Result<Self> {
        let temp_dir = tempfile::Builder::new()
            .prefix("boringbuilder-cache-")
            .tempdir()
            .context("failed to create temporary cache directory")?;

        let mut runtime_pipeline = pipeline.clone();
        if runtime_pipeline.export.is_some() && runtime_pipeline.outputs.is_empty() {
            runtime_pipeline.outputs = pipeline
                .inputs
                .iter()
                .filter(|input| !input.readonly)
                .map(|input| input.dest.clone())
                .collect();
        }

        let mut step_slice_stats = StepSliceStats::default();
        let prefetched_slices = if enable_step_slices && !no_cache {
            prefetch_step_slices(&runtime_pipeline, store.backend(), &mut step_slice_stats)
        } else {
            HashMap::new()
        };

        let prepared = Self {
            _temp_dir: temp_dir,
            pipeline: runtime_pipeline,
            store,
            no_cache,
            explain,
            enable_step_slices,
            pre_step_snapshot: None,
            current_step_index: 0,
            current_operation_index: 0,
            current_state_key: initial_step_state_key(pipeline),
            rootfs_state_dirty: true,
            pending_step_state_key: None,
            pending_step_state_debug: None,
            setup_snapshot_restored: false,
            prefetched_slices,
            step_slice_stats,
        };

        Ok(prepared)
    }

    fn emit_step_slice_summary(&self) {
        if let Some(summary) = self.step_slice_stats.summary_line() {
            emit_slice_debug(summary);
        }
    }

    fn capture_pre_step_snapshot(&mut self, snapshot_rootfs: &Path, step_label: &str, tag: &str) {
        self.pre_step_snapshot = match crate::cache::slice::snapshot_metadata(snapshot_rootfs) {
            Ok(snap) => {
                self.step_slice_stats.snapshot_captures += 1;
                emit_slice_debug(format!(
                    "captured pre-snapshot {step_label} tag={tag} entries={}",
                    snap.len()
                ));
                Some(snap)
            }
            Err(error) => {
                self.step_slice_stats.snapshot_failures += 1;
                emit_slice_debug(format!(
                    "pre-snapshot failed {step_label} tag={tag}: {error:#}"
                ));
                None
            }
        };
    }

    fn restore_setup_snapshot_if_needed(&mut self, snapshot_rootfs: Option<&Path>) -> Result<u128> {
        if self.no_cache || self.setup_snapshot_restored || self.current_operation_index != 0 {
            return Ok(0);
        }

        let Some(setup_snapshot) = self.pipeline.setup_snapshot.as_ref() else {
            return Ok(0);
        };
        let Some(snapshot_rootfs) = snapshot_rootfs else {
            return Ok(0);
        };

        self.setup_snapshot_restored = true;
        let destination = resolve_container_path(snapshot_rootfs, &setup_snapshot.path);
        let mut keys = Vec::with_capacity(1 + setup_snapshot.restore_from.len());
        keys.push(setup_snapshot.key.as_str());
        keys.extend(setup_snapshot.restore_from.iter().map(String::as_str));

        for key in keys {
            let started = Instant::now();
            match restore_setup_snapshot(self.store.backend(), key, &destination) {
                Ok(restored) if restored.hit => {
                    let ms = started.elapsed().as_millis();
                    ui::print_detail(format!(
                        "setup snapshot restored {} key={} ({ms} ms)",
                        setup_snapshot.path, key
                    ));
                    return Ok(ms);
                }
                Ok(_) => {
                    ui::print_detail(format!(
                        "setup snapshot miss {} key={}",
                        setup_snapshot.path, key
                    ));
                }
                Err(error) => {
                    ui::print_detail(format!(
                        "setup snapshot restore failed {} key={}: {error:#}",
                        setup_snapshot.path, key
                    ));
                }
            }
        }

        Ok(0)
    }

    fn save_setup_snapshot_if_needed(&mut self, snapshot_rootfs: Option<&Path>) -> Result<u128> {
        if self.no_cache || self.current_operation_index + 1 != self.pipeline.operations.len() {
            return Ok(0);
        }

        let Some(setup_snapshot) = self.pipeline.setup_snapshot.as_ref() else {
            return Ok(0);
        };
        let Some(snapshot_rootfs) = snapshot_rootfs else {
            return Ok(0);
        };

        let source = resolve_container_path(snapshot_rootfs, &setup_snapshot.path);
        if !source.exists() {
            ui::print_detail(format!(
                "setup snapshot skipped {}: source missing",
                setup_snapshot.path
            ));
            return Ok(0);
        }

        let started = Instant::now();
        let metadata = SetupSnapshotMetadata {
            platform: Some(self.pipeline.platform.clone()),
            base_image: Some(self.pipeline.image.clone()),
        };
        match save_setup_snapshot(
            self.store.backend(),
            &setup_snapshot.key,
            &source,
            &metadata,
        ) {
            Ok(_) => {
                let ms = started.elapsed().as_millis();
                ui::print_detail(format!(
                    "setup snapshot saved {} key={} ({ms} ms)",
                    setup_snapshot.path, setup_snapshot.key
                ));
                Ok(ms)
            }
            Err(error) => {
                ui::print_detail(format!(
                    "setup snapshot save failed {} key={}: {error:#}",
                    setup_snapshot.path, setup_snapshot.key
                ));
                Ok(0)
            }
        }
    }
}

impl OperationCacheHooks for PreparedCaches<'_> {
    fn before_operation(
        &mut self,
        operation: &Operation,
        materialize_rootfs: Option<&Path>,
        snapshot_rootfs: Option<&Path>,
    ) -> Result<CachedOperationPrime> {
        let mut total_ms = 0u128;
        let mut complete = false;
        let snapshot_rootfs = snapshot_rootfs.or(materialize_rootfs);
        total_ms += self.restore_setup_snapshot_if_needed(snapshot_rootfs)?;

        if self.enable_step_slices
            && let Some(snapshot_rootfs) = snapshot_rootfs
            && let Operation::Exec(step) = operation
        {
            let (step_label, tag) =
                step_slice_label(&self.pipeline, self.current_step_index, operation)
                    .expect("exec operations must have a step slice label");
            let step_state = compute_step_state(
                &self.current_state_key,
                &self.pipeline,
                step,
                snapshot_rootfs,
                self.rootfs_state_dirty,
            )?;
            let step_state_key = step_state.key.clone();
            self.pending_step_state_key = Some(step_state_key.clone());
            self.pending_step_state_debug = Some(step_state.debug.clone());
            if self.no_cache || step.build_cache == Some(false) {
                self.step_slice_stats.skip_count += 1;
                return Ok(CachedOperationPrime {
                    restore_ms: total_ms,
                    complete,
                });
            }
            if let Some(prefetched) = self.prefetched_slices.get(&self.current_step_index) {
                if prefetched.step_state_key == step_state_key {
                    emit_slice_debug(format!(
                        "restore hit {step_label} tag={tag} state-key={step_state_key}"
                    ));
                    match restore_prefetched_slice(
                        self.store.backend(),
                        prefetched,
                        self._temp_dir.path(),
                        snapshot_rootfs,
                    ) {
                        Ok(ms) => {
                            self.step_slice_stats.restore_hits += 1;
                            self.step_slice_stats.blob_fetches += 1;
                            total_ms += ms;
                            complete = true;
                            self.pre_step_snapshot = None;
                            emit_slice_debug(format!("restored {step_label} tag={tag} ({ms} ms)"));
                        }
                        Err(error) => {
                            self.step_slice_stats.restore_failures += 1;
                            emit_slice_debug(format!(
                                "restore failed {step_label} tag={tag}: {error:#}"
                            ));
                            self.capture_pre_step_snapshot(snapshot_rootfs, &step_label, &tag);
                        }
                    }
                } else {
                    self.step_slice_stats.restore_mismatches += 1;
                    self.step_slice_stats.blob_fetch_skips += 1;
                    emit_slice_debug(format!(
                        "restore skipped {step_label} tag={tag}: state key mismatch expected={} current={} (blob fetch skipped)",
                        prefetched.step_state_key, step_state_key,
                    ));
                    if self.explain {
                        for detail in explain_step_state_mismatch(
                            step,
                            snapshot_rootfs,
                            prefetched.step_state_debug.as_ref(),
                            &step_state.debug,
                        )? {
                            emit_slice_debug(detail);
                        }
                    }
                    self.capture_pre_step_snapshot(snapshot_rootfs, &step_label, &tag);
                }
            } else {
                self.step_slice_stats.restore_misses += 1;
                emit_slice_debug(format!(
                    "restore miss {step_label} tag={tag}: no manifest candidate"
                ));
                self.capture_pre_step_snapshot(snapshot_rootfs, &step_label, &tag);
            }
        }

        Ok(CachedOperationPrime {
            restore_ms: total_ms,
            complete,
        })
    }

    fn after_operation(
        &mut self,
        operation: &Operation,
        materialize_rootfs: Option<&Path>,
        snapshot_rootfs: Option<&Path>,
    ) -> Result<u128> {
        let mut total_ms = 0u128;
        let snapshot_rootfs = snapshot_rootfs.or(materialize_rootfs);

        if self.enable_step_slices
            && let Some(snapshot_rootfs) = snapshot_rootfs
            && let Operation::Exec(step) = operation
        {
            let (step_label, tag) =
                step_slice_label(&self.pipeline, self.current_step_index, operation)
                    .expect("exec operations must have a step slice label");
            if !self.no_cache && step.build_cache != Some(false) {
                if let Some(before) = self.pre_step_snapshot.take() {
                    let before_fingerprint = crate::cache::slice::snapshot_fingerprint(&before);
                    match crate::cache::slice::snapshot_metadata(snapshot_rootfs) {
                        Ok(after) => {
                            let (changed, deleted) =
                                crate::cache::slice::diff_snapshots(&before, &after);
                            emit_slice_debug(format!(
                                "diff {step_label} tag={tag} changed={} deleted={}",
                                changed.len(),
                                deleted.len()
                            ));
                            match crate::cache::slice::save_slice(
                                self.store.backend(),
                                &tag,
                                snapshot_rootfs,
                                &changed,
                                &deleted,
                                crate::cache::slice::SlicePublishMetadata {
                                    pre_snapshot_fingerprint: Some(before_fingerprint.as_str()),
                                    step_state_key: self.pending_step_state_key.as_deref(),
                                    step_state_debug: self.pending_step_state_debug.as_ref(),
                                    ..Default::default()
                                },
                            ) {
                                Ok(ms) => {
                                    self.step_slice_stats.saves += 1;
                                    total_ms += ms;
                                    emit_slice_debug(format!(
                                        "saved {step_label} tag={tag} ({} changed, {} deleted, {ms} ms)",
                                        changed.len(),
                                        deleted.len(),
                                    ));
                                }
                                Err(error) => {
                                    self.step_slice_stats.save_failures += 1;
                                    emit_slice_debug(format!(
                                        "save failed {step_label} tag={tag}: {error:#}"
                                    ));
                                }
                            }
                        }
                        Err(error) => {
                            self.step_slice_stats.snapshot_failures += 1;
                            emit_slice_debug(format!(
                                "post-snapshot failed {step_label} tag={tag}: {error:#}"
                            ));
                        }
                    }
                } else {
                    self.step_slice_stats.skip_count += 1;
                    emit_slice_debug(format!(
                        "missing pre-snapshot {step_label} tag={tag}; slice save skipped"
                    ));
                }
            }
        }

        total_ms += self.save_setup_snapshot_if_needed(snapshot_rootfs)?;
        if matches!(operation, Operation::Exec(_)) {
            if let Some(step_state_key) = self.pending_step_state_key.take() {
                self.current_state_key = step_state_key;
            }
            self.pending_step_state_debug = None;
            self.current_step_index += 1;
            self.rootfs_state_dirty = false;
        } else {
            self.rootfs_state_dirty = true;
        }
        self.current_operation_index += 1;
        self.pre_step_snapshot = None;

        Ok(total_ms)
    }

    fn on_cached_operation(&mut self, operation: &Operation) -> Result<u128> {
        self.pre_step_snapshot = None;
        if matches!(operation, Operation::Exec(_)) {
            if let Some(step_state_key) = self.pending_step_state_key.take() {
                self.current_state_key = step_state_key;
            }
            self.pending_step_state_debug = None;
            self.current_step_index += 1;
            self.rootfs_state_dirty = false;
        } else {
            self.rootfs_state_dirty = true;
        }
        self.current_operation_index += 1;
        Ok(0)
    }

    fn prime_cached_operation(
        &mut self,
        _operation: &Operation,
        _materialize_rootfs: &Path,
        _snapshot_rootfs: &Path,
    ) -> Result<CachedOperationPrime> {
        Ok(CachedOperationPrime::default())
    }

    fn hydrate_export_caches(
        &mut self,
        _pipeline: &Pipeline,
        _materialize_rootfs: &Path,
        _snapshot_rootfs: &Path,
    ) -> Result<CachedOperationPrime> {
        Ok(CachedOperationPrime::default())
    }
}

fn stage_snapshot_paths(multi: &MultiTargetRecipe, stage_name: &str) -> BTreeMap<String, bool> {
    let mut paths = BTreeMap::new();
    for pipeline in multi.targets.values() {
        for operation in &pipeline.operations {
            if let Operation::CopyFromStage(copy) = operation
                && copy.stage == stage_name
            {
                for source in &copy.sources {
                    paths
                        .entry(source.clone())
                        .and_modify(|follow| *follow |= copy.follow_symlinks)
                        .or_insert(copy.follow_symlinks);
                }
            }
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::Result;
    use tempfile::{TempDir, tempdir};

    use crate::backend::{
        CacheMetrics, ExecutionBackend, OperationCacheHooks, RunOptions, RunSummary, RunTimings,
    };
    use crate::cache::CacheStore;
    use crate::cache::backend::{CacheBackend, CacheManifest, tree_cache_manifest};
    use crate::cache::local::LocalCacheStore;
    use crate::cache::slice::{
        STEP_SLICE_STATE_KEY_METADATA_KEY, StepStateDebugMetadata, StepStateInputDebug,
    };
    use crate::runtime::step_slices::{
        PrefetchedSlice, StepSliceStats, compute_step_state_key, explain_step_state_mismatch,
        prefetch_step_slices,
    };
    use crate::schema::{
        Input, MultiTargetRecipe, Operation, Pipeline, SetupSnapshot, Step, StepRunBindSource,
        StepRunMount,
    };

    use super::{
        PreparedCaches, RunConfig, merge_step_slice_metrics, run_multi_target_recipe, run_pipeline,
    };

    fn test_run_config(root: &Path) -> RunConfig {
        RunConfig {
            cache_dir: Some(root.join("cache")),
            ..RunConfig::default()
        }
    }

    #[test]
    fn step_slice_activity_is_included_in_run_cache_metrics() {
        let mut timings = RunTimings {
            cache: CacheMetrics {
                hit_count: 1,
                miss_count: 2,
                save_count: 3,
                ..CacheMetrics::default()
            },
            ..RunTimings::default()
        };
        let stats = StepSliceStats {
            restore_hits: 2,
            restore_misses: 3,
            restore_mismatches: 4,
            restore_failures: 5,
            saves: 6,
            ..StepSliceStats::default()
        };

        merge_step_slice_metrics(&mut timings, &stats);

        assert_eq!(timings.cache.hit_count, 3);
        assert_eq!(timings.cache.miss_count, 14);
        assert_eq!(timings.cache.save_count, 9);
    }

    struct ManifestOnlyBackend {
        manifest: CacheManifest,
        has_blob_calls: AtomicUsize,
        fetch_blob_calls: AtomicUsize,
    }

    impl CacheBackend for ManifestOnlyBackend {
        fn kind(&self) -> &'static str {
            "test"
        }

        fn detail(&self) -> String {
            "test".to_string()
        }

        fn resolve_ref(&self, _key: &str) -> Result<Option<CacheManifest>> {
            Ok(Some(self.manifest.clone()))
        }

        fn publish_ref(&self, _key: &str, _manifest: &CacheManifest) -> Result<()> {
            Ok(())
        }

        fn has_blob(&self, _digest: &str) -> Result<bool> {
            self.has_blob_calls.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        }

        fn fetch_blob(&self, _digest: &str, _destination: &Path) -> Result<()> {
            self.fetch_blob_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn store_blob(&self, _digest: &str, _source: &Path) -> Result<()> {
            Ok(())
        }
    }

    fn git_capture(root: &std::path::Path, args: &[&str]) {
        let global_config = root.join(".gitconfig-test");
        if !global_config.exists() {
            fs::write(&global_config, "").unwrap();
        }
        let output = Command::new("git")
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", &global_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn store_slice_blob(store: &LocalCacheStore, digest: &str, archive: &Path) {
        store.backend().store_blob(digest, archive).unwrap();
    }

    #[test]
    fn exec_form_step_state_key_ignores_shell_but_differs_from_shell_form() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(&rootfs).unwrap();

        let exec_step = Step {
            name: Some("exec".to_string()),
            run: "echo '$HOME'".to_string(),
            run_exec: Some(vec!["echo".to_string(), "$HOME".to_string()]),
            run_mounts: Vec::new(),
            env: BTreeMap::new(),
            workdir: None,
            shell: Some("/bin/bash -lc".to_string()),
            build_cache_inputs: None,
            build_cache: None,
            tag: None,
        };
        let exec_step_same = Step {
            shell: None,
            ..exec_step.clone()
        };
        let shell_step = Step {
            run_exec: None,
            ..exec_step.clone()
        };

        let pipeline = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: Vec::new(),
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let exec_key = compute_step_state_key("previous", &pipeline, &exec_step, &rootfs).unwrap();
        let exec_key_same =
            compute_step_state_key("previous", &pipeline, &exec_step_same, &rootfs).unwrap();
        let shell_key =
            compute_step_state_key("previous", &pipeline, &shell_step, &rootfs).unwrap();

        assert_eq!(exec_key, exec_key_same);
        assert_ne!(exec_key, shell_key);
    }

    #[test]
    fn bind_run_mount_step_state_key_tracks_context_source_changes() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(&rootfs).unwrap();
        fs::write(temp.path().join("source.txt"), "one").unwrap();

        let pipeline = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: Vec::new(),
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: Some(crate::schema::DockerContext {
                root: temp.path().to_path_buf(),
                ignore_patterns: Vec::new(),
            }),
        };
        let step = Step {
            name: Some("mount".to_string()),
            run: "cat /workspace/source.txt".to_string(),
            run_exec: None,
            run_mounts: vec![StepRunMount::Bind {
                target: "/workspace/source.txt".to_string(),
                source: StepRunBindSource::Context {
                    path: "source.txt".to_string(),
                },
                readonly: true,
            }],
            env: BTreeMap::new(),
            workdir: None,
            shell: None,
            build_cache_inputs: None,
            build_cache: None,
            tag: None,
        };

        let first = compute_step_state_key("previous", &pipeline, &step, &rootfs).unwrap();
        fs::write(temp.path().join("source.txt"), "two").unwrap();
        let second = compute_step_state_key("previous", &pipeline, &step, &rootfs).unwrap();

        assert_ne!(first, second);
    }

    #[test]
    fn automatic_step_state_key_tracks_rootfs_content_changes() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("src")).unwrap();
        fs::write(rootfs.join("src/main.rs"), "fn main() {}\n").unwrap();

        let pipeline = Pipeline {
            image: "rust:1.94".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/src".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: Vec::new(),
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };
        let step = Step {
            name: Some("build".to_string()),
            run: "cargo build --release".to_string(),
            run_exec: None,
            run_mounts: Vec::new(),
            env: BTreeMap::new(),
            workdir: Some("/src".to_string()),
            shell: None,
            build_cache_inputs: None,
            build_cache: None,
            tag: None,
        };

        let first = compute_step_state_key("previous", &pipeline, &step, &rootfs).unwrap();
        fs::write(
            rootfs.join("src/main.rs"),
            "fn main() { println!(\"changed\"); }\n",
        )
        .unwrap();
        let second = compute_step_state_key("previous", &pipeline, &step, &rootfs).unwrap();

        assert_ne!(first, second);
    }

    #[test]
    fn automatic_step_state_key_ignores_uncaptured_package_manager_state() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("var/lib/apt/lists")).unwrap();
        fs::write(rootfs.join("var/lib/apt/lists/packages"), "one\n").unwrap();

        let pipeline = Pipeline {
            image: "debian:bookworm-slim".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: Vec::new(),
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };
        let step = Step {
            name: Some("build".to_string()),
            run: "true".to_string(),
            run_exec: None,
            run_mounts: Vec::new(),
            env: BTreeMap::new(),
            workdir: None,
            shell: None,
            build_cache_inputs: None,
            build_cache: None,
            tag: None,
        };

        let first = compute_step_state_key("previous", &pipeline, &step, &rootfs).unwrap();
        fs::write(rootfs.join("var/lib/apt/lists/packages"), "two\n").unwrap();
        let second = compute_step_state_key("previous", &pipeline, &step, &rootfs).unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn secret_run_mount_step_state_key_ignores_secret_value_changes() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(&rootfs).unwrap();

        let pipeline = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: Vec::new(),
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };
        let step = Step {
            name: Some("secret".to_string()),
            run: "cat /run/secrets/token".to_string(),
            run_exec: None,
            run_mounts: vec![StepRunMount::Secret {
                target: "/run/secrets/token".to_string(),
                id: "token".to_string(),
                env: Some("TOKEN".to_string()),
                required: true,
                mode: Some(0o400),
                uid: Some(0),
                gid: Some(0),
            }],
            env: BTreeMap::new(),
            workdir: None,
            shell: None,
            build_cache_inputs: None,
            build_cache: None,
            tag: None,
        };

        let previous = std::env::var_os("TOKEN");
        unsafe {
            std::env::set_var("TOKEN", "one");
        }
        let first = compute_step_state_key("previous", &pipeline, &step, &rootfs).unwrap();
        unsafe {
            std::env::set_var("TOKEN", "two");
        }
        let second = compute_step_state_key("previous", &pipeline, &step, &rootfs).unwrap();

        unsafe {
            match previous {
                Some(value) => std::env::set_var("TOKEN", value),
                None => std::env::remove_var("TOKEN"),
            }
        }

        assert_eq!(first, second);
    }

    #[test]
    fn mismatch_explain_identifies_changed_file_input() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("src")).unwrap();
        fs::write(rootfs.join("src/Gemfile.lock"), "new\n").unwrap();

        let step = Step {
            name: Some("build-assets".to_string()),
            run: "bundle exec rails assets:precompile".to_string(),
            run_exec: None,
            run_mounts: Vec::new(),
            env: BTreeMap::new(),
            workdir: None,
            shell: None,
            build_cache_inputs: None,
            build_cache: None,
            tag: Some("web-assets".to_string()),
        };
        let expected = StepStateDebugMetadata {
            inputs: vec![StepStateInputDebug {
                path: "/src/Gemfile.lock".to_string(),
                exclude: Vec::new(),
                hash: "old".to_string(),
            }],
        };
        let current = StepStateDebugMetadata {
            inputs: vec![StepStateInputDebug {
                path: "/src/Gemfile.lock".to_string(),
                exclude: Vec::new(),
                hash: "new".to_string(),
            }],
        };

        let details =
            explain_step_state_mismatch(&step, &rootfs, Some(&expected), &current).unwrap();

        assert!(
            details
                .iter()
                .any(|line| line.contains("invalidated by declared input /src/Gemfile.lock"))
        );
        assert!(
            details
                .iter()
                .any(|line| line.contains("changed path /src/Gemfile.lock"))
        );
    }

    #[test]
    fn mismatch_explain_summarizes_git_workspace_changes() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let src = rootfs.join("src");
        fs::create_dir_all(src.join("app")).unwrap();

        git_capture(&src, &["init"]);
        git_capture(&src, &["config", "user.email", "test@example.com"]);
        git_capture(&src, &["config", "user.name", "Test"]);
        fs::write(src.join("app/main.rb"), "puts 'one'\n").unwrap();
        fs::write(src.join("README.md"), "one\n").unwrap();
        git_capture(&src, &["add", "."]);
        git_capture(&src, &["commit", "-m", "seed"]);

        fs::write(src.join("app/main.rb"), "puts 'two'\n").unwrap();
        fs::create_dir_all(src.join("tmp")).unwrap();
        fs::write(src.join("tmp/build.log"), "ignored\n").unwrap();

        let step = Step {
            name: Some("sync-application-source".to_string()),
            run: "tar -C /src -cf - . | tar -C /workspace -xf -".to_string(),
            run_exec: None,
            run_mounts: Vec::new(),
            env: BTreeMap::new(),
            workdir: None,
            shell: None,
            build_cache_inputs: None,
            build_cache: None,
            tag: Some("web-source".to_string()),
        };
        let expected = StepStateDebugMetadata {
            inputs: vec![StepStateInputDebug {
                path: "/src".to_string(),
                exclude: vec!["tmp".to_string()],
                hash: "old".to_string(),
            }],
        };
        let current = StepStateDebugMetadata {
            inputs: vec![StepStateInputDebug {
                path: "/src".to_string(),
                exclude: vec!["tmp".to_string()],
                hash: "new".to_string(),
            }],
        };

        let details =
            explain_step_state_mismatch(&step, &rootfs, Some(&expected), &current).unwrap();

        assert!(
            details
                .iter()
                .any(|line| line.contains("invalidated by declared input /src"))
        );
        assert!(
            details
                .iter()
                .any(|line| line.contains("changed paths under /src:"))
        );
        assert!(details.iter().any(|line| line.contains("app/main.rb")));
        assert!(!details.iter().any(|line| line.contains("tmp/build.log")));
    }

    #[test]
    fn mismatch_explain_clean_git_workspace_points_to_commit_change() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let src = rootfs.join("src");
        fs::create_dir_all(&src).unwrap();

        git_capture(&src, &["init"]);
        git_capture(&src, &["config", "user.email", "test@example.com"]);
        git_capture(&src, &["config", "user.name", "Test"]);
        fs::write(src.join("README.md"), "one\n").unwrap();
        git_capture(&src, &["add", "."]);
        git_capture(&src, &["commit", "-m", "seed"]);

        let step = Step {
            name: Some("sync-application-source".to_string()),
            run: "tar -C /src -cf - . | tar -C /workspace -xf -".to_string(),
            run_exec: None,
            run_mounts: Vec::new(),
            env: BTreeMap::new(),
            workdir: None,
            shell: None,
            build_cache_inputs: None,
            build_cache: None,
            tag: Some("web-source".to_string()),
        };
        let expected = StepStateDebugMetadata {
            inputs: vec![StepStateInputDebug {
                path: "/src".to_string(),
                exclude: vec![".git".to_string()],
                hash: "old".to_string(),
            }],
        };
        let current = StepStateDebugMetadata {
            inputs: vec![StepStateInputDebug {
                path: "/src".to_string(),
                exclude: vec![".git".to_string()],
                hash: "new".to_string(),
            }],
        };

        let details =
            explain_step_state_mismatch(&step, &rootfs, Some(&expected), &current).unwrap();

        assert!(
            details
                .iter()
                .any(|line| line.contains("clean git workspace"))
        );
        assert!(
            details
                .iter()
                .any(|line| line.contains("different commit or branch"))
        );
    }

    #[test]
    fn step_slice_hit_marks_operation_complete_and_advances_on_cached_finalize() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(&rootfs).unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("workspace")).unwrap();
        fs::write(source.join("workspace/cached.txt"), "cached").unwrap();

        let archive = temp.path().join("slice.tar.zst");
        let (digest, _bytes) = crate::cache::slice::archive_slice(
            &source,
            &[
                PathBuf::from("workspace"),
                PathBuf::from("workspace/cached.txt"),
            ],
            &[],
            &archive,
        )
        .unwrap();

        let store = LocalCacheStore::open(temp.path().join("store").as_path()).unwrap();
        store_slice_blob(&store, &digest, &archive);
        let pipeline = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("install-runtime-tools".to_string()),
                run: "echo hi".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: Some("shared-runtime".to_string()),
            })],
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let mut prepared = PreparedCaches::new(&pipeline, &store, false, false, true).unwrap();
        let expected_state_key = compute_step_state_key(
            &prepared.current_state_key,
            &pipeline,
            match &pipeline.operations[0] {
                Operation::Exec(step) => step,
                _ => unreachable!(),
            },
            &rootfs,
        )
        .unwrap();
        prepared.prefetched_slices.insert(
            0,
            PrefetchedSlice {
                blob_digest: digest,
                archive_format: crate::cache::slice::STEP_SLICE_ARCHIVE_FORMAT_TAR_ZST.to_string(),
                step_state_key: expected_state_key,
                step_state_debug: None,
            },
        );

        let primed = prepared
            .before_operation(&pipeline.operations[0], Some(&rootfs), Some(&rootfs))
            .unwrap();

        assert!(primed.complete);
        assert_eq!(
            fs::read_to_string(rootfs.join("workspace/cached.txt")).unwrap(),
            "cached"
        );
        assert_eq!(prepared.step_slice_stats.restore_hits, 1);
        assert_eq!(prepared.step_slice_stats.blob_fetches, 1);
        assert_eq!(prepared.current_step_index, 0);
        assert!(prepared.pre_step_snapshot.is_none());

        let cached_ms = prepared
            .on_cached_operation(&pipeline.operations[0])
            .unwrap();

        assert_eq!(cached_ms, 0);
        assert_eq!(prepared.current_step_index, 1);
        assert!(prepared.pre_step_snapshot.is_none());
    }

    #[test]
    fn step_slice_hit_restores_into_snapshot_rootfs_not_materialize_rootfs() {
        let temp = tempdir().unwrap();
        let materialize_rootfs = temp.path().join("upper");
        let snapshot_rootfs = temp.path().join("rootfs");
        fs::create_dir_all(&materialize_rootfs).unwrap();
        fs::create_dir_all(&snapshot_rootfs).unwrap();

        let source = temp.path().join("source");
        fs::create_dir_all(source.join("workspace")).unwrap();
        fs::write(source.join("workspace/cached.txt"), "cached").unwrap();

        let archive = temp.path().join("slice.tar.zst");
        let (digest, _bytes) = crate::cache::slice::archive_slice(
            &source,
            &[
                PathBuf::from("workspace"),
                PathBuf::from("workspace/cached.txt"),
            ],
            &[],
            &archive,
        )
        .unwrap();

        let store = LocalCacheStore::open(temp.path().join("store").as_path()).unwrap();
        store_slice_blob(&store, &digest, &archive);
        let pipeline = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("restore-visible-output".to_string()),
                run: "echo hi".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: Some("restore-visible-output".to_string()),
            })],
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let mut prepared = PreparedCaches::new(&pipeline, &store, false, false, true).unwrap();
        let expected_state_key = compute_step_state_key(
            &prepared.current_state_key,
            &pipeline,
            match &pipeline.operations[0] {
                Operation::Exec(step) => step,
                _ => unreachable!(),
            },
            &snapshot_rootfs,
        )
        .unwrap();
        prepared.prefetched_slices.insert(
            0,
            PrefetchedSlice {
                blob_digest: digest,
                archive_format: crate::cache::slice::STEP_SLICE_ARCHIVE_FORMAT_TAR_ZST.to_string(),
                step_state_key: expected_state_key,
                step_state_debug: None,
            },
        );

        let primed = prepared
            .before_operation(
                &pipeline.operations[0],
                Some(&materialize_rootfs),
                Some(&snapshot_rootfs),
            )
            .unwrap();

        assert!(primed.complete);
        assert_eq!(
            fs::read_to_string(snapshot_rootfs.join("workspace/cached.txt")).unwrap(),
            "cached"
        );
        assert!(!materialize_rootfs.join("workspace/cached.txt").exists());
        assert_eq!(prepared.step_slice_stats.restore_hits, 1);
        assert_eq!(prepared.step_slice_stats.blob_fetches, 1);
    }

    #[test]
    fn step_slice_hit_requires_matching_pre_snapshot_fingerprint() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(&rootfs).unwrap();
        fs::create_dir_all(rootfs.join("workspace")).unwrap();
        fs::write(rootfs.join("workspace/current.txt"), "current").unwrap();

        let store = LocalCacheStore::open(temp.path().join("store").as_path()).unwrap();
        let pipeline = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("sync-application-source".to_string()),
                run: "echo hi".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: Some("shared-source".to_string()),
            })],
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let mut prepared = PreparedCaches::new(&pipeline, &store, false, false, true).unwrap();
        prepared.prefetched_slices.insert(
            0,
            PrefetchedSlice {
                blob_digest: "missing-digest-for-mismatch".to_string(),
                archive_format: crate::cache::slice::STEP_SLICE_ARCHIVE_FORMAT_TAR_ZST.to_string(),
                step_state_key: "stale-state-key".to_string(),
                step_state_debug: None,
            },
        );

        let primed = prepared
            .before_operation(&pipeline.operations[0], Some(&rootfs), Some(&rootfs))
            .unwrap();

        assert!(!primed.complete);
        assert!(prepared.pre_step_snapshot.is_some());
        assert_eq!(prepared.step_slice_stats.restore_mismatches, 1);
        assert_eq!(prepared.step_slice_stats.blob_fetch_skips, 1);
        assert_eq!(prepared.step_slice_stats.snapshot_captures, 1);
        assert_eq!(
            fs::read_to_string(rootfs.join("workspace/current.txt")).unwrap(),
            "current"
        );
        assert!(!rootfs.join("workspace/cached.txt").exists());
    }

    #[test]
    fn unchanged_step_slice_is_cached_on_second_run() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("workspace")).unwrap();

        let store = LocalCacheStore::open(temp.path().join("store").as_path()).unwrap();
        let pipeline = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("prepare-workspace".to_string()),
                run: "mkdir -p /workspace".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: Some("prepare-workspace".to_string()),
            })],
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let mut first = PreparedCaches::new(&pipeline, &store, false, false, true).unwrap();
        let primed = first
            .before_operation(&pipeline.operations[0], Some(&rootfs), Some(&rootfs))
            .unwrap();
        assert!(!primed.complete);
        let _saved_ms = first
            .after_operation(&pipeline.operations[0], Some(&rootfs), Some(&rootfs))
            .unwrap();

        let tag = crate::cache::slice::step_tag(
            &pipeline.image,
            &pipeline.platform,
            0,
            Some("prepare-workspace"),
        );
        assert!(store.inspect_key(&tag).unwrap().is_some());

        let mut second = PreparedCaches::new(&pipeline, &store, false, false, true).unwrap();
        let primed = second
            .before_operation(&pipeline.operations[0], Some(&rootfs), Some(&rootfs))
            .unwrap();
        assert!(primed.complete);
        assert!(second.pre_step_snapshot.is_none());
        assert_eq!(second.step_slice_stats.manifest_candidates, 1);
        assert_eq!(second.step_slice_stats.restore_hits, 1);

        let cached_ms = second.on_cached_operation(&pipeline.operations[0]).unwrap();
        assert_eq!(cached_ms, 0);
        assert_eq!(second.current_step_index, 1);
    }

    #[test]
    fn build_cache_false_skips_slice_restore_and_save() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(&rootfs).unwrap();

        let store = LocalCacheStore::open(temp.path().join("store").as_path()).unwrap();
        let pipeline = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("install-system-deps".to_string()),
                run: "echo hi".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: Some(false),
                tag: Some("shared-system-deps".to_string()),
            })],
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let mut prepared = PreparedCaches::new(&pipeline, &store, false, false, true).unwrap();
        let step = match &pipeline.operations[0] {
            Operation::Exec(step) => step,
            _ => unreachable!(),
        };
        let expected_state_key =
            compute_step_state_key(&prepared.current_state_key, &pipeline, step, &rootfs).unwrap();
        prepared.prefetched_slices.insert(
            0,
            PrefetchedSlice {
                blob_digest: "missing-digest-for-disabled-cache".to_string(),
                archive_format: crate::cache::slice::STEP_SLICE_ARCHIVE_FORMAT_TAR_ZST.to_string(),
                step_state_key: expected_state_key.clone(),
                step_state_debug: None,
            },
        );

        let primed = prepared
            .before_operation(&pipeline.operations[0], Some(&rootfs), Some(&rootfs))
            .unwrap();

        assert!(!primed.complete);
        assert!(!rootfs.join("workspace/cached.txt").exists());
        assert_eq!(prepared.step_slice_stats.skip_count, 1);

        fs::create_dir_all(rootfs.join("workspace")).unwrap();
        fs::write(rootfs.join("workspace/live.txt"), "live").unwrap();

        let saved_ms = prepared
            .after_operation(&pipeline.operations[0], Some(&rootfs), Some(&rootfs))
            .unwrap();

        assert_eq!(saved_ms, 0);
        assert_eq!(prepared.current_step_index, 1);
        assert_eq!(prepared.current_state_key, expected_state_key);
        let tag = crate::cache::slice::step_tag(
            &pipeline.image,
            &pipeline.platform,
            0,
            step.tag.as_deref(),
        );
        assert!(store.inspect_key(&tag).unwrap().is_none());
    }

    #[test]
    fn prefetch_step_slices_reads_manifests_without_fetching_blobs() {
        let mut manifest = tree_cache_manifest("sha256:test-blob".to_string(), 42, None);
        manifest.metadata.insert(
            STEP_SLICE_STATE_KEY_METADATA_KEY.to_string(),
            "cached-state-key".to_string(),
        );
        let backend = ManifestOnlyBackend {
            manifest,
            has_blob_calls: AtomicUsize::new(0),
            fetch_blob_calls: AtomicUsize::new(0),
        };
        let pipeline = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("sync-application-source".to_string()),
                run: "echo hi".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: Some("web-source".to_string()),
            })],
            export: None,
            metadata: None,
            base_dir: PathBuf::from("/tmp"),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let mut stats = StepSliceStats::default();
        let prefetched = prefetch_step_slices(&pipeline, &backend, &mut stats);

        assert_eq!(prefetched[&0].blob_digest, "sha256:test-blob");
        assert_eq!(prefetched[&0].step_state_key, "cached-state-key");
        assert_eq!(stats.manifest_candidates, 1);
        assert_eq!(backend.has_blob_calls.load(Ordering::SeqCst), 0);
        assert_eq!(backend.fetch_blob_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn step_slice_summary_line_reports_fetch_skips_separately() {
        let stats = StepSliceStats {
            manifest_candidates: 2,
            manifest_misses: 1,
            restore_hits: 1,
            restore_mismatches: 1,
            blob_fetches: 1,
            blob_fetch_skips: 1,
            snapshot_captures: 1,
            saves: 1,
            ..StepSliceStats::default()
        };

        assert_eq!(
            stats.summary_line().as_deref(),
            Some(
                "summary: manifests candidate=2 miss=1 missing_key=0 invalid=0; restore hit=1 miss=0 mismatch=1 fetch=1 fetch_skipped=1; save=1 snapshot=1 skip=0 fail=0"
            )
        );
    }

    #[test]
    fn setup_snapshot_round_trips_across_runs() {
        let temp = tempdir().unwrap();
        let store = LocalCacheStore::open(temp.path().join("store").as_path()).unwrap();
        let pipeline = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: Some(SetupSnapshot {
                path: "/opt/setup".to_string(),
                key: "shared-setup".to_string(),
                restore_from: Vec::new(),
            }),
            operations: vec![Operation::Exec(Step {
                name: Some("build".to_string()),
                run: "echo build".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: None,
            })],
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let first_rootfs = temp.path().join("first-rootfs");
        fs::create_dir_all(first_rootfs.join("opt/setup")).unwrap();
        let mut first = PreparedCaches::new(&pipeline, &store, false, false, false).unwrap();
        let primed = first
            .before_operation(
                &pipeline.operations[0],
                Some(&first_rootfs),
                Some(&first_rootfs),
            )
            .unwrap();
        assert!(!primed.complete);
        fs::write(first_rootfs.join("opt/setup/tool.txt"), "ready").unwrap();
        first
            .after_operation(
                &pipeline.operations[0],
                Some(&first_rootfs),
                Some(&first_rootfs),
            )
            .unwrap();
        assert!(store.inspect_key("shared-setup").unwrap().is_some());

        let second_rootfs = temp.path().join("second-rootfs");
        fs::create_dir_all(second_rootfs.join("opt/setup")).unwrap();
        fs::write(second_rootfs.join("opt/setup/stale.txt"), "stale").unwrap();
        let mut second = PreparedCaches::new(&pipeline, &store, false, false, false).unwrap();
        let primed = second
            .before_operation(
                &pipeline.operations[0],
                Some(&second_rootfs),
                Some(&second_rootfs),
            )
            .unwrap();
        assert!(!primed.complete);
        assert_eq!(
            fs::read_to_string(second_rootfs.join("opt/setup/tool.txt")).unwrap(),
            "ready"
        );
        assert!(!second_rootfs.join("opt/setup/stale.txt").exists());
    }

    struct RecordingBackend {
        name: &'static str,
        rootfs_root: TempDir,
        seen: std::sync::Mutex<Vec<Pipeline>>,
        seen_options: std::sync::Mutex<Vec<RunOptions>>,
        seen_cache_hooks: std::sync::Mutex<Vec<bool>>,
        seen_workspace_sources: std::sync::Mutex<Vec<Option<PathBuf>>>,
        seen_workspace_seed: std::sync::Mutex<Vec<Option<String>>>,
    }

    impl RecordingBackend {
        fn new() -> Self {
            Self::named("recording")
        }

        fn named(name: &'static str) -> Self {
            Self {
                name,
                rootfs_root: tempdir().unwrap(),
                seen: std::sync::Mutex::new(Vec::new()),
                seen_options: std::sync::Mutex::new(Vec::new()),
                seen_cache_hooks: std::sync::Mutex::new(Vec::new()),
                seen_workspace_sources: std::sync::Mutex::new(Vec::new()),
                seen_workspace_seed: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl ExecutionBackend for RecordingBackend {
        fn name(&self) -> &'static str {
            self.name
        }

        fn run(
            &self,
            pipeline: &Pipeline,
            options: &RunOptions,
            _cache_config: &RunConfig,
            cache_hooks: Option<&mut dyn OperationCacheHooks>,
        ) -> Result<RunSummary> {
            self.seen.lock().unwrap().push(pipeline.clone());
            self.seen_options.lock().unwrap().push(options.clone());
            self.seen_cache_hooks
                .lock()
                .unwrap()
                .push(cache_hooks.is_some());
            let workspace_input = pipeline
                .inputs
                .iter()
                .find(|input| !input.readonly && input.dest == pipeline.workdir)
                .map(|input| input.source.clone());
            let workspace_seed = workspace_input
                .as_ref()
                .and_then(|source| fs::read_to_string(source.join("seed.txt")).ok());
            self.seen_workspace_sources
                .lock()
                .unwrap()
                .push(workspace_input);
            self.seen_workspace_seed
                .lock()
                .unwrap()
                .push(workspace_seed);

            let rootfs_dir = if options.keep_rootfs {
                let rootfs = self.rootfs_root.path().join("setup-rootfs");
                fs::create_dir_all(rootfs.join("app")).unwrap();
                fs::write(rootfs.join("app/output.txt"), "artifact").unwrap();
                Some(rootfs)
            } else {
                None
            };

            Ok(RunSummary {
                container_name: None,
                export_path: None,
                rootfs_dir,
                _keep_alive: None,
                timings: RunTimings::default(),
            })
        }
    }

    #[test]
    fn propagates_stage_dependency_digests_to_dependent_targets() {
        let temp = tempdir().unwrap();
        let backend = RecordingBackend::new();

        let setup = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: vec!["/workspace".to_string()],
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("setup".to_string()),
                run: "echo setup".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: None,
            })],
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let build = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: vec!["/workspace".to_string()],
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("build".to_string()),
                run: "echo build".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: None,
            })],
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: vec!["setup".to_string()],
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let mut targets = indexmap::IndexMap::new();
        targets.insert("setup".to_string(), setup);
        targets.insert("build".to_string(), build);
        let multi = MultiTargetRecipe {
            targets,
            order: vec!["setup".to_string(), "build".to_string()],
            base_dir: temp.path().to_path_buf(),
        };

        run_multi_target_recipe(
            &backend,
            &multi,
            &RunOptions::default(),
            &test_run_config(temp.path()),
        )
        .unwrap();

        let seen = backend.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let dependent = &seen[1];
        let digest = dependent.stage_dependency_digests.get("setup").unwrap();
        assert!(!digest.is_empty());
        assert!(
            dependent
                .inputs
                .iter()
                .any(|input| input.dest == "/boringbuilder-stages/setup")
        );
    }

    #[test]
    fn untagged_pipeline_uses_automatic_cache_hooks() {
        let backend = RecordingBackend::new();
        let temp = tempdir().unwrap();
        let pipeline = Pipeline {
            image: "ghcr.io/boringcache/base:bookworm-build".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: vec![Operation::Exec(crate::schema::Step {
                name: Some("install-runtime-tools".to_string()),
                run: "echo hi".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: None,
            })],
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let config = RunConfig {
            cache_dir: Some(temp.path().join("cache")),
            ..RunConfig::default()
        };
        run_pipeline(&backend, &pipeline, &RunOptions::default(), &config).unwrap();

        assert_eq!(backend.seen_cache_hooks.lock().unwrap().as_slice(), &[true]);
    }

    #[test]
    fn macos_container_pipeline_bypasses_outer_cache_hooks() {
        let backend = RecordingBackend::named("macos-container");
        let temp = tempdir().unwrap();
        let pipeline = Pipeline {
            image: "ghcr.io/boringcache/base:bookworm-build".to_string(),
            platform: "linux/arm64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: vec![Operation::Exec(crate::schema::Step {
                name: Some("install-runtime-tools".to_string()),
                run: "echo hi".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: Some("shared-runtime-tools".to_string()),
            })],
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        run_pipeline(
            &backend,
            &pipeline,
            &RunOptions::default(),
            &test_run_config(temp.path()),
        )
        .unwrap();

        assert_eq!(
            backend.seen_cache_hooks.lock().unwrap().as_slice(),
            &[false]
        );
    }

    #[test]
    fn setup_snapshot_pipeline_uses_cache_hooks_without_step_tags() {
        let backend = RecordingBackend::new();
        let temp = tempdir().unwrap();
        let pipeline = Pipeline {
            image: "ghcr.io/boringcache/base:bookworm-build".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            operations: vec![Operation::Exec(crate::schema::Step {
                name: Some("install-runtime-tools".to_string()),
                run: "echo hi".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: None,
            })],
            outputs: Vec::new(),
            setup_snapshot: Some(SetupSnapshot {
                path: "/opt/setup".to_string(),
                key: "shared-setup".to_string(),
                restore_from: Vec::new(),
            }),
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let config = test_run_config(temp.path());
        run_pipeline(&backend, &pipeline, &RunOptions::default(), &config).unwrap();

        assert_eq!(backend.seen_cache_hooks.lock().unwrap().as_slice(), &[true]);
    }

    #[test]
    fn host_backend_with_writable_mounts_skips_cache_hooks() {
        let backend = RecordingBackend::named("macos-host");
        let temp = tempdir().unwrap();
        let pipeline = Pipeline {
            image: "ghcr.io/boringcache/base:bookworm-build".to_string(),
            platform: "darwin/arm64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: vec![Input {
                source: temp.path().to_path_buf(),
                dest: "/workspace".to_string(),
                readonly: false,
            }],
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: vec![Operation::Exec(crate::schema::Step {
                name: Some("build-release".to_string()),
                run: "echo hi".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: Some("shared-release-build".to_string()),
            })],
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let config = test_run_config(temp.path());
        run_pipeline(&backend, &pipeline, &RunOptions::default(), &config).unwrap();

        assert_eq!(
            backend.seen_cache_hooks.lock().unwrap().as_slice(),
            &[false]
        );
    }

    #[test]
    fn host_backend_without_writable_mounts_still_uses_cache_hooks() {
        let backend = RecordingBackend::named("macos-host");
        let temp = tempdir().unwrap();
        let pipeline = Pipeline {
            image: "ghcr.io/boringcache/base:bookworm-build".to_string(),
            platform: "darwin/arm64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: vec![Input {
                source: temp.path().join("src"),
                dest: "/src".to_string(),
                readonly: true,
            }],
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: vec![Operation::Exec(crate::schema::Step {
                name: Some("install-runtime-tools".to_string()),
                run: "echo hi".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: Some("shared-runtime-tools".to_string()),
            })],
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let config = RunConfig {
            cache_dir: Some(temp.path().join("cache")),
            ..RunConfig::default()
        };
        run_pipeline(&backend, &pipeline, &RunOptions::default(), &config).unwrap();

        assert_eq!(backend.seen_cache_hooks.lock().unwrap().as_slice(), &[true]);
    }

    #[test]
    fn suppresses_cli_export_override_for_non_final_targets_without_explicit_export() {
        let temp = tempdir().unwrap();
        let backend = RecordingBackend::new();

        let setup = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: vec!["/workspace".to_string()],
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("setup".to_string()),
                run: "echo setup".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: None,
            })],
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let final_target = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: vec!["/workspace".to_string()],
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("final".to_string()),
                run: "echo final".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: None,
            })],
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: vec!["setup".to_string()],
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let mut targets = indexmap::IndexMap::new();
        targets.insert("setup".to_string(), setup);
        targets.insert("final".to_string(), final_target);
        let multi = MultiTargetRecipe {
            targets,
            order: vec!["setup".to_string(), "final".to_string()],
            base_dir: temp.path().to_path_buf(),
        };

        run_multi_target_recipe(
            &backend,
            &multi,
            &RunOptions {
                output_path_override: Some(temp.path().join("out")),
                export_format_override: Some(crate::schema::ExportFormat::Oci),
                ..RunOptions::default()
            },
            &test_run_config(temp.path()),
        )
        .unwrap();

        let seen = backend.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].export.is_none());
        assert!(seen[1].export.is_none());
        let seen_options = backend.seen_options.lock().unwrap();
        assert!(seen_options[0].output_path_override.is_none());
        assert!(seen_options[0].export_format_override.is_none());
        assert_eq!(
            seen_options[1].output_path_override,
            Some(temp.path().join("out"))
        );
        assert_eq!(
            seen_options[1].export_format_override,
            Some(crate::schema::ExportFormat::Oci)
        );
    }

    #[test]
    fn narrows_non_final_stage_outputs_to_requested_copy_paths() {
        let temp = tempdir().unwrap();
        let backend = RecordingBackend::new();

        let setup = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: vec!["/".to_string()],
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("setup".to_string()),
                run: "echo setup".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: None,
            })],
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let final_target = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: vec!["/workspace".to_string()],
            setup_snapshot: None,
            operations: vec![Operation::CopyFromStage(crate::schema::StageCopyOp {
                name: Some("copy".to_string()),
                stage: "setup".to_string(),
                sources: vec!["/app".to_string(), "/usr/bin/tool".to_string()],
                dest: "/workspace".to_string(),
                exclude: Vec::new(),
                preserve_parents: false,
                follow_symlinks: true,
                chown: None,
                chmod: None,
            })],
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: vec!["setup".to_string()],
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let mut targets = indexmap::IndexMap::new();
        targets.insert("setup".to_string(), setup);
        targets.insert("final".to_string(), final_target);
        let multi = MultiTargetRecipe {
            targets,
            order: vec!["setup".to_string(), "final".to_string()],
            base_dir: temp.path().to_path_buf(),
        };

        run_multi_target_recipe(
            &backend,
            &multi,
            &RunOptions::default(),
            &test_run_config(temp.path()),
        )
        .unwrap();

        let seen = backend.seen.lock().unwrap();
        assert_eq!(
            seen[0].outputs,
            vec!["/app".to_string(), "/usr/bin/tool".to_string()]
        );
    }

    #[test]
    fn isolates_implicit_workspace_per_target() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("seed.txt"), "seed").unwrap();
        let backend = RecordingBackend::new();

        let setup = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: vec![crate::schema::Input {
                source: temp.path().to_path_buf(),
                dest: "/workspace".to_string(),
                readonly: false,
            }],
            outputs: vec!["/workspace".to_string()],
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("setup".to_string()),
                run: "echo setup".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: None,
            })],
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let build = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: vec![crate::schema::Input {
                source: temp.path().to_path_buf(),
                dest: "/workspace".to_string(),
                readonly: false,
            }],
            outputs: vec!["/workspace".to_string()],
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("build".to_string()),
                run: "echo build".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: None,
            })],
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: vec!["setup".to_string()],
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let multi = MultiTargetRecipe {
            targets: indexmap::IndexMap::from([
                ("setup".to_string(), setup),
                ("build".to_string(), build),
            ]),
            order: vec!["setup".to_string(), "build".to_string()],
            base_dir: temp.path().to_path_buf(),
        };

        run_multi_target_recipe(
            &backend,
            &multi,
            &RunOptions::default(),
            &test_run_config(temp.path()),
        )
        .unwrap();

        let seen = backend.seen.lock().unwrap();
        let sources = backend.seen_workspace_sources.lock().unwrap();
        let setup_workspace = sources[0].as_ref().unwrap();
        let build_workspace = sources[1].as_ref().unwrap();
        assert_ne!(setup_workspace, temp.path());
        assert_ne!(build_workspace, temp.path());
        assert_ne!(setup_workspace, build_workspace);
        let seeds = backend.seen_workspace_seed.lock().unwrap();
        assert_eq!(seeds[0].as_deref(), Some("seed"));
        assert_eq!(seeds[1].as_deref(), Some("seed"));
        drop(sources);
        drop(seeds);
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn slice_index_only_advances_for_exec_operations() {
        // Regression: current_step_index must only advance for Exec ops.
        // A COPY between two RUNs should not shift the slice index.
        let pipeline = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: vec![
                Operation::Exec(Step {
                    name: Some("step-1".to_string()),
                    run: "echo one".to_string(),
                    run_exec: None,
                    run_mounts: Vec::new(),
                    env: BTreeMap::new(),
                    workdir: None,
                    shell: None,
                    build_cache_inputs: None,
                    build_cache: None,
                    tag: None,
                }),
                Operation::CopyFromContext(crate::schema::ContextCopyOp {
                    name: Some("copy".to_string()),
                    sources: vec![".".to_string()],
                    dest: "/workspace".to_string(),
                    exclude: Vec::new(),
                    extract_archives: false,
                    preserve_parents: false,
                    chown: None,
                    chmod: None,
                }),
                Operation::Exec(Step {
                    name: Some("step-2".to_string()),
                    run: "echo two".to_string(),
                    run_exec: None,
                    run_mounts: Vec::new(),
                    env: BTreeMap::new(),
                    workdir: None,
                    shell: None,
                    build_cache_inputs: None,
                    build_cache: None,
                    tag: None,
                }),
            ],
            export: None,
            metadata: None,
            base_dir: PathBuf::from("/tmp"),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        // Verify prefetch indexes match: only 2 exec steps, not 3 total ops.
        let exec_count = pipeline
            .operations
            .iter()
            .filter(|op| matches!(op, Operation::Exec(_)))
            .count();
        assert_eq!(exec_count, 2);

        // Verify step_tag produces the same tag at index 0 for both
        // the prefetch path and the operation path.
        let prefetch_tag_0 =
            crate::cache::slice::step_tag(&pipeline.image, &pipeline.platform, 0, None);
        let prefetch_tag_1 =
            crate::cache::slice::step_tag(&pipeline.image, &pipeline.platform, 1, None);
        assert_ne!(
            prefetch_tag_0, prefetch_tag_1,
            "different steps get different tags"
        );
    }

    #[test]
    fn tagged_slices_use_human_tag_for_cross_pipeline_reuse() {
        // Regression: step_tag with a human tag must produce the same tag
        // regardless of image/platform/index — that's how cross-pipeline
        // reuse works.
        let tag_a =
            crate::cache::slice::step_tag("alpine:3.18", "linux/amd64", 0, Some("shared-packages"));
        let tag_b = crate::cache::slice::step_tag(
            "ubuntu:24.04",
            "linux/arm64",
            5,
            Some("shared-packages"),
        );
        assert_eq!(
            tag_a, tag_b,
            "human tags should produce identical cache keys"
        );
        assert!(tag_a.contains("shared-packages"));
    }
}
