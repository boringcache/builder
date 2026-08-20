use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail, ensure};
use flate2::read::GzDecoder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use nix::unistd::Uid;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tar::Archive;
use tempfile::TempDir;
use walkdir::WalkDir;

use crate::backend::image_cache::ImageCache;
use crate::backend::{
    ExecutionBackend, OperationCacheHooks, OperationTiming, RunOptions, RunSummary, RunTimings,
    print_timing_summary, resolve_export,
};
use crate::cache::{CacheStoreConfig, CacheStoreKind, open_cache_backend, open_cache_store};
use crate::dockerfile::context::{
    materialize_source as materialize_context_source, resolve_source as resolve_context_source,
};
use crate::export::archive::{ArchiveEntry, resolve_archive_entries};
use crate::export::docker::{export_pipeline_docker, export_pipeline_docker_from_overlay_upper};
use crate::export::oci::{export_pipeline_oci, export_pipeline_oci_from_overlay_upper};
use crate::export::tar::{export_pipeline_tar_from_rootfs, export_pipeline_tar_zst_from_rootfs};
use crate::schema::{
    CacheMode, CacheMount, ContextCopyOp, ExportFormat, Input, Operation, Pipeline, RemoteAddOp,
    StageCopyOp, Step, StepRunBindSource, StepRunMount,
};
use crate::ui;
use crate::util::fs::{copy_path, copy_path_dereferenced, path_size};
use crate::util::oci_rootfs::unpack_rootfs_from_dir_manifest;
use crate::util::process::{find_command, run_capture, run_checked, run_streaming};

#[derive(Debug, Default)]
pub struct LinuxExecBackend;

impl ExecutionBackend for LinuxExecBackend {
    fn name(&self) -> &'static str {
        "linux-exec"
    }

    fn run(
        &self,
        pipeline: &Pipeline,
        options: &RunOptions,
        cache_config: &CacheStoreConfig,
        mut cache_hooks: Option<&mut dyn OperationCacheHooks>,
    ) -> Result<RunSummary> {
        ensure!(
            cfg!(target_os = "linux"),
            "the Linux backend can only run on Linux"
        );

        let run_start = Instant::now();
        let show_timings = options.timings;

        let tools = LinuxTools::discover()?;
        let privilege = PrivilegeMode::detect(&tools.sudo)?;
        let (platform_os, platform_arch_str) = parse_platform(&pipeline.platform)?;

        let resolved_export = resolve_export(pipeline, options);
        let export_needs_image_dir = resolved_export
            .as_ref()
            .is_some_and(|(format, _)| matches!(format, ExportFormat::Oci | ExportFormat::Docker));
        let base_rootfs_backend = if !options.no_cache
            && matches!(cache_config.cache_store, CacheStoreKind::BoringCache)
        {
            Some(open_cache_backend(cache_config)?)
        } else {
            None
        };

        let t = Instant::now();
        let image_cache = ImageCache::open()?;
        let mut image_dir = image_cache.cached_image_dir(&pipeline.image, &pipeline.platform);
        let mut resolved_image_metadata = None;
        let mut image_env = BTreeMap::new();
        if image_dir.is_none() && !export_needs_image_dir && base_rootfs_backend.is_some() {
            ui::print_status(format!(
                "resolving image {} ({})",
                pipeline.image, pipeline.platform
            ));
            match crate::registry::resolve_image_metadata(
                &pipeline.image,
                platform_os,
                platform_arch_str,
            ) {
                Ok(metadata) => {
                    image_env = parse_image_env_from_config(&metadata.config);
                    resolved_image_metadata = Some(metadata);
                }
                Err(error) => {
                    ui::print_detail(format!(
                        "base rootfs cache preflight skipped {} ({}): {error:#}",
                        pipeline.image, pipeline.platform
                    ));
                }
            }
        }
        if image_dir.is_none() && resolved_image_metadata.is_none() {
            image_dir = Some(image_cache.pull_or_cached(
                &pipeline.image,
                &pipeline.platform,
                platform_os,
                platform_arch_str,
            )?);
        }
        let pull_ms = t.elapsed().as_millis();

        let t = Instant::now();
        let base_rootfs = if let Some(metadata) = resolved_image_metadata.as_ref() {
            if let Some(rootfs) = base_rootfs_backend
                .as_deref()
                .and_then(|backend| {
                    image_cache
                        .restore_rootfs_from_backend(
                            backend,
                            &metadata.manifest_digest,
                            &pipeline.image,
                            &pipeline.platform,
                        )
                        .transpose()
                })
                .transpose()?
            {
                rootfs
            } else {
                let pulled = image_cache.pull_or_cached(
                    &pipeline.image,
                    &pipeline.platform,
                    platform_os,
                    platform_arch_str,
                )?;
                let rootfs = image_cache.unpack_or_cached(
                    &pulled,
                    &pipeline.image,
                    &pipeline.platform,
                    base_rootfs_backend.as_deref(),
                    unpack_rootfs,
                )?;
                image_dir = Some(pulled);
                rootfs
            }
        } else {
            let image_dir = image_dir
                .as_deref()
                .ok_or_else(|| anyhow!("base image directory was not prepared"))?;
            image_cache.unpack_or_cached(
                image_dir,
                &pipeline.image,
                &pipeline.platform,
                base_rootfs_backend.as_deref(),
                unpack_rootfs,
            )?
        };
        let unpack_ms = t.elapsed().as_millis();

        let work_dir = tempfile::Builder::new()
            .prefix("boringbuilder-linux-")
            .tempdir()
            .context("failed to create temporary Linux runner directory")?;
        let rootfs_dir = work_dir.path().join("rootfs");
        let upper_dir = work_dir.path().join("upper");
        let overlay_work = work_dir.path().join("overlay-work");
        fs::create_dir_all(&rootfs_dir)?;
        fs::create_dir_all(&upper_dir)?;
        fs::create_dir_all(&overlay_work)?;

        let start_index = pipeline.step_start_index(options.from_step.as_deref())?;

        if image_env.is_empty()
            && let Some(image_dir) = image_dir.as_deref()
        {
            image_env = parse_image_env(image_dir);
        }
        let mut op_timings: Vec<OperationTiming> = Vec::new();
        let mut operation_ms_total = 0u128;
        let mut cache_restore_ms_total = 0u128;
        let mut cache_save_ms_total = 0u128;

        let mount_bin_path = tools.mount.clone();

        let mut env = LinuxEnv::new(
            work_dir,
            rootfs_dir.clone(),
            privilege,
            tools,
            options.keep,
            image_env,
        );
        env.mount_overlay(&base_rootfs, &upper_dir, &overlay_work, &rootfs_dir)?;

        {
            let bind_args: Vec<String> = vec![
                "--bind".to_string(),
                rootfs_dir.display().to_string(),
                rootfs_dir.display().to_string(),
            ];
            run_checked(&mount_bin_path, &bind_args)
                .context("bind-mount rootfs onto itself for pivot_root in privileged steps")?;
            let private_args: Vec<String> = vec![
                "--make-private".to_string(),
                rootfs_dir.display().to_string(),
            ];
            run_checked(&mount_bin_path, &private_args).context("make rootfs self-bind private")?;
            env.mounts.push(MountRecord::normal(rootfs_dir.clone()));
        }

        let prepare_started = Instant::now();
        env.prepare(pipeline)?;
        let prepare_ms = prepare_started.elapsed().as_millis();

        for (idx, operation) in pipeline.operations.iter().enumerate().skip(start_index) {
            let label = operation_label(operation, idx);
            let cache_prime = if let Some(hooks) = cache_hooks.as_deref_mut() {
                let prime =
                    hooks.before_operation(operation, Some(&upper_dir), Some(&rootfs_dir))?;
                cache_restore_ms_total += prime.restore_ms;
                prime
            } else {
                Default::default()
            };
            ui::print_step(
                idx + 1,
                pipeline.operations.len(),
                &label,
                cache_prime.complete,
            );
            if cache_prime.complete {
                if let Some(hooks) = cache_hooks.as_deref_mut() {
                    cache_save_ms_total += hooks.on_cached_operation(operation)?;
                }
                op_timings.push(OperationTiming {
                    label,
                    ms: 0,
                    cached: true,
                });
                continue;
            }
            let t = Instant::now();
            env.exec_operation(pipeline, operation, cache_config)?;
            env.fix_upper_permissions(&upper_dir)?;
            let op_ms = t.elapsed().as_millis();
            operation_ms_total += op_ms;
            op_timings.push(OperationTiming {
                label,
                ms: op_ms,
                cached: false,
            });
            if let Some(hooks) = cache_hooks.as_deref_mut() {
                cache_save_ms_total +=
                    hooks.after_operation(operation, Some(&upper_dir), Some(&rootfs_dir))?;
            }
        }

        let t = Instant::now();
        let export_path = if let Some((format, path)) = resolved_export {
            let full_rootfs_export = pipeline.outputs.len() == 1 && pipeline.outputs[0] == "/";
            match format {
                ExportFormat::Tar => {
                    ui::print_status(format!("exporting tar {}", path.display()));
                    Some(export_pipeline_tar_from_rootfs(
                        pipeline,
                        &path,
                        &rootfs_dir,
                    )?)
                }
                ExportFormat::TarZst => {
                    ui::print_status(format!("exporting tar.zst {}", path.display()));
                    Some(export_pipeline_tar_zst_from_rootfs(
                        pipeline,
                        &path,
                        &rootfs_dir,
                    )?)
                }
                ExportFormat::Oci => {
                    ui::print_status(format!("exporting OCI image layout {}", path.display()));
                    let image_dir = image_dir
                        .as_deref()
                        .ok_or_else(|| anyhow!("base image directory missing for OCI export"))?;
                    Some(if full_rootfs_export {
                        export_pipeline_oci_from_overlay_upper(
                            pipeline, &path, image_dir, &upper_dir,
                        )?
                    } else {
                        export_pipeline_oci(pipeline, &path, image_dir, &rootfs_dir)?
                    })
                }
                ExportFormat::Docker => {
                    ui::print_status(format!("exporting Docker image {}", path.display()));
                    let image_dir = image_dir
                        .as_deref()
                        .ok_or_else(|| anyhow!("base image directory missing for Docker export"))?;
                    Some(if full_rootfs_export {
                        export_pipeline_docker_from_overlay_upper(
                            pipeline, &path, image_dir, &upper_dir,
                        )?
                    } else {
                        export_pipeline_docker(pipeline, &path, image_dir, &rootfs_dir)?
                    })
                }
            }
        } else {
            None
        };
        let export_ms = t.elapsed().as_millis();

        // Preserve the rootfs when requested (multi-stage builds) or when
        // the user explicitly asked to keep it.
        let (rootfs_dir, _keep_alive): (Option<PathBuf>, Option<Arc<dyn Send + Sync>>) =
            if options.keep_rootfs {
                if should_snapshot_selected_stage_outputs(pipeline) {
                    let snapshot = snapshot_stage_outputs(pipeline, &env.rootfs)?;
                    let path = snapshot.path().to_path_buf();
                    (Some(path), Some(Arc::new(snapshot) as _))
                } else {
                    let rootfs_path = env.rootfs.clone();
                    let guard: Option<Arc<dyn Send + Sync>> =
                        env.take_for_stage().map(|g| Arc::new(g) as _);
                    (Some(rootfs_path), guard)
                }
            } else {
                (None, None)
            };

        if options.keep {
            ui::print_status(format!("rootfs kept at {}", env.rootfs.display()));
            env.disarm();
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
            operations: op_timings,
            ..RunTimings::default()
        };

        if show_timings {
            print_timing_summary(&timings);
        }

        Ok(RunSummary {
            container_name: None,
            export_path,
            rootfs_dir,
            _keep_alive,
            timings,
        })
    }
}

