use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::schema::{CacheKeyFile, CacheKeySpecFile};

pub fn resolve_cache_key(
    base_dir: &Path,
    platform: &str,
    pipeline_env: &BTreeMap<String, String>,
    cache_path: &str,
    key: &CacheKeyFile,
) -> Result<String> {
    match key {
        CacheKeyFile::Literal(value) => {
            let expanded = expand_cache_key_template(value.trim(), platform, pipeline_env)?;
            ensure!(
                !expanded.trim().is_empty(),
                "cache key literal must not be empty"
            );
            Ok(expanded.trim().to_string())
        }
        CacheKeyFile::Structured(spec) => {
            structured_cache_key(base_dir, platform, pipeline_env, cache_path, spec)
        }
    }
}

fn structured_cache_key(
    base_dir: &Path,
    platform: &str,
    pipeline_env: &BTreeMap<String, String>,
    cache_path: &str,
    spec: &CacheKeySpecFile,
) -> Result<String> {
    let prefix = expand_cache_key_template(
        spec.prefix.as_deref().unwrap_or("cache").trim(),
        platform,
        pipeline_env,
    )?;
    ensure!(!prefix.is_empty(), "cache key prefix must not be empty");

    let mut hasher = Sha256::new();
    hash_str(&mut hasher, "boringbuilder-cache-key-v1");
    hash_str(&mut hasher, platform);
    hash_str(&mut hasher, cache_path);

    let mut env_names = spec.env.clone();
    env_names.sort();
    for name in env_names {
        let value = pipeline_env
            .get(&name)
            .cloned()
            .or_else(|| std::env::var(&name).ok())
            .unwrap_or_default();
        hash_str(&mut hasher, "env");
        hash_str(&mut hasher, &name);
        hash_str(&mut hasher, &value);
    }

    let mut files = spec.files.clone();
    files.sort();
    for relative in files {
        let absolute = base_dir.join(&relative);
        hash_path(&mut hasher, &relative, &absolute)?;
    }

    let digest = hex::encode(hasher.finalize());
    Ok(format!("{prefix}-{}", &digest[..16]))
}

fn expand_cache_key_template(
    raw: &str,
    platform: &str,
    pipeline_env: &BTreeMap<String, String>,
) -> Result<String> {
    let (os, arch) = platform
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("invalid platform '{platform}', expected os/arch"))?;
    let platform_slug = format!("{os}-{arch}");

    let mut expanded = String::new();
    let mut rest = raw;

    while let Some(start) = rest.find('{') {
        expanded.push_str(&rest[..start]);
        let after_open = &rest[start + 1..];
        let Some(end) = after_open.find('}') else {
            bail!("unterminated cache key template placeholder in '{raw}'");
        };
        let token = &after_open[..end];
        let replacement = match token {
            "platform" => platform_slug.clone(),
            "os" => os.to_string(),
            "arch" => arch.to_string(),
            _ if token.starts_with("env:") => {
                let name = token.trim_start_matches("env:");
                ensure!(
                    !name.is_empty(),
                    "cache key env placeholder must name an environment variable"
                );
                pipeline_env
                    .get(name)
                    .cloned()
                    .or_else(|| std::env::var(name).ok())
                    .unwrap_or_default()
            }
            _ => bail!("unsupported cache key template placeholder '{{{token}}}'"),
        };
        expanded.push_str(&replacement);
        rest = &after_open[end + 1..];
    }

    expanded.push_str(rest);
    Ok(expanded)
}

pub(crate) fn hash_path(hasher: &mut Sha256, relative: &Path, absolute: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(absolute)
        .with_context(|| format!("cache key path does not exist: {}", absolute.display()))?;

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(absolute)
            .with_context(|| format!("failed to read symlink {}", absolute.display()))?;
        hash_str(hasher, "symlink");
        hash_str(hasher, &relative.to_string_lossy());
        hash_mode(hasher, metadata.permissions().mode());
        hash_str(hasher, &target.to_string_lossy());
        return Ok(());
    }

    if metadata.is_file() {
        hash_str(hasher, "file");
        hash_str(hasher, &relative.to_string_lossy());
        hash_mode(hasher, metadata.permissions().mode());
        hash_file_contents(hasher, absolute)?;
        return Ok(());
    }

    if !metadata.is_dir() {
        bail!("unsupported cache key path type: {}", absolute.display());
    }

    hash_str(hasher, "dir");
    hash_str(hasher, &relative.to_string_lossy());
    hash_mode(hasher, metadata.permissions().mode());
    let mut entries = WalkDir::new(absolute)
        .follow_links(false)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by(|left, right| left.path().cmp(right.path()));

    for entry in entries {
        let path = entry.path();
        if path == absolute {
            continue;
        }

        let rel = path.strip_prefix(absolute).with_context(|| {
            format!(
                "failed to compute cache key relative path for {}",
                path.display()
            )
        })?;
        let display = PathBuf::from(relative).join(rel);
        let metadata = entry.metadata()?;
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(path)?;
            hash_str(hasher, "symlink");
            hash_str(hasher, &display.to_string_lossy());
            hash_mode(hasher, metadata.permissions().mode());
            hash_str(hasher, &target.to_string_lossy());
            continue;
        }
        if metadata.is_dir() {
            hash_str(hasher, "dir-entry");
            hash_str(hasher, &display.to_string_lossy());
            hash_mode(hasher, metadata.permissions().mode());
            continue;
        }
        hash_str(hasher, "file-entry");
        hash_str(hasher, &display.to_string_lossy());
        hash_mode(hasher, metadata.permissions().mode());
        hash_file_contents(hasher, path)?;
    }

    Ok(())
}

