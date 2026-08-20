use std::fs;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::util::process::{find_command, run_capture, run_streaming};

/// Minimum macOS major version required by Apple's container CLI.
/// Full support requires macOS 26 (Tahoe), but it works on macOS 15
/// (Sequoia) with some limitations (e.g. no container-to-container networking).
const MIN_MACOS_MAJOR: u32 = 15;
const TEMP_CONTAINER_PKG_NAME: &str = "container-installer.pkg";
const TEMP_CONTAINER_EXPAND_DIR: &str = "container-pkg-expand";
const TEMP_CONTAINER_RUNTIME_DIR: &str = "runtime";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerCliSource {
    Override,
    Installed,
    CachedTemporary,
    BootstrappedTemporary,
}

#[derive(Debug, Clone)]
pub struct ResolvedContainerCli {
    pub path: PathBuf,
    pub source: ContainerCliSource,
}

impl ResolvedContainerCli {
    pub fn uses_temporary_runtime(&self) -> bool {
        matches!(
            self.source,
            ContainerCliSource::CachedTemporary | ContainerCliSource::BootstrappedTemporary
        )
    }
}

/// Return the macOS major version (e.g. 15, 26) or an error on non-macOS.
fn macos_major_version() -> Result<u32> {
    let output = run_capture(
        Path::new("/usr/bin/sw_vers"),
        &["-productVersion".to_string()],
    )?;
    let version = output.stdout.trim().to_string();
    let major = version
        .split('.')
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .with_context(|| format!("failed to parse macOS version from '{version}'"))?;
    Ok(major)
}

pub fn container_cache_root() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("BORINGBUILDER_CONTAINER_CACHE_ROOT") {
        return Ok(PathBuf::from(path));
    }

    let Some(home) = std::env::var_os("HOME") else {
        return Err(anyhow!(
            "HOME is not set; set BORINGBUILDER_CONTAINER_CACHE_ROOT explicitly"
        ));
    };

    Ok(PathBuf::from(home)
        .join("Library")
        .join("Caches")
        .join("boringbuilder")
        .join("container"))
}

fn temporary_container_pkg_path() -> Result<PathBuf> {
    Ok(container_cache_root()?.join(TEMP_CONTAINER_PKG_NAME))
}

fn temporary_container_expand_root() -> Result<PathBuf> {
    Ok(container_cache_root()?.join(TEMP_CONTAINER_EXPAND_DIR))
}

fn temporary_container_runtime_root() -> Result<PathBuf> {
    Ok(container_cache_root()?.join(TEMP_CONTAINER_RUNTIME_DIR))
}

fn default_container_app_root() -> Result<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Err(anyhow!(
            "HOME is not set; cannot prepare Apple container app root"
        ));
    };

    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("com.apple.container"))
}

pub fn temporary_container_binary_path() -> Result<PathBuf> {
    Ok(temporary_container_expand_root()?
        .join("Payload")
        .join("bin")
        .join("container"))
}

pub fn is_temporary_container_binary(path: &Path) -> bool {
    temporary_container_expand_root()
        .ok()
        .map(|root| path.starts_with(root))
        .unwrap_or(false)
}

fn latest_signed_pkg_download_url() -> Result<String> {
    let api_output = run_capture(
        Path::new("/usr/bin/curl"),
        &[
            "-fsSL".to_string(),
            "-H".to_string(),
            "Accept: application/vnd.github+json".to_string(),
            "https://api.github.com/repos/apple/container/releases/latest".to_string(),
        ],
    )
    .context("failed to query GitHub API for container CLI releases")?;

    let release: Value =
        serde_json::from_str(&api_output.stdout).context("failed to parse GitHub releases JSON")?;
    release["assets"]
        .as_array()
        .context("no assets in GitHub release")?
        .iter()
        .filter_map(|asset| asset["browser_download_url"].as_str())
        .find(|url| url.contains("installer-signed.pkg"))
        .map(str::to_string)
        .with_context(|| {
            format!(
                "no signed .pkg installer found in GitHub release assets for tag {}",
                release["tag_name"].as_str().unwrap_or("unknown")
            )
        })
}

