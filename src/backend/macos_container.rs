use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::backend::macos_guest_helper::{
    GuestHelperHost, helper_batch_label, helper_mount_guest_dir, helper_snapshot_guest_dir,
    is_helper_operation,
};
use crate::backend::{
    ExecutionBackend, OperationCacheHooks, OperationTiming, RunOptions, RunSummary, RunTimings,
    print_timing_summary, resolve_export,
};
use crate::cache::{CacheStoreConfig, CacheStoreKind, open_cache_store};
use crate::dockerfile::context::materialize_source as materialize_context_source;
use crate::export::docker::export_oci_layout_as_docker;
use crate::export::tar::{export_pipeline_tar_from_rootfs, export_pipeline_tar_zst_from_rootfs};
use crate::macos_runtime::{ensure_container_system_ready, resolve_container_cli};
use crate::schema::{
    CacheMode, CacheMount, ExportFormat, Operation, Pipeline, Step, StepRunBindSource, StepRunMount,
};
use crate::ui;
use crate::util::fs::path_size;
use crate::util::interrupt;
use crate::util::oci_rootfs::unpack_rootfs_from_oci_layout;
use crate::util::process::{find_command, run_capture, run_checked, run_streaming};
use anyhow::{Context, Result, anyhow, bail, ensure};
use sha2::{Digest, Sha256};

#[derive(Debug, Default)]
pub struct MacosContainerBackend {
    pub container_binary: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
enum ArchiveExportKind {
    Tar,
    TarZst,
}

const RUN_MOUNT_GUEST_DIR: &str = "/boringbuilder-run-mounts";
const LOCAL_CACHE_GUEST_DIR: &str = "/boringbuilder-local-cache";
const CACHE_TOOLS_GUEST_DIR: &str = "/boringbuilder-cache-tools";
const SSH_AGENT_GUEST_DIR: &str = "/boringbuilder-ssh-agents";
const CONTEXT_GUEST_DIR: &str = "/boringbuilder-context";
const STEP_SLICE_RESTORE_MIN_FREE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const STEP_SLICE_RESTORE_FREE_BUFFER_BYTES: u64 = 512 * 1024 * 1024;
const STEP_SLICE_RESTORE_ARCHIVE_EXPANSION_FACTOR: u64 = 6;
const MACOS_CONTAINER_STEP_SLICE_CAPTURE_ABI: &str = "macos-container-ctime-marker-v2";

enum ActiveRunMountSource {
    Bind {
        guest_source: String,
        source_is_dir: bool,
    },
    Tmpfs {
        size: Option<String>,
    },
}

struct ActiveRunMount {
    guest_target: String,
    source: ActiveRunMountSource,
    readonly: bool,
    host_source: Option<PathBuf>,
    cache_entry: Option<CacheMount>,
    cache_save: bool,
    env: BTreeMap<String, String>,
    chown: Option<String>,
    chmod: Option<String>,
    _lock: Option<crate::cache::CacheLock>,
}

#[derive(Debug, Clone)]
struct LocalContainerCacheMount {
    host_root: PathBuf,
}

struct ContainerCacheTools {
    host_dir: tempfile::TempDir,
    boringcache: Option<ContainerBoringCacheTools>,
}

#[derive(Debug, Clone)]
struct ContainerBoringCacheTools {
    workspace: String,
    binary_guest_path: String,
    token_file_guest_path: Option<String>,
    api_url: Option<String>,
}

impl ArchiveExportKind {
    fn label(self) -> &'static str {
        match self {
            Self::Tar => "tar",
            Self::TarZst => "tar.zst",
        }
    }

    fn export(self, pipeline: &Pipeline, output_path: &Path, rootfs: &Path) -> Result<PathBuf> {
        match self {
            Self::Tar => export_pipeline_tar_from_rootfs(pipeline, output_path, rootfs),
            Self::TarZst => export_pipeline_tar_zst_from_rootfs(pipeline, output_path, rootfs),
        }
    }
}

struct ArchiveExportContext<'a> {
    binary: &'a Path,
    container_name: &'a str,
    pipeline: &'a Pipeline,
    platform: &'a str,
    guard: &'a mut ContainerGuard,
}

struct CapturedSliceArchive {
    temp: tempfile::NamedTempFile,
    digest: String,
    bytes: u64,
}

struct CapturedStoredSlice {
    digest: String,
    bytes: u64,
    archive_format: String,
}

impl ExecutionBackend for MacosContainerBackend {
    fn name(&self) -> &'static str {
        "macos-container"
    }

    fn run(
        &self,
        pipeline: &Pipeline,
        options: &RunOptions,
        cache_config: &CacheStoreConfig,
        _cache_hooks: Option<&mut dyn OperationCacheHooks>,
    ) -> Result<RunSummary> {
        ensure!(
            cfg!(target_os = "macos"),
            "the macOS backend can only run on macOS"
        );

        let run_start = Instant::now();
        let show_timings = options.timings;

        let binary = self.binary_path()?;
        ensure_system_ready(&binary)?;

        let resolved_export = resolve_export(pipeline, options);

        // Pull image via the container runtime (for execution).
        ui::print_status(format!(
            "pulling image {} ({})",
            pipeline.image, pipeline.platform
        ));
        let pull_started = Instant::now();
        run_checked(
            &binary,
            &[
                "image".to_string(),
                "pull".to_string(),
                "--progress".to_string(),
                "none".to_string(),
                "--platform".to_string(),
                pipeline.platform.clone(),
                pipeline.image.clone(),
            ],
        )?;
        let pull_ms = pull_started.elapsed().as_millis();

        let container_name = unique_container_name();
        let keepalive = "trap 'exit 0' TERM INT; while :; do sleep 3600; done";
        let mut guest_helper =
            if pipeline.operations.iter().any(is_helper_operation) || options.keep_rootfs {
                Some(GuestHelperHost::create()?)
            } else {
                None
            };
        let snapshot_dir = if options.keep_rootfs {
            Some(
                tempfile::Builder::new()
                    .prefix("boringbuilder-macos-snapshot-")
                    .tempdir()
                    .context("failed to create temporary macOS snapshot directory")?,
            )
        } else {
            None
        };
        let run_mount_dir = if pipeline.operations.iter().any(
            |operation| matches!(operation, Operation::Exec(step) if !step.run_mounts.is_empty()),
        ) {
            Some(
                tempfile::Builder::new()
                    .prefix("boringbuilder-macos-run-mounts-")
                    .tempdir()
                    .context("failed to create temporary macOS RUN mount directory")?,
            )
        } else {
            None
        };
        let local_cache_mount =
            prepare_local_container_cache_mount(cache_config, options.no_cache)?;
        let container_cache_tools =
            prepare_container_cache_tools(cache_config, pipeline, options.no_cache)?;
        let mut create_args = vec![
            "create".to_string(),
            "--name".to_string(),
            container_name.clone(),
            "--platform".to_string(),
            pipeline.platform.clone(),
            "--network".to_string(),
            "default".to_string(),
            "--workdir".to_string(),
            "/".to_string(),
            "--entrypoint".to_string(),
            "/bin/sh".to_string(),
            "--init".to_string(),
        ];

        // Do NOT use --rm when we need the container alive after stop for
        // `container export` or multi-stage rootfs capture.
        if !options.keep && resolved_export.is_none() && !options.keep_rootfs {
            create_args.push("--rm".to_string());
        }

        append_resource_args(&mut create_args);
        append_dns_args(&mut create_args);
        append_env_args(&mut create_args, &pipeline.env);
        append_mount_args(&mut create_args, pipeline)?;
        append_context_mount_arg(&mut create_args, pipeline)?;
        if let Some(helper) = &guest_helper {
            append_mount_arg(
                &mut create_args,
                helper.mount_source(),
                helper_mount_guest_dir(),
                true,
            );
        }
        if let Some(snapshot_dir) = &snapshot_dir {
            append_mount_arg(
                &mut create_args,
                snapshot_dir.path(),
                helper_snapshot_guest_dir(),
                false,
            );
        }
        if let Some(run_mount_dir) = &run_mount_dir {
            append_mount_arg(
                &mut create_args,
                run_mount_dir.path(),
                RUN_MOUNT_GUEST_DIR,
                false,
            );
        }
        if let Some(cache_mount) = &local_cache_mount {
            append_mount_arg(
                &mut create_args,
                &cache_mount.host_root,
                LOCAL_CACHE_GUEST_DIR,
                false,
            );
        }
        if let Some(cache_tools) = &container_cache_tools {
            append_mount_arg(
                &mut create_args,
                cache_tools.host_dir.path(),
                CACHE_TOOLS_GUEST_DIR,
                true,
            );
        }
        for ssh_source in pipeline_ssh_mount_sources(pipeline)? {
            append_mount_arg(
                &mut create_args,
                &ssh_source,
                &guest_ssh_mount_source(&ssh_source),
                true,
            );
        }
        create_args.push(pipeline.image.clone());
        create_args.push("-c".to_string());
        create_args.push(keepalive.to_string());

        interrupt::reset()?;
        let _interrupt_registration =
            register_macos_container_interrupt(&binary, container_name.clone())?;

        ui::print_status(format!("creating container {container_name}"));
        let prepare_started = Instant::now();
        run_checked(&binary, &create_args)?;
        let mut guard = ContainerGuard::new(binary.clone(), container_name.clone(), options.keep);
        interrupt::check("creating container")?;

        ui::print_status(format!("starting container {container_name}"));
        run_checked(&binary, &["start".to_string(), container_name.clone()])?;
        interrupt::check("starting container")?;
        self.ensure_workdirs(&binary, &container_name, pipeline)?;
        interrupt::check("preparing container workdirs")?;
        let prepare_ms = prepare_started.elapsed().as_millis();

        let start_index = pipeline.step_start_index(options.from_step.as_deref())?;
        // Open cache backend for step slices (same store as other cache artifacts).
        let slice_backend = if options.no_cache {
            ui::print_detail("step slices disabled: --no-cache");
            None
        } else {
            crate::cache::open_cache_backend(cache_config).ok()
        };
        if let Some(backend) = &slice_backend {
            println!(
                "{} {} {}",
                ui::prefix(),
                ui::accent("cache-store:"),
                format_args!("{} ({})", backend.kind(), backend.detail())
            );
        }

        let mut operations = Vec::new();
        let mut operation_ms_total = 0u128;
        let mut cache_restore_ms_total = 0u128;
        let mut cache_save_ms_total = 0u128;
        let mut step_ordinal = 0usize;
        let mut current_state_key = initial_step_state_key(pipeline);
        let mut rootfs_state_dirty = true;
        let mut idx = start_index;
        while idx < pipeline.operations.len() {
            if matches!(&pipeline.operations[idx], Operation::Exec(_)) {
                let operation = &pipeline.operations[idx];
                let label = operation_label(operation, idx);
                let step = match operation {
                    Operation::Exec(step) => step,
                    _ => unreachable!(),
                };

                // Step slice: restore previous delta into VM.
                let mut cached = false;
                let mut step_state_key = None;
                let mut capture_zstd_allowed = false;
                let slice_cache_enabled = step.build_cache != Some(false);
                let tag = if let Some(backend) = &slice_backend {
                    let tag = crate::cache::slice::step_tag(
                        &pipeline.image,
                        &pipeline.platform,
                        step_ordinal,
                        step.tag.as_deref(),
                    );
                    let step_label = format!(
                        "step {} ({})",
                        step_ordinal + 1,
                        step.name.as_deref().unwrap_or("unnamed")
                    );
                    let computed_state_key = compute_step_state_key(
                        &binary,
                        &container_name,
                        &current_state_key,
                        pipeline,
                        step,
                        rootfs_state_dirty,
                    )?;
                    step_state_key = Some(computed_state_key.clone());
                    if slice_cache_enabled {
                        match backend.resolve_ref(&tag) {
                            Ok(Some(manifest)) => {
                                let manifest_state_key =
                                    crate::cache::slice::manifest_step_state_key(&manifest);
                                let manifest_capture_abi =
                                    crate::cache::slice::manifest_capture_abi(&manifest);
                                if manifest_capture_abi
                                    != Some(MACOS_CONTAINER_STEP_SLICE_CAPTURE_ABI)
                                {
                                    ui::print_detail(format!(
                                        "step slice restore miss {step_label} tag={tag}: capture-abi mismatch expected={} cached={}",
                                        MACOS_CONTAINER_STEP_SLICE_CAPTURE_ABI,
                                        manifest_capture_abi.unwrap_or("-"),
                                    ));
                                } else if manifest_state_key == Some(computed_state_key.as_str()) {
                                    match crate::cache::backend::single_blob_from_manifest(
                                        &manifest,
                                        crate::cache::backend::TREE_CACHE_ARTIFACT_KIND,
                                        backend.kind(),
                                    ) {
                                        Ok(blob) => {
                                            if backend.has_blob(&blob.digest).unwrap_or(false) {
                                                ui::print_detail(format!(
                                                    "step slice restore hit {step_label} tag={tag} state-key={computed_state_key}"
                                                ));
                                                let restore_started = Instant::now();
                                                let archive_format =
                                                    crate::cache::slice::manifest_archive_format(
                                                        &manifest,
                                                    );
                                                let restore = if let Some(boringcache) =
                                                    container_cache_tools
                                                        .as_ref()
                                                        .and_then(|tools| {
                                                            tools.boringcache.as_ref()
                                                        })
                                                        .filter(|_| backend.kind() == "boringcache")
                                                {
                                                    Self::restore_boringcache_slice_to_container(
                                                        &binary,
                                                        &container_name,
                                                        boringcache,
                                                        &blob.digest,
                                                        archive_format,
                                                    )
                                                } else if local_cache_mount.is_some() {
                                                    Self::restore_local_slice_to_container(
                                                        &binary,
                                                        &container_name,
                                                        &blob.digest,
                                                        archive_format,
                                                    )
                                                } else {
                                                    Self::ensure_step_slice_restore_headroom(
                                                        blob.bytes,
                                                    )?;
                                                    let temp = tempfile::NamedTempFile::new().context(
                                                    "failed to create temporary slice restore file",
                                                )?;
                                                    backend
                                                        .fetch_blob(&blob.digest, temp.path())?;
                                                    Self::restore_slice_to_container(
                                                        &binary,
                                                        &container_name,
                                                        temp.path(),
                                                    )
                                                };
                                                if restore.is_ok() {
                                                    let restore_ms =
                                                        restore_started.elapsed().as_millis();
                                                    cache_restore_ms_total += restore_ms;
                                                    ui::print_detail(format!(
                                                        "step slice restored {step_label} tag={tag} ({restore_ms} ms)"
                                                    ));
                                                    cached = true;
                                                } else if let Err(err) = restore {
                                                    ui::print_detail(format!(
                                                        "step slice restore failed {step_label} tag={tag}: {err:#}"
                                                    ));
                                                }
                                            } else {
                                                ui::print_detail(format!(
                                                    "step slice restore miss {step_label} tag={tag}: blob missing digest={}",
                                                    blob.digest
                                                ));
                                            }
                                        }
                                        Err(err) => {
                                            ui::print_detail(format!(
                                                "step slice restore miss {step_label} tag={tag}: invalid manifest: {err:#}"
                                            ));
                                        }
                                    }
                                } else {
                                    ui::print_detail(format!(
                                        "step slice restore miss {step_label} tag={tag}: state-key mismatch expected={} current={computed_state_key}",
                                        manifest_state_key.unwrap_or("-"),
                                    ));
                                }
                            }
                            Ok(None) => {
                                ui::print_detail(format!(
                                    "step slice restore miss {step_label} tag={tag}: ref not found"
                                ));
                            }
                            Err(err) => {
                                ui::print_detail(format!(
                                    "step slice restore miss {step_label} tag={tag}: ref lookup failed: {err:#}"
                                ));
                            }
                        }
                    }
                    if !cached && slice_cache_enabled {
                        if backend.kind() == "boringcache"
                            && container_cache_tools
                                .as_ref()
                                .and_then(|tools| tools.boringcache.as_ref())
                                .is_some()
                        {
                            capture_zstd_allowed =
                                Self::container_zstd_available(&binary, &container_name);
                        }
                        let _ = Self::touch_slice_marker(&binary, &container_name);
                    }
                    Some(tag)
                } else {
                    None
                };

                ui::print_step(idx + 1, pipeline.operations.len(), &label, cached);
                if cached {
                    operations.push(OperationTiming {
                        label,
                        ms: 0,
                        cached: true,
                    });
                    if let Some(step_state_key) = step_state_key.take() {
                        current_state_key = step_state_key;
                    }
                    rootfs_state_dirty = false;
                    step_ordinal += 1;
                    idx += 1;
                    continue;
                }
                let op_started = Instant::now();
                self.exec_operation(
                    &binary,
                    &container_name,
                    pipeline,
                    operation,
                    cache_config,
                    run_mount_dir.as_ref().map(|dir| dir.path()),
                )?;
                let op_ms = op_started.elapsed().as_millis();
                operation_ms_total += op_ms;

                // Step slice: capture delta and save.
                if slice_cache_enabled
                    && let Some(tag) = tag
                    && let Some(backend) = &slice_backend
                    && let Some(step_state_key) = step_state_key.as_deref()
                {
                    let mut excluded_targets = step
                        .run_mounts
                        .iter()
                        .map(run_mount_target)
                        .collect::<Vec<_>>();
                    excluded_targets.extend(pipeline.inputs.iter().map(|input| input.dest.clone()));
                    if pipeline
                        .operations
                        .iter()
                        .any(|operation| matches!(operation, Operation::CopyFromContext(_)))
                    {
                        excluded_targets.push(CONTEXT_GUEST_DIR.to_string());
                    }
                    let metadata = crate::cache::slice::SlicePublishMetadata {
                        step_state_key: Some(step_state_key),
                        capture_abi: Some(MACOS_CONTAINER_STEP_SLICE_CAPTURE_ABI),
                        ..Default::default()
                    };
                    if let Some(boringcache) = container_cache_tools
                        .as_ref()
                        .and_then(|tools| tools.boringcache.as_ref())
                        .filter(|_| backend.kind() == "boringcache")
                    {
                        match Self::capture_boringcache_slice_to_container(
                            &binary,
                            &container_name,
                            boringcache,
                            &excluded_targets,
                            capture_zstd_allowed,
                        ) {
                            Ok(Some(captured)) => {
                                let metadata = crate::cache::slice::SlicePublishMetadata {
                                    archive_format: Some(&captured.archive_format),
                                    ..metadata
                                };
                                match crate::cache::slice::publish_stored_slice_prehashed(
                                    backend.as_ref(),
                                    &tag,
                                    &captured.digest,
                                    captured.bytes,
                                    metadata,
                                ) {
                                    Ok(save_ms) => {
                                        cache_save_ms_total += save_ms;
                                        ui::print_detail(
                                            "step slice saved from container into BoringCache",
                                        );
                                    }
                                    Err(err) => {
                                        ui::print_detail(format!(
                                            "step slice manifest publish failed tag={tag}: {err:#}"
                                        ));
                                    }
                                }
                            }
                            Ok(None) => {
                                ui::print_detail(format!(
                                    "step slice capture empty step {} ({}) tag={tag}",
                                    step_ordinal + 1,
                                    step.name.as_deref().unwrap_or("unnamed"),
                                ));
                            }
                            Err(err) => {
                                ui::print_detail(format!(
                                    "step slice in-container BoringCache save failed tag={tag}: {err:#}"
                                ));
                            }
                        }
                    } else if local_cache_mount.is_some() {
                        match Self::capture_local_slice_to_container(
                            &binary,
                            &container_name,
                            &excluded_targets,
                        ) {
                            Ok(Some(captured)) => {
                                let metadata = crate::cache::slice::SlicePublishMetadata {
                                    archive_format: Some(&captured.archive_format),
                                    ..metadata
                                };
                                match crate::cache::slice::publish_stored_slice_prehashed(
                                    backend.as_ref(),
                                    &tag,
                                    &captured.digest,
                                    captured.bytes,
                                    metadata,
                                ) {
                                    Ok(save_ms) => {
                                        cache_save_ms_total += save_ms;
                                        ui::print_detail(
                                            "step slice saved from container into local cache",
                                        );
                                    }
                                    Err(err) => {
                                        ui::print_detail(format!(
                                            "step slice save failed tag={tag}: {err:#}"
                                        ));
                                    }
                                }
                            }
                            Ok(None) => {
                                ui::print_detail(format!(
                                    "step slice capture empty step {} ({}) tag={tag}",
                                    step_ordinal + 1,
                                    step.name.as_deref().unwrap_or("unnamed"),
                                ));
                            }
                            Err(err) => {
                                ui::print_detail(format!(
                                    "step slice in-container local save failed tag={tag}: {err:#}"
                                ));
                            }
                        }
                    } else {
                        match Self::capture_slice_from_container(
                            &binary,
                            &container_name,
                            &excluded_targets,
                        ) {
                            Ok(Some(captured)) => {
                                match crate::cache::slice::publish_archived_slice_prehashed(
                                    backend.as_ref(),
                                    &tag,
                                    captured.temp.path(),
                                    &captured.digest,
                                    captured.bytes,
                                    metadata,
                                ) {
                                    Ok(save_ms) => {
                                        cache_save_ms_total += save_ms;
                                        ui::print_detail("step slice saved from container");
                                    }
                                    Err(err) => {
                                        ui::print_detail(format!(
                                            "step slice save failed tag={tag}: {err:#}"
                                        ));
                                    }
                                }
                            }
                            Ok(None) => {
                                ui::print_detail(format!(
                                    "step slice capture empty step {} ({}) tag={tag}",
                                    step_ordinal + 1,
                                    step.name.as_deref().unwrap_or("unnamed"),
                                ));
                            }
                            Err(err) => {
                                ui::print_detail(format!(
                                    "step slice capture failed tag={tag}: {err:#}"
                                ));
                            }
                        }
                    }
                }

                operations.push(OperationTiming {
                    label,
                    ms: op_ms,
                    cached: false,
                });
                if let Some(step_state_key) = step_state_key.take() {
                    current_state_key = step_state_key;
                }
                rootfs_state_dirty = false;
                step_ordinal += 1;
                idx += 1;
                continue;
            }

            let batch_start = idx;
            while idx < pipeline.operations.len() && is_helper_operation(&pipeline.operations[idx])
            {
                idx += 1;
            }
            let batch = &pipeline.operations[batch_start..idx];
            let label = helper_batch_label(batch, batch_start);
            ui::print_step(batch_start + 1, pipeline.operations.len(), &label, false);
            let op_started = Instant::now();
            let helper = guest_helper
                .as_mut()
                .expect("guest helper must exist for filesystem operation batches");
            self.exec_helper_batch(&binary, &container_name, helper, batch)?;
            rootfs_state_dirty = true;
            let op_ms = op_started.elapsed().as_millis();
            operation_ms_total += op_ms;
            operations.push(OperationTiming {
                label,
                ms: op_ms,
                cached: false,
            });
        }

        let export_started = Instant::now();
        let mut unpack_ms = 0u128;
        let export_path = if let Some((format, path)) = resolved_export {
            match format {
                ExportFormat::Tar => Some(export_archive_from_container(
                    ArchiveExportContext {
                        binary: &binary,
                        container_name: &container_name,
                        pipeline,
                        platform: &pipeline.platform,
                        guard: &mut guard,
                    },
                    ArchiveExportKind::Tar,
                    &path,
                )?),
                ExportFormat::TarZst => Some(export_archive_from_container(
                    ArchiveExportContext {
                        binary: &binary,
                        container_name: &container_name,
                        pipeline,
                        platform: &pipeline.platform,
                        guard: &mut guard,
                    },
                    ArchiveExportKind::TarZst,
                    &path,
                )?),
                ExportFormat::Oci => {
                    ui::print_status("stopping container for image export");
                    if stop_container_for_export(&binary, &container_name, "image export") {
                        guard.disarm();
                    }

                    ui::print_status(format!("exporting OCI image layout {}", path.display()));
                    Some(export_container_as_oci_layout(
                        &binary,
                        &container_name,
                        &pipeline.platform,
                        &path,
                    )?)
                }
                ExportFormat::Docker => {
                    ui::print_status("stopping container for image export");
                    if stop_container_for_export(&binary, &container_name, "image export") {
                        guard.disarm();
                    }

                    let work_dir = tempfile::Builder::new()
                        .prefix("boringbuilder-macos-docker-oci-")
                        .tempdir()
                        .context("failed to create temporary macOS Docker export directory")?;
                    let oci_dir = work_dir.path().join("oci");

                    ui::print_status(format!("exporting OCI image layout {}", oci_dir.display()));
                    export_container_as_oci_layout(
                        &binary,
                        &container_name,
                        &pipeline.platform,
                        &oci_dir,
                    )?;
                    ui::print_status(format!("exporting Docker image {}", path.display()));
                    Some(export_oci_layout_as_docker(&oci_dir, &path)?)
                }
            }
        } else {
            None
        };
        let export_ms = export_started.elapsed().as_millis();

        // For multi-stage builds, export the container's rootfs so dependent
        // stages can mount it at /boringbuilder-stages/<name>/.
        let (rootfs_dir, _keep_alive): (Option<PathBuf>, Option<Arc<dyn Send + Sync>>) = if options
            .keep_rootfs
        {
            let snapshot_root = snapshot_dir
                .as_ref()
                .expect("stage snapshot dir must exist")
                .path()
                .to_path_buf();
            if should_export_full_rootfs(pipeline) {
                ui::print_status("stopping container for full stage rootfs export");
                if let Err(err) = stop_container(&binary, &container_name) {
                    eprintln!(
                        "warning: failed to stop container before stage export: {err:#}; continuing with live export"
                    );
                }
                guard.disarm();

                let t = Instant::now();
                export_stage_rootfs(&binary, &container_name, &pipeline.platform, &snapshot_root)?;
                unpack_ms += t.elapsed().as_millis();
            } else {
                let helper = guest_helper
                    .as_mut()
                    .expect("guest helper must exist for stage snapshots");
                let t = Instant::now();
                snapshot_stage_paths(&binary, &container_name, helper, &pipeline.outputs)?;
                unpack_ms += t.elapsed().as_millis();

                ui::print_status("stopping container after stage snapshot");
                if stop_container_for_export(&binary, &container_name, "stage snapshot cleanup") {
                    guard.disarm();
                }
            }

            guard.disarm();
            let _ = Command::new(&binary)
                .args(["rm", &container_name].map(String::from))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let work = snapshot_dir.expect("stage snapshot dir must exist");
            (Some(snapshot_root), Some(Arc::new(work) as _))
        } else {
            (None, None)
        };

        if options.keep {
            guard.disarm();
            ui::print_status(format!("container kept running as {}", container_name));
        }

        let export_bytes = export_path
            .as_deref()
            .map(path_size)
            .transpose()?
            .unwrap_or(0);
        let stage_rootfs_bytes = rootfs_dir
            .as_deref()
            .map(path_size)
            .transpose()?
            .unwrap_or(0);

        let timings = RunTimings {
            pull_ms,
            unpack_ms,
            prepare_ms,
            operation_ms: operation_ms_total,
            cache_restore_ms: cache_restore_ms_total,
            cache_save_ms: cache_save_ms_total,
            export_ms,
            total_ms: run_start.elapsed().as_millis(),
            export_bytes,
            stage_rootfs_bytes,
            operations,
            ..RunTimings::default()
        };

        if show_timings {
            print_timing_summary(&timings);
        }

        Ok(RunSummary {
            container_name: options.keep.then_some(container_name),
            export_path,
            rootfs_dir,
            _keep_alive,
            timings,
        })
    }
}

