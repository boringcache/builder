use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::cache::backend::CacheBackend;
use crate::schema::{
    Operation, Pipeline, Step, StepBuildCacheInput, StepRunBindSource, StepRunMount,
};
use crate::util::workspace::{
    hash_path_with_patterns, summarize_git_workspace_changes_with_patterns,
};

pub(crate) struct PrefetchedSlice {
    pub(crate) blob_digest: String,
    pub(crate) archive_format: String,
    pub(crate) step_state_key: String,
    pub(crate) step_state_debug: Option<crate::cache::slice::StepStateDebugMetadata>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StepSliceStats {
    pub(crate) manifest_candidates: u32,
    pub(crate) manifest_misses: u32,
    pub(crate) manifest_missing_state_keys: u32,
    pub(crate) manifest_invalid: u32,
    pub(crate) restore_hits: u32,
    pub(crate) restore_misses: u32,
    pub(crate) restore_mismatches: u32,
    pub(crate) restore_failures: u32,
    pub(crate) blob_fetches: u32,
    pub(crate) blob_fetch_skips: u32,
    pub(crate) snapshot_captures: u32,
    pub(crate) snapshot_failures: u32,
    pub(crate) saves: u32,
    pub(crate) save_failures: u32,
    pub(crate) skip_count: u32,
}

impl StepSliceStats {
    pub(crate) fn has_activity(&self) -> bool {
        self.manifest_candidates > 0
            || self.manifest_misses > 0
            || self.manifest_missing_state_keys > 0
            || self.manifest_invalid > 0
            || self.restore_hits > 0
            || self.restore_misses > 0
            || self.restore_mismatches > 0
            || self.restore_failures > 0
            || self.blob_fetches > 0
            || self.blob_fetch_skips > 0
            || self.snapshot_captures > 0
            || self.snapshot_failures > 0
            || self.saves > 0
            || self.save_failures > 0
            || self.skip_count > 0
    }

    pub(crate) fn failure_count(&self) -> u32 {
        self.restore_failures + self.snapshot_failures + self.save_failures
    }

