pub mod archive;
pub mod backend;
pub(crate) mod boringcache;
pub mod key;
pub mod local;
pub mod registry;
pub mod setup_snapshot;
pub mod slice;
pub mod stage_digest;
pub mod tree;

use std::path::Path;
use std::{fs, fs::File, path::PathBuf};

use anyhow::{Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::cache::boringcache::BoringCacheStore;
use crate::cache::local::{LocalCacheBackend, LocalCacheStore, default_cache_dir};
use crate::schema::CacheMount;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheStoreKind {
    #[default]
    Local,
    BoringCache,
    /// OCI registry (e.g. `ghcr.io/org/cache`).
    Registry {
        reference: String,
        #[serde(default)]
        insecure: bool,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheStoreConfig {
    pub cache_dir: Option<PathBuf>,
    pub cache_store: CacheStoreKind,
    #[serde(default)]
    pub cache_store_explicit: bool,
    pub cache_workspace: Option<String>,
    pub cache_bin: Option<PathBuf>,
    /// Platform for registry cache (e.g. `linux/amd64`).
    #[serde(default)]
    pub platform: Option<String>,
}

pub fn cache_backend_cli_spec(config: &CacheStoreConfig) -> Option<(String, bool)> {
    match &config.cache_store {
        CacheStoreKind::Local if config.cache_store_explicit => Some(("local".to_string(), false)),
        CacheStoreKind::Local => None,
        CacheStoreKind::BoringCache => Some(("boringcache".to_string(), false)),
        CacheStoreKind::Registry {
            reference,
            insecure,
        } => Some((reference.clone(), *insecure)),
    }
}

pub fn cache_tag(logical_key: &str) -> String {
    normalize_cache_tag(logical_key)
}

pub fn normalize_cache_tag(value: &str) -> String {
    let mut out = value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | '0'..='9' | '.' | '_' | '-' => ch,
            'A'..='Z' => ch.to_ascii_lowercase(),
            _ => '-',
        })
        .collect::<String>();

    while out.contains("--") {
        out = out.replace("--", "-");
    }

    let trimmed = out.trim_matches(['-', '.', '_']);
    if trimmed.is_empty() {
        "cache".to_string()
    } else {
        trimmed.to_string()
    }
}

#[derive(Debug, Clone)]
pub struct CacheRestore {
    pub hit: bool,
    pub digest: Option<String>,
    pub bytes: u64,
    pub content_hash: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CacheSave {
    pub digest: String,
    pub bytes: u64,
    pub content_hash: String,
}

pub fn cache_restore_keys(entry: &CacheMount) -> Vec<&str> {
    let mut keys = Vec::with_capacity(1 + entry.restore_from.len());
    keys.push(entry.key.as_str());
    for fallback in &entry.restore_from {
        if fallback != &entry.key && !keys.contains(&fallback.as_str()) {
            keys.push(fallback.as_str());
        }
    }
    keys
}

#[derive(Debug)]
pub struct CacheLock {
    _file: File,
    _path: PathBuf,
}

pub trait CacheStore {
    fn kind(&self) -> &'static str;
    fn detail(&self) -> String;
    fn restore(&self, entry: &CacheMount, destination: &Path) -> Result<CacheRestore>;
    fn save(&self, entry: &CacheMount, source: &Path) -> Result<CacheSave>;
    fn lock(&self, entry: &CacheMount) -> Result<CacheLock>;
    /// Access the underlying backend for low-level operations (step slices).
    fn backend(&self) -> &dyn backend::CacheBackend;
}

/// Open the underlying `CacheBackend` at the shared cache directory.
///
/// Step slices and other cache artifacts use this same backend, so blobs are
/// deduplicated across cache types.
pub fn open_cache_backend(config: &CacheStoreConfig) -> Result<Box<dyn backend::CacheBackend>> {
    match &config.cache_store {
        CacheStoreKind::Local => {
            let cache_root = default_cache_dir(config.cache_dir.as_deref())?;
            Ok(Box::new(LocalCacheBackend::open(&cache_root)?))
        }
        CacheStoreKind::BoringCache => {
            if config.cache_dir.is_some() {
                bail!("--cache-dir is only supported with the local cache store");
            }
            Ok(Box::new(
                crate::cache::boringcache::BoringCacheBackend::open(
                    config.cache_workspace.clone(),
                    config.cache_bin.clone(),
                    crate::boringcache_cli::BoringCacheTagScope::platform_scoped_no_git(),
                )?,
            ))
        }
        CacheStoreKind::Registry {
            reference,
            insecure,
        } => {
            let platform = config.platform.as_deref().unwrap_or("linux/amd64");
            Ok(Box::new(
                crate::cache::registry::RegistryCacheBackend::open(
                    config.cache_dir.as_deref(),
                    reference.clone(),
                    platform,
                    *insecure,
                )?,
            ))
        }
    }
}

pub fn open_cache_store(config: &CacheStoreConfig) -> Result<Box<dyn CacheStore>> {
    match &config.cache_store {
        CacheStoreKind::Local => {
            let cache_root = default_cache_dir(config.cache_dir.as_deref())?;
            Ok(Box::new(LocalCacheStore::open(&cache_root)?))
        }
        CacheStoreKind::BoringCache => {
            if config.cache_dir.is_some() {
                bail!("--cache-dir is only supported with the local cache store");
            }
            Ok(Box::new(BoringCacheStore::open(
                config.cache_workspace.clone(),
                config.cache_bin.clone(),
            )?))
        }
        CacheStoreKind::Registry {
            reference,
            insecure,
        } => {
            let platform = config.platform.as_deref().unwrap_or("linux/amd64");
            Ok(Box::new(crate::cache::registry::RegistryCacheStore::open(
                config.cache_dir.as_deref(),
                reference.clone(),
                platform,
                *insecure,
            )?))
        }
    }
}

pub fn acquire_cache_lock(lock_path: &Path) -> Result<CacheLock> {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    file.lock_exclusive()?;
    Ok(CacheLock {
        _file: file,
        _path: lock_path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::{cache_tag, normalize_cache_tag};

    #[test]
    fn normalizes_cache_tags_without_hashing() {
        assert_eq!(
            normalize_cache_tag("toolchain:linux/amd64:abc123"),
            "toolchain-linux-amd64-abc123"
        );
        assert_eq!(
            normalize_cache_tag("  bundle key with spaces  "),
            "bundle-key-with-spaces"
        );
    }

    #[test]
    fn uses_normalized_logical_key_as_cache_tag() {
        assert_eq!(
            cache_tag("bootsnap-ec05991b4103ae45"),
            "bootsnap-ec05991b4103ae45"
        );
    }
}