#[derive(Debug, Clone)]
pub struct LinuxDoctorStatus {
    pub mount_path: PathBuf,
    pub umount_path: PathBuf,
    pub chroot_path: PathBuf,
    pub sudo_path: Option<PathBuf>,
    pub privilege_detail: String,
}

pub fn doctor_status() -> Result<LinuxDoctorStatus> {
    let tools = LinuxTools::discover()?;
    let privilege = PrivilegeMode::detect(&tools.sudo)?;

    Ok(LinuxDoctorStatus {
        mount_path: tools.mount,
        umount_path: tools.umount,
        chroot_path: tools.chroot,
        sudo_path: tools.sudo,
        privilege_detail: privilege.describe(),
    })
}

#[derive(Debug)]
struct LinuxEnv {
    _work_dir: Option<TempDir>,
    rootfs: PathBuf,
    privilege: PrivilegeMode,
    tools: LinuxTools,
    mounts: Vec<MountRecord>,
    keep: bool,
    armed: bool,
    image_env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy)]
struct CopyOwnership {
    uid: u32,
    gid: u32,
}

#[derive(Debug)]
enum ActiveRunMountSource {
    Bind(PathBuf),
    Tmpfs { size: Option<String> },
}

impl ActiveRunMountSource {
    fn path(&self) -> Option<&Path> {
        match self {
            Self::Bind(path) => Some(path.as_path()),
            Self::Tmpfs { .. } => None,
        }
    }
}

#[derive(Debug)]
struct ActiveRunMount {
    target: PathBuf,
    source: ActiveRunMountSource,
    readonly: bool,
    cache_entry: Option<CacheMount>,
    cache_save: bool,
    env: BTreeMap<String, String>,
    _lock: Option<crate::cache::CacheLock>,
    _staging: Option<TempDir>,
}

struct CacheRunMountSpec<'a> {
    target: &'a str,
    id: &'a str,
    key: Option<&'a str>,
    restore_from: &'a [String],
    readonly: bool,
    sharing: CacheMode,
}

impl LinuxEnv {
    fn new(
        work_dir: TempDir,
        rootfs: PathBuf,
        privilege: PrivilegeMode,
        tools: LinuxTools,
        keep: bool,
        image_env: BTreeMap<String, String>,
    ) -> Self {
        Self {
            _work_dir: Some(work_dir),
            rootfs,
            privilege,
            tools,
            mounts: Vec::new(),
            keep,
            armed: true,
            image_env,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    /// Take ownership of the work directory and mounts so the rootfs survives
    /// after LinuxEnv is dropped.  Returns a guard that will properly unmount
    /// and delete the temp directory when dropped.
    fn take_for_stage(&mut self) -> Option<StageCleanup> {
        self.keep = true; // prevent mount cleanup in Drop
        let work_dir = self._work_dir.take()?;
        let mounts = std::mem::take(&mut self.mounts);
        Some(StageCleanup {
            mounts,
            privilege: self.privilege.clone(),
            umount_path: self.tools.umount.clone(),
            _work_dir: work_dir,
        })
    }

    fn prepare(&mut self, pipeline: &Pipeline) -> Result<()> {
        self.ensure_rootfs_dir("/proc", true)?;
        self.ensure_rootfs_dir("/dev", true)?;
        self.ensure_rootfs_dir("/dev/pts", true)?;
        self.ensure_rootfs_dir("/dev/shm", true)?;
        self.ensure_rootfs_dir("/sys", true)?;
        self.ensure_rootfs_dir("/tmp", true)?;
        self.ensure_rootfs_dir(&pipeline.workdir, true)?;

        let mut script_lines: Vec<String> = vec!["set -e".to_string()];
        let mount = self.tools.mount.display().to_string();
        let mkdir = self.tools.mkdir.display().to_string();
        let ln = self.tools.ln.display().to_string();
        let rm = self.tools.rm.display().to_string();
        let mknod = self.tools.mknod.display().to_string();

        script_lines.push(format!(
            "{mount} -t proc proc {}",
            self.rootfs.join("proc").display()
        ));
        self.mounts
            .push(MountRecord::normal(self.rootfs.join("proc")));

        let dev_target = self.rootfs.join("dev");
        let dev_pts_target = self.rootfs.join("dev/pts");
        let dev_shm_target = self.rootfs.join("dev/shm");
        script_lines.push(format!(
            "{mount} -t tmpfs -o nosuid,mode=755 tmpfs {}",
            dev_target.display()
        ));
        self.mounts.push(MountRecord::normal(dev_target.clone()));
        script_lines.push(format!(
            "{mkdir} -p {} {}",
            shell_words::quote(&dev_pts_target.display().to_string()),
            shell_words::quote(&dev_shm_target.display().to_string())
        ));
        script_lines.push(format!(
            "{mount} -t devpts -o newinstance,ptmxmode=0666,mode=620,gid=5 devpts {}",
            dev_pts_target.display()
        ));
        self.mounts
            .push(MountRecord::normal(dev_pts_target.clone()));
        script_lines.push(format!("chmod 1777 {}", dev_shm_target.display()));
        for (name, major, minor, mode) in [
            ("null", 1, 3, 0o666),
            ("zero", 1, 5, 0o666),
            ("full", 1, 7, 0o666),
            ("random", 1, 8, 0o666),
            ("urandom", 1, 9, 0o666),
            ("tty", 5, 0, 0o666),
        ] {
            let target = dev_target.join(name);
            script_lines.push(format!(
                "{rm} -f {}",
                shell_words::quote(&target.display().to_string())
            ));
            script_lines.push(format!(
                "{mknod} -m {:o} {} c {major} {minor}",
                mode,
                shell_words::quote(&target.display().to_string()),
            ));
        }
        for (name, target) in [
            ("ptmx", "pts/ptmx"),
            ("fd", "/proc/self/fd"),
            ("stdin", "/proc/self/fd/0"),
            ("stdout", "/proc/self/fd/1"),
            ("stderr", "/proc/self/fd/2"),
        ] {
            let link_path = dev_target.join(name);
            script_lines.push(format!(
                "{rm} -f {}",
                shell_words::quote(&link_path.display().to_string())
            ));
            script_lines.push(format!(
                "{ln} -s {} {}",
                shell_words::quote(target),
                shell_words::quote(&link_path.display().to_string())
            ));
        }

        let sys_target = self.rootfs.join("sys");
        script_lines.push(format!(
            "{mount} --rbind /sys {t} && {mount} --make-rslave {t} && {mount} -o remount,bind,ro {t}",
            t = sys_target.display()
        ));
        self.mounts.push(MountRecord {
            target: sys_target,
            recursive: true,
        });

        // Ensure /tmp is writable (matches Docker behavior).
        // Use chmod rather than tmpfs so temp files persist across steps.
        let tmp_target = self.rootfs.join("tmp");
        script_lines.push(format!("chmod 1777 {}", tmp_target.display()));

        // Ensure /etc/passwd exists (many tools including git need it).
        // Base images normally include it, but create a minimal one if missing.
        let passwd_target = self.rootfs.join("etc/passwd");
        if !passwd_target.exists() {
            fs::create_dir_all(self.rootfs.join("etc"))?;
            fs::write(
                &passwd_target,
                "root:x:0:0:root:/root:/bin/sh\nnobody:x:65534:65534:nobody:/:/sbin/nologin\n",
            )?;
        }
        let group_target = self.rootfs.join("etc/group");
        if !group_target.exists() {
            fs::write(&group_target, "root:x:0:\nnobody:x:65534:\n")?;
        }

        for file in ["/etc/resolv.conf", "/etc/hosts"] {
            let source = Path::new(file);
            if !source.exists() {
                continue;
            }
            let target = self.rootfs.join(file.trim_start_matches('/'));
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            if !target.exists() {
                File::create(&target)
                    .with_context(|| format!("failed to create {}", target.display()))?;
            }
            script_lines.push(format!(
                "{mount} --bind {} {t} && {mount} -o remount,bind,ro {t}",
                source.display(),
                t = target.display()
            ));
            self.mounts.push(MountRecord::normal(target));
        }

        for input in &pipeline.inputs {
            let target = self.rootfs.join(input.dest.trim_start_matches('/'));
            if input.source.is_dir() {
                ensure_nested_mount_targets_exist_in_source(input, &pipeline.inputs)?;
                fs::create_dir_all(&target)
                    .with_context(|| format!("failed to create {}", target.display()))?;
                script_lines.push(format!(
                    "mkdir -p {}",
                    shell_words::quote(&target.display().to_string())
                ));
            } else {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                    script_lines.push(format!(
                        "mkdir -p {}",
                        shell_words::quote(&parent.display().to_string())
                    ));
                }
                if !target.exists() {
                    File::create(&target)
                        .with_context(|| format!("failed to create {}", target.display()))?;
                }
                script_lines.push(format!(
                    "touch {}",
                    shell_words::quote(&target.display().to_string())
                ));
            }
            if input.readonly {
                script_lines.push(format!(
                    "{mount} --bind {} {t} && {mount} -o remount,bind,ro {t}",
                    input.source.display(),
                    t = target.display()
                ));
            } else {
                script_lines.push(format!(
                    "{mount} --bind {} {}",
                    input.source.display(),
                    target.display()
                ));
            }
            self.mounts.push(MountRecord::normal(target));
        }

        let script = script_lines.join("\n");
        self.run_privileged_streaming(Path::new("/bin/sh"), &["-c".to_string(), script])
            .context("failed to prepare mount environment")?;

        Ok(())
    }

    fn exec_operation(
        &self,
        pipeline: &Pipeline,
        operation: &Operation,
        cache_config: &CacheStoreConfig,
    ) -> Result<()> {
        match operation {
            Operation::Exec(step) => self.exec_step(pipeline, step, cache_config),
            Operation::CopyFromContext(op) => self.copy_from_context(pipeline, op),
            Operation::CopyFromStage(op) => self.copy_from_stage(pipeline, op),
            Operation::AddRemote(op) => self.add_remote(op),
        }
    }

    fn exec_step(
        &self,
        pipeline: &Pipeline,
        step: &Step,
        cache_config: &CacheStoreConfig,
    ) -> Result<()> {
        let workdir = step.workdir.as_deref().unwrap_or(&pipeline.workdir);
        let mounts = self.prepare_run_mounts(pipeline, step, cache_config)?;

        let mut env_vars = BTreeMap::new();
        env_vars.insert(
            "PATH".to_string(),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        );
        env_vars.insert("HOME".to_string(), "/root".to_string());
        env_vars.insert("HOSTNAME".to_string(), "boringbuilder".to_string());
        env_vars.insert("TERM".to_string(), "xterm".to_string());
        // Mark all directories as safe for git.  Docker/BuildKit achieves
        // this implicitly via uid namespaces; in a chroot+overlay the
        // ownership can look inconsistent, causing git safe.directory
        // checks to fail with exit 128.
        env_vars.insert("GIT_CONFIG_COUNT".to_string(), "1".to_string());
        env_vars.insert("GIT_CONFIG_KEY_0".to_string(), "safe.directory".to_string());
        env_vars.insert("GIT_CONFIG_VALUE_0".to_string(), "*".to_string());
        // Layer image env from the OCI config (e.g. golang image sets
        // PATH=/usr/local/go/bin:...).  These override the hardcoded
        // defaults above but are themselves overridden by pipeline/step env.
        for (key, value) in &self.image_env {
            env_vars.insert(key.clone(), value.clone());
        }
        for (key, value) in &pipeline.env {
            env_vars.insert(key.clone(), value.clone());
        }
        for (key, value) in &step.env {
            env_vars.insert(key.clone(), value.clone());
        }
        for mount in &mounts {
            for (key, value) in &mount.env {
                env_vars.insert(key.clone(), value.clone());
            }
        }

        let privileged = env_vars
            .remove(crate::schema::PRIVILEGED_STEP_ENV)
            .map(|value| value == "1")
            .unwrap_or(false);

        let mut args = vec![
            self.rootfs.display().to_string(),
            "/usr/bin/env".to_string(),
        ];
        args.push("-i".to_string());
        for (key, value) in env_vars {
            args.push(format!("{key}={value}"));
        }
        if let Some(argv) = &step.run_exec {
            args.extend(build_step_exec_args(workdir, argv));
        } else {
            let shell = parse_shell(step.shell.as_deref().unwrap_or("/bin/sh"))?;
            let script = build_step_script(workdir, &shell, &step.run);
            args.push("/bin/sh".to_string());
            args.push("-c".to_string());
            args.push(script);
        }

        let mut mounted = 0usize;
        let mount_result = (|| -> Result<()> {
            for mount in &mounts {
                self.mount_run_mount(mount)?;
                mounted += 1;
            }
            Ok(())
        })();
        if let Err(error) = mount_result {
            let _ = self.cleanup_run_mounts(&mounts[..mounted], false, cache_config);
            return Err(error);
        }

        let (program, run_args) = if privileged {
            let unshare = self.tools.unshare.clone().ok_or_else(|| {
                anyhow!("privileged step requires the 'unshare' command (util-linux)")
            })?;
            let mut wrapped = vec![
                "--mount".to_string(),
                "--propagation".to_string(),
                "private".to_string(),
                self.tools.chroot.display().to_string(),
            ];
            wrapped.extend(args);
            (unshare, wrapped)
        } else {
            (self.tools.chroot.clone(), args)
        };
        let result = self
            .run_privileged_streaming(&program, &run_args)
            .with_context(|| {
                format!(
                    "step '{}' failed",
                    step.name.as_deref().unwrap_or("<unnamed>")
                )
            });
        let cleanup_result =
            self.cleanup_run_mounts(&mounts[..mounted], result.is_ok(), cache_config);
        result?;
        cleanup_result
    }

    fn prepare_run_mounts(
        &self,
        pipeline: &Pipeline,
        step: &Step,
        cache_config: &CacheStoreConfig,
    ) -> Result<Vec<ActiveRunMount>> {
        let mut mounts = Vec::new();
        for mount in &step.run_mounts {
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
                    CacheRunMountSpec {
                        target,
                        id,
                        key: key.as_deref(),
                        restore_from,
                        readonly: *readonly,
                        sharing: *sharing,
                    },
                    cache_config,
                )?),
                StepRunMount::Bind {
                    target,
                    source,
                    readonly,
                } => mounts.push(self.prepare_bind_run_mount(pipeline, target, source, *readonly)?),
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
        mounts.sort_by_key(|mount| mount.target.components().count());
        Ok(mounts)
    }