pub fn bootstrap_temporary_container_cli() -> Result<PathBuf> {
    if !cfg!(target_os = "macos") {
        bail!("Apple container CLI is only supported on macOS");
    }

    let major = macos_major_version()?;
    if major < MIN_MACOS_MAJOR {
        bail!(
            "Apple container CLI requires macOS {MIN_MACOS_MAJOR}+, \
             but this machine is running macOS {major}. \
             See https://github.com/apple/container for details."
        );
    }

    let cache_root = container_cache_root()?;
    let pkg_path = temporary_container_pkg_path()?;
    let expand_root = temporary_container_expand_root()?;
    let binary_path = temporary_container_binary_path()?;
    if binary_path.is_file() {
        return Ok(binary_path);
    }

    fs::create_dir_all(&cache_root)
        .with_context(|| format!("failed to create {}", cache_root.display()))?;
    if expand_root.exists() {
        fs::remove_dir_all(&expand_root)
            .with_context(|| format!("failed to remove {}", expand_root.display()))?;
    }
    if pkg_path.exists() {
        fs::remove_file(&pkg_path)
            .with_context(|| format!("failed to remove {}", pkg_path.display()))?;
    }

    let download_url = latest_signed_pkg_download_url()?;
    println!("==> bootstrapping Apple container CLI from {download_url}");
    run_streaming(
        Path::new("/usr/bin/curl"),
        &[
            "-fSL".to_string(),
            "-o".to_string(),
            pkg_path.to_string_lossy().to_string(),
            download_url,
        ],
    )
    .context("failed to download container CLI installer")?;

    run_streaming(
        Path::new("/usr/sbin/pkgutil"),
        &[
            "--expand-full".to_string(),
            pkg_path.to_string_lossy().to_string(),
            expand_root.to_string_lossy().to_string(),
        ],
    )
    .context("failed to expand container CLI installer payload")?;

    if !binary_path.is_file() {
        bail!(
            "temporary Apple container CLI bootstrap did not produce {}",
            binary_path.display()
        );
    }

    Ok(binary_path)
}

pub fn resolve_container_cli(
    override_path: Option<PathBuf>,
    allow_bootstrap: bool,
) -> Result<ResolvedContainerCli> {
    if let Some(path) = override_path
        && path.is_file()
    {
        let source = if is_temporary_container_binary(&path) {
            ContainerCliSource::CachedTemporary
        } else {
            ContainerCliSource::Override
        };
        return Ok(ResolvedContainerCli { path, source });
    }

    if let Some(path) = find_command("container") {
        return Ok(ResolvedContainerCli {
            path,
            source: ContainerCliSource::Installed,
        });
    }

    let cached_temporary = temporary_container_binary_path()?;
    if cached_temporary.is_file() {
        return Ok(ResolvedContainerCli {
            path: cached_temporary,
            source: ContainerCliSource::CachedTemporary,
        });
    }

    if allow_bootstrap {
        return Ok(ResolvedContainerCli {
            path: bootstrap_temporary_container_cli()?,
            source: ContainerCliSource::BootstrappedTemporary,
        });
    }

    Err(anyhow!(
        "Apple `container` CLI was not found in PATH. Run `boringbuilder container setup` to bootstrap a temporary runtime without sudo, or install Apple `container` into PATH"
    ))
}

fn prepare_temporary_runtime(binary: &Path) -> Result<()> {
    if !is_temporary_container_binary(binary) {
        return Ok(());
    }

    let runtime_root = temporary_container_runtime_root()?;
    prepare_container_content_dirs(&runtime_root.join("app"))?;
    prepare_container_content_dirs(&default_container_app_root()?)?;
    fs::create_dir_all(runtime_root.join("log"))
        .with_context(|| format!("failed to create {}", runtime_root.display()))?;
    Ok(())
}

fn prepare_container_content_dirs(app_root: &Path) -> Result<()> {
    fs::create_dir_all(app_root.join("content/blobs/sha256"))
        .with_context(|| format!("failed to create {}", app_root.display()))?;
    fs::create_dir_all(app_root.join("content/ingest"))
        .with_context(|| format!("failed to create {}", app_root.display()))?;
    fs::create_dir_all(app_root.join("kernels"))
        .with_context(|| format!("failed to create {}", app_root.display()))?;
    Ok(())
}

pub fn container_system_status(binary: &Path) -> Result<String> {
    let output = run_capture(
        binary,
        &[
            "system".to_string(),
            "status".to_string(),
            "--format".to_string(),
            "json".to_string(),
        ],
    )?;

    if !output.status.success() {
        bail!(
            "container system status failed: {}",
            if output.stderr.trim().is_empty() {
                output.stdout.trim()
            } else {
                output.stderr.trim()
            }
        );
    }

    let parsed: Value = serde_json::from_str(&output.stdout)
        .with_context(|| "container system status did not return valid JSON".to_string())?;
    Ok(parsed.to_string())
}

fn ensure_recommended_kernel(binary: &Path) -> Result<()> {
    if !is_temporary_container_binary(binary) {
        return Ok(());
    }

    let output = run_capture(
        binary,
        &[
            "system".to_string(),
            "kernel".to_string(),
            "set".to_string(),
            "--recommended".to_string(),
            "--arch".to_string(),
            "arm64".to_string(),
        ],
    )?;
    if output.status.success() {
        return Ok(());
    }

    let detail = if output.stderr.trim().is_empty() {
        output.stdout.trim()
    } else {
        output.stderr.trim()
    };
    if detail.contains("because an item with the same name already exists")
        || detail.contains("Code=17 \"File exists\"")
    {
        return Ok(());
    }

    bail!("failed to configure recommended Apple container kernel: {detail}");
}