impl MacosContainerBackend {
    fn binary_path(&self) -> Result<PathBuf> {
        resolve_container_cli(
            self.container_binary
                .clone()
                .or_else(|| std::env::var_os("BORINGBUILDER_CONTAINER_BIN").map(PathBuf::from)),
            true,
        )
        .map(|resolved| resolved.path)
    }

    fn exec_step(
        &self,
        binary: &Path,
        container_name: &str,
        pipeline: &Pipeline,
        step: &Step,
        cache_config: &CacheStoreConfig,
        run_mount_root: Option<&Path>,
    ) -> Result<()> {
        let workdir = step
            .workdir
            .as_deref()
            .unwrap_or(&pipeline.workdir)
            .to_string();
        let mounts = self.prepare_run_mounts(
            binary,
            container_name,
            pipeline,
            step,
            cache_config,
            run_mount_root,
        )?;

        let mut args = vec!["exec".to_string(), "--workdir".to_string(), workdir];
        append_env_args(&mut args, &step.env);
        for mount in &mounts {
            append_env_args(&mut args, &mount.env);
        }
        args.push(container_name.to_string());
        if let Some(argv) = &step.run_exec {
            args.extend(argv.iter().cloned());
        } else {
            let shell = parse_shell(step.shell.as_deref().unwrap_or("/bin/sh"))?;
            args.extend(shell);
            args.push("-c".to_string());
            args.push(step.run.clone());
        }

        let mut mounted = 0usize;
        let mount_result = (|| -> Result<()> {
            for mount in &mounts {
                self.mount_run_mount(binary, container_name, mount)?;
                mounted += 1;
            }
            Ok(())
        })();
        if let Err(error) = mount_result {
            let _ = self.cleanup_run_mounts(
                binary,
                container_name,
                &mounts[..mounted],
                false,
                cache_config,
            );
            return Err(error);
        }

        let result = run_streaming(binary, &args).with_context(|| {
            format!(
                "step '{}' failed",
                step.name.as_deref().unwrap_or("<unnamed>")
            )
        });
        let cleanup = self.cleanup_run_mounts(
            binary,
            container_name,
            &mounts[..mounted],
            result.is_ok(),
            cache_config,
        );
        result?;
        cleanup
    }