    fn prepare_cache_run_mount(
        &self,
        pipeline: &Pipeline,
        spec: CacheRunMountSpec<'_>,
        cache_config: &CacheStoreConfig,
    ) -> Result<ActiveRunMount> {
        let target_path = self.rootfs.join(spec.target.trim_start_matches('/'));
        let staging = tempfile::Builder::new()
            .prefix("boringbuilder-run-cache-")
            .tempdir()
            .context("failed to create temporary RUN cache mount directory")?;
        let source = staging.path().join("mount");
        fs::create_dir_all(&source)
            .with_context(|| format!("failed to create {}", source.display()))?;
        let entry = CacheMount {
            id: format!("dockerfile-run-cache-{}", crate::cache::cache_tag(spec.id)),
            path: spec.target.to_string(),
            key: spec
                .key
                .map(str::to_string)
                .unwrap_or_else(|| run_mount_cache_key(&pipeline.platform, spec.id)),
            restore_from: spec.restore_from.to_vec(),
            mode: spec.sharing,
        };
        let lock = if spec.sharing == CacheMode::Locked {
            Some(open_cache_store(cache_config)?.lock(&entry)?)
        } else {
            None
        };
        open_cache_store(cache_config)?.restore(&entry, &source)?;
        Ok(ActiveRunMount {
            target: target_path,
            source: ActiveRunMountSource::Bind(source),
            readonly: spec.readonly,
            cache_entry: Some(entry),
            cache_save: !spec.readonly,
            env: BTreeMap::new(),
            _lock: lock,
            _staging: Some(staging),
        })
    }

    fn prepare_bind_run_mount(
        &self,
        pipeline: &Pipeline,
        target: &str,
        source: &StepRunBindSource,
        readonly: bool,
    ) -> Result<ActiveRunMount> {
        let target_path = self.rootfs.join(target.trim_start_matches('/'));
        match source {
            StepRunBindSource::Context { path } => {
                let context = pipeline.docker_context.as_ref().ok_or_else(|| {
                    anyhow!("missing Docker build context for RUN --mount bind source")
                })?;
                let staging = tempfile::Builder::new()
                    .prefix("boringbuilder-run-bind-")
                    .tempdir()
                    .context("failed to create temporary RUN bind mount directory")?;
                let source_path = staging.path().join("source");
                materialize_context_source(context, path, &source_path)?;
                Ok(ActiveRunMount {
                    target: target_path,
                    source: ActiveRunMountSource::Bind(source_path),
                    readonly,
                    cache_entry: None,
                    cache_save: false,
                    env: BTreeMap::new(),
                    _lock: None,
                    _staging: Some(staging),
                })
            }
            StepRunBindSource::Stage { stage, path } => {
                let stage_source = resolve_stage_run_mount_source(pipeline, stage, path)?;
                if readonly {
                    return Ok(ActiveRunMount {
                        target: target_path,
                        source: ActiveRunMountSource::Bind(stage_source),
                        readonly,
                        cache_entry: None,
                        cache_save: false,
                        env: BTreeMap::new(),
                        _lock: None,
                        _staging: None,
                    });
                }

                let staging = tempfile::Builder::new()
                    .prefix("boringbuilder-run-bind-")
                    .tempdir()
                    .context("failed to create temporary writable RUN bind source")?;
                let source_path = staging.path().join("source");
                copy_path(&stage_source, &source_path).with_context(|| {
                    format!(
                        "failed to materialize writable RUN bind source {}",
                        stage_source.display()
                    )
                })?;
                Ok(ActiveRunMount {
                    target: target_path,
                    source: ActiveRunMountSource::Bind(source_path),
                    readonly,
                    cache_entry: None,
                    cache_save: false,
                    env: BTreeMap::new(),
                    _lock: None,
                    _staging: Some(staging),
                })
            }
        }
    }