fn hash_file_contents(hasher: &mut Sha256, path: &Path) -> Result<()> {
    let mut file =
        fs::File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(())
}

pub(crate) fn hash_str(hasher: &mut Sha256, value: &str) {
    hasher.update(value.as_bytes());
    hasher.update([0]);
}

fn hash_mode(hasher: &mut Sha256, mode: u32) {
    hasher.update(mode.to_le_bytes());
    hasher.update([0]);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use crate::schema::{CacheKeyFile, CacheKeySpecFile};

    use super::resolve_cache_key;

    #[test]
    fn hashes_files_and_env_for_structured_key() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("Gemfile.lock"), "abc").unwrap();

        let mut env = BTreeMap::new();
        env.insert("RUBY_VERSION".to_string(), "3.4".to_string());
        let key = resolve_cache_key(
            temp.path(),
            "linux/amd64",
            &env,
            "/workspace/vendor/bundle",
            &CacheKeyFile::Structured(CacheKeySpecFile {
                prefix: Some("bundle".to_string()),
                files: vec![PathBuf::from("Gemfile.lock")],
                env: vec!["RUBY_VERSION".to_string()],
            }),
        )
        .unwrap();

        assert!(key.starts_with("bundle-"));
    }

    #[test]
    fn keeps_literal_key_verbatim() {
        let key = resolve_cache_key(
            Path::new("."),
            "linux/amd64",
            &BTreeMap::new(),
            "/cache",
            &CacheKeyFile::Literal("shared-cache".to_string()),
        )
        .unwrap();

        assert_eq!(key, "shared-cache");
    }

    #[test]
    fn expands_literal_cache_key_templates() {
        let mut env = BTreeMap::new();
        env.insert("RUBY_VERSION".to_string(), "4.0.1".to_string());

        let key = resolve_cache_key(
            Path::new("."),
            "linux/amd64",
            &env,
            "/workspace/vendor/bundle",
            &CacheKeyFile::Literal("bundler-ruby-{env:RUBY_VERSION}-{platform}".to_string()),
        )
        .unwrap();

        assert_eq!(key, "bundler-ruby-4.0.1-linux-amd64");
    }

    #[test]
    fn expands_structured_prefix_templates() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("Gemfile.lock"), "abc").unwrap();

        let mut env = BTreeMap::new();
        env.insert("RUBY_VERSION".to_string(), "4.0.1".to_string());
        let key = resolve_cache_key(
            temp.path(),
            "linux/amd64",
            &env,
            "/workspace/vendor/bundle",
            &CacheKeyFile::Structured(CacheKeySpecFile {
                prefix: Some("bundle-ruby-{env:RUBY_VERSION}-{arch}".to_string()),
                files: vec![PathBuf::from("Gemfile.lock")],
                env: vec![],
            }),
        )
        .unwrap();

        assert!(key.starts_with("bundle-ruby-4.0.1-amd64-"));
    }

    #[test]
    fn file_mode_changes_structured_key() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("script.sh");
        fs::write(&path, "echo hi\n").unwrap();

        let key_before = resolve_cache_key(
            temp.path(),
            "linux/amd64",
            &BTreeMap::new(),
            "/workspace/bin",
            &CacheKeyFile::Structured(CacheKeySpecFile {
                prefix: Some("scripts".to_string()),
                files: vec![PathBuf::from("script.sh")],
                env: vec![],
            }),
        )
        .unwrap();

        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();

        let key_after = resolve_cache_key(
            temp.path(),
            "linux/amd64",
            &BTreeMap::new(),
            "/workspace/bin",
            &CacheKeyFile::Structured(CacheKeySpecFile {
                prefix: Some("scripts".to_string()),
                files: vec![PathBuf::from("script.sh")],
                env: vec![],
            }),
        )
        .unwrap();

        assert_ne!(key_before, key_after);
    }
}