    fn prepare_run_mounts(
        &self,
        _binary: &Path,
        _container_name: &str,
        pipeline: &Pipeline,
        step: &Step,
        cache_config: &CacheStoreConfig,
        run_mount_root: Option<&Path>,
    ) -> Result<Vec<ActiveRunMount>> {
        if step.run_mounts.is_empty() {
            return Ok(Vec::new());
        }
        let run_mount_root = run_mount_root
            .ok_or_else(|| anyhow!("missing macOS container RUN mount staging directory"))?;
        let mut mounts = Vec::new();
        for (index, mount) in step.run_mounts.iter().enumerate() {
            let slot = format!(
                "mount-{}-{}",
                crate::cache::cache_tag(step.name.as_deref().unwrap_or("step")),
                index
            );
            match mount {
                StepRunMount::Cache {
                    target,
                    id,
                    key,
                    restore_from,
                    readonly,
                    sharing,
                } => mounts.push(self.prepare_cache_run_mount(
                    pipeline,
                    target,
                    id,
                    key.as_deref(),
                    restore_from,
                    *readonly,
                    *sharing,
                    cache_config,
                    run_mount_root,
                    &slot,
                )?),
                StepRunMount::Bind {
                    target,
                    source,
                    readonly,
                } => mounts.push(self.prepare_bind_run_mount(
                    pipeline,
                    target,
                    source,
                    *readonly,
                    run_mount_root,
                    &slot,
                )?),
                StepRunMount::Tmpfs { target, size } => {
                    mounts.push(self.prepare_tmpfs_run_mount(target, size.as_deref())?)
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
                    if let Some(mount) = self.prepare_secret_run_mount(
                        target,
                        id,
                        env.as_deref(),
                        *required,
                        *mode,
                        *uid,
                        *gid,
                        run_mount_root,
                        &slot,
                    )? {
                        mounts.push(mount);
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
                    if let Some(mount) =
                        self.prepare_ssh_run_mount(target, id, *required, *mode, *uid, *gid)?
                    {
                        mounts.push(mount);
                    }
                }
            }
        }
        mounts.sort_by_key(|mount| mount.guest_target.matches('/').count());
        Ok(mounts)
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_cache_run_mount(
        &self,
        pipeline: &Pipeline,
        target: &str,
        id: &str,
        key: Option<&str>,
        restore_from: &[String],
        readonly: bool,
        sharing: CacheMode,
        cache_config: &CacheStoreConfig,
        run_mount_root: &Path,
        slot: &str,
    ) -> Result<ActiveRunMount> {
        let host_source = run_mount_root.join(slot).join("source");
        fs::create_dir_all(&host_source)
            .with_context(|| format!("failed to create {}", host_source.display()))?;
        let entry = CacheMount {
            id: format!("dockerfile-run-cache-{}", crate::cache::cache_tag(id)),
            path: target.to_string(),
            key: key
                .map(str::to_string)
                .unwrap_or_else(|| run_mount_cache_key(&pipeline.platform, id)),
            restore_from: restore_from.to_vec(),
            mode: sharing,
        };
        let lock = if sharing == CacheMode::Locked {
            Some(open_cache_store(cache_config)?.lock(&entry)?)
        } else {
            None
        };
        open_cache_store(cache_config)?.restore(&entry, &host_source)?;
        Ok(ActiveRunMount {
            guest_target: target.to_string(),
            source: ActiveRunMountSource::Bind {
                guest_source: format!("{RUN_MOUNT_GUEST_DIR}/{slot}/source"),
                source_is_dir: true,
            },
            readonly,
            host_source: Some(host_source),
            cache_entry: Some(entry),
            cache_save: !readonly,
            env: BTreeMap::new(),
            chown: None,
            chmod: None,
            _lock: lock,
        })
    }

    fn prepare_bind_run_mount(
        &self,
        pipeline: &Pipeline,
        target: &str,
        source: &StepRunBindSource,
        readonly: bool,
        run_mount_root: &Path,
        slot: &str,
    ) -> Result<ActiveRunMount> {
        match source {
            StepRunBindSource::Context { path } => {
                let context = pipeline.docker_context.as_ref().ok_or_else(|| {
                    anyhow!("missing Docker build context for RUN --mount bind source")
                })?;
                let host_source = run_mount_root.join(slot).join("source");
                materialize_context_source(context, path, &host_source)?;
                let source_is_dir = fs::symlink_metadata(&host_source)
                    .with_context(|| format!("failed to stat {}", host_source.display()))?
                    .is_dir();
                Ok(ActiveRunMount {
                    guest_target: target.to_string(),
                    source: ActiveRunMountSource::Bind {
                        guest_source: format!("{RUN_MOUNT_GUEST_DIR}/{slot}/source"),
                        source_is_dir,
                    },
                    readonly,
                    host_source: Some(host_source),
                    cache_entry: None,
                    cache_save: false,
                    env: BTreeMap::new(),
                    chown: None,
                    chmod: None,
                    _lock: None,
                })
            }
            StepRunBindSource::Stage { stage, path } => {
                let host_stage_source = resolve_stage_run_mount_source(pipeline, stage, path)?;
                let source_is_dir = fs::symlink_metadata(&host_stage_source)
                    .with_context(|| format!("failed to stat {}", host_stage_source.display()))?
                    .is_dir();
                if readonly {
                    return Ok(ActiveRunMount {
                        guest_target: target.to_string(),
                        source: ActiveRunMountSource::Bind {
                            guest_source: guest_stage_run_mount_source(stage, path),
                            source_is_dir,
                        },
                        readonly,
                        host_source: None,
                        cache_entry: None,
                        cache_save: false,
                        env: BTreeMap::new(),
                        chown: None,
                        chmod: None,
                        _lock: None,
                    });
                }

                let host_source = run_mount_root.join(slot).join("source");
                crate::util::fs::copy_path(&host_stage_source, &host_source).with_context(
                    || {
                        format!(
                            "failed to materialize writable RUN bind source {}",
                            host_stage_source.display()
                        )
                    },
                )?;
                Ok(ActiveRunMount {
                    guest_target: target.to_string(),
                    source: ActiveRunMountSource::Bind {
                        guest_source: format!("{RUN_MOUNT_GUEST_DIR}/{slot}/source"),
                        source_is_dir,
                    },
                    readonly,
                    host_source: Some(host_source),
                    cache_entry: None,
                    cache_save: false,
                    env: BTreeMap::new(),
                    chown: None,
                    chmod: None,
                    _lock: None,
                })
            }
        }
    }

    fn prepare_tmpfs_run_mount(&self, target: &str, size: Option<&str>) -> Result<ActiveRunMount> {
        Ok(ActiveRunMount {
            guest_target: target.to_string(),
            source: ActiveRunMountSource::Tmpfs {
                size: size.map(str::to_string),
            },
            readonly: false,
            host_source: None,
            cache_entry: None,
            cache_save: false,
            env: BTreeMap::new(),
            chown: None,
            chmod: None,
            _lock: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_secret_run_mount(
        &self,
        target: &str,
        id: &str,
        env_name: Option<&str>,
        required: bool,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        run_mount_root: &Path,
        slot: &str,
    ) -> Result<Option<ActiveRunMount>> {
        let Some(secret) = resolve_secret_run_mount(id, env_name)? else {
            if required {
                let explicit_source = env_name
                    .map(|name| format!("env var {name}"))
                    .unwrap_or_else(|| "an explicit env=... source".to_string());
                let fallback_env = format!("BORINGBUILDER_SECRET_{}", mount_id_env_suffix(id));
                bail!(
                    "RUN --mount=type=secret,id={} is required but no secret source is available; set {} or {}",
                    id,
                    explicit_source,
                    fallback_env
                );
            }
            return Ok(None);
        };

        let host_source = run_mount_root.join(slot).join("source");
        if let Some(parent) = host_source.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(&host_source, &secret.bytes)
            .with_context(|| format!("failed to write {}", host_source.display()))?;

        let mut env = BTreeMap::new();
        if let Some(env_name) = env_name {
            env.insert(env_name.to_string(), secret.env_value);
        }

        Ok(Some(ActiveRunMount {
            guest_target: target.to_string(),
            source: ActiveRunMountSource::Bind {
                guest_source: format!("{RUN_MOUNT_GUEST_DIR}/{slot}/source"),
                source_is_dir: false,
            },
            readonly: true,
            host_source: Some(host_source),
            cache_entry: None,
            cache_save: false,
            env,
            chown: Some(format!("{}:{}", uid.unwrap_or(0), gid.unwrap_or(0))),
            chmod: Some(format!("{:o}", mode.unwrap_or(0o400))),
            _lock: None,
        }))
    }

    fn prepare_ssh_run_mount(
        &self,
        target: &str,
        id: &str,
        required: bool,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
    ) -> Result<Option<ActiveRunMount>> {
        let Some(host_source) = resolve_ssh_run_mount_source(id)? else {
            if required {
                bail!(
                    "RUN --mount=type=ssh,id={} is required but no SSH agent socket is available; set BORINGBUILDER_SSH_AUTH_SOCK_{} or SSH_AUTH_SOCK",
                    id,
                    mount_id_env_suffix(id)
                );
            }
            return Ok(None);
        };
        let metadata = fs::metadata(&host_source).with_context(|| {
            format!("failed to stat SSH agent socket {}", host_source.display())
        })?;
        ensure!(
            !metadata.is_dir(),
            "RUN --mount=type=ssh,id={} requires a socket-like file, got directory {}",
            id,
            host_source.display()
        );

        let mut env = BTreeMap::new();
        env.insert("SSH_AUTH_SOCK".to_string(), target.to_string());
        Ok(Some(ActiveRunMount {
            guest_target: target.to_string(),
            source: ActiveRunMountSource::Bind {
                guest_source: guest_ssh_mount_source(&host_source),
                source_is_dir: false,
            },
            readonly: true,
            host_source: None,
            cache_entry: None,
            cache_save: false,
            env,
            chown: None,
            chmod: None,
            _lock: None,
        }))
    }

    fn mount_run_mount(
        &self,
        binary: &Path,
        container_name: &str,
        mount: &ActiveRunMount,
    ) -> Result<()> {
        let mut script = String::from("set -eu\n");
        match &mount.source {
            ActiveRunMountSource::Bind {
                guest_source,
                source_is_dir,
            } => {
                if *source_is_dir {
                    script.push_str("mkdir -p ");
                    script.push_str(&shell_words::quote(&mount.guest_target));
                    script.push('\n');
                } else {
                    let parent = Path::new(&mount.guest_target)
                        .parent()
                        .unwrap_or_else(|| Path::new("/"))
                        .to_string_lossy()
                        .into_owned();
                    script.push_str("mkdir -p ");
                    script.push_str(&shell_words::quote(&parent));
                    script.push('\n');
                    script.push_str("touch ");
                    script.push_str(&shell_words::quote(&mount.guest_target));
                    script.push('\n');
                }
                script.push_str("mount --bind ");
                script.push_str(&shell_words::quote(guest_source));
                script.push(' ');
                script.push_str(&shell_words::quote(&mount.guest_target));
                if let Some(chown) = &mount.chown {
                    script.push('\n');
                    script.push_str("chown ");
                    script.push_str(&shell_words::quote(chown));
                    script.push(' ');
                    script.push_str(&shell_words::quote(&mount.guest_target));
                }
                if let Some(chmod) = &mount.chmod {
                    script.push('\n');
                    script.push_str("chmod ");
                    script.push_str(&shell_words::quote(chmod));
                    script.push(' ');
                    script.push_str(&shell_words::quote(&mount.guest_target));
                }
                if mount.readonly {
                    script.push('\n');
                    script.push_str("mount -o remount,bind,ro ");
                    script.push_str(&shell_words::quote(&mount.guest_target));
                }
            }
            ActiveRunMountSource::Tmpfs { size } => {
                script.push_str("mkdir -p ");
                script.push_str(&shell_words::quote(&mount.guest_target));
                script.push('\n');
                script.push_str("mount -t tmpfs -o ");
                let mut options = vec!["nosuid".to_string()];
                if let Some(size) = size {
                    options.push(format!("size={size}"));
                }
                script.push_str(&shell_words::quote(&options.join(",")));
                script.push_str(" tmpfs ");
                script.push_str(&shell_words::quote(&mount.guest_target));
            }
        }
        run_checked(
            binary,
            &[
                "exec".to_string(),
                container_name.to_string(),
                "/bin/sh".to_string(),
                "-c".to_string(),
                script,
            ],
        )
        .with_context(|| format!("failed to mount RUN source at {}", mount.guest_target))
    }

    fn cleanup_run_mounts(
        &self,
        binary: &Path,
        container_name: &str,
        mounts: &[ActiveRunMount],
        save_cache: bool,
        cache_config: &CacheStoreConfig,
    ) -> Result<()> {
        let mut first_error: Option<anyhow::Error> = None;
        for mount in mounts.iter().rev() {
            let unmount = run_checked(
                binary,
                &[
                    "exec".to_string(),
                    container_name.to_string(),
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    format!("umount {}", shell_words::quote(&mount.guest_target)),
                ],
            )
            .with_context(|| format!("failed to unmount {}", mount.guest_target));
            if let Err(error) = unmount
                && first_error.is_none()
            {
                first_error = Some(error);
            }

            if save_cache
                && mount.cache_save
                && let (Some(entry), Some(host_source)) = (&mount.cache_entry, &mount.host_source)
            {
                let save = open_cache_store(cache_config)?
                    .save(entry, host_source)
                    .with_context(|| format!("failed to save RUN cache mount {}", entry.id));
                if let Err(error) = save
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
            }
        }

        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    fn ensure_workdirs(
        &self,
        binary: &Path,
        container_name: &str,
        pipeline: &Pipeline,
    ) -> Result<()> {
        let mut workdirs = BTreeSet::new();
        workdirs.insert(pipeline.workdir.clone());
        for operation in &pipeline.operations {
            if let Operation::Exec(step) = operation
                && let Some(workdir) = &step.workdir
            {
                workdirs.insert(workdir.clone());
            }
        }

        let workdirs: Vec<String> = workdirs.into_iter().filter(|path| path != "/").collect();
        if workdirs.is_empty() {
            return Ok(());
        }

        let mut script = "mkdir -p".to_string();
        for workdir in &workdirs {
            script.push(' ');
            script.push_str(&shell_words::quote(workdir));
        }

        run_checked(
            binary,
            &[
                "exec".to_string(),
                "--workdir".to_string(),
                "/".to_string(),
                container_name.to_string(),
                "/bin/sh".to_string(),
                "-c".to_string(),
                script,
            ],
        )
        .context("failed to prepare container workdirs")
    }

    fn exec_operation(
        &self,
        binary: &Path,
        container_name: &str,
        pipeline: &Pipeline,
        operation: &Operation,
        cache_config: &CacheStoreConfig,
        run_mount_root: Option<&Path>,
    ) -> Result<()> {
        match operation {
            Operation::Exec(step) => self.exec_step(
                binary,
                container_name,
                pipeline,
                step,
                cache_config,
                run_mount_root,
            ),
            Operation::CopyFromContext(_)
            | Operation::CopyFromStage(_)
            | Operation::AddRemote(_) => {
                bail!(
                    "filesystem operations must execute through the macOS guest helper batch path"
                )
            }
        }
    }

    fn exec_helper_batch(
        &self,
        binary: &Path,
        container_name: &str,
        helper: &mut GuestHelperHost,
        operations: &[Operation],
    ) -> Result<()> {
        let batch_script = helper.write_batch(operations)?;
        run_streaming(
            binary,
            &[
                "exec".to_string(),
                container_name.to_string(),
                "/bin/sh".to_string(),
                helper.guest_helper_path(),
                "batch".to_string(),
                batch_script,
            ],
        )
        .context("guest helper batch failed")
    }

    /// Touch a marker file inside the VM for delta detection.
    fn touch_slice_marker(binary: &Path, container_name: &str) -> Result<()> {
        let status = Command::new(binary)
            .args([
                "exec",
                container_name,
                "/bin/sh",
                "-c",
                touch_slice_marker_script(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("failed to touch slice marker in container")?;
        ensure!(
            status.success(),
            "failed to touch slice marker in container"
        );
        Ok(())
    }

    /// Stream files changed since the marker out of the VM as a tar archive.
    /// Returns the archive as a temp file, or None if no changes detected.
    fn capture_slice_from_container(
        binary: &Path,
        container_name: &str,
        excluded_paths: &[String],
    ) -> Result<Option<CapturedSliceArchive>> {
        // Find files whose inode metadata changed since the marker, excluding
        // pseudo-filesystems and the same package-manager scratch paths
        // ignored by generic step slices. ctime is required here because
        // package installers often preserve file mtimes when extracting.
        let script = build_capture_slice_script(excluded_paths);
        let mut child = Command::new(binary)
            .args(["exec", container_name, "/bin/sh", "-c", &script])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start slice capture from container")?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to capture slice stdout"))?;

        // Write the plain tar to a temp file, then compress.
        let temp =
            tempfile::NamedTempFile::new().context("failed to create temp file for slice")?;
        let out_file = crate::util::hashing::HashingWriter::new(fs::File::create(temp.path())?);
        // Favor faster local wall time on the macOS container path; these
        // slices can be very large after toolchain/bootstrap steps.
        let mut encoder = zstd::Encoder::new(out_file, 1)?;
        let workers = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
            .min(4) as u32;
        encoder
            .multithread(workers)
            .context("failed to enable multithreaded slice compression")?;
        std::io::copy(&mut std::io::BufReader::new(stdout), &mut encoder)?;
        let hashing_file = encoder.finish()?;
        let (_, hasher, bytes) = hashing_file.into_parts();
        let digest = hex::encode(hasher.finalize());

        let _ = child.wait();

        // A zstd frame with an empty tar is ~22 bytes (magic + header + empty block).
        const EMPTY_ZSTD_TAR_MAX: u64 = 22;
        if bytes <= EMPTY_ZSTD_TAR_MAX {
            return Ok(None);
        }

        Ok(Some(CapturedSliceArchive {
            temp,
            digest,
            bytes,
        }))
    }

    /// Store a changed step slice from inside the container into the
    /// host-mounted local cache. The host only publishes the small ref manifest
    /// after this returns.
    fn capture_local_slice_to_container(
        binary: &Path,
        container_name: &str,
        excluded_paths: &[String],
    ) -> Result<Option<CapturedStoredSlice>> {
        let script = build_capture_local_slice_script(excluded_paths)?;
        let output = run_capture(
            binary,
            &[
                "exec".to_string(),
                container_name.to_string(),
                "/bin/sh".to_string(),
                "-c".to_string(),
                script,
            ],
        )
        .context("failed to start local slice capture in container")?;
        ensure!(
            output.status.success(),
            "local slice capture in container failed: {}",
            capture_command_failure(&output)
        );
        parse_local_slice_capture_output(&output.stdout)
    }

    /// Compress and upload a changed step slice to BoringCache from inside the
    /// Linux container. The host only publishes the step manifest afterward.
    fn capture_boringcache_slice_to_container(
        binary: &Path,
        container_name: &str,
        tools: &ContainerBoringCacheTools,
        excluded_paths: &[String],
        allow_zstd: bool,
    ) -> Result<Option<CapturedStoredSlice>> {
        let script = build_capture_boringcache_slice_script(tools, excluded_paths, allow_zstd)?;
        let output = run_capture(
            binary,
            &[
                "exec".to_string(),
                container_name.to_string(),
                "/bin/sh".to_string(),
                "-c".to_string(),
                script,
            ],
        )
        .context("failed to start BoringCache slice capture in container")?;
        ensure!(
            output.status.success(),
            "BoringCache slice capture in container failed: {}",
            capture_command_failure(&output)
        );
        parse_local_slice_capture_output(&output.stdout)
    }

    fn container_zstd_available(binary: &Path, container_name: &str) -> bool {
        let script = format!(
            r#"set -eu
PATH={cache_tools_bin}:$PATH
export PATH
{zstd_helpers}
bb_have_zstd
"#,
            cache_tools_bin = shell_words::quote(&format!("{CACHE_TOOLS_GUEST_DIR}/bin")),
            zstd_helpers = zstd_shell_helpers(),
        );
        run_capture(
            binary,
            &[
                "exec".to_string(),
                container_name.to_string(),
                "/bin/sh".to_string(),
                "-c".to_string(),
                script,
            ],
        )
        .map(|output| output.status.success())
        .unwrap_or(false)
    }

    /// Expand a slice archive into the VM by streaming tar through container exec.
    fn restore_slice_to_container(
        binary: &Path,
        container_name: &str,
        archive: &Path,
    ) -> Result<()> {
        let file = fs::File::open(archive)
            .with_context(|| format!("failed to open slice {}", archive.display()))?;
        let decoder = zstd::Decoder::new(file)
            .with_context(|| format!("failed to decode slice {}", archive.display()))?;

        let mut child = Command::new(binary)
            .args([
                "exec",
                "--interactive",
                container_name,
                "/bin/sh",
                "-c",
                "tar -xf - -C / 2>/dev/null || true",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start slice restore into container")?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to capture slice stdin"))?;
        let mut writer = std::io::BufWriter::new(stdin);
        std::io::copy(&mut std::io::BufReader::new(decoder), &mut writer)?;
        drop(writer);

        let status = child.wait().context("failed to wait for slice restore")?;
        ensure!(status.success(), "slice restore into container failed");
        Ok(())
    }

    /// Expand a local-cache slice inside the container without fetching or
    /// decompressing the blob on the host.
    fn restore_local_slice_to_container(
        binary: &Path,
        container_name: &str,
        digest: &str,
        archive_format: &str,
    ) -> Result<()> {
        let archive = local_cache_blob_guest_path(digest)?;
        let script = format!(
            r#"set -eu
PATH={cache_tools_bin}:$PATH
export PATH
{zstd_helpers}
archive={archive}
if [ ! -f "$archive" ]; then
  echo "local cache blob not found: $archive" >&2
  exit 66
fi
format={archive_format}
case "$format" in
  tar.zst)
    if ! bb_have_zstd; then
      echo "local cache slice restore requires zstd for tar.zst archive" >&2
      exit 127
    fi
    bb_zstd -dc -- "$archive" | tar -xf - -C / 2>/dev/null
    ;;
  tar)
    tar -xf "$archive" -C / 2>/dev/null
    ;;
  *)
    echo "unsupported local cache slice archive format: $format" >&2
    exit 65
    ;;
esac
"#,
            cache_tools_bin = shell_words::quote(&format!("{CACHE_TOOLS_GUEST_DIR}/bin")),
            zstd_helpers = zstd_shell_helpers(),
            archive = shell_words::quote(&archive),
            archive_format = shell_words::quote(archive_format),
        );
        run_checked(
            binary,
            &[
                "exec".to_string(),
                container_name.to_string(),
                "/bin/sh".to_string(),
                "-c".to_string(),
                script,
            ],
        )
        .context("failed to restore local slice inside container")
    }

    /// Restore a BoringCache step slice inside the Linux container without
    /// downloading the blob to the macOS host.
    fn restore_boringcache_slice_to_container(
        binary: &Path,
        container_name: &str,
        tools: &ContainerBoringCacheTools,
        digest: &str,
        archive_format: &str,
    ) -> Result<()> {
        validate_cache_digest(digest)?;
        let tag = cache_blob_tag(digest)?;
        let script = build_restore_boringcache_slice_script(tools, &tag, archive_format)?;
        run_checked(
            binary,
            &[
                "exec".to_string(),
                container_name.to_string(),
                "/bin/sh".to_string(),
                "-c".to_string(),
                script,
            ],
        )
        .context("failed to restore BoringCache slice inside container")
    }

    fn ensure_step_slice_restore_headroom(archive_bytes: u64) -> Result<()> {
        let temp_root = std::env::temp_dir();
        let Ok(available) = fs2::available_space(&temp_root) else {
            return Ok(());
        };
        let required = required_step_slice_restore_free_bytes(archive_bytes);
        ensure!(
            available >= required,
            "not enough local temporary disk space to restore cached step slice: {} available under {}, need at least {} for a {} slice archive; free disk space, set TMPDIR to a larger volume, rerun with --no-cache, or set build_cache: false for this step",
            ui::format_bytes(available),
            temp_root.display(),
            ui::format_bytes(required),
            ui::format_bytes(archive_bytes)
        );
        Ok(())
    }
}

fn touch_slice_marker_script() -> &'static str {
    r#"stamp="$(date -u -d @$(( $(date +%s) - 2 )) +%Y%m%d%H%M.%S)"
rm -f /.boringbuilder-slice-marker
touch -t "$stamp" /.boringbuilder-slice-marker"#
}

fn required_step_slice_restore_free_bytes(archive_bytes: u64) -> u64 {
    STEP_SLICE_RESTORE_MIN_FREE_BYTES.max(
        archive_bytes
            .saturating_mul(STEP_SLICE_RESTORE_ARCHIVE_EXPANSION_FACTOR)
            .saturating_add(STEP_SLICE_RESTORE_FREE_BUFFER_BYTES),
    )
}

fn build_capture_slice_script(excluded_paths: &[String]) -> String {
    let file_list_script = build_capture_file_list_script(excluded_paths);
    format!("({file_list_script} || true) | tar --no-recursion -cf - -T - 2>/dev/null || true")
}

fn build_capture_file_list_script(excluded_paths: &[String]) -> String {
    let ctime = build_capture_find_command("-cnewer", excluded_paths);
    let mtime = build_capture_find_command("-newer", excluded_paths);
    format!(
        "if find / -maxdepth 0 -cnewer /.boringbuilder-slice-marker >/dev/null 2>&1; then\n  {ctime}\nelse\n  {mtime}\nfi"
    )
}

fn build_capture_find_command(predicate: &str, excluded_paths: &[String]) -> String {
    let mut script = format!("find / -mindepth 1 {predicate} /.boringbuilder-slice-marker ");
    for path in [
        "/proc",
        "/sys",
        "/dev",
        "/tmp",
        "/run",
        LOCAL_CACHE_GUEST_DIR,
        CACHE_TOOLS_GUEST_DIR,
        RUN_MOUNT_GUEST_DIR,
        SSH_AGENT_GUEST_DIR,
        helper_mount_guest_dir(),
        helper_snapshot_guest_dir(),
    ] {
        append_find_exclude_path(&mut script, path);
    }
    append_find_exclude_exact_path(&mut script, "/.boringbuilder-slice-marker");
    for prefix in crate::cache::slice::STEP_SLICE_IGNORED_PREFIXES {
        append_find_exclude_path(&mut script, &format!("/{prefix}"));
    }
    for path in excluded_paths {
        append_find_exclude_path(&mut script, path);
    }
    script.push_str("-print 2>/dev/null");
    script
}

fn zstd_shell_helpers() -> &'static str {
    r#"bb_zstd_binary() {
  for candidate in /usr/bin/zstd /bin/zstd /boringbuilder-cache-tools/bin/zstd; do
    if [ -x "$candidate" ] && printf '' | "$candidate" -1 >/dev/null 2>&1; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 127
}
bb_zstd() {
  candidate="$(bb_zstd_binary)" || return 127
  "$candidate" "$@"
}
bb_have_zstd() {
  bb_zstd_binary >/dev/null
}
"#
}

fn build_capture_local_slice_script(excluded_paths: &[String]) -> Result<String> {
    let file_list_script = build_capture_file_list_script(excluded_paths);
    Ok(format!(
        r#"set -eu
bb_sha256_file() {{
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | sed 's/[[:space:]].*//'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | sed 's/[[:space:]].*//'
  else
    echo "slice capture requires sha256sum or shasum inside the container" >&2
    exit 127
  fi
}}
cache_root={cache_root}
tmp_dir="$cache_root/tmp"
blob_dir="$cache_root/blobs/sha256"
mkdir -p "$tmp_dir" "$blob_dir"
tmp="$(mktemp "$tmp_dir/macos-slice.XXXXXX")"
list="$(mktemp "$tmp_dir/macos-slice-list.XXXXXX")"
trap 'rm -f "$tmp" "$list"' EXIT HUP INT TERM
({file_list_script} || true) > "$list"
if [ ! -s "$list" ]; then
  rm -f "$tmp" "$list"
  trap - EXIT HUP INT TERM
  printf 'empty\n'
  exit 0
fi
archive_format=tar
tar --no-recursion -cf "$tmp" -T "$list"
bytes="$(wc -c < "$tmp" | tr -d '[:space:]')"
digest="$(bb_sha256_file "$tmp")"
case "$digest" in
  ""|*[!0123456789abcdefABCDEF]*)
    echo "invalid slice digest: $digest" >&2
    exit 1
    ;;
esac
blob="$blob_dir/$digest.tar.zst"
if [ ! -f "$blob" ]; then
  mv "$tmp" "$blob"
else
  rm -f "$tmp"
fi
rm -f "$list"
trap - EXIT HUP INT TERM
printf 'stored %s %s %s\n' "$digest" "$bytes" "$archive_format"
"#,
        cache_root = shell_words::quote(LOCAL_CACHE_GUEST_DIR),
        file_list_script = file_list_script,
    ))
}