    fn prepare_tmpfs_run_mount(&self, target: &str, size: Option<&str>) -> Result<ActiveRunMount> {
        Ok(ActiveRunMount {
            target: self.rootfs.join(target.trim_start_matches('/')),
            source: ActiveRunMountSource::Tmpfs {
                size: size.map(str::to_string),
            },
            readonly: false,
            cache_entry: None,
            cache_save: false,
            env: BTreeMap::new(),
            _lock: None,
            _staging: None,
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

        let staging = tempfile::Builder::new()
            .prefix("boringbuilder-run-secret-")
            .tempdir()
            .context("failed to create temporary RUN secret mount directory")?;
        let source_path = staging.path().join("secret");
        fs::write(&source_path, &secret.bytes)
            .with_context(|| format!("failed to write {}", source_path.display()))?;
        fs::set_permissions(
            &source_path,
            fs::Permissions::from_mode(mode.unwrap_or(0o400)),
        )
        .with_context(|| format!("failed to chmod {}", source_path.display()))?;
        self.run_privileged_checked(
            &self.tools.chown,
            &[
                format!("{}:{}", uid.unwrap_or(0), gid.unwrap_or(0)),
                source_path.display().to_string(),
            ],
        )
        .with_context(|| format!("failed to chown {}", source_path.display()))?;

        let mut mount_env = BTreeMap::new();
        if let Some(env_name) = env_name {
            mount_env.insert(env_name.to_string(), secret.env_value);
        }
        Ok(Some(ActiveRunMount {
            target: self.rootfs.join(target.trim_start_matches('/')),
            source: ActiveRunMountSource::Bind(source_path),
            readonly: true,
            cache_entry: None,
            cache_save: false,
            env: mount_env,
            _lock: None,
            _staging: Some(staging),
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
        let Some(source_path) = resolve_ssh_run_mount_source(id)? else {
            if required {
                bail!(
                    "RUN --mount=type=ssh,id={} is required but no SSH agent socket is available; set BORINGBUILDER_SSH_AUTH_SOCK_{} or SSH_AUTH_SOCK",
                    id,
                    mount_id_env_suffix(id)
                );
            }
            return Ok(None);
        };

        let metadata = fs::metadata(&source_path).with_context(|| {
            format!("failed to stat SSH agent socket {}", source_path.display())
        })?;
        ensure!(
            metadata.file_type().is_socket(),
            "RUN --mount=type=ssh,id={} requires a Unix socket, got {}",
            id,
            source_path.display()
        );

        let mut mount_env = BTreeMap::new();
        mount_env.insert("SSH_AUTH_SOCK".to_string(), target.to_string());
        Ok(Some(ActiveRunMount {
            target: self.rootfs.join(target.trim_start_matches('/')),
            source: ActiveRunMountSource::Bind(source_path),
            readonly: true,
            cache_entry: None,
            cache_save: false,
            env: mount_env,
            _lock: None,
            _staging: None,
        }))
    }

    fn mount_run_mount(&self, mount: &ActiveRunMount) -> Result<()> {
        let mount_bin_display = self.tools.mount.display().to_string();
        let target_display = mount.target.display().to_string();
        let mount_bin = shell_words::quote(&mount_bin_display);
        let target = shell_words::quote(&target_display);
        let script = match &mount.source {
            ActiveRunMountSource::Bind(source_path) => {
                if source_path.is_dir() {
                    fs::create_dir_all(&mount.target)
                        .with_context(|| format!("failed to create {}", mount.target.display()))?;
                } else {
                    if let Some(parent) = mount.target.parent() {
                        fs::create_dir_all(parent)
                            .with_context(|| format!("failed to create {}", parent.display()))?;
                    }
                    if !mount.target.exists() {
                        File::create(&mount.target).with_context(|| {
                            format!("failed to create {}", mount.target.display())
                        })?;
                    }
                }

                let source_display = source_path.display().to_string();
                let source = shell_words::quote(&source_display);
                let mut script = format!("{mount_bin} --bind {source} {target}");
                if mount.readonly {
                    script.push_str(&format!(" && {mount_bin} -o remount,bind,ro {target}"));
                }
                script
            }
            ActiveRunMountSource::Tmpfs { size } => {
                fs::create_dir_all(&mount.target)
                    .with_context(|| format!("failed to create {}", mount.target.display()))?;
                let mut options = vec!["nosuid".to_string()];
                if let Some(size) = size {
                    options.push(format!("size={size}"));
                }
                format!(
                    "{mount_bin} -t tmpfs -o {} tmpfs {target}",
                    shell_words::quote(&options.join(","))
                )
            }
        };
        self.run_privileged_streaming(Path::new("/bin/sh"), &["-c".to_string(), script])
            .with_context(|| format!("failed to mount RUN source at {}", mount.target.display()))
    }

    fn cleanup_run_mounts(
        &self,
        mounts: &[ActiveRunMount],
        save_cache: bool,
        cache_config: &CacheStoreConfig,
    ) -> Result<()> {
        let mut first_error: Option<anyhow::Error> = None;

        for mount in mounts.iter().rev() {
            let unmount = self
                .run_privileged_checked(&self.tools.umount, &[mount.target.display().to_string()])
                .with_context(|| format!("failed to unmount {}", mount.target.display()));
            if let Err(error) = unmount
                && first_error.is_none()
            {
                first_error = Some(error);
            }

            if save_cache
                && mount.cache_save
                && let Some(entry) = &mount.cache_entry
            {
                let source = mount
                    .source
                    .path()
                    .expect("cache-backed RUN mount should have a source path");
                let save_result = open_cache_store(cache_config)?
                    .save(entry, source)
                    .with_context(|| format!("failed to save RUN cache mount {}", entry.id));
                if let Err(error) = save_result
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

    fn copy_from_context(&self, pipeline: &Pipeline, op: &ContextCopyOp) -> Result<()> {
        let context = pipeline
            .docker_context
            .as_ref()
            .ok_or_else(|| anyhow!("missing Docker build context for COPY operation"))?;
        let ownership = resolve_copy_ownership(&self.rootfs, op.chown.as_deref())?;
        let exclude_matcher = compile_copy_exclude_matcher(&op.exclude)?;

        for source in &op.sources {
            let (lookup_source, preserve_path) = decode_copy_parent_source(source)?;
            let (relative, absolute) = resolve_context_source(context, &lookup_source)?;
            let dest = if op.preserve_parents {
                join_copy_destination(&op.dest, &preserve_path)
            } else {
                op.dest.clone()
            };
            if op.extract_archives && try_extract_supported_archive(&absolute, &self.rootfs, &dest)?
            {
                continue;
            }
            let copied = if let Some(exclude_matcher) = &exclude_matcher {
                self.copy_host_path_with_excludes(
                    &absolute,
                    &relative,
                    &dest,
                    false,
                    !op.preserve_parents && op.sources.len() > 1,
                    exclude_matcher,
                )?
            } else {
                self.copy_host_path(
                    &absolute,
                    &dest,
                    false,
                    !op.preserve_parents && op.sources.len() > 1,
                    true,
                )?
            };
            self.apply_copy_attributes(&copied, ownership, op.chmod.as_deref())?;
        }

        Ok(())
    }

    fn copy_from_stage(&self, pipeline: &Pipeline, op: &StageCopyOp) -> Result<()> {
        let stage_root = pipeline
            .inputs
            .iter()
            .find(|input| input.dest == format!("/boringbuilder-stages/{}", op.stage))
            .map(|input| input.source.clone())
            .ok_or_else(|| anyhow!("missing mounted stage rootfs for '{}'", op.stage))?;
        let ownership = resolve_copy_ownership(&self.rootfs, op.chown.as_deref())?;
        let exclude_matcher = compile_copy_exclude_matcher(&op.exclude)?;

        for source in &op.sources {
            let (lookup_source, preserve_path) = decode_copy_parent_source(source)?;
            let relative = normalize_container_source_path(&lookup_source)?;
            let absolute = stage_root.join(&relative);
            let dest = if op.preserve_parents {
                join_copy_destination(&op.dest, &preserve_path)
            } else {
                op.dest.clone()
            };
            fs::symlink_metadata(&absolute).with_context(|| {
                format!(
                    "stage source '{}' was not found in {}",
                    lookup_source,
                    stage_root.display()
                )
            })?;
            let copied = if let Some(exclude_matcher) = &exclude_matcher {
                self.copy_host_path_with_excludes(
                    &absolute,
                    &relative,
                    &dest,
                    op.follow_symlinks,
                    !op.preserve_parents && op.sources.len() > 1,
                    exclude_matcher,
                )?
            } else {
                self.copy_host_path(
                    &absolute,
                    &dest,
                    op.follow_symlinks,
                    !op.preserve_parents && op.sources.len() > 1,
                    false,
                )?
            };
            self.apply_copy_attributes(&copied, ownership, op.chmod.as_deref())?;
        }

        Ok(())
    }

    fn add_remote(&self, op: &RemoteAddOp) -> Result<()> {
        let target = resolve_remote_dest(&self.rootfs, &op.dest, &op.url)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut reader = reqwest::blocking::get(&op.url)
            .with_context(|| format!("failed to download {}", op.url))?
            .error_for_status()
            .with_context(|| format!("failed to download {}", op.url))?;
        let mut file = File::create(&target)
            .with_context(|| format!("failed to create {}", target.display()))?;
        std::io::copy(&mut reader, &mut file)
            .with_context(|| format!("failed to write {}", target.display()))?;
        file.flush()
            .with_context(|| format!("failed to flush {}", target.display()))?;
        if let Some(checksum) = op.checksum.as_deref() {
            verify_download_checksum(&target, checksum)?;
        }

        Ok(())
    }

    fn copy_host_path(
        &self,
        source: &Path,
        dest: &str,
        follow_symlinks: bool,
        multi_source: bool,
        _source_contents_only: bool,
    ) -> Result<Vec<PathBuf>> {
        let metadata = fs::symlink_metadata(source)
            .with_context(|| format!("failed to stat {}", source.display()))?;
        let root_target = self.rootfs.join(dest.trim_start_matches('/'));
        if metadata.is_dir() {
            fs::create_dir_all(&root_target)
                .with_context(|| format!("failed to create {}", root_target.display()))?;
            let source_contents = format!("{}/.", source.display());
            let args = vec![
                if follow_symlinks {
                    "-aL".to_string()
                } else {
                    "-a".to_string()
                },
                source_contents,
                root_target.display().to_string(),
            ];
            self.run_privileged_checked(&self.tools.cp, &args)
                .with_context(|| format!("failed to copy directory {}", source.display()))?;
            return copied_directory_roots(source, &root_target);
        }

        let final_target = if multi_source || dest.ends_with('/') || root_target.is_dir() {
            fs::create_dir_all(&root_target)
                .with_context(|| format!("failed to create {}", root_target.display()))?;
            root_target.join(
                source
                    .file_name()
                    .ok_or_else(|| anyhow!("source '{}' has no file name", source.display()))?,
            )
        } else {
            if let Some(parent) = root_target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            root_target
        };

        let args = vec![
            if follow_symlinks {
                "-aL".to_string()
            } else {
                "-a".to_string()
            },
            source.display().to_string(),
            final_target.display().to_string(),
        ];
        self.run_privileged_checked(&self.tools.cp, &args)
            .with_context(|| format!("failed to copy {}", source.display()))?;
        Ok(vec![final_target])
    }

    fn copy_host_path_with_excludes(
        &self,
        source: &Path,
        source_prefix: &Path,
        dest: &str,
        follow_symlinks: bool,
        multi_source: bool,
        exclude_matcher: &Gitignore,
    ) -> Result<Vec<PathBuf>> {
        copy_host_path_with_excludes(
            source,
            source_prefix,
            &self.rootfs,
            dest,
            follow_symlinks,
            multi_source,
            exclude_matcher,
        )
    }

    fn apply_copy_attributes(
        &self,
        copied_roots: &[PathBuf],
        ownership: CopyOwnership,
        chmod: Option<&str>,
    ) -> Result<()> {
        for root in copied_roots {
            self.run_privileged_checked(
                &self.tools.chown,
                &[
                    "-hR".to_string(),
                    format!("{}:{}", ownership.uid, ownership.gid),
                    root.display().to_string(),
                ],
            )
            .with_context(|| format!("failed to chown {}", root.display()))?;

            if let Some(mode) = chmod {
                self.run_privileged_checked(
                    &self.tools.chmod,
                    &[
                        "-R".to_string(),
                        mode.to_string(),
                        root.display().to_string(),
                    ],
                )
                .with_context(|| format!("failed to chmod {} to {}", root.display(), mode))?;
            }
        }

        Ok(())
    }

    fn mount_overlay(
        &mut self,
        lower: &Path,
        upper: &Path,
        work: &Path,
        target: &Path,
    ) -> Result<()> {
        let opts = format!(
            "lowerdir={},upperdir={},workdir={}",
            lower.display(),
            upper.display(),
            work.display()
        );
        self.run_privileged_checked(
            &self.tools.mount,
            &[
                "-t".to_string(),
                "overlay".to_string(),
                "overlay".to_string(),
                "-o".to_string(),
                opts,
                target.display().to_string(),
            ],
        )?;
        self.mounts.push(MountRecord::normal(target.to_path_buf()));
        Ok(())
    }

    fn ensure_rootfs_dir(&self, path: &str, create: bool) -> Result<()> {
        let target = self.rootfs.join(path.trim_start_matches('/'));
        if create {
            fs::create_dir_all(&target)
                .with_context(|| format!("failed to create {}", target.display()))?;
        }
        Ok(())
    }

    /// After a chroot step runs as root, files in the overlay upper dir are
    /// owned by root.  Make them world-readable so the unprivileged host
    /// process can archive them for step-cache and export.
    fn fix_upper_permissions(&self, upper_dir: &Path) -> Result<()> {
        if matches!(self.privilege, PrivilegeMode::Direct) {
            return Ok(()); // already root, no permission issue
        }
        let script = format!(
            "chmod -R a+rX {}",
            shell_words::quote(&upper_dir.display().to_string())
        );
        self.run_privileged_checked(Path::new("/bin/sh"), &["-c".to_string(), script])
            .context("failed to fix upper dir permissions after step")
    }

    fn run_privileged_checked(&self, program: &Path, args: &[String]) -> Result<()> {
        let (program, args) = self.privilege.wrap(program, args);
        run_checked(&program, &args)
    }

    fn run_privileged_streaming(&self, program: &Path, args: &[String]) -> Result<()> {
        let (program, args) = self.privilege.wrap(program, args);
        run_streaming(&program, &args)
    }
}

fn ensure_nested_mount_targets_exist_in_source(parent: &Input, inputs: &[Input]) -> Result<()> {
    let parent_dest = Path::new(&parent.dest);
    for nested in inputs {
        if nested.dest == parent.dest {
            continue;
        }
        let nested_dest = Path::new(&nested.dest);
        let Ok(relative) = nested_dest.strip_prefix(parent_dest) else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        let nested_source_path = parent.source.join(relative);
        if nested.source.is_dir() {
            if nested_source_path.exists() && !nested_source_path.is_dir() {
                bail!(
                    "nested mount point {} must be a directory inside {}",
                    nested_source_path.display(),
                    parent.source.display()
                );
            }
            fs::create_dir_all(&nested_source_path).with_context(|| {
                format!(
                    "failed to create nested mount point {}",
                    nested_source_path.display()
                )
            })?;
        } else {
            if let Some(parent_dir) = nested_source_path.parent() {
                fs::create_dir_all(parent_dir).with_context(|| {
                    format!(
                        "failed to create nested mount parent {}",
                        parent_dir.display()
                    )
                })?;
            }
            if nested_source_path.exists() && nested_source_path.is_dir() {
                bail!(
                    "nested mount point {} must be a file inside {}",
                    nested_source_path.display(),
                    parent.source.display()
                );
            }
            if !nested_source_path.exists() {
                File::create(&nested_source_path).with_context(|| {
                    format!(
                        "failed to create nested mount point {}",
                        nested_source_path.display()
                    )
                })?;
            }
        }
    }
    Ok(())
}

impl Drop for LinuxEnv {
    fn drop(&mut self) {
        if !self.armed || self.keep {
            return;
        }

        for mount in self.mounts.iter().rev() {
            let args = if mount.recursive {
                vec!["-R".to_string(), mount.target.display().to_string()]
            } else {
                vec![mount.target.display().to_string()]
            };
            let (program, wrapped_args) = self.privilege.wrap(&self.tools.umount, &args);
            let _ = Command::new(program)
                .args(wrapped_args.iter().map(OsStr::new))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// Keeps a stage's rootfs alive (overlay mounts + temp directory) and
/// performs proper cleanup (unmount then delete) when dropped.
struct StageCleanup {
    mounts: Vec<MountRecord>,
    privilege: PrivilegeMode,
    umount_path: PathBuf,
    _work_dir: TempDir,
}

impl Drop for StageCleanup {
    fn drop(&mut self) {
        for mount in self.mounts.iter().rev() {
            let args = if mount.recursive {
                vec!["-R".to_string(), mount.target.display().to_string()]
            } else {
                vec![mount.target.display().to_string()]
            };
            let (program, wrapped_args) = self.privilege.wrap(&self.umount_path, &args);
            let _ = Command::new(program)
                .args(wrapped_args.iter().map(OsStr::new))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        // TempDir::drop will now clean up the directory since mounts are gone
    }
}

#[derive(Debug, Clone)]
struct LinuxTools {
    mount: PathBuf,
    umount: PathBuf,
    chroot: PathBuf,
    cp: PathBuf,
    chown: PathBuf,
    chmod: PathBuf,
    mkdir: PathBuf,
    ln: PathBuf,
    rm: PathBuf,
    mknod: PathBuf,
    unshare: Option<PathBuf>,
    sudo: Option<PathBuf>,
}

impl LinuxTools {
    fn discover() -> Result<Self> {
        Ok(Self {
            mount: require_command("mount")?,
            umount: require_command("umount")?,
            chroot: require_command("chroot")?,
            cp: require_command("cp")?,
            chown: require_command("chown")?,
            chmod: require_command("chmod")?,
            mkdir: require_command("mkdir")?,
            ln: require_command("ln")?,
            rm: require_command("rm")?,
            mknod: require_command("mknod")?,
            unshare: find_command("unshare"),
            sudo: find_command("sudo"),
        })
    }
}

#[derive(Debug, Clone)]
enum PrivilegeMode {
    Direct,
    Sudo(PathBuf),
}

impl PrivilegeMode {
    fn detect(sudo_path: &Option<PathBuf>) -> Result<Self> {
        if Uid::effective().is_root() {
            return Ok(Self::Direct);
        }

        let Some(sudo) = sudo_path else {
            bail!("root privileges are required; rerun as root or install sudo");
        };

        let output = run_capture(sudo, &["-n".to_string(), "true".to_string()])
            .context("failed to probe sudo")?;
        if output.status.success() {
            return Ok(Self::Sudo(sudo.clone()));
        }

        bail!("root privileges are required; rerun with sudo or configure passwordless sudo")
    }

    fn wrap(&self, program: &Path, args: &[String]) -> (PathBuf, Vec<String>) {
        match self {
            Self::Direct => (program.to_path_buf(), args.to_vec()),
            Self::Sudo(sudo) => {
                let mut wrapped = vec!["-n".to_string(), program.display().to_string()];
                wrapped.extend(args.iter().cloned());
                (sudo.clone(), wrapped)
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Direct => "running as root".to_string(),
            Self::Sudo(path) => format!("sudo available via {}", path.display()),
        }
    }
}

#[derive(Debug, Clone)]
struct MountRecord {
    target: PathBuf,
    recursive: bool,
}

impl MountRecord {
    fn normal(target: PathBuf) -> Self {
        Self {
            target,
            recursive: false,
        }
    }
}

#[derive(Debug, Deserialize)]
struct DirManifest {
    config: Option<ConfigDescriptor>,
}

#[derive(Debug, Deserialize)]
struct ConfigDescriptor {
    digest: String,
}

/// Read the `Env` array from the OCI image config blob and return it as
/// key-value pairs.  Returns an empty map if the config cannot be read or
/// does not contain an `Env` section.
fn parse_image_env(image_dir: &Path) -> BTreeMap<String, String> {
    let manifest_path = image_dir.join("manifest.json");
    let manifest: DirManifest = match File::open(&manifest_path)
        .ok()
        .and_then(|f| serde_json::from_reader(f).ok())
    {
        Some(m) => m,
        None => return BTreeMap::new(),
    };

    let config_digest = match &manifest.config {
        Some(c) => &c.digest,
        None => return BTreeMap::new(),
    };
    let config_hash = config_digest
        .strip_prefix("sha256:")
        .unwrap_or(config_digest);
    let config_path = image_dir.join(config_hash);

    let config: serde_json::Value = match File::open(&config_path)
        .ok()
        .and_then(|f| serde_json::from_reader(f).ok())
    {
        Some(c) => c,
        None => return BTreeMap::new(),
    };

    parse_image_env_from_config(&config)
}

fn parse_image_env_from_config(config: &serde_json::Value) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();

    if let Some(env_array) = config.pointer("/config/Env").and_then(|v| v.as_array()) {
        for entry in env_array {
            if let Some(s) = entry.as_str()
                && let Some((key, value)) = s.split_once('=')
            {
                env.insert(key.to_string(), value.to_string());
            }
        }
    }

    env
}

fn unpack_rootfs(image_dir: &Path, rootfs_dir: &Path) -> Result<()> {
    unpack_rootfs_from_dir_manifest(image_dir, rootfs_dir)
}

fn operation_label(operation: &Operation, index: usize) -> String {
    operation
        .name()
        .map(str::to_string)
        .unwrap_or_else(|| format!("step-{}", index + 1))
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
    let relative = normalize_container_source_path(source)?;
    Ok(if relative.as_os_str().is_empty() {
        stage_root
    } else {
        stage_root.join(relative)
    })
}

fn normalize_container_source_path(path: &str) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Prefix(_) => bail!("container source path is invalid: {path}"),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => bail!("container source path escapes its root: {path}"),
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn normalize_copy_parent_relative(path: &str) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Prefix(_) => bail!("COPY source path is invalid: {path}"),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => bail!("COPY source path escapes its root: {path}"),
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn decode_copy_parent_source(source: &str) -> Result<(String, PathBuf)> {
    if let Some(rest) = source.strip_prefix("/./") {
        return Ok((format!("/{rest}"), normalize_copy_parent_relative(rest)?));
    }
    if let Some(rest) = source.strip_prefix("./") {
        return Ok((rest.to_string(), normalize_copy_parent_relative(rest)?));
    }
    if let Some((prefix, suffix)) = source.split_once("/./") {
        let lookup = if prefix == "/" {
            format!("/{suffix}")
        } else {
            format!("{prefix}/{suffix}")
        };
        return Ok((lookup, normalize_copy_parent_relative(suffix)?));
    }
    Ok((source.to_string(), normalize_copy_parent_relative(source)?))
}

fn join_copy_destination(dest: &str, preserve_path: &Path) -> String {
    if preserve_path.as_os_str().is_empty() {
        return dest.to_string();
    }
    Path::new(dest)
        .join(preserve_path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn copied_directory_roots(source: &Path, target_root: &Path) -> Result<Vec<PathBuf>> {
    let mut copied = fs::read_dir(source)?
        .map(|entry| entry.map(|entry| target_root.join(entry.file_name())))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    copied.sort();
    if copied.is_empty() {
        copied.push(target_root.to_path_buf());
    }
    Ok(copied)
}

fn compile_copy_exclude_matcher(patterns: &[String]) -> Result<Option<Gitignore>> {
    if patterns.is_empty() {
        return Ok(None);
    }

    let mut builder = GitignoreBuilder::new("/");
    for pattern in patterns {
        builder
            .add_line(None, pattern)
            .with_context(|| format!("invalid COPY --exclude pattern: {pattern}"))?;
    }
    let matcher = builder
        .build()
        .map_err(|err| anyhow!("failed to compile COPY --exclude patterns: {err}"))?;
    Ok(Some(matcher))
}

fn copy_host_path_with_excludes(
    source: &Path,
    source_prefix: &Path,
    rootfs: &Path,
    dest: &str,
    follow_symlinks: bool,
    multi_source: bool,
    exclude_matcher: &Gitignore,
) -> Result<Vec<PathBuf>> {
    let metadata = if follow_symlinks {
        fs::metadata(source)
    } else {
        fs::symlink_metadata(source)
    }
    .with_context(|| format!("failed to stat {}", source.display()))?;
    let root_target = rootfs.join(dest.trim_start_matches('/'));
    let normalized_prefix = normalize_copy_exclude_prefix(source_prefix);

    if metadata.is_dir() {
        fs::create_dir_all(&root_target)
            .with_context(|| format!("failed to create {}", root_target.display()))?;
        return copy_directory_contents_with_excludes(
            source,
            &root_target,
            &normalized_prefix,
            follow_symlinks,
            exclude_matcher,
        );
    }

    let file_name = source
        .file_name()
        .ok_or_else(|| anyhow!("source '{}' has no file name", source.display()))?;
    let relative = PathBuf::from(file_name);
    if copy_path_is_excluded(&relative, &normalized_prefix, false, exclude_matcher) {
        return Ok(Vec::new());
    }

    let final_target = if multi_source || dest.ends_with('/') || root_target.is_dir() {
        fs::create_dir_all(&root_target)
            .with_context(|| format!("failed to create {}", root_target.display()))?;
        root_target.join(file_name)
    } else {
        if let Some(parent) = root_target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        root_target
    };

    copy_entry_preserving_mode(source, &final_target, follow_symlinks)?;
    Ok(vec![final_target])
}

fn normalize_copy_exclude_prefix(source_prefix: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in source_prefix.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {}
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn copy_directory_contents_with_excludes(
    source: &Path,
    target_root: &Path,
    source_prefix: &Path,
    follow_symlinks: bool,
    exclude_matcher: &Gitignore,
) -> Result<Vec<PathBuf>> {
    let mut copied_roots = std::collections::BTreeSet::new();
    let walker = WalkDir::new(source)
        .follow_links(follow_symlinks)
        .into_iter()
        .filter_entry(|entry| {
            if entry.path() == source {
                return true;
            }
            let Ok(relative) = entry.path().strip_prefix(source) else {
                return true;
            };
            !copy_path_is_excluded(
                relative,
                source_prefix,
                entry.file_type().is_dir(),
                exclude_matcher,
            )
        });

    for entry in walker {
        let entry = entry?;
        let path = entry.path();
        if path == source {
            continue;
        }

        let relative = path
            .strip_prefix(source)
            .with_context(|| format!("failed to strip prefix from {}", path.display()))?;
        let target = target_root.join(relative);
        let metadata = if follow_symlinks {
            fs::metadata(path)
        } else {
            fs::symlink_metadata(path)
        }
        .with_context(|| format!("failed to stat {}", path.display()))?;

        if let Some(first_component) = relative.components().next() {
            copied_roots.insert(target_root.join(first_component.as_os_str()));
        } else {
            copied_roots.insert(target_root.to_path_buf());
        }

        if metadata.is_dir() {
            fs::create_dir_all(&target)
                .with_context(|| format!("failed to create {}", target.display()))?;
            #[cfg(unix)]
            fs::set_permissions(&target, metadata.permissions())
                .with_context(|| format!("failed to set permissions on {}", target.display()))?;
            continue;
        }

        copy_entry_preserving_mode(path, &target, follow_symlinks)?;
    }

    if copied_roots.is_empty() {
        copied_roots.insert(target_root.to_path_buf());
    }
    Ok(copied_roots.into_iter().collect())
}

fn copy_entry_preserving_mode(
    source: &Path,
    destination: &Path,
    follow_symlinks: bool,
) -> Result<()> {
    let metadata = if follow_symlinks {
        fs::metadata(source)
    } else {
        fs::symlink_metadata(source)
    }
    .with_context(|| format!("failed to stat {}", source.display()))?;

    if metadata.file_type().is_symlink() && !follow_symlinks {
        #[cfg(unix)]
        {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            symlink(
                fs::read_link(source)
                    .with_context(|| format!("failed to read symlink {}", source.display()))?,
                destination,
            )
            .with_context(|| {
                format!(
                    "failed to create symlink {} from {}",
                    destination.display(),
                    source.display()
                )
            })?;
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            bail!("symlink-preserving copy is only supported on unix");
        }
    }

    if metadata.is_dir() {
        fs::create_dir_all(destination)
            .with_context(|| format!("failed to create {}", destination.display()))?;
        #[cfg(unix)]
        fs::set_permissions(destination, metadata.permissions())
            .with_context(|| format!("failed to set permissions on {}", destination.display()))?;
        return Ok(());
    }

    if !metadata.is_file() {
        return Ok(());
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::copy(source, destination).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    #[cfg(unix)]
    fs::set_permissions(destination, metadata.permissions())
        .with_context(|| format!("failed to set permissions on {}", destination.display()))?;
    Ok(())
}

fn copy_path_is_excluded(
    relative: &Path,
    source_prefix: &Path,
    is_dir: bool,
    exclude_matcher: &Gitignore,
) -> bool {
    if exclude_matcher
        .matched_path_or_any_parents(relative, is_dir)
        .is_ignore()
    {
        return true;
    }
    if source_prefix.as_os_str().is_empty() {
        return false;
    }
    let prefixed = source_prefix.join(relative);
    exclude_matcher
        .matched_path_or_any_parents(&prefixed, is_dir)
        .is_ignore()
}

fn resolve_copy_ownership(rootfs: &Path, spec: Option<&str>) -> Result<CopyOwnership> {
    let Some(spec) = spec.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(CopyOwnership { uid: 0, gid: 0 });
    };

    let (user, group) = match spec.split_once(':') {
        Some((user, group)) => (user.trim(), Some(group.trim())),
        None => (spec, None),
    };
    ensure!(!user.is_empty(), "COPY --chown requires a user or uid");
    let uid = resolve_user_id(rootfs, user)?;
    let gid = match group.filter(|value| !value.is_empty()) {
        Some(group) => resolve_group_id(rootfs, group)?,
        None => uid,
    };
    Ok(CopyOwnership { uid, gid })
}

fn resolve_user_id(rootfs: &Path, value: &str) -> Result<u32> {
    if let Ok(uid) = value.parse::<u32>() {
        return Ok(uid);
    }

    let passwd_path = rootfs.join("etc/passwd");
    let contents = fs::read_to_string(&passwd_path)
        .with_context(|| format!("failed to read {}", passwd_path.display()))?;
    for line in contents.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 3 && parts[0] == value {
            return parts[2].parse::<u32>().with_context(|| {
                format!(
                    "invalid uid for user '{}' in {}",
                    value,
                    passwd_path.display()
                )
            });
        }
    }

    bail!(
        "COPY --chown user '{}' was not found in {}",
        value,
        passwd_path.display()
    )
}

fn resolve_group_id(rootfs: &Path, value: &str) -> Result<u32> {
    if let Ok(gid) = value.parse::<u32>() {
        return Ok(gid);
    }

    let group_path = rootfs.join("etc/group");
    let contents = fs::read_to_string(&group_path)
        .with_context(|| format!("failed to read {}", group_path.display()))?;
    for line in contents.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 3 && parts[0] == value {
            return parts[2].parse::<u32>().with_context(|| {
                format!(
                    "invalid gid for group '{}' in {}",
                    value,
                    group_path.display()
                )
            });
        }
    }

    bail!(
        "COPY --chown group '{}' was not found in {}",
        value,
        group_path.display()
    )
}

fn resolve_remote_dest(rootfs: &Path, dest: &str, url: &str) -> Result<PathBuf> {
    let base = rootfs.join(dest.trim_start_matches('/'));
    if dest.ends_with('/') || base.is_dir() {
        let name = remote_basename(url);
        return Ok(base.join(name));
    }
    Ok(base)
}

fn remote_basename(url: &str) -> String {
    let without_query = url.split('?').next().unwrap_or(url);
    let candidate = without_query.rsplit('/').next().unwrap_or("download");
    if candidate.is_empty() {
        "download".to_string()
    } else {
        candidate.to_string()
    }
}

fn verify_download_checksum(path: &Path, checksum: &str) -> Result<()> {
    let Some(expected) = checksum.strip_prefix("sha256:") else {
        bail!("unsupported ADD --checksum algorithm '{checksum}' (supported: sha256)");
    };

    let mut file = File::open(path)
        .with_context(|| format!("failed to open {} for checksum", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {} for checksum", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = hex::encode(hasher.finalize());
    ensure!(
        actual == expected,
        "ADD --checksum mismatch for {}: expected sha256:{expected}, got sha256:{actual}",
        path.display()
    );
    Ok(())
}

fn try_extract_supported_archive(source: &Path, rootfs: &Path, dest: &str) -> Result<bool> {
    let source_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let target_dir = rootfs.join(dest.trim_start_matches('/'));

    if source_name.ends_with(".tar") {
        fs::create_dir_all(&target_dir)?;
        let file = File::open(source)?;
        let mut archive = Archive::new(file);
        archive.unpack(&target_dir).with_context(|| {
            format!(
                "failed to extract local ADD archive {} into {}",
                source.display(),
                target_dir.display()
            )
        })?;
        return Ok(true);
    }

    if source_name.ends_with(".tar.gz") || source_name.ends_with(".tgz") {
        fs::create_dir_all(&target_dir)?;
        let file = File::open(source)?;
        let decoder = GzDecoder::new(file);
        let mut archive = Archive::new(decoder);
        archive.unpack(&target_dir).with_context(|| {
            format!(
                "failed to extract local ADD archive {} into {}",
                source.display(),
                target_dir.display()
            )
        })?;
        return Ok(true);
    }

    if source_name.ends_with(".tar.xz")
        || source_name.ends_with(".txz")
        || source_name.ends_with(".tar.bz2")
        || source_name.ends_with(".tbz2")
    {
        bail!(
            "local ADD archive format is not supported yet for {} (supported: .tar, .tar.gz, .tgz)",
            source.display()
        );
    }

    Ok(false)
}

fn require_command(binary: &str) -> Result<PathBuf> {
    find_command(binary).ok_or_else(|| anyhow!("required command `{binary}` was not found in PATH"))
}

fn parse_platform(platform: &str) -> Result<(&str, &str)> {
    let parts: Vec<&str> = platform.splitn(2, '/').collect();
    if parts.len() != 2 {
        bail!("invalid platform format '{platform}', expected 'os/arch'");
    }
    Ok((parts[0], parts[1]))
}

fn parse_shell(shell: &str) -> Result<Vec<String>> {
    let parts = shell_words::split(shell)
        .with_context(|| format!("invalid shell declaration '{shell}'"))?;
    ensure!(!parts.is_empty(), "shell must not be empty");
    Ok(parts)
}

fn build_step_script(workdir: &str, shell: &[String], command: &str) -> String {
    let shell_words = shell
        .iter()
        .map(|part| shell_words::quote(part).into_owned())
        .collect::<Vec<_>>()
        .join(" ");

    format!(
        "cd {} && exec {} -c {}",
        shell_words::quote(workdir),
        shell_words,
        shell_words::quote(command)
    )
}

fn build_step_exec_args(workdir: &str, argv: &[String]) -> Vec<String> {
    let mut args = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "cd \"$1\" && shift && exec \"$@\"".to_string(),
        "sh".to_string(),
        workdir.to_string(),
    ];
    args.extend(argv.iter().cloned());
    args
}

fn should_snapshot_selected_stage_outputs(pipeline: &Pipeline) -> bool {
    !(pipeline.outputs.is_empty() || pipeline.outputs.len() == 1 && pipeline.outputs[0] == "/")
}

fn snapshot_stage_outputs(pipeline: &Pipeline, rootfs_dir: &Path) -> Result<TempDir> {
    let snapshot_dir = tempfile::Builder::new()
        .prefix("boringbuilder-linux-stage-")
        .tempdir()
        .context("failed to create temporary Linux stage snapshot directory")?;
    let entries =
        prune_nested_archive_entries(resolve_archive_entries(pipeline, Some(rootfs_dir))?);

    for entry in entries {
        let destination = if entry.archive_prefix.as_os_str().is_empty() {
            snapshot_dir.path().to_path_buf()
        } else {
            snapshot_dir.path().join(&entry.archive_prefix)
        };
        let follow_symlinks = pipeline
            .stage_snapshot_follow_symlinks
            .contains(&format!("/{}", entry.archive_prefix.display()));
        let copy_result = if follow_symlinks {
            copy_path_dereferenced(&entry.host_path, &destination)
        } else {
            copy_path(&entry.host_path, &destination)
        };
        copy_result.with_context(|| {
            format!(
                "failed to snapshot stage output {} into {}",
                entry.host_path.display(),
                destination.display()
            )
        })?;
    }

    Ok(snapshot_dir)
}

fn prune_nested_archive_entries(entries: Vec<ArchiveEntry>) -> Vec<ArchiveEntry> {
    let mut entries = entries;
    entries.sort_by_key(|entry| entry.archive_prefix.components().count());

    let mut pruned: Vec<ArchiveEntry> = Vec::new();
    for entry in entries {
        if pruned
            .iter()
            .any(|existing| path_contains(&entry.archive_prefix, &existing.archive_prefix))
        {
            continue;
        }
        pruned.push(entry);
    }

    pruned
}

fn path_contains(candidate: &Path, parent: &Path) -> bool {
    parent.as_os_str().is_empty() || candidate == parent || candidate.starts_with(parent)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::fs::File;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use tar::Builder;
    use tempfile::tempdir;

    use crate::export::archive::ArchiveEntry;
    use crate::schema::{Input, Pipeline};
    use crate::util::oci_rootfs::{
        Compression, compression_for, normalize_archive_path, unpack_layer,
    };

    use super::{
        LinuxEnv, LinuxTools, PrivilegeMode, build_step_exec_args, build_step_script,
        compile_copy_exclude_matcher, copy_host_path_with_excludes, decode_copy_parent_source,
        ensure_nested_mount_targets_exist_in_source, join_copy_destination, parse_image_env,
        parse_platform, prune_nested_archive_entries, require_command, snapshot_stage_outputs,
        verify_download_checksum,
    };

    #[test]
    fn parses_platform_string() {
        assert_eq!(parse_platform("linux/amd64").unwrap(), ("linux", "amd64"));
        assert_eq!(parse_platform("linux/arm64").unwrap(), ("linux", "arm64"));
        assert!(parse_platform("invalid").is_err());
    }

    #[test]
    fn builds_exec_form_wrapper_args() {
        let args = build_step_exec_args(
            "/workspace",
            &[
                "echo".to_string(),
                "$HOME".to_string(),
                "hello world".to_string(),
            ],
        );
        assert_eq!(
            args,
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "cd \"$1\" && shift && exec \"$@\"".to_string(),
                "sh".to_string(),
                "/workspace".to_string(),
                "echo".to_string(),
                "$HOME".to_string(),
                "hello world".to_string(),
            ]
        );
    }

    #[test]
    fn stage_snapshot_dereferences_followed_symlinks() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("usr/lib")).unwrap();
        fs::write(rootfs.join("usr/lib/libstdc++.so.6.0.32"), b"real-lib").unwrap();
        std::os::unix::fs::symlink("libstdc++.so.6.0.32", rootfs.join("usr/lib/libstdc++.so.6"))
            .unwrap();

        let pipeline = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: vec!["/usr/lib/libstdc++.so.6".to_string()],
            setup_snapshot: None,
            operations: Vec::new(),
            export: None,
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: BTreeSet::from(["/usr/lib/libstdc++.so.6".to_string()]),
            docker_context: None,
        };

        let snapshot = snapshot_stage_outputs(&pipeline, &rootfs).unwrap();
        let copied = snapshot.path().join("usr/lib/libstdc++.so.6");
        assert_eq!(fs::read(&copied).unwrap(), b"real-lib");
        assert!(
            !fs::symlink_metadata(&copied)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn normalizes_archive_paths() {
        assert_eq!(
            normalize_archive_path(Path::new("./etc/hosts")).unwrap(),
            PathBuf::from("etc/hosts")
        );
        assert!(normalize_archive_path(Path::new("../etc/passwd")).is_err());
    }

    #[test]
    fn copy_host_path_uses_privileged_cp_wrapper() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let work = tempdir().unwrap();
        let log_path = temp.path().join("sudo.log");
        let fake_sudo = temp.path().join("fake-sudo");
        let source = temp.path().join("hello.txt");
        fs::create_dir_all(&rootfs).unwrap();
        fs::write(&source, "hello").unwrap();
        fs::write(
            &fake_sudo,
            format!(
                "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" > {}\nif [ \"$1\" = \"-n\" ]; then shift; fi\nexec \"$@\"\n",
                shell_words::quote(&log_path.display().to_string())
            ),
        )
        .unwrap();
        let mut perms = fs::metadata(&fake_sudo).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake_sudo, perms).unwrap();

        let true_path = require_command("true").unwrap();
        let env = LinuxEnv::new(
            work,
            rootfs.clone(),
            PrivilegeMode::Sudo(fake_sudo.clone()),
            LinuxTools {
                mount: true_path.clone(),
                umount: true_path.clone(),
                chroot: true_path.clone(),
                cp: require_command("cp").unwrap(),
                chown: require_command("chown").unwrap(),
                chmod: require_command("chmod").unwrap(),
                mkdir: true_path.clone(),
                ln: true_path.clone(),
                rm: true_path.clone(),
                mknod: true_path,
                unshare: None,
                sudo: Some(fake_sudo.clone()),
            },
            true,
            BTreeMap::new(),
        );

        let copied = env
            .copy_host_path(&source, "/app/hello.txt", false, false, false)
            .unwrap();
        assert_eq!(copied, vec![rootfs.join("app/hello.txt")]);
        assert_eq!(
            fs::read_to_string(rootfs.join("app/hello.txt")).unwrap(),
            "hello"
        );

        let logged = fs::read_to_string(log_path).unwrap();
        assert!(logged.lines().any(|line| line == "-n"));
        assert!(logged.lines().any(|line| line.ends_with("/cp")));
    }

    #[test]
    fn prepare_builds_private_dev_mounts_before_teardown() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let work = tempdir().unwrap();
        let log_path = temp.path().join("sudo.log");
        let fake_sudo = temp.path().join("fake-sudo");
        fs::create_dir_all(&rootfs).unwrap();
        fs::write(
            &fake_sudo,
            format!(
                "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" > {}\nif [ \"$1\" = \"-n\" ]; then shift; fi\nexec \"$@\"\n",
                shell_words::quote(&log_path.display().to_string())
            ),
        )
        .unwrap();
        let mut perms = fs::metadata(&fake_sudo).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake_sudo, perms).unwrap();

        let true_path = require_command("true").unwrap();
        let mut env = LinuxEnv::new(
            work,
            rootfs.clone(),
            PrivilegeMode::Sudo(fake_sudo.clone()),
            LinuxTools {
                mount: true_path.clone(),
                umount: true_path.clone(),
                chroot: true_path.clone(),
                cp: true_path.clone(),
                chown: true_path.clone(),
                chmod: true_path.clone(),
                mkdir: true_path.clone(),
                ln: true_path.clone(),
                rm: true_path.clone(),
                mknod: true_path,
                unshare: None,
                sudo: Some(fake_sudo),
            },
            true,
            BTreeMap::new(),
        );

        let pipeline = Pipeline {
            image: "ubuntu:24.04".to_string(),
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

        env.prepare(&pipeline).unwrap();

        let logged = fs::read_to_string(log_path).unwrap();
        let true_cmd = require_command("true").unwrap();
        assert!(logged.lines().any(|line| line == "-n"));
        assert!(logged.lines().any(|line| line == "-c"));
        assert!(logged.contains("-t tmpfs -o nosuid,mode=755 tmpfs"));
        assert!(logged.contains("-t devpts -o newinstance,ptmxmode=0666,mode=620,gid=5 devpts"));
        assert!(logged.contains("-m 666"));
        assert!(logged.contains("c 1 3"));
        assert!(logged.contains(" -s pts/ptmx "));
        assert!(!logged.contains("--rbind /dev"));
        assert!(logged.contains(&format!(
            "{} -t tmpfs -o nosuid,mode=755 tmpfs {}",
            true_cmd.display(),
            rootfs.join("dev").display(),
        )));
        assert!(logged.contains(&format!(
            "{} -t devpts -o newinstance,ptmxmode=0666,mode=620,gid=5 devpts {}",
            true_cmd.display(),
            rootfs.join("dev/pts").display(),
        )));
        assert!(logged.contains(&format!(
            "{} --rbind /sys {} && {} --make-rslave {}",
            true_cmd.display(),
            rootfs.join("sys").display(),
            true_cmd.display(),
            rootfs.join("sys").display(),
        )));
    }

    #[test]
    fn copy_with_excludes_matches_source_prefixed_patterns() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("app");
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("keep.js"), "keep").unwrap();
        fs::write(source.join("nested/drop.map"), "drop").unwrap();
        let matcher = compile_copy_exclude_matcher(&["app/**/*.map".to_string()])
            .unwrap()
            .unwrap();

        let copied = copy_host_path_with_excludes(
            &source,
            Path::new("app"),
            &rootfs,
            "/workspace",
            false,
            false,
            &matcher,
        )
        .unwrap();

        assert!(copied.contains(&rootfs.join("workspace/keep.js")));
        assert!(rootfs.join("workspace/keep.js").exists());
        assert!(rootfs.join("workspace/nested").is_dir());
        assert!(!rootfs.join("workspace/nested/drop.map").exists());
    }

    #[test]
    fn copy_parent_marker_decodes_into_lookup_and_preserved_dest() {
        let (lookup, preserved) = decode_copy_parent_source("src/./lib/app.rb").unwrap();
        assert_eq!(lookup, "src/lib/app.rb");
        assert_eq!(preserved, PathBuf::from("lib/app.rb"));
        assert_eq!(
            join_copy_destination("/workspace/", &preserved),
            "/workspace/lib/app.rb"
        );
    }

    #[test]
    fn verifies_download_checksum() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("archive.tgz");
        fs::write(&path, b"hello-world").unwrap();
        verify_download_checksum(
            &path,
            "sha256:afa27b44d43b02a9fea41d13cedc2e4016cfcf87c5dbf990e593669aa8ce286d",
        )
        .unwrap();
        let err = verify_download_checksum(
            &path,
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        )
        .unwrap_err();
        assert!(err.to_string().contains("ADD --checksum mismatch"));
    }

    #[test]
    fn rejects_unsupported_checksum_algorithm() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("archive.tgz");
        fs::write(&path, b"hello-world").unwrap();
        let err = verify_download_checksum(&path, "sha512:deadbeef").unwrap_err();
        assert!(
            err.to_string()
                .contains("unsupported ADD --checksum algorithm")
        );
    }

    #[test]
    fn builds_step_script_with_shell() {
        let script = build_step_script(
            "/workspace",
            &[
                "bash".to_string(),
                "-euxo".to_string(),
                "pipefail".to_string(),
            ],
            "echo hi",
        );
        assert!(script.contains("cd /workspace"));
        assert!(script.contains("bash -euxo pipefail -c 'echo hi'"));
    }

    #[test]
    fn snapshots_only_selected_stage_outputs() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
        fs::create_dir_all(rootfs.join("usr/lib")).unwrap();
        fs::create_dir_all(rootfs.join("workspace/unused")).unwrap();
        fs::write(rootfs.join("usr/bin/hugo"), b"hugo").unwrap();
        fs::write(rootfs.join("usr/lib/libstdc++.so.6"), b"lib").unwrap();
        fs::write(rootfs.join("workspace/unused/file.txt"), b"nope").unwrap();

        let pipeline = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: vec![
                "/usr/bin/hugo".to_string(),
                "/usr/lib/libstdc++.so.6".to_string(),
            ],
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

        let snapshot = snapshot_stage_outputs(&pipeline, &rootfs).unwrap();
        assert_eq!(
            fs::read(snapshot.path().join("usr/bin/hugo")).unwrap(),
            b"hugo"
        );
        assert_eq!(
            fs::read(snapshot.path().join("usr/lib/libstdc++.so.6")).unwrap(),
            b"lib"
        );
        assert!(!snapshot.path().join("workspace/unused/file.txt").exists());
    }

    #[test]
    fn prunes_nested_archive_entries_when_parent_is_present() {
        let entries = vec![
            ArchiveEntry {
                host_path: PathBuf::from("/rootfs/usr"),
                archive_prefix: PathBuf::from("usr"),
            },
            ArchiveEntry {
                host_path: PathBuf::from("/rootfs/usr/bin/hugo"),
                archive_prefix: PathBuf::from("usr/bin/hugo"),
            },
        ];

        let pruned = prune_nested_archive_entries(entries);
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].archive_prefix, PathBuf::from("usr"));
    }

    #[test]
    fn detects_gzip_magic() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("layer.tar.gz");
        fs::write(&path, [0x1F, 0x8B, 0x08, 0x00]).unwrap();
        assert_eq!(compression_for(&path, None).unwrap(), Compression::Gzip);
    }

    #[test]
    fn creates_nested_mount_points_inside_parent_source_tree() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let cache_target = temp.path().join("cache-target");
        fs::create_dir_all(&cache_target).unwrap();
        let parent = Input {
            source: workspace.clone(),
            dest: "/workspace".to_string(),
            readonly: false,
        };
        let nested = Input {
            source: cache_target,
            dest: "/workspace/target".to_string(),
            readonly: false,
        };

        ensure_nested_mount_targets_exist_in_source(&parent, &[parent.clone(), nested]).unwrap();

        assert!(workspace.join("target").is_dir());
    }

    #[test]
    fn applies_whiteout_entries() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("app")).unwrap();
        fs::write(rootfs.join("app/old.txt"), "old").unwrap();

        let layer_path = temp.path().join("layer.tar");
        let file = File::create(&layer_path).unwrap();
        let mut builder = Builder::new(file);

        let mut header = tar::Header::new_gnu();
        header.set_path("app/.wh.old.txt").unwrap();
        header.set_size(0);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, &[][..]).unwrap();

