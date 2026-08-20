use std::fs::{self, File};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use crate::ui;
use crate::util::process::find_command;

#[derive(Debug)]
pub(crate) struct PreparedPrebuiltBoringCache {
    pub(crate) local_path: PathBuf,
    pub(crate) source: String,
    pub(crate) digest: String,
}

pub(crate) fn prepare_prebuilt_boringcache(
    explicit: Option<&Path>,
    platform: &str,
) -> Result<Option<PreparedPrebuiltBoringCache>> {
    if let Some(path) = explicit {
        ensure!(
            path.is_file(),
            "--cache-bin {} is not a file",
            path.display()
        );
        ensure!(
            binary_compatible_with_platform(path, platform)?,
            "--cache-bin {} is not compatible with {platform}",
            path.display()
        );
        return prepare(path.to_path_buf(), "explicit --cache-bin".to_string()).map(Some);
    }

    for (path, source) in candidates(platform) {
        if !path.is_file() {
            continue;
        }
        if !binary_compatible_with_platform(&path, platform)? {
            ui::print_detail(format!(
                "skipping BoringCache binary {}: incompatible with {platform}",
                path.display()
            ));
            continue;
        }
        return prepare(path, source).map(Some);
    }

    Ok(None)
}

fn candidates(platform: &str) -> Vec<(PathBuf, String)> {
    let platform_key = platform.replace(['/', '-'], "_").to_ascii_uppercase();
    let platform_env = format!("BORINGBUILDER_PREBUILT_BORINGCACHE_{platform_key}");
    let mut candidates = Vec::new();

    if let Some(path) = std::env::var_os(&platform_env) {
        candidates.push((expand_tilde(path.into()), format!("env {platform_env}")));
    }
    if let Some(root) = std::env::var_os("BORINGBUILDER_PREBUILT_BORINGCACHE_DIR") {
        candidates.push((
            expand_tilde(root.into()).join(format!("boringcache-{}", platform.replace('/', "-"))),
            "env BORINGBUILDER_PREBUILT_BORINGCACHE_DIR".to_string(),
        ));
    }
    if let Some(path) = std::env::var_os("BORINGBUILDER_CACHE_BIN") {
        candidates.push((
            expand_tilde(path.into()),
            "env BORINGBUILDER_CACHE_BIN".to_string(),
        ));
    }
    if let Some(path) = find_command("boringcache") {
        candidates.push((path, "PATH boringcache".to_string()));
    }
    candidates
}

fn prepare(path: PathBuf, source: String) -> Result<PreparedPrebuiltBoringCache> {
    ensure_executable(&path)?;
    ui::print_detail(format!(
        "using BoringCache guest binary {} ({source})",
        path.display()
    ));
    Ok(PreparedPrebuiltBoringCache {
        digest: sha256_file(&path)?,
        local_path: path,
        source,
    })
}

fn ensure_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(path)
            .with_context(|| format!("failed to stat {}", path.display()))?
            .permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("failed to chmod {}", path.display()))?;
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn expand_tilde(path: PathBuf) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(relative) = raw.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(relative);
    }
    path
}

fn binary_compatible_with_platform(path: &Path, platform: &str) -> Result<bool> {
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read binary header from {}", path.display()))?;
    if bytes.len() < 20 || &bytes[..4] != b"\x7FELF" {
        return Ok(false);
    }

    let machine_bytes = [bytes[18], bytes[19]];
    let machine = match bytes[5] {
        1 => u16::from_le_bytes(machine_bytes),
        2 => u16::from_be_bytes(machine_bytes),
        _ => return Ok(false),
    };
    let expected = match platform {
        "linux/amd64" => 62,
        "linux/arm64" => 183,
        _ => return Ok(false),
    };
    Ok(machine == expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_elf_architecture() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("boringcache");
        let mut elf = vec![0; 64];
        elf[..4].copy_from_slice(b"\x7FELF");
        elf[5] = 1;
        elf[18..20].copy_from_slice(&183u16.to_le_bytes());
        fs::write(&path, elf).unwrap();

        assert!(binary_compatible_with_platform(&path, "linux/arm64").unwrap());
        assert!(!binary_compatible_with_platform(&path, "linux/amd64").unwrap());
    }
}