fn build_capture_boringcache_slice_script(
    tools: &ContainerBoringCacheTools,
    excluded_paths: &[String],
    allow_zstd: bool,
) -> Result<String> {
    let file_list_script = build_capture_file_list_script(excluded_paths);
    let workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(4);
    let env_script = boringcache_container_env_script(tools);
    Ok(format!(
        r#"set -eu
PATH={cache_tools_bin}:$PATH
export PATH
{zstd_helpers}
bb_sha256_file() {{
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | sed 's/[[:space:]].*//'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | sed 's/[[:space:]].*//'
  else
    echo "slice capture requires sha256sum or shasum inside the container" >&2
    exit 127
  fi
}}
{env_script}
tmp_root="$(mktemp -d /tmp/boringbuilder-boringcache-slice.XXXXXX)"
trap 'rm -rf "$tmp_root"' EXIT HUP INT TERM
payload="$tmp_root/payload.bin"
list="$tmp_root/files.list"
({file_list_script} || true) > "$list"
if [ ! -s "$list" ]; then
  rm -rf "$tmp_root"
  trap - EXIT HUP INT TERM
  printf 'empty\n'
  exit 0
fi
archive_format=tar
allow_zstd={allow_zstd}
if [ "$allow_zstd" = "1" ] && bb_have_zstd; then
  tar --no-recursion -cf - -T "$list" | bb_zstd -1 -T{workers} > "$payload"
  archive_format=tar.zst
else
  tar --no-recursion -cf "$payload" -T "$list"
fi
bytes="$(wc -c < "$payload" | tr -d '[:space:]')"
digest="$(bb_sha256_file "$payload")"
case "$digest" in
  ""|*[!0123456789abcdefABCDEF]*)
    echo "invalid slice digest: $digest" >&2
    exit 1
    ;;
esac
tag="cache-blob-$digest"
{boringcache} save {workspace} "$tag:$tmp_root" --fail-on-cache-error --no-platform --no-git
rm -rf "$tmp_root"
trap - EXIT HUP INT TERM
printf 'stored %s %s %s\n' "$digest" "$bytes" "$archive_format"
"#,
        cache_tools_bin = shell_words::quote(&format!("{CACHE_TOOLS_GUEST_DIR}/bin")),
        zstd_helpers = zstd_shell_helpers(),
        env_script = env_script,
        file_list_script = file_list_script,
        allow_zstd = if allow_zstd { "1" } else { "0" },
        workers = workers,
        boringcache = shell_words::quote(&tools.binary_guest_path),
        workspace = shell_words::quote(&tools.workspace),
    ))
}

fn build_restore_boringcache_slice_script(
    tools: &ContainerBoringCacheTools,
    tag: &str,
    archive_format: &str,
) -> Result<String> {
    ensure!(
        !tag.trim().is_empty(),
        "BoringCache blob tag must not be empty"
    );
    let env_script = boringcache_container_env_script(tools);
    Ok(format!(
        r#"set -eu
PATH={cache_tools_bin}:$PATH
export PATH
{zstd_helpers}
{env_script}
tmp_root="$(mktemp -d /tmp/boringbuilder-boringcache-restore.XXXXXX)"
trap 'rm -rf "$tmp_root"' EXIT HUP INT TERM
tag={tag}
tag_path="$tag:$tmp_root"
{boringcache} restore {workspace} "$tag_path" --fail-on-cache-error --no-platform --no-git
payload="$tmp_root/payload.bin"
if [ ! -f "$payload" ]; then
  echo "BoringCache blob payload missing: $payload" >&2
  exit 66
fi
format={archive_format}
case "$format" in
  tar.zst)
    if ! bb_have_zstd; then
      echo "BoringCache slice restore requires zstd for tar.zst archive" >&2
      exit 127
    fi
    bb_zstd -dc -- "$payload" | tar -xf - -C / 2>/dev/null
    ;;
  tar)
    tar -xf "$payload" -C / 2>/dev/null
    ;;
  *)
    echo "unsupported BoringCache slice archive format: $format" >&2
    exit 65
    ;;
esac
rm -rf "$tmp_root"
trap - EXIT HUP INT TERM
"#,
        cache_tools_bin = shell_words::quote(&format!("{CACHE_TOOLS_GUEST_DIR}/bin")),
        zstd_helpers = zstd_shell_helpers(),
        env_script = env_script,
        boringcache = shell_words::quote(&tools.binary_guest_path),
        workspace = shell_words::quote(&tools.workspace),
        tag = shell_words::quote(tag),
        archive_format = shell_words::quote(archive_format),
    ))
}

fn boringcache_container_env_script(tools: &ContainerBoringCacheTools) -> String {
    let mut script =
        String::from("export HOME=/tmp/boringbuilder-boringcache-home\nmkdir -p \"$HOME\"\n");
    if let Some(token_file) = &tools.token_file_guest_path {
        script.push_str("export BORINGCACHE_TOKEN_FILE=");
        script.push_str(&shell_words::quote(token_file));
        script.push('\n');
    }
    if let Some(api_url) = &tools.api_url {
        script.push_str("export BORINGCACHE_API_URL=");
        script.push_str(&shell_words::quote(api_url));
        script.push('\n');
    }
    script
}

fn parse_local_slice_capture_output(output: &str) -> Result<Option<CapturedStoredSlice>> {
    let Some(line) = output.lines().rev().find(|line| !line.trim().is_empty()) else {
        bail!("local slice capture did not report a result");
    };
    let line = line.trim();
    if line == "empty" {
        return Ok(None);
    }

    let mut parts = line.split_whitespace();
    ensure!(
        parts.next() == Some("stored"),
        "invalid local slice capture result: {line}"
    );
    let digest = parts
        .next()
        .ok_or_else(|| anyhow!("local slice capture did not report a digest"))?;
    validate_cache_digest(digest)?;
    let bytes = parts
        .next()
        .ok_or_else(|| anyhow!("local slice capture did not report byte size"))?
        .parse::<u64>()
        .context("local slice capture reported invalid byte size")?;
    let archive_format = parts
        .next()
        .unwrap_or(crate::cache::slice::STEP_SLICE_ARCHIVE_FORMAT_TAR_ZST);
    ensure!(
        matches!(
            archive_format,
            crate::cache::slice::STEP_SLICE_ARCHIVE_FORMAT_TAR
                | crate::cache::slice::STEP_SLICE_ARCHIVE_FORMAT_TAR_ZST
        ),
        "invalid local slice capture archive format: {archive_format}"
    );
    ensure!(
        parts.next().is_none(),
        "invalid local slice capture result: {line}"
    );

    Ok(Some(CapturedStoredSlice {
        digest: digest.to_string(),
        bytes,
        archive_format: archive_format.to_string(),
    }))
}