        let mut header = tar::Header::new_gnu();
        let data = b"new";
        header.set_path("app/new.txt").unwrap();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, &data[..]).unwrap();
        builder.finish().unwrap();

        unpack_layer(
            &layer_path,
            &rootfs,
            Some("application/vnd.oci.image.layer.v1.tar"),
        )
        .unwrap();

        assert!(!rootfs.join("app/old.txt").exists());
        assert_eq!(
            fs::read_to_string(rootfs.join("app/new.txt")).unwrap(),
            "new"
        );
    }

    #[test]
    fn defers_hard_links_until_targets_exist() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("usr/bin")).unwrap();

        let layer_path = temp.path().join("layer.tar");
        let file = File::create(&layer_path).unwrap();
        let mut builder = Builder::new(file);

        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Link);
        header.set_link_name("usr/bin/perl").unwrap();
        header.set_path("usr/bin/perl5.38.2").unwrap();
        header.set_size(0);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append(&header, &[][..]).unwrap();

        let mut header = tar::Header::new_gnu();
        let data = b"perl";
        header.set_path("usr/bin/perl").unwrap();
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append(&header, &data[..]).unwrap();
        builder.finish().unwrap();

        unpack_layer(
            &layer_path,
            &rootfs,
            Some("application/vnd.oci.image.layer.v1.tar"),
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(rootfs.join("usr/bin/perl5.38.2")).unwrap(),
            "perl"
        );
    }

    #[test]
    fn parses_image_env_from_config() {
        let tmp = tempdir().unwrap();
        let image_dir = tmp.path();

        // Write a fake config blob with Env entries.
        let config = serde_json::json!({
            "config": {
                "Env": [
                    "PATH=/usr/local/go/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                    "GOPATH=/go",
                    "GOVERSION=go1.22.0"
                ]
            }
        });
        let config_bytes = serde_json::to_vec(&config).unwrap();
        let config_digest = format!("sha256:{}", sha2_hex(&config_bytes));
        let config_hash = config_digest.strip_prefix("sha256:").unwrap();
        fs::write(image_dir.join(config_hash), &config_bytes).unwrap();

        // Write manifest.json referencing the config.
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "config": {
                "digest": config_digest,
                "size": config_bytes.len()
            },
            "layers": []
        });
        fs::write(
            image_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let env = parse_image_env(image_dir);
        assert_eq!(
            env.get("PATH").unwrap(),
            "/usr/local/go/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
        );
        assert_eq!(env.get("GOPATH").unwrap(), "/go");
        assert_eq!(env.get("GOVERSION").unwrap(), "go1.22.0");
    }

    #[test]
    fn parse_image_env_returns_empty_for_missing_dir() {
        let env = parse_image_env(Path::new("/nonexistent/image/dir"));
        assert!(env.is_empty());
    }

    fn sha2_hex(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    }
}
