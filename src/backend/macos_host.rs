use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail, ensure};
use fs2::FileExt;
use tempfile::TempDir;

use crate::backend::{
    ExecutionBackend, OperationCacheHooks, OperationTiming, RunOptions, RunSummary, RunTimings,
    print_timing_summary, resolve_export,
};
use crate::cache::CacheStoreConfig;
use crate::export::archive::{ArchiveEntry, resolve_archive_entries};
use crate::export::tar::{export_pipeline_tar, export_pipeline_tar_zst};
use crate::schema::{ExportFormat, Operation, Pipeline, Step};
use crate::ui;
use crate::util::fs::{copy_path, copy_path_dereferenced, path_size};

const HOST_LOCK_ENV: &str = "BORINGBUILDER_HOST_LOCK";

#[derive(Debug, Default)]
pub struct MacosHostBackend;

#[derive(Debug, Clone)]
struct HostMount {
    virtual_dest: String,
    host_dest: PathBuf,
}

#[derive(Debug)]
struct HostRuntimeLock {
    _file: File,
    _path: PathBuf,
}

impl ExecutionBackend for MacosHostBackend {
    fn name(&self) -> &'static str {
        "macos-host"
    }

    fn run(
        &self,
        pipeline: &Pipeline,
        options: &RunOptions,
        _cache_config: &CacheStoreConfig,
        mut cache_hooks: Option<&mut dyn OperationCacheHooks>,
    ) -> Result<RunSummary> {
        ensure!(
            cfg!(target_os = "macos"),
            "the macOS host backend can only run on macOS"
        );
        ensure!(
            pipeline.platform.starts_with("darwin/"),
            "macOS host runtime requires a darwin/* platform, got '{}'",
            pipeline.platform
        );
        ensure_host_operations_supported(pipeline)?;

        let run_started = Instant::now();
        let show_timings = options.timings;
        let resolved_export = resolve_export(pipeline, options);
        let _host_runtime_lock = if host_runtime_lock_already_held() {
            None
        } else {
            Some(acquire_host_runtime_lock(
                self.name(),
                pipeline
                    .env
                    .get(HOST_LOCK_ENV)
                    .map(String::as_str)
                    .unwrap_or(&pipeline.platform),
            )?)
        };

        let workspace = tempfile::Builder::new()
            .prefix("boringbuilder-macos-host-")
            .tempdir()
            .context("failed to create temporary macOS host workspace")?;
        let rootfs_dir = workspace.path().join("rootfs");
        fs::create_dir_all(&rootfs_dir)
            .with_context(|| format!("failed to create {}", rootfs_dir.display()))?;

        ui::print_status("preparing host workspace");
        let prepare_started = Instant::now();
        let mounts = materialize_mounts(pipeline, &rootfs_dir)?;
        ensure_host_workdirs(pipeline, &rootfs_dir, &mounts)?;
        let prepare_ms = prepare_started.elapsed().as_millis();

        let start_index = pipeline.step_start_index(options.from_step.as_deref())?;
        let mut operations = Vec::new();
        let mut operation_ms_total = 0u128;
        let mut cache_restore_ms_total = 0u128;
        let mut cache_save_ms_total = 0u128;

        for (index, operation) in pipeline.operations.iter().enumerate().skip(start_index) {
            let label = operation_label(operation, index);
            let cache_prime = if let Some(hooks) = cache_hooks.as_deref_mut() {
                let prime =
                    hooks.before_operation(operation, Some(&rootfs_dir), Some(&rootfs_dir))?;
                cache_restore_ms_total += prime.restore_ms;
                prime
            } else {
                Default::default()
            };
            ui::print_step(
                index + 1,
                pipeline.operations.len(),
                &label,
                cache_prime.complete,
            );
            if cache_prime.complete {
                if let Some(hooks) = cache_hooks.as_deref_mut() {
                    cache_save_ms_total += hooks.on_cached_operation(operation)?;
                }
                operations.push(OperationTiming {
                    label,
                    ms: 0,
                    cached: true,
                });
                continue;
            }

            let op_started = Instant::now();
            run_operation(pipeline, operation, &rootfs_dir, &mounts)?;
            let op_ms = op_started.elapsed().as_millis();
            operation_ms_total += op_ms;

            if let Some(hooks) = cache_hooks.as_deref_mut() {
                cache_save_ms_total +=
                    hooks.after_operation(operation, Some(&rootfs_dir), Some(&rootfs_dir))?;
            }
            operations.push(OperationTiming {
                label,
                ms: op_ms,
                cached: false,
            });
        }

        let export_started = Instant::now();
        let export_path = if let Some((format, path)) = resolved_export {
            Some(match format {
                ExportFormat::Tar => export_pipeline_tar(pipeline, &path)?,
                ExportFormat::TarZst => export_pipeline_tar_zst(pipeline, &path)?,
                ExportFormat::Oci | ExportFormat::Docker => {
                    bail!(
                        "runtime: host does not support {} export; use tar or tar.zst",
                        match format {
                            ExportFormat::Oci => "oci",
                            ExportFormat::Docker => "docker",
                            _ => unreachable!(),
                        }
                    );
                }
            })
        } else {
            None
        };
        let export_ms = export_started.elapsed().as_millis();
        let export_bytes = export_path
            .as_ref()
            .and_then(|path| path_size(path).ok())
            .unwrap_or(0);

        let (rootfs_dir, keep_alive): (Option<PathBuf>, Option<Arc<dyn Send + Sync>>) =
            if options.keep_rootfs {
                let snapshot = snapshot_stage_outputs(pipeline)?;
                let path = snapshot.path().to_path_buf();
                let guard: Arc<dyn Send + Sync> = Arc::new(snapshot);
                (Some(path), Some(guard))
            } else {
                (None, None)
            };
        let stage_rootfs_bytes = rootfs_dir
            .as_ref()
            .and_then(|path| path_size(path).ok())
            .unwrap_or(0);

        let total_ms = run_started.elapsed().as_millis();
        let timings = RunTimings {
            pull_ms: 0,
            unpack_ms: 0,
            prepare_ms,
            operation_ms: operation_ms_total,
            cache_restore_ms: cache_restore_ms_total,
            cache_save_ms: cache_save_ms_total,
            export_ms,
            total_ms,
            export_bytes,
            stage_rootfs_bytes,
            cache: Default::default(),
            operations,
        };

        if show_timings {
            print_timing_summary(&timings);
        }

        Ok(RunSummary {
            container_name: None,
            export_path,
            rootfs_dir,
            _keep_alive: keep_alive,
            timings,
        })
    }
}