fn capture_command_failure(output: &crate::util::process::CommandOutput) -> String {
    let status = output
        .status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_string());
    let stderr = output.stderr.trim();
    let stdout = output.stdout.trim();
    if !stderr.is_empty() {
        format!("status {status}: {stderr}")
    } else if !stdout.is_empty() {
        format!("status {status}: {stdout}")
    } else {
        format!("status {status} with no output")
    }
}

fn local_cache_blob_guest_path(digest: &str) -> Result<String> {
    validate_cache_digest(digest)?;
    Ok(format!(
        "{LOCAL_CACHE_GUEST_DIR}/blobs/sha256/{digest}.tar.zst"
    ))
}

fn cache_blob_tag(digest: &str) -> Result<String> {
    validate_cache_digest(digest)?;
    Ok(crate::cache::cache_tag(&format!("cache-blob-{digest}")))
}

fn validate_cache_digest(digest: &str) -> Result<()> {
    ensure!(
        digest.len() == 64 && digest.chars().all(|ch| ch.is_ascii_hexdigit()),
        "invalid local cache digest: {digest}"
    );
    Ok(())
}

fn append_find_exclude_path(script: &mut String, path: &str) {
    append_find_exclude_exact_path(script, path);
    append_find_exclude_exact_path(script, &format!("{path}/*"));
}

fn append_find_exclude_exact_path(script: &mut String, path: &str) {
    script.push_str("! -path ");
    script.push_str(&shell_words::quote(path));
    script.push(' ');
}

fn hash_step_state_field(hasher: &mut Sha256, label: &str, value: impl AsRef<[u8]>) {
    hasher.update(label.as_bytes());
    hasher.update(b"\0");
    hasher.update(value.as_ref());
    hasher.update(b"\0");
}

fn initial_step_state_key(pipeline: &Pipeline) -> String {
    let mut hasher = Sha256::new();
    hash_step_state_field(&mut hasher, "version", "boringbuilder-step-state-v3");
    hash_step_state_field(&mut hasher, "image", pipeline.image.as_bytes());
    hash_step_state_field(&mut hasher, "platform", pipeline.platform.as_bytes());
    hash_step_state_field(&mut hasher, "workdir", pipeline.workdir.as_bytes());
    for (key, value) in &pipeline.env {
        hash_step_state_field(&mut hasher, "pipeline-env-key", key.as_bytes());
        hash_step_state_field(&mut hasher, "pipeline-env-value", value.as_bytes());
    }
    hex::encode(hasher.finalize())
}

fn run_mount_target(mount: &StepRunMount) -> String {
    match mount {
        StepRunMount::Cache { target, .. }
        | StepRunMount::Bind { target, .. }
        | StepRunMount::Tmpfs { target, .. }
        | StepRunMount::Secret { target, .. }
        | StepRunMount::Ssh { target, .. } => target.clone(),
    }
}

fn run_mount_cache_key(platform: &str, id: &str) -> String {
    crate::cache::cache_tag(&format!("dockerfile-run-mount:{platform}:{id}"))
}

struct ResolvedSecretMount {
    bytes: Vec<u8>,
    env_value: String,
}

fn mount_id_env_suffix(id: &str) -> String {
    let normalized = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    if normalized.is_empty() {
        "DEFAULT".to_string()
    } else {
        normalized
    }
}

fn nonempty_env_var(name: &str) -> Option<String> {
    let value = std::env::var_os(name)?;
    if value.is_empty() {
        return None;
    }
    Some(value.to_string_lossy().into_owned())
}

fn resolve_secret_run_mount(
    id: &str,
    env_name: Option<&str>,
) -> Result<Option<ResolvedSecretMount>> {
    if let Some(env_name) = env_name
        && let Some(value) = nonempty_env_var(env_name)
    {
        return Ok(Some(ResolvedSecretMount {
            bytes: value.as_bytes().to_vec(),
            env_value: value,
        }));
    }

    let suffix = mount_id_env_suffix(id);
    if let Some(value) = nonempty_env_var(&format!("BORINGBUILDER_SECRET_{suffix}")) {
        return Ok(Some(ResolvedSecretMount {
            bytes: value.as_bytes().to_vec(),
            env_value: value,
        }));
    }

    if let Some(path) = nonempty_env_var(&format!("BORINGBUILDER_SECRET_FILE_{suffix}")) {
        let bytes = fs::read(&path).with_context(|| {
            format!("failed to read secret file {}", Path::new(&path).display())
        })?;
        return Ok(Some(ResolvedSecretMount {
            env_value: String::from_utf8_lossy(&bytes).into_owned(),
            bytes,
        }));
    }

    Ok(None)
}

fn resolve_ssh_run_mount_source(id: &str) -> Result<Option<PathBuf>> {
    let suffix = mount_id_env_suffix(id);
    for env_name in [
        format!("BORINGBUILDER_SSH_AUTH_SOCK_{suffix}"),
        "BORINGBUILDER_SSH_AUTH_SOCK".to_string(),
        "SSH_AUTH_SOCK".to_string(),
    ] {
        if let Some(path) = nonempty_env_var(&env_name) {
            return Ok(Some(PathBuf::from(path)));
        }
    }
    Ok(None)
}

fn guest_ssh_mount_source(source: &Path) -> String {
    format!(
        "{}/{}",
        SSH_AGENT_GUEST_DIR,
        crate::cache::cache_tag(&source.display().to_string())
    )
}

fn pipeline_ssh_mount_sources(pipeline: &Pipeline) -> Result<Vec<PathBuf>> {
    let mut seen = BTreeSet::new();
    let mut sources = Vec::new();
    for operation in &pipeline.operations {
        let Operation::Exec(step) = operation else {
            continue;
        };
        for mount in &step.run_mounts {
            let StepRunMount::Ssh { id, .. } = mount else {
                continue;
            };
            if let Some(source) = resolve_ssh_run_mount_source(id)?
                && seen.insert(source.clone())
            {
                sources.push(source);
            }
        }
    }
    Ok(sources)
}

fn guest_stage_run_mount_source(stage: &str, source: &str) -> String {
    if source == "/" {
        format!("/boringbuilder-stages/{stage}")
    } else {
        format!(
            "/boringbuilder-stages/{stage}/{}",
            source.trim_start_matches('/')
        )
    }
}

fn resolve_stage_run_mount_source(
    pipeline: &Pipeline,
    stage: &str,
    source: &str,
) -> Result<PathBuf> {
    let stage_root = pipeline
        .inputs
        .iter()
        .find(|input| input.dest == format!("/boringbuilder-stages/{stage}"))
        .map(|input| input.source.clone())
        .ok_or_else(|| anyhow!("missing mounted stage rootfs for '{stage}'"))?;
    let relative = Path::new(source.trim_start_matches('/'));
    Ok(if source == "/" || relative.as_os_str().is_empty() {
        stage_root
    } else {
        stage_root.join(relative)
    })
}

fn hash_container_path(
    binary: &Path,
    container_name: &str,
    container_path: &str,
    excludes: &[String],
) -> Result<String> {
    let script = build_container_hash_script(container_path, excludes)?;
    let output = run_capture(
        binary,
        &[
            "exec".to_string(),
            container_name.to_string(),
            "/bin/sh".to_string(),
            "-c".to_string(),
            script,
        ],
    )
    .with_context(|| format!("failed to hash {container_path} inside container"))?;
    ensure!(
        output.status.success(),
        "failed to hash {} inside container: {}",
        container_path,
        output.stderr.trim()
    );

    let digest = output.stdout.trim();
    ensure!(
        digest.starts_with("container-tree-v1:") || digest.starts_with("container-missing-v1:"),
        "container hash for {} returned invalid output: {}",
        container_path,
        digest
    );
    Ok(digest.to_string())
}

fn build_container_hash_script(container_path: &str, excludes: &[String]) -> Result<String> {
    ensure!(
        container_path.starts_with('/'),
        "container hash path must be absolute: {container_path}"
    );
    let relative = match container_path.trim_start_matches('/') {
        "" => ".",
        relative => relative,
    };

    let mut prune_terms = Vec::new();
    let mut effective_excludes = excludes.to_vec();
    if container_path == "/" {
        effective_excludes.extend(
            [
                "proc",
                "sys",
                "dev",
                "tmp",
                "run",
                LOCAL_CACHE_GUEST_DIR,
                CACHE_TOOLS_GUEST_DIR,
                RUN_MOUNT_GUEST_DIR,
                SSH_AGENT_GUEST_DIR,
                CONTEXT_GUEST_DIR,
                helper_mount_guest_dir(),
                helper_snapshot_guest_dir(),
                "/.boringbuilder-slice-marker",
            ]
            .into_iter()
            .map(str::to_string),
        );
        effective_excludes.extend(
            crate::cache::slice::STEP_SLICE_IGNORED_PREFIXES
                .iter()
                .map(|path| (*path).to_string()),
        );
    }
    for exclude in &effective_excludes {
        ensure!(
            !exclude.trim().is_empty(),
            "empty step cache input exclude is not allowed"
        );
        ensure!(
            !exclude.starts_with('!'),
            "negated step cache input excludes are not supported by macOS in-container hashing: {exclude}"
        );
        let prefixed = prefixed_container_exclude(relative, exclude);
        prune_terms.push(format!("-path {}", shell_words::quote(&prefixed)));
        prune_terms.push(format!(
            "-path {}",
            shell_words::quote(&format!("{prefixed}/*"))
        ));
    }
    let prune = if prune_terms.is_empty() {
        String::new()
    } else {
        format!("\\( {} \\) -prune -o ", prune_terms.join(" -o "))
    };

    Ok(format!(
        r#"set -eu
bb_sha256() {{
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum | sed 's/[[:space:]].*//'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 | sed 's/[[:space:]].*//'
  else
    echo "step-state hashing requires sha256sum or shasum in the container" >&2
    exit 127
  fi
}}
path={path}
if [ ! -e "$path" ]; then
  digest="$(printf '%s\n' "missing:$path" | bb_sha256)"
  printf 'container-missing-v1:%s\n' "$digest"
  exit 0
fi
relative={relative}
digest="$(
  cd /
  {{
    find "$relative" {prune}-type d -printf 'dir %m %p\n'
    find "$relative" {prune}-type l -printf 'link %m %p ' -exec readlink {{}} \;
    find "$relative" {prune}-type f -printf 'file %m %p ' -exec sha256sum {{}} \;
  }} | LC_ALL=C sort | bb_sha256
)"
printf 'container-tree-v1:%s\n' "$digest"
"#,
        path = shell_words::quote(container_path),
        relative = shell_words::quote(relative),
        prune = prune,
    ))
}

fn prefixed_container_exclude(container_relative: &str, exclude: &str) -> String {
    let trimmed = exclude.trim_start_matches('/');
    if trimmed.is_empty() {
        container_relative.to_string()
    } else {
        format!("{container_relative}/{trimmed}")
    }
}

fn compute_step_state_key(
    binary: &Path,
    container_name: &str,
    previous_state_key: &str,
    pipeline: &Pipeline,
    step: &Step,
    hash_default_rootfs: bool,
) -> Result<String> {
    let mut hasher = Sha256::new();
    hash_step_state_field(&mut hasher, "version", "boringbuilder-step-state-v3");
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
                        CacheMode::Shared => b"shared",
                        CacheMode::Locked => b"locked",
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
                        let source_hash = hash_container_path(
                            binary,
                            container_name,
                            &guest_stage_run_mount_source(stage, path),
                            &[],
                        )?;
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
                let input_hash =
                    hash_container_path(binary, container_name, &input.path, &input.exclude)?;
                hash_step_state_field(&mut hasher, "input-hash", input_hash.as_bytes());
            }
        }
        None if hash_default_rootfs => {
            hash_step_state_field(&mut hasher, "input-path", b"/");
            let input_hash = hash_container_path(binary, container_name, "/", &[])?;
            hash_step_state_field(&mut hasher, "input-hash", input_hash.as_bytes());
        }
        None => {}
    }
    Ok(hex::encode(hasher.finalize()))
}

fn hash_context_run_mount_source(pipeline: &Pipeline, source: &str) -> Result<String> {
    let context = pipeline
        .docker_context
        .as_ref()
        .ok_or_else(|| anyhow!("missing Docker build context for RUN --mount bind source"))?;
    crate::dockerfile::context::hash_source(context, source)
}

pub fn container_binary_path() -> Result<PathBuf> {
    resolve_container_cli(
        std::env::var_os("BORINGBUILDER_CONTAINER_BIN").map(PathBuf::from),
        false,
    )
    .map(|resolved| resolved.path)
}

pub fn resolve_container_binary(override_path: Option<PathBuf>) -> Result<PathBuf> {
    resolve_container_cli(override_path, false).map(|resolved| resolved.path)
}

pub fn container_system_status(binary: &Path) -> Result<String> {
    crate::macos_runtime::container_system_status(binary)
}

fn stop_container(binary: &Path, container_name: &str) -> Result<()> {
    run_checked(
        binary,
        &[
            "stop".to_string(),
            "--time".to_string(),
            "0".to_string(),
            container_name.to_string(),
        ],
    )
}

fn stop_container_for_export(binary: &Path, container_name: &str, purpose: &str) -> bool {
    const MAX_ATTEMPTS: usize = 4;
    const RETRY_DELAY: Duration = Duration::from_millis(750);

    let mut last_err = None;
    for attempt in 0..MAX_ATTEMPTS {
        match stop_container(binary, container_name) {
            Ok(()) => return true,
            Err(err) => {
                last_err = Some(err);
                if attempt + 1 < MAX_ATTEMPTS {
                    sleep(RETRY_DELAY);
                }
            }
        }
    }

    if let Some(err) = last_err {
        eprintln!(
            "warning: failed to stop container before {purpose} after {MAX_ATTEMPTS} attempts: {err:#}; continuing with live export"
        );
    }
    false
}

fn export_error_indicates_running_container(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains("container is not stopped")
}

fn oci_archive_write_error_is_retryable(err: &anyhow::Error) -> bool {
    let message = format!("{err:#}");
    message.contains("unable to write data to the archive") || message.contains("code -30")
}

fn oci_image_export_error_is_retryable(err: &anyhow::Error) -> bool {
    export_error_indicates_running_container(err) || oci_archive_write_error_is_retryable(err)
}

fn export_archive_from_container(
    ctx: ArchiveExportContext<'_>,
    kind: ArchiveExportKind,
    output_path: &Path,
) -> Result<PathBuf> {
    if should_export_full_rootfs(ctx.pipeline) {
        ui::print_status("stopping container for archive export");
        if stop_container_for_export(ctx.binary, ctx.container_name, "archive export") {
            ctx.guard.disarm();
        }
        let rootfs = export_container_rootfs(ctx.binary, ctx.container_name, ctx.platform)?;

        ui::print_status(format!(
            "exporting {} {}",
            kind.label(),
            output_path.display()
        ));
        return kind.export(ctx.pipeline, output_path, rootfs.path());
    }

    ui::print_status(format!(
        "exporting {} {} from container",
        kind.label(),
        output_path.display()
    ));
    export_container_archive_stream(
        ctx.binary,
        ctx.container_name,
        ctx.pipeline,
        kind,
        output_path,
    )
}