    pub(crate) fn summary_line(&self) -> Option<String> {
        if !self.has_activity() {
            return None;
        }

        Some(format!(
            "summary: manifests candidate={} miss={} missing_key={} invalid={}; restore hit={} miss={} mismatch={} fetch={} fetch_skipped={}; save={} snapshot={} skip={} fail={}",
            self.manifest_candidates,
            self.manifest_misses,
            self.manifest_missing_state_keys,
            self.manifest_invalid,
            self.restore_hits,
            self.restore_misses,
            self.restore_mismatches,
            self.blob_fetches,
            self.blob_fetch_skips,
            self.saves,
            self.snapshot_captures,
            self.skip_count,
            self.failure_count(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComputedStepState {
    pub(crate) key: String,
    pub(crate) debug: crate::cache::slice::StepStateDebugMetadata,
}

pub(crate) fn restore_prefetched_slice(
    backend: &dyn CacheBackend,
    prefetched: &PrefetchedSlice,
    temp_root: &Path,
    rootfs: &Path,
) -> Result<u128> {
    let started = Instant::now();
    if !backend.has_blob(&prefetched.blob_digest)? {
        anyhow::bail!("cached slice blob {} is missing", prefetched.blob_digest);
    }

    let temp = tempfile::Builder::new()
        .prefix("slice-restore-")
        .suffix(".tar.zst")
        .tempfile_in(temp_root)
        .context("failed to create temporary slice restore file")?;
    backend.fetch_blob(&prefetched.blob_digest, temp.path())?;
    crate::cache::archive::unpack_archive_with_format(
        temp.path(),
        rootfs,
        &prefetched.archive_format,
    )?;

    Ok(started.elapsed().as_millis())
}

pub(crate) fn emit_slice_debug(message: impl AsRef<str>) {
    let message = message.as_ref();
    if std::env::var_os("BORINGBUILDER_SLICE_DEBUG").is_some() {
        crate::ui::print_status(format!("step slice {message}"));
    } else {
        crate::ui::print_detail(format!("step slice {message}"));
    }
}

pub(crate) fn step_slice_label(
    pipeline: &Pipeline,
    step_index: usize,
    operation: &Operation,
) -> Option<(String, String)> {
    match operation {
        Operation::Exec(step) => {
            let step_name = step.name.as_deref().unwrap_or("unnamed");
            let tag = crate::cache::slice::step_tag(
                &pipeline.image,
                &pipeline.platform,
                step_index,
                step.tag.as_deref(),
            );
            Some((format!("step {} ({step_name})", step_index + 1), tag))
        }
        _ => None,
    }
}

fn hash_step_state_field(hasher: &mut Sha256, label: &str, value: impl AsRef<[u8]>) {
    hasher.update(label.as_bytes());
    hasher.update(b"\0");
    hasher.update(value.as_ref());
    hasher.update(b"\0");
}

pub(crate) fn initial_step_state_key(pipeline: &Pipeline) -> String {
    let mut hasher = Sha256::new();
    hash_step_state_field(&mut hasher, "version", "boringbuilder-step-state-v4");
    hash_step_state_field(&mut hasher, "image", pipeline.image.as_bytes());
    hash_step_state_field(&mut hasher, "platform", pipeline.platform.as_bytes());
    hash_step_state_field(&mut hasher, "workdir", pipeline.workdir.as_bytes());
    for (key, value) in &pipeline.env {
        hash_step_state_field(&mut hasher, "pipeline-env-key", key.as_bytes());
        hash_step_state_field(&mut hasher, "pipeline-env-value", value.as_bytes());
    }
    hex::encode(hasher.finalize())
}

pub(crate) fn resolve_container_path(rootfs: &Path, container_path: &str) -> PathBuf {
    if container_path == "/" {
        rootfs.to_path_buf()
    } else {
        rootfs.join(container_path.trim_start_matches('/'))
    }
}

fn hash_step_build_cache_input(rootfs: &Path, input: &StepBuildCacheInput) -> Result<String> {
    let host_path = resolve_container_path(rootfs, &input.path);
    hash_path_with_patterns(&host_path, &input.exclude).with_context(|| {
        format!(
            "failed to hash step cache input {} at {}",
            input.path,
            host_path.display()
        )
    })
}

fn summarize_step_input_change(
    snapshot_rootfs: &Path,
    input: &crate::cache::slice::StepStateInputDebug,
) -> Result<Option<String>> {
    const LIMIT: usize = 3;

    let host_path = resolve_container_path(snapshot_rootfs, &input.path);
    let metadata = match fs::symlink_metadata(&host_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some(format!("input {} is missing", input.path)));
        }
        Err(err) => return Err(err.into()),
    };

    if !metadata.is_dir() {
        return Ok(Some(format!("changed path {}", input.path)));
    }

    let Some(summary) =
        summarize_git_workspace_changes_with_patterns(&host_path, &input.exclude, LIMIT)?
    else {
        return Ok(None);
    };
    if summary.paths.is_empty() {
        return Ok(Some(format!(
            "input {} is a clean git workspace; cached slice likely came from a different commit or branch",
            input.path
        )));
    }

    let listed = summary
        .paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = if summary.truncated {
        format!(
            ", +{} more",
            summary.total_paths.saturating_sub(summary.paths.len())
        )
    } else {
        String::new()
    };
    Ok(Some(format!(
        "changed paths under {}: {}{}",
        input.path, listed, suffix
    )))
}

pub(crate) fn explain_step_state_mismatch(
    _step: &Step,
    snapshot_rootfs: &Path,
    expected_debug: Option<&crate::cache::slice::StepStateDebugMetadata>,
    current_debug: &crate::cache::slice::StepStateDebugMetadata,
) -> Result<Vec<String>> {
    let Some(expected_debug) = expected_debug else {
        return Ok(vec![
            "cache explain: cached slice predates declared input metadata; mismatch may come from input content, step definition, or prior state".to_string(),
        ]);
    };

    if current_debug.inputs.is_empty() && expected_debug.inputs.is_empty() {
        return Ok(vec![
            "cache explain: declared inputs matched; mismatch came from prior step state or the step definition".to_string(),
        ]);
    }

    let mut details = Vec::new();
    let mut matched_expected = vec![false; expected_debug.inputs.len()];

    for current in &current_debug.inputs {
        let expected_index = expected_debug.inputs.iter().position(|expected| {
            expected.path == current.path && expected.exclude == current.exclude
        });
        match expected_index {
            Some(index) => {
                matched_expected[index] = true;
                if expected_debug.inputs[index].hash != current.hash {
                    details.push(format!(
                        "cache explain: invalidated by declared input {} (content changed)",
                        current.path
                    ));
                    if let Some(summary) = summarize_step_input_change(snapshot_rootfs, current)? {
                        details.push(format!("cache explain: {summary}"));
                    }
                }
            }
            None => details.push(format!(
                "cache explain: invalidated by declared input {} (input path or excludes changed)",
                current.path
            )),
        }
    }

    for (index, expected) in expected_debug.inputs.iter().enumerate() {
        if !matched_expected[index] {
            details.push(format!(
                "cache explain: cached slice expected declared input {} which is no longer part of this step",
                expected.path
            ));
        }
    }

    if details.is_empty() {
        details.push(
            "cache explain: declared inputs matched; mismatch came from prior step state or the step definition"
                .to_string(),
        );
    }

    Ok(details)
}

pub(crate) fn compute_step_state(
    previous_state_key: &str,
    pipeline: &Pipeline,
    step: &Step,
    snapshot_rootfs: &Path,
    hash_default_rootfs: bool,
) -> Result<ComputedStepState> {
    let mut hasher = Sha256::new();
    let mut input_debug = Vec::new();
    hash_step_state_field(&mut hasher, "version", "boringbuilder-step-state-v4");
    hash_step_state_field(&mut hasher, "previous", previous_state_key.as_bytes());
    if let Some(argv) = &step.run_exec {
        hash_step_state_field(&mut hasher, "run-mode", b"exec");
        for arg in argv {
            hash_step_state_field(&mut hasher, "run-arg", arg.as_bytes());
        }
    } else {
        hash_step_state_field(&mut hasher, "run-mode", b"shell");
        hash_step_state_field(&mut hasher, "run", step.run.as_bytes());
    }
    if let Some(workdir) = &step.workdir {
        hash_step_state_field(&mut hasher, "workdir", workdir.as_bytes());
    }
    if step.run_exec.is_none()
        && let Some(shell) = &step.shell
    {
        hash_step_state_field(&mut hasher, "shell", shell.as_bytes());
    }
    for (key, value) in &step.env {
        hash_step_state_field(&mut hasher, "env-key", key.as_bytes());
        hash_step_state_field(&mut hasher, "env-value", value.as_bytes());
    }
    for mount in &step.run_mounts {
        match mount {
            StepRunMount::Cache {
                target,
                id,
                key,
                restore_from,
                readonly,
                sharing,
            } => {
                hash_step_state_field(&mut hasher, "mount-type", b"cache");
                hash_step_state_field(&mut hasher, "mount-target", target.as_bytes());
                hash_step_state_field(&mut hasher, "mount-id", id.as_bytes());
                if let Some(key) = key {
                    hash_step_state_field(&mut hasher, "mount-key", key.as_bytes());
                }
                for restore_key in restore_from {
                    hash_step_state_field(
                        &mut hasher,
                        "mount-restore-from",
                        restore_key.as_bytes(),
                    );
                }
                hash_step_state_field(
                    &mut hasher,
                    "mount-readonly",
                    if *readonly { "true" } else { "false" },
                );
                hash_step_state_field(
                    &mut hasher,
                    "mount-sharing",
                    match sharing {
                        crate::schema::CacheMode::Shared => b"shared",
                        crate::schema::CacheMode::Locked => b"locked",
                    },
                );
            }
            StepRunMount::Bind {
                target,
                source,
                readonly,
            } => {
                hash_step_state_field(&mut hasher, "mount-type", b"bind");
                hash_step_state_field(&mut hasher, "mount-target", target.as_bytes());
                hash_step_state_field(
                    &mut hasher,
                    "mount-readonly",
                    if *readonly { "true" } else { "false" },
                );
                match source {
                    StepRunBindSource::Context { path } => {
                        hash_step_state_field(&mut hasher, "mount-source-kind", b"context");
                        hash_step_state_field(&mut hasher, "mount-source-path", path.as_bytes());
                        let source_hash = hash_context_run_mount_source(pipeline, path)?;
                        hash_step_state_field(&mut hasher, "mount-source-hash", source_hash);
                    }
                    StepRunBindSource::Stage { stage, path } => {
                        hash_step_state_field(&mut hasher, "mount-source-kind", b"stage");
                        hash_step_state_field(&mut hasher, "mount-source-stage", stage.as_bytes());
                        hash_step_state_field(&mut hasher, "mount-source-path", path.as_bytes());
                        let source_hash =
                            hash_stage_run_mount_source(snapshot_rootfs, stage, path)?;
                        hash_step_state_field(&mut hasher, "mount-source-hash", source_hash);
                    }
                }
            }
            StepRunMount::Tmpfs { target, size } => {
                hash_step_state_field(&mut hasher, "mount-type", b"tmpfs");
                hash_step_state_field(&mut hasher, "mount-target", target.as_bytes());
                if let Some(size) = size {
                    hash_step_state_field(&mut hasher, "mount-size", size.as_bytes());
                }
            }
            StepRunMount::Secret {
                target,
                id,
                env,
                required,
                mode,
                uid,
                gid,
            } => {
                hash_step_state_field(&mut hasher, "mount-type", b"secret");
                hash_step_state_field(&mut hasher, "mount-target", target.as_bytes());
                hash_step_state_field(&mut hasher, "mount-id", id.as_bytes());
                hash_step_state_field(
                    &mut hasher,
                    "mount-required",
                    if *required { "true" } else { "false" },
                );
                if let Some(env) = env {
                    hash_step_state_field(&mut hasher, "mount-env", env.as_bytes());
                }
                if let Some(mode) = mode {
                    hash_step_state_field(
                        &mut hasher,
                        "mount-mode",
                        format!("{mode:o}").as_bytes(),
                    );
                }
                if let Some(uid) = uid {
                    hash_step_state_field(&mut hasher, "mount-uid", uid.to_string().as_bytes());
                }
                if let Some(gid) = gid {
                    hash_step_state_field(&mut hasher, "mount-gid", gid.to_string().as_bytes());
                }
            }
            StepRunMount::Ssh {
                target,
                id,
                required,
                mode,
                uid,
                gid,
            } => {
                hash_step_state_field(&mut hasher, "mount-type", b"ssh");
                hash_step_state_field(&mut hasher, "mount-target", target.as_bytes());
                hash_step_state_field(&mut hasher, "mount-id", id.as_bytes());
                hash_step_state_field(
                    &mut hasher,
                    "mount-required",
                    if *required { "true" } else { "false" },
                );
                if let Some(mode) = mode {
                    hash_step_state_field(
                        &mut hasher,
                        "mount-mode",
                        format!("{mode:o}").as_bytes(),
                    );
                }
                if let Some(uid) = uid {
                    hash_step_state_field(&mut hasher, "mount-uid", uid.to_string().as_bytes());
                }
                if let Some(gid) = gid {
                    hash_step_state_field(&mut hasher, "mount-gid", gid.to_string().as_bytes());
                }
            }
        }
    }
    match &step.build_cache_inputs {
        Some(inputs) => {
            for input in inputs {
                hash_step_state_field(&mut hasher, "input-path", input.path.as_bytes());
                for exclude in &input.exclude {
                    hash_step_state_field(&mut hasher, "input-exclude", exclude.as_bytes());
                }
                let input_hash = hash_step_build_cache_input(snapshot_rootfs, input)?;
                hash_step_state_field(&mut hasher, "input-hash", input_hash.as_bytes());
                input_debug.push(crate::cache::slice::StepStateInputDebug {
                    path: input.path.clone(),
                    exclude: input.exclude.clone(),
                    hash: input_hash,
                });
            }
        }
        None if hash_default_rootfs => {
            let input = StepBuildCacheInput {
                path: "/".to_string(),
                exclude: crate::util::fs_tree::PSEUDO_FS_DIRS
                    .iter()
                    .chain(crate::cache::slice::STEP_SLICE_IGNORED_PREFIXES)
                    .map(|path| (*path).to_string())
                    .collect(),
            };
            hash_step_state_field(&mut hasher, "input-path", input.path.as_bytes());
            let input_hash = hash_step_build_cache_input(snapshot_rootfs, &input)?;
            hash_step_state_field(&mut hasher, "input-hash", input_hash.as_bytes());
            input_debug.push(crate::cache::slice::StepStateInputDebug {
                path: input.path,
                exclude: input.exclude,
                hash: input_hash,
            });
        }
        None => {}
    }
    Ok(ComputedStepState {
        key: hex::encode(hasher.finalize()),
        debug: crate::cache::slice::StepStateDebugMetadata {
            inputs: input_debug,
        },
    })
}

#[cfg(test)]
pub(crate) fn compute_step_state_key(
    previous_state_key: &str,
    pipeline: &Pipeline,
    step: &Step,
    snapshot_rootfs: &Path,
) -> Result<String> {
    Ok(compute_step_state(previous_state_key, pipeline, step, snapshot_rootfs, true)?.key)
}

fn hash_context_run_mount_source(pipeline: &Pipeline, source: &str) -> Result<String> {
    let context = pipeline.docker_context.as_ref().ok_or_else(|| {
        anyhow::anyhow!("missing Docker build context for RUN --mount bind source")
    })?;
    crate::dockerfile::context::hash_source(context, source)
}

fn hash_stage_run_mount_source(
    snapshot_rootfs: &Path,
    stage: &str,
    source: &str,
) -> Result<String> {
    let host_path = resolve_stage_run_mount_source(snapshot_rootfs, stage, source);
    hash_path_with_patterns(&host_path, &[])
}

fn resolve_stage_run_mount_source(snapshot_rootfs: &Path, stage: &str, source: &str) -> PathBuf {
    let root = resolve_container_path(snapshot_rootfs, &format!("/boringbuilder-stages/{stage}"));
    if source == "/" {
        root
    } else {
        root.join(source.trim_start_matches('/'))
    }
}

/// Pre-fetch step slice manifests at pipeline start.
///
/// The current rootfs state is not known yet, so fetching blobs here can make
/// misses slow.  Blob fetch is deferred until `before_operation` confirms the
/// cached state key matches the current step state.
pub(crate) fn prefetch_step_slices(
    pipeline: &Pipeline,
    backend: &dyn CacheBackend,
    stats: &mut StepSliceStats,
) -> HashMap<usize, PrefetchedSlice> {
    let mut prefetched = HashMap::new();
    let exec_ops: Vec<_> = pipeline
        .operations
        .iter()
        .filter_map(|op| match op {
            Operation::Exec(step) => Some(step),
            _ => None,
        })
        .collect();

    for (step_index, step) in exec_ops.iter().enumerate() {
        if step.build_cache == Some(false) {
            continue;
        }
        let tag = crate::cache::slice::step_tag(
            &pipeline.image,
            &pipeline.platform,
            step_index,
            step.tag.as_deref(),
        );
        let step_name = step.name.as_deref().unwrap_or("unnamed");
        let label = format!("step {} ({step_name})", step_index + 1);
        let Ok(Some(manifest)) = backend.resolve_ref(&tag) else {
            stats.manifest_misses += 1;
            emit_slice_debug(format!("prefetch miss {label} tag={tag}"));
            continue;
        };
        let step_state_key =
            crate::cache::slice::manifest_step_state_key(&manifest).map(str::to_string);
        if step_state_key.is_none() {
            stats.manifest_missing_state_keys += 1;
            emit_slice_debug(format!("prefetch missing state key {label} tag={tag}"));
            continue;
        }
        let step_state_debug = crate::cache::slice::manifest_step_state_debug(&manifest);
        let Ok(blob) = crate::cache::backend::single_blob_from_manifest(
            &manifest,
            crate::cache::backend::TREE_CACHE_ARTIFACT_KIND,
            backend.kind(),
        ) else {
            stats.manifest_invalid += 1;
            emit_slice_debug(format!("prefetch invalid manifest {label} tag={tag}"));
            continue;
        };
        stats.manifest_candidates += 1;
        emit_slice_debug(format!(
            "prefetch candidate {label} tag={tag} digest={} state-key={}",
            blob.digest,
            step_state_key.as_deref().unwrap_or("-"),
        ));
        prefetched.insert(
            step_index,
            PrefetchedSlice {
                blob_digest: blob.digest,
                archive_format: crate::cache::slice::manifest_archive_format(&manifest).to_string(),
                step_state_key: step_state_key.expect("missing state key should be skipped"),
                step_state_debug,
            },
        );
    }

    if !prefetched.is_empty() {
        emit_slice_debug(format!(
            "pre-fetched {} step slice manifest(s)",
            prefetched.len()
        ));
    }

    prefetched
}