fn ensure_host_operations_supported(pipeline: &Pipeline) -> Result<()> {
    for operation in &pipeline.operations {
        ensure!(
            matches!(operation, Operation::Exec(_)),
            "runtime: host currently supports only shell exec steps"
        );
    }
    Ok(())
}

fn acquire_host_runtime_lock(backend_name: &str, platform: &str) -> Result<HostRuntimeLock> {
    let Some(home) = std::env::var_os("HOME") else {
        bail!("HOME is not set");
    };
    let lock_dir = PathBuf::from(home)
        .join(".boringbuilder")
        .join("host-runtime-locks");
    fs::create_dir_all(&lock_dir)
        .with_context(|| format!("failed to create {}", lock_dir.display()))?;

    let lock_tag = host_runtime_lock_tag(platform);
    let lock_path = lock_dir.join(format!("{backend_name}-{lock_tag}.lock"));
    let file = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;

    ui::print_status(format!(
        "waiting for host runtime slot ({backend_name}, {platform})"
    ));
    file.lock_exclusive()
        .with_context(|| format!("failed to lock {}", lock_path.display()))?;

    Ok(HostRuntimeLock {
        _file: file,
        _path: lock_path,
    })
}

fn host_runtime_lock_tag(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn host_runtime_lock_already_held() -> bool {
    std::env::var_os("BORINGBUILDER_HOST_RUNTIME_LOCK_HELD").is_some()
}

fn materialize_mounts(pipeline: &Pipeline, rootfs_dir: &Path) -> Result<Vec<HostMount>> {
    let _ = rootfs_dir;
    let mut mounts = pipeline
        .inputs
        .iter()
        .map(|input| HostMount {
            virtual_dest: input.dest.clone(),
            host_dest: input.source.clone(),
        })
        .collect::<Vec<_>>();

    mounts.sort_by_key(|mount| std::cmp::Reverse(mount.virtual_dest.len()));
    Ok(mounts)
}

fn ensure_host_workdirs(
    pipeline: &Pipeline,
    rootfs_dir: &Path,
    mounts: &[HostMount],
) -> Result<()> {
    let pipeline_workdir = resolve_virtual_path(rootfs_dir, mounts, &pipeline.workdir)?;
    fs::create_dir_all(&pipeline_workdir)
        .with_context(|| format!("failed to create {}", pipeline_workdir.display()))?;
    for operation in &pipeline.operations {
        let Operation::Exec(step) = operation else {
            continue;
        };
        if let Some(workdir) = &step.workdir {
            let host_workdir = resolve_virtual_path(rootfs_dir, mounts, workdir)?;
            fs::create_dir_all(&host_workdir)
                .with_context(|| format!("failed to create {}", host_workdir.display()))?;
        }
    }
    Ok(())
}

fn run_operation(
    pipeline: &Pipeline,
    operation: &Operation,
    rootfs_dir: &Path,
    mounts: &[HostMount],
) -> Result<()> {
    let Operation::Exec(step) = operation else {
        bail!("runtime: host only supports exec operations today");
    };
    run_step(pipeline, step, rootfs_dir, mounts)
}

fn run_step(
    pipeline: &Pipeline,
    step: &Step,
    rootfs_dir: &Path,
    mounts: &[HostMount],
) -> Result<()> {
    ensure!(
        step.run_mounts.is_empty(),
        "RUN --mount is not supported on the macOS host runtime; use a container runtime for Dockerfile builds that rely on mount semantics"
    );
    let workdir = step.workdir.as_deref().unwrap_or(&pipeline.workdir);
    let host_workdir = resolve_virtual_path(rootfs_dir, mounts, workdir)?;
    let host_user_home = std::env::var_os("HOME").filter(|path| !path.is_empty());
    let default_env =
        build_default_host_env(rootfs_dir, mounts, host_user_home.as_deref().map(Path::new))?;
    ensure_host_env_directories(&default_env)?;

    let mut process = if let Some(argv) = &step.run_exec {
        let rewritten = argv
            .iter()
            .map(|arg| rewrite_virtual_paths(arg, mounts))
            .collect::<Vec<_>>();
        let mut process = Command::new(&rewritten[0]);
        process.args(&rewritten[1..]);
        process
    } else {
        let shell = parse_shell(step.shell.as_deref().unwrap_or("/bin/sh"))?;
        let command = rewrite_virtual_paths(&step.run, mounts);
        let mut process = Command::new(&shell[0]);
        process.args(&shell[1..]).arg("-c").arg(command);
        process
    };
    process
        .current_dir(&host_workdir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .env("BORINGBUILDER_RUNTIME", "host")
        .env("BORINGBUILDER_HOST_RUNTIME_LOCK_HELD", "1")
        .env("BORINGBUILDER_HOST_ROOT", rootfs_dir);
    if let Some(host_user_home) = host_user_home {
        process.env("BORINGBUILDER_HOST_USER_HOME", host_user_home);
    }
    for (key, value) in &default_env {
        process.env(key, value);
    }

    let mut env = BTreeMap::new();
    env.extend(pipeline.env.clone());
    env.extend(step.env.clone());
    for (key, value) in env {
        process.env(key, rewrite_virtual_env_value(&value, rootfs_dir, mounts)?);
    }

    let status = process.status().with_context(|| {
        format!(
            "failed to run host step '{}' in {}",
            step.name.as_deref().unwrap_or("<unnamed>"),
            host_workdir.display()
        )
    })?;
    if status.success() {
        return Ok(());
    }

    Err(anyhow!(
        "step '{}' failed with status {}",
        step.name.as_deref().unwrap_or("<unnamed>"),
        status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string())
    ))
}

fn build_default_host_env(
    rootfs_dir: &Path,
    mounts: &[HostMount],
    host_user_home: Option<&Path>,
) -> Result<BTreeMap<String, OsString>> {
    let home = host_user_home
        .map(Path::to_path_buf)
        .unwrap_or(resolve_virtual_path(rootfs_dir, mounts, "/root")?);
    let cargo_home = resolve_virtual_path(rootfs_dir, mounts, "/root/.cargo")?;
    let rustup_home = resolve_virtual_path(rootfs_dir, mounts, "/root/.rustup")?;
    let tmp_dir = resolve_virtual_path(rootfs_dir, mounts, "/tmp")?;
    let xdg_cache_home = resolve_virtual_path(rootfs_dir, mounts, "/root/.cache")?;
    let local_bin = home.join(".local/bin");
    let cargo_bin = cargo_home.join("bin");

    let mut path_entries = vec![cargo_bin.clone(), local_bin.clone()];
    if let Some(host_user_home) = host_user_home {
        path_entries.push(host_user_home.join(".cargo/bin"));
        path_entries.push(host_user_home.join(".local/bin"));
    }
    if let Some(existing_path) = std::env::var_os("PATH") {
        path_entries.extend(std::env::split_paths(&existing_path));
    }
    let joined_path =
        std::env::join_paths(path_entries).context("failed to build PATH for host runtime")?;

    let mut env = BTreeMap::new();
    env.insert("HOME".to_string(), home.into_os_string());
    env.insert("CARGO_HOME".to_string(), cargo_home.into_os_string());
    env.insert("RUSTUP_HOME".to_string(), rustup_home.into_os_string());
    env.insert("TMPDIR".to_string(), tmp_dir.into_os_string());
    env.insert(
        "XDG_CACHE_HOME".to_string(),
        xdg_cache_home.into_os_string(),
    );
    env.insert(
        "RUSTUP_INIT_SKIP_PATH_CHECK".to_string(),
        OsString::from("yes"),
    );
    env.insert("PATH".to_string(), joined_path);
    Ok(env)
}

fn ensure_host_env_directories(default_env: &BTreeMap<String, OsString>) -> Result<()> {
    for key in [
        "HOME",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "TMPDIR",
        "XDG_CACHE_HOME",
    ] {
        let Some(path) = default_env.get(key) else {
            continue;
        };
        let path = Path::new(path);
        fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    }

    for key in ["HOME", "CARGO_HOME"] {
        let Some(path) = default_env.get(key) else {
            continue;
        };
        let path = Path::new(path).join("bin");
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
    }
    Ok(())
}

fn rewrite_virtual_paths(value: &str, mounts: &[HostMount]) -> String {
    let mut rewritten = value.to_string();
    for mount in mounts {
        rewritten = rewrite_virtual_path_value(
            &rewritten,
            &mount.virtual_dest,
            mount.host_dest.to_string_lossy().as_ref(),
        );
    }
    rewritten
}

fn rewrite_virtual_env_value(
    value: &str,
    rootfs_dir: &Path,
    mounts: &[HostMount],
) -> Result<String> {
    if value.contains(':') {
        let parts = value.split(':').collect::<Vec<_>>();
        if parts
            .iter()
            .all(|part| part.is_empty() || part.starts_with('/'))
        {
            let mut rewritten = Vec::with_capacity(parts.len());
            for part in parts {
                if part.is_empty() {
                    rewritten.push(String::new());
                } else {
                    rewritten.push(
                        resolve_virtual_path(rootfs_dir, mounts, part)?
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
            return Ok(rewritten.join(":"));
        }
    }
    if value.starts_with('/') {
        return Ok(resolve_virtual_path(rootfs_dir, mounts, value)?
            .to_string_lossy()
            .into_owned());
    }
    Ok(rewrite_virtual_paths(value, mounts))
}

fn rewrite_virtual_path_value(value: &str, virtual_dest: &str, host_dest: &str) -> String {
    let mut rewritten = String::with_capacity(value.len());
    let mut cursor = 0;

    while let Some(found) = value[cursor..].find(virtual_dest) {
        let start = cursor + found;
        let end = start + virtual_dest.len();
        let previous = value[..start].chars().next_back();
        let next = value[end..].chars().next();

        if is_virtual_path_boundary_before(previous) && is_virtual_path_boundary_after(next) {
            rewritten.push_str(&value[cursor..start]);
            rewritten.push_str(host_dest);
            cursor = end;
        } else {
            rewritten.push_str(&value[cursor..start + 1]);
            cursor = start + 1;
        }
    }

    rewritten.push_str(&value[cursor..]);
    rewritten
}

fn is_virtual_path_boundary_before(previous: Option<char>) -> bool {
    previous.is_none_or(|ch| !is_virtual_path_char(ch))
}

fn is_virtual_path_boundary_after(next: Option<char>) -> bool {
    next.is_none_or(|ch| ch == '/' || !is_virtual_path_char(ch))
}

fn is_virtual_path_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '~')
}

fn resolve_virtual_path(
    rootfs_dir: &Path,
    mounts: &[HostMount],
    virtual_path_str: &str,
) -> Result<PathBuf> {
    ensure!(
        virtual_path_str.starts_with('/'),
        "runtime: host requires absolute virtual paths, got '{}'",
        virtual_path_str
    );
    let requested = Path::new(virtual_path_str);
    for mount in mounts {
        let mount_path = Path::new(&mount.virtual_dest);
        if let Ok(relative) = requested.strip_prefix(mount_path) {
            return Ok(if relative.as_os_str().is_empty() {
                mount.host_dest.clone()
            } else {
                mount.host_dest.join(relative)
            });
        }
    }
    virtual_path(rootfs_dir, virtual_path_str)
}

fn parse_shell(shell: &str) -> Result<Vec<String>> {
    let parts = shell_words::split(shell)
        .with_context(|| format!("invalid shell declaration '{shell}'"))?;
    ensure!(!parts.is_empty(), "shell must not be empty");
    Ok(parts)
}

fn operation_label(operation: &Operation, index: usize) -> String {
    match operation {
        Operation::Exec(step) => step
            .name
            .clone()
            .unwrap_or_else(|| format!("step-{}", index + 1)),
        Operation::CopyFromContext(op) => op
            .name
            .clone()
            .unwrap_or_else(|| format!("copy-context-{}", index + 1)),
        Operation::CopyFromStage(op) => op
            .name
            .clone()
            .unwrap_or_else(|| format!("copy-stage-{}", index + 1)),
        Operation::AddRemote(op) => op
            .name
            .clone()
            .unwrap_or_else(|| format!("remote-add-{}", index + 1)),
    }
}

fn snapshot_stage_outputs(pipeline: &Pipeline) -> Result<TempDir> {
    let snapshot_dir = tempfile::Builder::new()
        .prefix("boringbuilder-macos-host-stage-")
        .tempdir()
        .context("failed to create temporary macOS host snapshot directory")?;
    let entries = prune_nested_archive_entries(resolve_archive_entries(pipeline, None)?);

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

    let mut pruned = Vec::new();
    for entry in entries {
        if pruned.iter().any(|existing: &ArchiveEntry| {
            path_contains(&entry.archive_prefix, &existing.archive_prefix)
        }) {
            continue;
        }
        pruned.push(entry);
    }
    pruned
}

fn path_contains(candidate: &Path, parent: &Path) -> bool {
    parent.as_os_str().is_empty() || candidate == parent || candidate.starts_with(parent)
}

fn virtual_path(rootfs_dir: &Path, virtual_path: &str) -> Result<PathBuf> {
    ensure!(
        virtual_path.starts_with('/'),
        "runtime: host requires absolute virtual paths, got '{}'",
        virtual_path
    );
    let relative = virtual_path.trim_start_matches('/');
    Ok(if relative.is_empty() {
        rootfs_dir.to_path_buf()
    } else {
        rootfs_dir.join(relative)
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    #[cfg(target_os = "macos")]
    use crate::backend::{ExecutionBackend, RunOptions};
    #[cfg(target_os = "macos")]
    use crate::cache::CacheStoreConfig;
    #[cfg(target_os = "macos")]
    use crate::schema::Pipeline;
    #[cfg(target_os = "macos")]
    use crate::schema::{Input, Operation, Step};
    #[cfg(target_os = "macos")]
    use std::collections::{BTreeMap, BTreeSet};
    #[cfg(target_os = "macos")]
    use std::fs;
    #[cfg(target_os = "macos")]
    use tempfile::tempdir;

    #[cfg(target_os = "macos")]
    use super::MacosHostBackend;
    use super::{
        build_default_host_env, resolve_virtual_path, rewrite_virtual_env_value,
        rewrite_virtual_paths,
    };

    #[test]
    fn rewrites_nested_mounts_before_parents() {
        let mounts = vec![
            super::HostMount {
                virtual_dest: "/workspace/target".to_string(),
                host_dest: PathBuf::from("/tmp/rootfs/workspace/target"),
            },
            super::HostMount {
                virtual_dest: "/workspace".to_string(),
                host_dest: PathBuf::from("/tmp/rootfs/workspace"),
            },
        ];
        let rewritten =
            rewrite_virtual_paths("cp /workspace/target/app /workspace/dist/app", &mounts);
        assert!(rewritten.contains("/tmp/rootfs/workspace/target/app"));
        assert!(rewritten.contains("/tmp/rootfs/workspace/dist/app"));
    }

    #[test]
    fn rewrite_virtual_paths_avoids_partial_segment_rewrites() {
        let mounts = vec![
            super::HostMount {
                virtual_dest: "/workspace".to_string(),
                host_dest: PathBuf::from("/repo"),
            },
            super::HostMount {
                virtual_dest: "/mise".to_string(),
                host_dest: PathBuf::from("/cache/mise"),
            },
        ];
        let rewritten = rewrite_virtual_paths(
            "ls /workspace/mise.toml && echo /Users/gaurav/.config/mise",
            &mounts,
        );
        assert!(rewritten.contains("ls /repo/mise.toml"));
        assert!(rewritten.contains("echo /Users/gaurav/.config/mise"));
        assert!(!rewritten.contains("/repo/cache/mise.toml"));
    }

    #[test]
    fn rewrite_virtual_env_value_maps_unmounted_virtual_paths_into_host_root() {
        let mounts = vec![super::HostMount {
            virtual_dest: "/workspace".to_string(),
            host_dest: PathBuf::from("/repo"),
        }];
        let rewritten =
            rewrite_virtual_env_value("/root/.rustup", Path::new("/tmp/rootfs"), &mounts).unwrap();
        assert_eq!(rewritten, "/tmp/rootfs/root/.rustup");
    }

    #[test]
    fn rewrite_virtual_env_value_rewrites_colon_separated_virtual_path_lists() {
        let mounts = vec![super::HostMount {
            virtual_dest: "/workspace".to_string(),
            host_dest: PathBuf::from("/repo"),
        }];
        let rewritten = rewrite_virtual_env_value(
            "/workspace/bin:/root/.cargo/bin",
            Path::new("/tmp/rootfs"),
            &mounts,
        )
        .unwrap();
        assert_eq!(rewritten, "/repo/bin:/tmp/rootfs/root/.cargo/bin");
    }

    #[test]
    fn resolves_nested_mount_paths_before_parent_workspace() {
        let mounts = vec![
            super::HostMount {
                virtual_dest: "/workspace/target".to_string(),
                host_dest: PathBuf::from("/tmp/cache-target"),
            },
            super::HostMount {
                virtual_dest: "/workspace".to_string(),
                host_dest: PathBuf::from("/repo"),
            },
        ];
        let resolved_target =
            resolve_virtual_path(Path::new("/tmp/rootfs"), &mounts, "/workspace/target/debug")
                .unwrap();
        let resolved_workspace =
            resolve_virtual_path(Path::new("/tmp/rootfs"), &mounts, "/workspace/src").unwrap();
        assert_eq!(resolved_target, PathBuf::from("/tmp/cache-target/debug"));
        assert_eq!(resolved_workspace, PathBuf::from("/repo/src"));
    }

    #[test]
    fn builds_target_local_host_env_defaults() {
        let mounts = vec![super::HostMount {
            virtual_dest: "/workspace".to_string(),
            host_dest: PathBuf::from("/repo"),
        }];
        let env = build_default_host_env(
            Path::new("/tmp/rootfs"),
            &mounts,
            Some(Path::new("/Users/gaurav")),
        )
        .unwrap();
        assert_eq!(env.get("HOME"), Some(&OsString::from("/Users/gaurav")));
        assert_eq!(
            env.get("CARGO_HOME"),
            Some(&OsString::from("/tmp/rootfs/root/.cargo"))
        );
        assert_eq!(
            env.get("RUSTUP_HOME"),
            Some(&OsString::from("/tmp/rootfs/root/.rustup"))
        );
        assert_eq!(env.get("TMPDIR"), Some(&OsString::from("/tmp/rootfs/tmp")));
        let path = env.get("PATH").unwrap().to_string_lossy();
        assert!(path.contains("/tmp/rootfs/root/.cargo/bin"));
        assert!(path.contains("/Users/gaurav/.local/bin"));
        assert!(path.contains("/Users/gaurav/.cargo/bin"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn runs_simple_host_pipeline() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();

        let pipeline = Pipeline {
            image: crate::schema::HOST_RUNTIME_IMAGE.to_string(),
            platform: crate::util::platform::default_host_platform().unwrap(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: vec![Input {
                source: workspace.clone(),
                dest: "/workspace".to_string(),
                readonly: false,
            }],
            outputs: vec!["/workspace/dist".to_string()],
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("write".to_string()),
                run: "mkdir -p /workspace/dist && printf hello >/workspace/dist/message.txt"
                    .to_string(),
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
            stage_snapshot_follow_symlinks: BTreeSet::new(),
            docker_context: None,
        };

        let backend = MacosHostBackend;
        backend
            .run(
                &pipeline,
                &RunOptions::default(),
                &CacheStoreConfig::default(),
                None,
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(workspace.join("dist/message.txt")).unwrap(),
            "hello"
        );
    }
}