fn export_container_archive_stream(
    binary: &Path,
    container_name: &str,
    pipeline: &Pipeline,
    kind: ArchiveExportKind,
    output_path: &Path,
) -> Result<PathBuf> {
    const MAX_ATTEMPTS: usize = 3;

    let mut last_err = None;
    for attempt in 0..MAX_ATTEMPTS {
        match export_container_archive_stream_once(
            binary,
            container_name,
            pipeline,
            kind,
            output_path,
        ) {
            Ok(path) => return Ok(path),
            Err(err) if archive_stream_error_is_retryable(&err) => {
                last_err = Some(err);
                if attempt + 1 < MAX_ATTEMPTS {
                    eprintln!(
                        "warning: container {} export was interrupted; retrying ({}/{MAX_ATTEMPTS})",
                        kind.label(),
                        attempt + 2
                    );
                    sleep(Duration::from_millis(750));
                }
            }
            Err(err) => return Err(err),
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("container archive export did not complete")))
}

fn archive_stream_error_is_retryable(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains("export failed with status signal")
}

fn export_container_archive_stream_once(
    binary: &Path,
    container_name: &str,
    pipeline: &Pipeline,
    kind: ArchiveExportKind,
    output_path: &Path,
) -> Result<PathBuf> {
    ensure!(
        !pipeline.outputs.is_empty(),
        "container-native archive export needs declared outputs"
    );

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let output_file = fs::File::create(output_path)
        .with_context(|| format!("failed to create {}", output_path.display()))?;

    let script = build_container_archive_export_script(&pipeline.outputs, kind)?;
    let mut child = Command::new(binary)
        .args(["exec", container_name, "/bin/sh", "-c", script.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to start container {} export", kind.label()))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to capture container archive stdout"))?;
    let write_result: Result<()> = match kind {
        ArchiveExportKind::TarZst => (|| {
            let mut encoder = zstd::Encoder::new(io::BufWriter::new(output_file), 1)
                .context("failed to initialize zstd export encoder")?;
            encoder
                .multithread(4)
                .context("failed to enable multithreaded zstd export")?;
            let encoder = normalize_tar_stream(io::BufReader::new(stdout), encoder)?;
            encoder
                .finish()
                .and_then(|mut writer| writer.flush())
                .context("failed to finish zstd export")
        })(),
        ArchiveExportKind::Tar => (|| {
            let writer = io::BufWriter::new(output_file);
            let mut writer = normalize_tar_stream(io::BufReader::new(stdout), writer)?;
            writer.flush().context("failed to finish tar export")
        })(),
    };
    let status = child
        .wait()
        .with_context(|| format!("failed to wait for container {} export", kind.label()))?;
    if !status.success() {
        let _ = fs::remove_file(output_path);
        bail!(
            "container {} export failed with status {}",
            kind.label(),
            status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string())
        );
    }
    if let Err(err) = write_result {
        let _ = fs::remove_file(output_path);
        return Err(err).with_context(|| format!("failed to write {}", output_path.display()));
    }

    Ok(output_path.to_path_buf())
}

fn normalize_tar_stream<R: Read, W: Write>(reader: R, writer: W) -> Result<W> {
    let mut source = tar::Archive::new(reader);
    let mut destination = tar::Builder::new(writer);
    for entry in source
        .entries()
        .context("failed to read container tar stream")?
    {
        let mut entry = entry.context("failed to read container tar entry")?;
        let path = entry
            .path()
            .context("failed to read container tar entry path")?
            .into_owned();
        let link_name = entry
            .link_name()
            .context("failed to read container tar link target")?
            .map(|path| path.into_owned());
        let source_header = entry.header();
        let source_entry_type = source_header.entry_type();
        let entry_type = if source_entry_type.is_gnu_sparse() {
            tar::EntryType::Regular
        } else {
            source_entry_type
        };
        let size = if source_entry_type.is_gnu_sparse() {
            source_header
                .as_gnu()
                .ok_or_else(|| anyhow!("GNU sparse entry is missing a GNU header"))?
                .real_size()
                .context("failed to read GNU sparse entry size")?
        } else {
            entry.size()
        };
        let mode = source_header
            .mode()
            .context("failed to read container tar entry mode")?;
        let (device_major, device_minor) =
            if entry_type.is_character_special() || entry_type.is_block_special() {
                (
                    source_header
                        .device_major()
                        .context("failed to read container tar device major")?,
                    source_header
                        .device_minor()
                        .context("failed to read container tar device minor")?,
                )
            } else {
                (None, None)
            };

        // Rebuild every header instead of copying the raw one. Logical path,
        // link and size values can live in GNU/PAX extension records; copying
        // only the following raw header silently drops those values and emits
        // an archive some readers cannot unpack.
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(entry_type);
        header.set_mode(mode);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_username("")?;
        header.set_groupname("")?;
        if let Some(major) = device_major {
            header.set_device_major(major)?;
        }
        if let Some(minor) = device_minor {
            header.set_device_minor(minor)?;
        }

        if entry_type.is_symlink() || entry_type.is_hard_link() {
            let link_name = link_name.ok_or_else(|| {
                anyhow!(
                    "container tar link {} is missing its target",
                    path.display()
                )
            })?;
            header.set_size(0);
            destination
                .append_link(&mut header, &path, &link_name)
                .context("failed to normalize container tar link")?;
        } else {
            header.set_size(size);
            destination
                .append_data(&mut header, &path, &mut entry)
                .context("failed to normalize container tar entry")?;
        }
    }
    destination
        .finish()
        .context("failed to finish tar stream")?;
    destination
        .into_inner()
        .context("failed to finalize tar stream")
}

fn build_container_archive_export_script(
    outputs: &[String],
    _kind: ArchiveExportKind,
) -> Result<String> {
    let mut find_commands = Vec::with_capacity(outputs.len());
    for output in outputs {
        ensure!(
            output.starts_with('/'),
            "container archive output must be absolute: {output}"
        );
        let relative = output.trim_start_matches('/');
        ensure!(
            !relative.is_empty(),
            "container-native selected archive export cannot export root '/'"
        );
        find_commands.push(format!("find {} -print", shell_words::quote(relative)));
    }
    Ok(format!(
        "set -eu\ncd /\n{{\n  {}\n}} | LC_ALL=C sort | tar --numeric-owner --no-recursion -cf - -T -",
        find_commands.join("\n  ")
    ))
}

fn export_container_rootfs(
    binary: &Path,
    container_name: &str,
    platform: &str,
) -> Result<tempfile::TempDir> {
    let oci_dir = tempfile::Builder::new()
        .prefix("boringbuilder-macos-export-oci-")
        .tempdir()
        .context("failed to create temporary macOS OCI export directory")?;
    export_container_as_oci_layout(binary, container_name, platform, oci_dir.path())?;

    let rootfs_dir = tempfile::Builder::new()
        .prefix("boringbuilder-macos-export-rootfs-")
        .tempdir()
        .context("failed to create temporary macOS rootfs export directory")?;
    unpack_rootfs_from_oci_layout(oci_dir.path(), platform, rootfs_dir.path())
        .context("failed to unpack macOS OCI layout into rootfs export")?;
    Ok(rootfs_dir)
}

fn snapshot_stage_paths(
    binary: &Path,
    container_name: &str,
    helper: &mut GuestHelperHost,
    paths: &[String],
) -> Result<()> {
    if paths.is_empty() {
        bail!("stage snapshot requested without any paths");
    }

    let mut args = vec![
        "exec".to_string(),
        "--user".to_string(),
        "0:0".to_string(),
        container_name.to_string(),
        "/bin/sh".to_string(),
        helper.guest_helper_path(),
        "snapshot".to_string(),
    ];
    for path in paths {
        args.push(path.clone());
    }
    run_streaming(binary, &args).context("failed to snapshot stage paths")
}

fn export_stage_rootfs(
    binary: &Path,
    container_name: &str,
    platform: &str,
    snapshot_root: &Path,
) -> Result<()> {
    let oci_dir = tempfile::Builder::new()
        .prefix("boringbuilder-macos-stage-oci-")
        .tempdir()
        .context("failed to create temporary macOS stage OCI export directory")?;
    export_container_as_oci_layout(binary, container_name, platform, oci_dir.path())?;
    unpack_rootfs_from_oci_layout(oci_dir.path(), platform, snapshot_root)
        .context("failed to unpack macOS stage OCI layout into rootfs snapshot")
}

fn should_export_full_rootfs(pipeline: &Pipeline) -> bool {
    pipeline.outputs.is_empty() || pipeline.outputs.iter().any(|path| path == "/")
}

fn export_container_as_oci_layout(
    binary: &Path,
    container_name: &str,
    platform: &str,
    output_path: &Path,
) -> Result<PathBuf> {
    let temp_ref = format!("boringbuilder-export-{}", unique_container_name());
    let work_dir = tempfile::Builder::new()
        .prefix("boringbuilder-macos-oci-")
        .tempdir()
        .context("failed to create temporary OCI export directory")?;
    let tar_path = work_dir.path().join("image.tar");

    const MAX_EXPORT_ATTEMPTS: usize = 3;
    let mut export_succeeded = false;
    let mut last_export_err = None;
    for attempt in 0..MAX_EXPORT_ATTEMPTS {
        match run_checked(
            binary,
            &[
                "export".to_string(),
                "--image".to_string(),
                temp_ref.clone(),
                container_name.to_string(),
            ],
        ) {
            Ok(()) => {
                export_succeeded = true;
                break;
            }
            Err(err) if oci_image_export_error_is_retryable(&err) => {
                last_export_err = Some(err);
                if attempt + 1 < MAX_EXPORT_ATTEMPTS {
                    let _ = stop_container_for_export(binary, container_name, "image export retry");
                    sleep(Duration::from_millis(750));
                    continue;
                }
            }
            Err(err) => return Err(err).context("failed to export container state as image"),
        }
    }

    if !export_succeeded {
        return Err(last_export_err
            .unwrap_or_else(|| anyhow!("container export did not complete successfully")))
        .context("failed to export container state as image");
    }

    const MAX_SAVE_ATTEMPTS: usize = 3;
    let mut save_succeeded = false;
    let mut last_save_err = None;
    for attempt in 0..MAX_SAVE_ATTEMPTS {
        match run_checked(
            binary,
            &[
                "image".to_string(),
                "save".to_string(),
                "--platform".to_string(),
                platform.to_string(),
                "--output".to_string(),
                tar_path.to_string_lossy().to_string(),
                temp_ref.clone(),
            ],
        ) {
            Ok(()) => {
                save_succeeded = true;
                break;
            }
            Err(err) if oci_archive_write_error_is_retryable(&err) => {
                last_save_err = Some(err);
                if attempt + 1 < MAX_SAVE_ATTEMPTS {
                    sleep(Duration::from_millis(750));
                    continue;
                }
            }
            Err(err) => {
                let _ = Command::new(binary)
                    .args(["image", "rm", &temp_ref])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                return Err(err).context("failed to save exported image as OCI archive");
            }
        }
    }

    let _ = Command::new(binary)
        .args(["image", "rm", &temp_ref])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    if !save_succeeded {
        return Err(
            last_save_err.unwrap_or_else(|| anyhow!("image save did not complete successfully"))
        )
        .context("failed to save exported image as OCI archive");
    }

    if output_path.exists() {
        if output_path.is_dir() {
            fs::remove_dir_all(output_path)
                .with_context(|| format!("failed to remove {}", output_path.display()))?;
        } else {
            fs::remove_file(output_path)
                .with_context(|| format!("failed to remove {}", output_path.display()))?;
        }
    }
    fs::create_dir_all(output_path)
        .with_context(|| format!("failed to create {}", output_path.display()))?;

    let tar_file = fs::File::open(&tar_path)
        .with_context(|| format!("failed to open exported tar {}", tar_path.display()))?;
    let mut archive = tar::Archive::new(tar_file);
    archive.unpack(output_path).with_context(|| {
        format!(
            "failed to extract OCI layout tar to {}",
            output_path.display()
        )
    })?;

    Ok(output_path.to_path_buf())
}

fn ensure_system_ready(binary: &Path) -> Result<()> {
    ensure_container_system_ready(binary).map(|_| ())
}

fn append_env_args(args: &mut Vec<String>, env: &std::collections::BTreeMap<String, String>) {
    for (key, value) in env {
        args.push("--env".to_string());
        args.push(format!("{key}={value}"));
    }
}

fn append_dns_args(args: &mut Vec<String>) {
    for server in resolved_dns_servers(std::env::var("BORINGBUILDER_CONTAINER_DNS").ok().as_deref())
    {
        args.push("--dns".to_string());
        args.push(server);
    }
}

fn append_resource_args(args: &mut Vec<String>) {
    if let Some(memory) = resolved_container_memory_override(
        std::env::var("BORINGBUILDER_MACOS_CONTAINER_MEMORY")
            .ok()
            .as_deref(),
    )
    .or_else(|| host_memory_bytes().ok().map(default_container_memory_arg))
    {
        args.push("--memory".to_string());
        args.push(memory);
    }
}

fn resolved_container_memory_override(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

fn default_container_memory_arg(host_memory_bytes: u64) -> String {
    const ONE_GIB: u64 = 1024 * 1024 * 1024;

    let host_gib = host_memory_bytes / ONE_GIB;
    let guest_gib = if host_gib <= 4 {
        2
    } else {
        host_gib.saturating_sub(2).clamp(4, 8)
    };
    format!("{guest_gib}G")
}

fn host_memory_bytes() -> Result<u64> {
    let sysctl = find_command("sysctl")
        .ok_or_else(|| anyhow!("sysctl was not found in PATH; cannot inspect host memory"))?;
    let output = run_capture(&sysctl, &["-n".to_string(), "hw.memsize".to_string()])?;
    if !output.status.success() {
        bail!(
            "sysctl -n hw.memsize failed: {}",
            if output.stderr.trim().is_empty() {
                output.stdout.trim()
            } else {
                output.stderr.trim()
            }
        );
    }

    output
        .stdout
        .trim()
        .parse::<u64>()
        .context("failed to parse sysctl hw.memsize output")
}

fn append_mount_args(args: &mut Vec<String>, pipeline: &Pipeline) -> Result<()> {
    for input in &pipeline.inputs {
        let source = input
            .source
            .canonicalize()
            .with_context(|| format!("failed to resolve mount {}", input.source.display()))?;
        append_mount_arg(args, &source, &input.dest, input.readonly);
    }
    Ok(())
}

fn append_context_mount_arg(args: &mut Vec<String>, pipeline: &Pipeline) -> Result<()> {
    if !pipeline
        .operations
        .iter()
        .any(|operation| matches!(operation, Operation::CopyFromContext(_)))
    {
        return Ok(());
    }

    let Some(context) = &pipeline.docker_context else {
        bail!("missing Docker build context for COPY operation");
    };
    let source = context
        .root
        .canonicalize()
        .with_context(|| format!("failed to resolve mount {}", context.root.display()))?;
    append_mount_arg(args, &source, CONTEXT_GUEST_DIR, true);
    Ok(())
}

fn prepare_local_container_cache_mount(
    cache_config: &CacheStoreConfig,
    no_cache: bool,
) -> Result<Option<LocalContainerCacheMount>> {
    if no_cache || !matches!(&cache_config.cache_store, CacheStoreKind::Local) {
        return Ok(None);
    }

    let cache_root = crate::cache::local::default_cache_dir(cache_config.cache_dir.as_deref())?;
    crate::cache::local::LocalCacheBackend::open(&cache_root)
        .with_context(|| format!("failed to prepare local cache at {}", cache_root.display()))?;
    let host_root = cache_root
        .canonicalize()
        .with_context(|| format!("failed to resolve local cache {}", cache_root.display()))?;
    Ok(Some(LocalContainerCacheMount { host_root }))
}

fn prepare_container_cache_tools(
    cache_config: &CacheStoreConfig,
    pipeline: &Pipeline,
    no_cache: bool,
) -> Result<Option<ContainerCacheTools>> {
    let needs_boringcache =
        !no_cache && matches!(&cache_config.cache_store, CacheStoreKind::BoringCache);
    if !needs_boringcache {
        return Ok(None);
    }

    let host_dir = tempfile::Builder::new()
        .prefix("boringbuilder-macos-cache-tools-")
        .tempdir()
        .context("failed to create temporary macOS cache tools directory")?;
    let bin_dir = host_dir.path().join("bin");
    fs::create_dir_all(&bin_dir)
        .with_context(|| format!("failed to create {}", bin_dir.display()))?;
    let boringcache = if needs_boringcache {
        let Some(prebuilt) = crate::guest_tools::prepare_prebuilt_boringcache(
            cache_config.cache_bin.as_deref(),
            &pipeline.platform,
        )?
        else {
            bail!(
                "macOS container BoringCache requires a Linux boringcache binary for {}; refusing to fall back to host-side cache blobs",
                pipeline.platform
            );
        };
        ui::print_detail(format!(
            "container-native BoringCache binary {} ({}, sha256:{})",
            prebuilt.local_path.display(),
            prebuilt.source,
            prebuilt.digest
        ));

        let workspace = resolve_boringcache_workspace(cache_config)?;
        let binary_host_path = bin_dir.join("boringcache");
        fs::copy(&prebuilt.local_path, &binary_host_path).with_context(|| {
            format!(
                "failed to copy BoringCache binary {} into {}",
                prebuilt.local_path.display(),
                binary_host_path.display()
            )
        })?;
        set_executable(&binary_host_path)?;

        let token_file_guest_path = write_container_boringcache_token_file(host_dir.path())?;
        let api_url = nonempty_env_var("BORINGCACHE_API_URL");
        Some(ContainerBoringCacheTools {
            workspace,
            binary_guest_path: format!("{CACHE_TOOLS_GUEST_DIR}/bin/boringcache"),
            token_file_guest_path,
            api_url,
        })
    } else {
        None
    };

    Ok(Some(ContainerCacheTools {
        host_dir,
        boringcache,
    }))
}

fn set_executable(path: &Path) -> Result<()> {
    let mut permissions = fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to chmod {}", path.display()))
}

fn resolve_boringcache_workspace(cache_config: &CacheStoreConfig) -> Result<String> {
    cache_config
        .cache_workspace
        .clone()
        .or_else(|| std::env::var("BORINGBUILDER_CACHE_WORKSPACE").ok())
        .or_else(|| std::env::var("BORINGCACHE_DEFAULT_WORKSPACE").ok())
        .map(|workspace| workspace.trim().to_string())
        .filter(|workspace| !workspace.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "BoringCache integration requires a workspace; pass --cache-workspace or set BORINGBUILDER_CACHE_WORKSPACE/BORINGCACHE_DEFAULT_WORKSPACE"
            )
        })
}