pub fn start_container_system(binary: &Path) -> Result<String> {
    prepare_temporary_runtime(binary)?;
    run_streaming(
        binary,
        &[
            "system".to_string(),
            "start".to_string(),
            "--enable-kernel-install".to_string(),
        ],
    )?;

    sleep(Duration::from_secs(2));
    ensure_recommended_kernel(binary)?;
    container_system_status(binary)
}

pub fn ensure_container_system_ready(binary: &Path) -> Result<String> {
    prepare_temporary_runtime(binary)?;
    match container_system_status(binary) {
        Ok(status) => {
            ensure_recommended_kernel(binary)?;
            Ok(status)
        }
        Err(_) => start_container_system(binary),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};

    use tempfile::tempdir;

    use super::{
        ContainerCliSource, prepare_temporary_runtime, resolve_container_cli,
        temporary_container_binary_path,
    };

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn write_executable(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut perms = fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).unwrap();
        }
    }

    #[test]
    fn resolves_override_before_other_sources() {
        let temp = tempdir().unwrap();
        let override_path = temp.path().join("container");
        write_executable(&override_path);

        let resolved = resolve_container_cli(Some(override_path.clone()), false).unwrap();
        assert_eq!(resolved.path, override_path);
        assert_eq!(resolved.source, ContainerCliSource::Override);
    }

    #[test]
    fn resolves_cached_temporary_cli_without_bootstrapping() {
        let _guard = env_lock().lock().unwrap();
        let temp = tempdir().unwrap();
        let fake_path = temp
            .path()
            .join("container-pkg-expand")
            .join("Payload")
            .join("bin")
            .join("container");
        write_executable(&fake_path);

        let previous_cache_root = std::env::var_os("BORINGBUILDER_CONTAINER_CACHE_ROOT");
        let previous_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("BORINGBUILDER_CONTAINER_CACHE_ROOT", temp.path());
            std::env::set_var("PATH", temp.path().join("missing-bin"));
        }

        let resolved = resolve_container_cli(None, false).unwrap();
        assert_eq!(resolved.path, temporary_container_binary_path().unwrap());
        assert_eq!(resolved.source, ContainerCliSource::CachedTemporary);

        unsafe {
            match previous_cache_root {
                Some(value) => std::env::set_var("BORINGBUILDER_CONTAINER_CACHE_ROOT", value),
                None => std::env::remove_var("BORINGBUILDER_CONTAINER_CACHE_ROOT"),
            }
            match previous_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    #[test]
    fn missing_cli_error_points_to_setup() {
        let _guard = env_lock().lock().unwrap();
        let temp = tempdir().unwrap();
        let previous_cache_root = std::env::var_os("BORINGBUILDER_CONTAINER_CACHE_ROOT");
        let previous_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("BORINGBUILDER_CONTAINER_CACHE_ROOT", temp.path());
            std::env::set_var("PATH", temp.path().join("missing-bin"));
        }

        let error = resolve_container_cli(None, false).unwrap_err().to_string();
        assert!(error.contains("boringbuilder container setup"));
        assert!(error.contains("without sudo"));

        unsafe {
            match previous_cache_root {
                Some(value) => std::env::set_var("BORINGBUILDER_CONTAINER_CACHE_ROOT", value),
                None => std::env::remove_var("BORINGBUILDER_CONTAINER_CACHE_ROOT"),
            }
            match previous_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    #[test]
    fn temporary_runtime_prepares_actual_container_app_root() {
        let _guard = env_lock().lock().unwrap();
        let temp = tempdir().unwrap();
        let fake_path = temp
            .path()
            .join("container-pkg-expand")
            .join("Payload")
            .join("bin")
            .join("container");
        write_executable(&fake_path);

        let home = temp.path().join("home");
        let previous_cache_root = std::env::var_os("BORINGBUILDER_CONTAINER_CACHE_ROOT");
        let previous_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("BORINGBUILDER_CONTAINER_CACHE_ROOT", temp.path());
            std::env::set_var("HOME", &home);
        }

        prepare_temporary_runtime(&fake_path).unwrap();

        assert!(
            temp.path()
                .join("runtime/app/content/blobs/sha256")
                .is_dir()
        );
        assert!(
            home.join("Library/Application Support/com.apple.container/content/blobs/sha256")
                .is_dir()
        );
        assert!(
            home.join("Library/Application Support/com.apple.container/kernels")
                .is_dir()
        );

        unsafe {
            match previous_cache_root {
                Some(value) => std::env::set_var("BORINGBUILDER_CONTAINER_CACHE_ROOT", value),
                None => std::env::remove_var("BORINGBUILDER_CONTAINER_CACHE_ROOT"),
            }
            match previous_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}