fn write_container_boringcache_token_file(cache_tools_root: &Path) -> Result<Option<String>> {
    let token = if let Some(path) = nonempty_env_var("BORINGCACHE_TOKEN_FILE") {
        Some(fs::read(&path).with_context(|| {
            format!(
                "failed to read BoringCache token file {}",
                Path::new(&path).display()
            )
        })?)
    } else {
        boringcache_api_token_env_value()
            .map(|value| value.to_string_lossy().into_owned().into_bytes())
    };

    let Some(token) = token else {
        return Ok(None);
    };

    let token_path = cache_tools_root.join("boringcache-token");
    fs::write(&token_path, token)
        .with_context(|| format!("failed to write {}", token_path.display()))?;
    let mut permissions = fs::metadata(&token_path)?.permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(&token_path, permissions)?;
    Ok(Some(format!("{CACHE_TOOLS_GUEST_DIR}/boringcache-token")))
}

fn boringcache_api_token_env_value() -> Option<OsString> {
    for name in [
        "BORINGCACHE_API_TOKEN",
        "BORINGCACHE_ADMIN_TOKEN",
        "BORINGCACHE_SAVE_TOKEN",
        "BORINGCACHE_RESTORE_TOKEN",
    ] {
        if let Some(value) = std::env::var_os(name)
            && !value.is_empty()
        {
            return Some(value);
        }
    }
    None
}

fn append_mount_arg(args: &mut Vec<String>, source: &Path, dest: &str, readonly: bool) {
    let mut mount = format!("source={},target={}", source.display(), dest);
    if readonly {
        mount.push_str(",readonly");
    }
    args.push("--mount".to_string());
    args.push(mount);
}

fn parse_shell(shell: &str) -> Result<Vec<String>> {
    let parts = shell_words::split(shell)
        .with_context(|| format!("invalid shell declaration '{shell}'"))?;
    ensure!(!parts.is_empty(), "shell must not be empty");
    Ok(parts)
}

fn unique_container_name() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("boringbuilder-{}-{seconds}", std::process::id())
}

fn dns_servers_from_env(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn resolved_dns_servers(env_value: Option<&str>) -> Vec<String> {
    let env_servers = dns_servers_from_env(env_value);
    if !env_servers.is_empty() {
        return env_servers;
    }

    let host_servers = host_dns_servers().unwrap_or_default();
    if !host_servers.is_empty() {
        return host_servers;
    }

    // Apple `container` sometimes leaves guests without usable DNS when the
    // host only exposes loopback/stub resolvers. Fall back to public resolvers
    // so local runs work out of the box; callers can override via
    // BORINGBUILDER_CONTAINER_DNS when they need private or corporate DNS.
    vec!["1.1.1.1".to_string(), "8.8.8.8".to_string()]
}

fn host_dns_servers() -> Result<Vec<String>> {
    let scutil = find_command("scutil")
        .ok_or_else(|| anyhow!("scutil was not found in PATH; cannot inspect host DNS"))?;
    let output = run_capture(&scutil, &["--dns".to_string()])?;
    if !output.status.success() {
        bail!(
            "scutil --dns failed: {}",
            if output.stderr.trim().is_empty() {
                output.stdout.trim()
            } else {
                output.stderr.trim()
            }
        );
    }

    Ok(parse_scutil_dns_servers(&output.stdout))
}

fn parse_scutil_dns_servers(output: &str) -> Vec<String> {
    let mut servers = Vec::new();
    for line in output.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if !key.trim().starts_with("nameserver[") {
            continue;
        }

        let candidate = value.trim();
        if dns_server_usable(candidate) && !servers.iter().any(|server| server == candidate) {
            servers.push(candidate.to_string());
        }
    }
    servers
}

fn dns_server_usable(candidate: &str) -> bool {
    let Ok(addr) = candidate.parse::<IpAddr>() else {
        return false;
    };

    match addr {
        IpAddr::V4(addr) => !addr.is_loopback() && !addr.is_unspecified() && !addr.is_multicast(),
        IpAddr::V6(addr) => {
            let mapped = addr.to_ipv4_mapped();
            if let Some(mapped) = mapped {
                return !mapped.is_loopback() && !mapped.is_unspecified() && !mapped.is_multicast();
            }
            !addr.is_loopback()
                && !addr.is_unspecified()
                && !addr.is_multicast()
                && !addr.is_unicast_link_local()
        }
    }
}

fn operation_label(operation: &Operation, index: usize) -> String {
    operation
        .name()
        .map(str::to_string)
        .unwrap_or_else(|| format!("step-{}", index + 1))
}

struct ContainerGuard {
    binary: PathBuf,
    container_name: String,
    keep: bool,
    armed: bool,
}

impl ContainerGuard {
    fn new(binary: PathBuf, container_name: String, keep: bool) -> Self {
        Self {
            binary,
            container_name,
            keep,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

fn register_macos_container_interrupt(
    binary: &Path,
    container_name: String,
) -> Result<interrupt::Registration> {
    let binary = binary.to_path_buf();
    let cleanup_container_name = container_name.clone();
    interrupt::register_cleanup(
        format!("stopping active macOS container build {container_name}"),
        move || stop_container_quiet(&binary, &cleanup_container_name),
    )
}

fn stop_container_quiet(binary: &Path, container_name: &str) {
    let _ = Command::new(binary)
        .args(["stop", "--time", "0", container_name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        if !self.armed || self.keep {
            return;
        }

        stop_container_quiet(&self.binary, &self.container_name);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Cursor, Read};
    use std::path::Path;

    use tempfile::tempdir;

    use tar::Builder;

    use super::{
        ArchiveExportKind, ContainerBoringCacheTools, build_capture_boringcache_slice_script,
        build_capture_local_slice_script, build_capture_slice_script,
        build_container_archive_export_script, build_container_hash_script,
        build_restore_boringcache_slice_script, cache_blob_tag, default_container_memory_arg,
        dns_server_usable, dns_servers_from_env, export_container_archive_stream,
        export_container_as_oci_layout, local_cache_blob_guest_path, normalize_tar_stream,
        parse_local_slice_capture_output, parse_scutil_dns_servers, prefixed_container_exclude,
        required_step_slice_restore_free_bytes, resolve_container_binary,
        resolved_container_memory_override, resolved_dns_servers, should_export_full_rootfs,
        stop_container_for_export, touch_slice_marker_script,
    };

    #[test]
    fn respects_container_binary_override() {
        let temp = tempdir().unwrap();
        let fake = temp.path().join("fake-container");
        fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&fake).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&fake, perms).unwrap();
        }

        let resolved = resolve_container_binary(Some(fake.clone())).unwrap();
        assert_eq!(resolved, fake);
    }

    #[test]
    fn capture_script_excludes_apt_archives() {
        let script = build_capture_slice_script(&[]);

        assert!(script.contains("-cnewer /.boringbuilder-slice-marker"));
        for excluded in [
            "/boringbuilder-cache-tools",
            "/boringbuilder-helper",
            "/boringbuilder-local-cache",
            "/boringbuilder-run-mounts",
            "/boringbuilder-ssh-agents",
            "/boringbuilder-stage-snapshot",
            "/var/cache/apt/archives",
            "/var/cache/apt/archives/*",
            "/var/lib/apt/lists",
            "/var/log/apt",
        ] {
            assert!(script.contains(shell_words::quote(excluded).as_ref()));
        }

        assert!(script.contains("-mindepth 1"));
        assert!(script.contains("-cnewer /.boringbuilder-slice-marker"));
        assert!(script.contains("-newer /.boringbuilder-slice-marker"));
        assert!(script.contains("-print"));
        assert!(script.contains("|| true) | tar"));
        assert!(script.contains("--no-recursion"));
        assert!(script.contains("-T -"));
    }

    #[test]
    fn slice_marker_has_a_small_timestamp_safety_margin() {
        let script = touch_slice_marker_script();

        assert!(script.contains("$(date +%s) - 2"));
        assert!(script.contains("touch -t \"$stamp\" /.boringbuilder-slice-marker"));
    }

    #[test]
    fn local_slice_capture_script_stores_blob_inside_mounted_cache() {
        let script = build_capture_local_slice_script(&["/opt/app/tmp".to_string()]).unwrap();

        assert!(script.contains("/boringbuilder-local-cache"));
        assert!(!script.contains("bb_zstd"));
        assert!(!script.contains("allow_zstd"));
        assert!(script.contains("archive_format=tar"));
        assert!(script.contains("tar --no-recursion -cf \"$tmp\" -T \"$list\""));
        assert!(script.contains("sha256sum"));
        assert!(script.contains("blob=\"$blob_dir/$digest.tar.zst\""));
        assert!(script.contains("mv \"$tmp\" \"$blob\""));
        assert!(script.contains("printf 'stored %s %s %s\\n'"));
        assert!(script.contains("|| true) > \"$list\""));
        assert!(script.contains("/boringbuilder-local-cache"));
        assert!(script.contains("/opt/app/tmp"));
    }

    #[test]
    fn local_slice_capture_script_uses_raw_tar_without_zstd_bootstrap() {
        let script = build_capture_local_slice_script(&[]).unwrap();

        assert!(script.contains("archive_format=tar"));
        assert!(script.contains("tar --no-recursion -cf \"$tmp\" -T \"$list\""));
        assert!(!script.contains("bb_have_zstd"));
        assert!(!script.contains("bb_zstd"));
    }

    #[test]
    fn local_slice_capture_output_parses_stored_and_empty_results() {
        let digest = "a".repeat(64);
        let parsed =
            parse_local_slice_capture_output(&format!("noise\nstored {digest} 1234\n")).unwrap();
        let parsed = parsed.unwrap();

        assert_eq!(parsed.digest, digest);
        assert_eq!(parsed.bytes, 1234);
        assert_eq!(
            parsed.archive_format,
            crate::cache::slice::STEP_SLICE_ARCHIVE_FORMAT_TAR_ZST
        );
        let parsed = parse_local_slice_capture_output(&format!("stored {digest} 1234 tar\n"))
            .unwrap()
            .unwrap();
        assert_eq!(
            parsed.archive_format,
            crate::cache::slice::STEP_SLICE_ARCHIVE_FORMAT_TAR
        );
        assert!(
            parse_local_slice_capture_output("empty\n")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn local_cache_blob_guest_path_rejects_unsafe_digest() {
        let digest = "b".repeat(64);
        assert_eq!(
            local_cache_blob_guest_path(&digest).unwrap(),
            format!("/boringbuilder-local-cache/blobs/sha256/{digest}.tar.zst")
        );
        assert!(local_cache_blob_guest_path("../nope").is_err());
    }

    #[test]
    fn boringcache_slice_capture_script_uploads_blob_inside_container() {
        let tools = ContainerBoringCacheTools {
            workspace: "boringcache/web".to_string(),
            binary_guest_path: "/boringbuilder-cache-tools/bin/boringcache".to_string(),
            token_file_guest_path: Some("/boringbuilder-cache-tools/token".to_string()),
            api_url: Some("http://host.docker.internal:3000/api/v2".to_string()),
        };
        let script =
            build_capture_boringcache_slice_script(&tools, &["/cache".to_string()], true).unwrap();

        assert!(script.contains("BORINGCACHE_TOKEN_FILE="));
        assert!(script.contains("BORINGCACHE_API_URL="));
        assert!(script.contains("payload=\"$tmp_root/payload.bin\""));
        assert!(script.contains("bb_zstd_binary()"));
        assert!(script.contains("bb_have_zstd"));
        assert!(script.contains("allow_zstd=1"));
        assert!(script.contains("bb_zstd -1"));
        assert!(script.contains("archive_format=tar"));
        assert!(script.contains("save boringcache/web \"$tag:$tmp_root\""));
        assert!(script.contains("printf 'stored %s %s %s\\n'"));
        assert!(script.contains("|| true) > \"$list\""));
        assert!(script.contains("/boringbuilder-cache-tools"));
        assert!(script.contains("/boringbuilder-local-cache"));
        assert!(script.contains("/cache"));
    }

    #[test]
    fn boringcache_slice_restore_script_restores_blob_inside_container() {
        let tools = ContainerBoringCacheTools {
            workspace: "boringcache/web".to_string(),
            binary_guest_path: "/boringbuilder-cache-tools/bin/boringcache".to_string(),
            token_file_guest_path: None,
            api_url: None,
        };
        let script =
            build_restore_boringcache_slice_script(&tools, "cache-blob-abc123", "tar.zst").unwrap();

        assert!(script.contains("tag=cache-blob-abc123"));
        assert!(script.contains("tag_path=\"$tag:$tmp_root\""));
        assert!(script.contains("restore boringcache/web \"$tag_path\""));
        assert!(script.contains("payload=\"$tmp_root/payload.bin\""));
        assert!(script.contains("BoringCache slice restore requires zstd for tar.zst archive"));
        assert!(script.contains("bb_zstd -dc -- \"$payload\" | tar -xf - -C /"));
    }

    #[test]
    fn cache_blob_tag_matches_boringcache_blob_tag_shape() {
        let digest = "d".repeat(64);
        assert_eq!(
            cache_blob_tag(&digest).unwrap(),
            format!("cache-blob-{digest}")
        );
        assert!(cache_blob_tag("sha256:abc").is_err());
    }

    #[test]
    fn container_hash_script_hashes_inside_container_without_streaming_inputs_to_host() {
        let script = build_container_hash_script(
            "/src",
            &[
                ".git".to_string(),
                "tmp".to_string(),
                "deploy/tmp".to_string(),
            ],
        )
        .unwrap();

        assert!(script.contains("find \"$relative\""));
        assert!(script.contains("-path src/.git"));
        assert!(script.contains("-path 'src/.git/*'"));
        assert!(script.contains("-path src/tmp"));
        assert!(script.contains("-path src/deploy/tmp"));
        assert!(script.contains("-type f -printf 'file %m %p ' -exec sha256sum"));
        assert!(script.contains("printf 'container-tree-v1:%s\\n'"));
        assert!(script.contains("printf 'container-missing-v1:%s\\n'"));
    }

    #[test]
    fn container_hash_script_can_hash_root_without_runtime_state() {
        let script = build_container_hash_script("/", &[]).unwrap();

        assert!(script.contains("relative=."));
        for excluded in [
            "./proc",
            "./sys",
            "./dev",
            "./tmp",
            "./run",
            "./boringbuilder-context",
            "./boringbuilder-local-cache",
            "./boringbuilder-run-mounts",
            "./var/lib/apt/lists",
            "./.boringbuilder-slice-marker",
        ] {
            assert!(script.contains(&format!("-path {excluded}")));
        }
    }

    #[test]
    fn prefixed_container_excludes_are_relative_to_hashed_path() {
        assert_eq!(prefixed_container_exclude("src", ".git"), "src/.git");
        assert_eq!(
            prefixed_container_exclude("src/app", "/assets"),
            "src/app/assets"
        );
    }

    #[test]
    fn container_archive_export_script_streams_selected_outputs_from_container() {
        let script = build_container_archive_export_script(
            &[
                "/opt/boringcache/current".to_string(),
                "/usr/local/bin/mise".to_string(),
            ],
            ArchiveExportKind::TarZst,
        )
        .unwrap();

        assert!(script.contains("tar "));
        assert!(script.contains("opt/boringcache/current"));
        assert!(script.contains("usr/local/bin/mise"));
        assert!(script.contains("LC_ALL=C sort"));
        assert!(script.contains("--numeric-owner --no-recursion -cf - -T -"));
        assert!(!script.contains("zstd"));
        assert!(!script.contains("boringbuilder-stage-snapshot"));
    }

    #[test]
    fn normalized_tar_preserves_logical_long_paths_and_portable_metadata() {
        let path = format!("opt/{}/payload.txt", "long-segment/".repeat(12));
        let payload = b"portable archive\n";
        let mut source = Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        header.set_uid(501);
        header.set_gid(20);
        header.set_mtime(1234);
        header.set_size(payload.len() as u64);
        source
            .append_data(&mut header, &path, payload.as_slice())
            .unwrap();
        let source = source.into_inner().unwrap();

        let normalized = normalize_tar_stream(Cursor::new(source), Vec::new()).unwrap();
        let mut archive = tar::Archive::new(Cursor::new(normalized));
        let mut entries = archive.entries().unwrap();
        let mut entry = entries.next().unwrap().unwrap();
        assert_eq!(entry.path().unwrap().as_ref(), Path::new(&path));
        assert_eq!(entry.header().mode().unwrap(), 0o755);
        assert_eq!(entry.header().uid().unwrap(), 0);
        assert_eq!(entry.header().gid().unwrap(), 0);
        assert_eq!(entry.header().mtime().unwrap(), 0);
        let mut contents = String::new();
        entry.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "portable archive\n");
        assert!(entries.next().is_none());
    }

    #[test]
    fn container_memory_override_uses_trimmed_value() {
        assert_eq!(
            resolved_container_memory_override(Some(" 7G ")).as_deref(),
            Some("7G")
        );
        assert_eq!(resolved_container_memory_override(Some("  ")), None);
        assert_eq!(resolved_container_memory_override(None), None);
    }

    #[test]
    fn default_container_memory_scales_with_host_size() {
        const ONE_GIB: u64 = 1024 * 1024 * 1024;

        assert_eq!(default_container_memory_arg(4 * ONE_GIB), "2G");
        assert_eq!(default_container_memory_arg(6 * ONE_GIB), "4G");
        assert_eq!(default_container_memory_arg(8 * ONE_GIB), "6G");
        assert_eq!(default_container_memory_arg(16 * ONE_GIB), "8G");
        assert_eq!(default_container_memory_arg(32 * ONE_GIB), "8G");
    }

    #[test]
    fn step_slice_restore_headroom_has_floor_and_archive_margin() {
        const ONE_MIB: u64 = 1024 * 1024;
        const ONE_GIB: u64 = 1024 * 1024 * 1024;

        assert_eq!(required_step_slice_restore_free_bytes(1), 2 * ONE_GIB);
        assert_eq!(
            required_step_slice_restore_free_bytes(400 * ONE_MIB),
            400 * ONE_MIB * 6 + 512 * ONE_MIB
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn runs_fake_container_commands_in_order() {
        use std::collections::BTreeMap;

        use crate::backend::{ExecutionBackend, RunOptions};
        use crate::schema::{Input, Operation, Pipeline, Step};

        use super::MacosContainerBackend;

        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("artifact.txt"), "ok").unwrap();

        let fake = temp.path().join("container");
        let log = temp.path().join("container.log");
        let script = format!(
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >> {}\nif [ \"$1\" = system ] && [ \"$2\" = status ]; then echo '{{\"status\":\"ok\"}}'; exit 0; fi\nexit 0\n",
            shell_words::quote(log.to_str().unwrap())
        );
        fs::write(&fake, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&fake).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&fake, perms).unwrap();
        }

        let pipeline = Pipeline {
            image: "alpine:latest".to_string(),
            platform: "linux/arm64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: vec![Input {
                source: workspace,
                dest: "/workspace".to_string(),
                readonly: false,
            }],
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("echo".to_string()),
                run: "echo hello".to_string(),
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

        let backend = MacosContainerBackend {
            container_binary: Some(fake),
        };
        backend
            .run(
                &pipeline,
                &RunOptions {
                    no_cache: true,
                    ..RunOptions::default()
                },
                &crate::cache::CacheStoreConfig::default(),
                None,
            )
            .unwrap();

        let commands = fs::read_to_string(log).unwrap();
        assert!(commands.contains("system status --format json"));
        assert!(
            commands.contains("image pull --progress none --platform linux/arm64 alpine:latest")
        );
        assert!(commands.contains("create --name boringbuilder-"));
        assert!(commands.contains("--network default"));
        assert!(commands.contains("start boringbuilder-"));
        assert!(commands.contains("exec --workdir /workspace"));
        assert!(commands.contains("stop --time 0 boringbuilder-"));
    }

    #[test]
    fn parses_dns_servers() {
        assert_eq!(
            dns_servers_from_env(Some("1.1.1.1, 8.8.8.8")),
            vec!["1.1.1.1".to_string(), "8.8.8.8".to_string()]
        );
        assert!(dns_servers_from_env(None).is_empty());
    }

    #[test]
    fn filters_stub_dns_servers_from_scutil_output() {
        let output = r#"
resolver #1
  nameserver[0] : 127.0.2.2
  nameserver[1] : 127.0.2.3
  nameserver[2] : ::ffff:127.0.2.2
  nameserver[3] : 192.168.1.1
  nameserver[4] : 1.1.1.1
"#;

        assert_eq!(
            parse_scutil_dns_servers(output),
            vec!["192.168.1.1".to_string(), "1.1.1.1".to_string()]
        );
    }

    #[test]
    fn rejects_loopback_and_link_local_dns_servers() {
        assert!(!dns_server_usable("127.0.2.2"));
        assert!(!dns_server_usable("::1"));
        assert!(!dns_server_usable("fe80::1"));
        assert!(dns_server_usable("192.168.1.1"));
        assert!(dns_server_usable("1.1.1.1"));
    }

    #[test]
    fn prefers_env_dns_when_provided() {
        assert_eq!(
            resolved_dns_servers(Some("9.9.9.9")),
            vec!["9.9.9.9".to_string()]
        );
    }

    #[test]
    fn full_stage_rootfs_export_handles_empty_and_root_outputs() {
        use std::collections::BTreeMap;

        use crate::schema::Pipeline;

        let base = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/arm64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: Vec::new(),
            export: None,
            metadata: None,
            base_dir: std::env::temp_dir(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        assert!(should_export_full_rootfs(&base));

        let mut pipeline = base.clone();
        pipeline.outputs = vec!["/".to_string()];
        assert!(should_export_full_rootfs(&pipeline));

        pipeline.outputs = vec!["/workspace/dist".to_string()];
        assert!(!should_export_full_rootfs(&pipeline));
    }

    #[test]
    fn retries_stop_container_for_export() {
        let temp = tempdir().unwrap();
        let fake = temp.path().join("container");
        let log = temp.path().join("container.log");
        let count = temp.path().join("stop.count");
        let script = format!(
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >> {}\nif [ \"$1\" = stop ]; then\n  count=0\n  if [ -f {} ]; then count=$(cat {}); fi\n  count=$((count + 1))\n  printf '%s' \"$count\" > {}\n  if [ \"$count\" -lt 2 ]; then\n    echo 'internalError: XPC timeout for request' >&2\n    exit 1\n  fi\n  exit 0\nfi\nexit 0\n",
            shell_words::quote(log.to_str().unwrap()),
            shell_words::quote(count.to_str().unwrap()),
            shell_words::quote(count.to_str().unwrap()),
            shell_words::quote(count.to_str().unwrap()),
        );
        fs::write(&fake, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&fake).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&fake, perms).unwrap();
        }

        assert!(stop_container_for_export(&fake, "demo", "image export"));

        let commands = fs::read_to_string(log).unwrap();
        assert_eq!(commands.lines().count(), 2);
        assert!(commands.contains("stop --time 0 demo"));
    }

    #[test]
    fn retries_archive_stream_when_container_exec_is_signaled() {
        use std::collections::BTreeMap;

        use crate::schema::Pipeline;

        let temp = tempdir().unwrap();
        let fake = temp.path().join("container");
        let count = temp.path().join("exec.count");
        let fixture_root = temp.path().join("fixture");
        fs::create_dir_all(&fixture_root).unwrap();
        fs::write(fixture_root.join("hello.txt"), "hello\n").unwrap();
        let fixture_tar = temp.path().join("fixture.tar");
        let tar_file = fs::File::create(&fixture_tar).unwrap();
        let mut builder = Builder::new(tar_file);
        builder
            .append_path_with_name(fixture_root.join("hello.txt"), "out/hello.txt")
            .unwrap();
        builder.finish().unwrap();

        let script = format!(
            "#!/bin/sh\nset -eu\ncount=0\nif [ -f {} ]; then count=$(cat {}); fi\ncount=$((count + 1))\nprintf '%s' \"$count\" > {}\ncat {}\nif [ \"$count\" -lt 2 ]; then kill -TERM $$; fi\n",
            shell_words::quote(count.to_str().unwrap()),
            shell_words::quote(count.to_str().unwrap()),
            shell_words::quote(count.to_str().unwrap()),
            shell_words::quote(fixture_tar.to_str().unwrap()),
        );
        fs::write(&fake, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&fake).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&fake, perms).unwrap();
        }

        let pipeline = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/arm64".to_string(),
            workdir: "/".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: vec!["/out".to_string()],
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
        let output = temp.path().join("output.tar");

        export_container_archive_stream(&fake, "demo", &pipeline, ArchiveExportKind::Tar, &output)
            .unwrap();

        assert_eq!(fs::read_to_string(count).unwrap(), "2");
        let mut archive = tar::Archive::new(fs::File::open(output).unwrap());
        let paths = archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec![Path::new("out/hello.txt").to_path_buf()]);
    }

    #[test]
    fn retries_retryable_oci_image_save_errors() {
        let temp = tempdir().unwrap();
        let fake = temp.path().join("container");
        let log = temp.path().join("container.log");
        let count = temp.path().join("save.count");
        let fixture_root = temp.path().join("fixture");
        fs::create_dir_all(&fixture_root).unwrap();
        fs::write(fixture_root.join("index.json"), "{}").unwrap();
        fs::write(
            fixture_root.join("oci-layout"),
            "{\"imageLayoutVersion\":\"1.0.0\"}",
        )
        .unwrap();
        let fixture_tar = temp.path().join("fixture.tar");
        let tar_file = fs::File::create(&fixture_tar).unwrap();
        let mut builder = Builder::new(tar_file);
        builder
            .append_path_with_name(fixture_root.join("index.json"), "index.json")
            .unwrap();
        builder
            .append_path_with_name(fixture_root.join("oci-layout"), "oci-layout")
            .unwrap();
        builder.finish().unwrap();

        let script = format!(
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >> {}\nif [ \"$1\" = export ] && [ \"$2\" = --image ]; then\n  exit 0\nfi\nif [ \"$1\" = image ] && [ \"$2\" = save ]; then\n  output=''\n  while [ \"$#\" -gt 0 ]; do\n    if [ \"$1\" = --output ]; then\n      output=\"$2\"\n      shift 2\n      continue\n    fi\n    shift\n  done\n  count=0\n  if [ -f {} ]; then count=$(cat {}); fi\n  count=$((count + 1))\n  printf '%s' \"$count\" > {}\n  if [ \"$count\" -lt 2 ]; then\n    echo 'unable to write data to the archive, code -30' >&2\n    exit 1\n  fi\n  cp {} \"$output\"\n  exit 0\nfi\nif [ \"$1\" = image ] && [ \"$2\" = rm ]; then\n  exit 0\nfi\nexit 0\n",
            shell_words::quote(log.to_str().unwrap()),
            shell_words::quote(count.to_str().unwrap()),
            shell_words::quote(count.to_str().unwrap()),
            shell_words::quote(count.to_str().unwrap()),
            shell_words::quote(fixture_tar.to_str().unwrap()),
        );
        fs::write(&fake, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&fake).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&fake, perms).unwrap();
        }

        let output = temp.path().join("oci-out");
        export_container_as_oci_layout(&fake, "demo", "linux/arm64", &output).unwrap();

        assert!(output.join("index.json").exists());
        assert!(output.join("oci-layout").exists());

        let commands = fs::read_to_string(log).unwrap();
        let save_calls = commands
            .lines()
            .filter(|line| line.starts_with("image save "))
            .count();
        assert_eq!(save_calls, 2);
    }

    #[test]
    fn retries_retryable_oci_image_export_errors() {
        let temp = tempdir().unwrap();
        let fake = temp.path().join("container");
        let log = temp.path().join("container.log");
        let export_count = temp.path().join("export.count");
        let fixture_root = temp.path().join("fixture");
        fs::create_dir_all(&fixture_root).unwrap();
        fs::write(fixture_root.join("index.json"), "{}").unwrap();
        fs::write(
            fixture_root.join("oci-layout"),
            "{\"imageLayoutVersion\":\"1.0.0\"}",
        )
        .unwrap();
        let fixture_tar = temp.path().join("fixture.tar");
        let tar_file = fs::File::create(&fixture_tar).unwrap();
        let mut builder = Builder::new(tar_file);
        builder
            .append_path_with_name(fixture_root.join("index.json"), "index.json")
            .unwrap();
        builder
            .append_path_with_name(fixture_root.join("oci-layout"), "oci-layout")
            .unwrap();
        builder.finish().unwrap();

        let script = format!(
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >> {}\nif [ \"$1\" = export ] && [ \"$2\" = --image ]; then\n  count=0\n  if [ -f {} ]; then count=$(cat {}); fi\n  count=$((count + 1))\n  printf '%s' \"$count\" > {}\n  if [ \"$count\" -lt 2 ]; then\n    echo 'unable to write data to the archive, code -30' >&2\n    exit 1\n  fi\n  exit 0\nfi\nif [ \"$1\" = image ] && [ \"$2\" = save ]; then\n  output=''\n  while [ \"$#\" -gt 0 ]; do\n    if [ \"$1\" = --output ]; then\n      output=\"$2\"\n      shift 2\n      continue\n    fi\n    shift\n  done\n  cp {} \"$output\"\n  exit 0\nfi\nif [ \"$1\" = stop ]; then\n  exit 0\nfi\nif [ \"$1\" = image ] && [ \"$2\" = rm ]; then\n  exit 0\nfi\nexit 0\n",
            shell_words::quote(log.to_str().unwrap()),
            shell_words::quote(export_count.to_str().unwrap()),
            shell_words::quote(export_count.to_str().unwrap()),
            shell_words::quote(export_count.to_str().unwrap()),
            shell_words::quote(fixture_tar.to_str().unwrap()),
        );
        fs::write(&fake, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&fake).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&fake, perms).unwrap();
        }

        let output = temp.path().join("oci-out");
        export_container_as_oci_layout(&fake, "demo", "linux/arm64", &output).unwrap();

        assert!(output.join("index.json").exists());
        assert!(output.join("oci-layout").exists());

        let commands = fs::read_to_string(log).unwrap();
        let export_calls = commands
            .lines()
            .filter(|line| line.starts_with("export --image "))
            .count();
        assert_eq!(export_calls, 2);
    }
}
