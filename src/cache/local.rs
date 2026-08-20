use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use tempfile::NamedTempFile;

use crate::cache::backend::{CacheBackend, CacheManifest};
use crate::cache::tree::{restore_tree_cache, save_tree_cache};
use crate::cache::{
    CacheLock, CacheRestore, CacheSave, CacheStore, acquire_cache_lock, cache_restore_keys,
    normalize_cache_tag,
};
use crate::schema::CacheMount;

#[derive(Debug, Clone)]
pub struct LocalCacheStore {
    backend: LocalCacheBackend,
}

#[derive(Debug, Clone)]
pub struct LocalCacheBackend {
    root: PathBuf,
    refs_dir: PathBuf,
    blobs_dir: PathBuf,
    tmp_dir: PathBuf,
}

impl LocalCacheStore {
    pub fn open(root: &Path) -> Result<Self> {
        Ok(Self {
            backend: LocalCacheBackend::open(root)?,
        })
    }

    pub fn root(&self) -> &Path {
        self.backend.root()
    }

    #[cfg(test)]
    pub(crate) fn inspect_key(&self, key: &str) -> Result<Option<CacheManifest>> {
        self.backend.resolve_ref(key)
    }
}

impl LocalCacheBackend {
    pub fn open(root: &Path) -> Result<Self> {
        let root = root.to_path_buf();
        let refs_dir = root.join("refs");
        let blobs_dir = root.join("blobs").join("sha256");
        let tmp_dir = root.join("tmp");

        fs::create_dir_all(&refs_dir)
            .with_context(|| format!("failed to create {}", refs_dir.display()))?;
        fs::create_dir_all(&blobs_dir)
            .with_context(|| format!("failed to create {}", blobs_dir.display()))?;
        fs::create_dir_all(&tmp_dir)
            .with_context(|| format!("failed to create {}", tmp_dir.display()))?;

        Ok(Self {
            root,
            refs_dir,
            blobs_dir,
            tmp_dir,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn tmp_dir(&self) -> &Path {
        &self.tmp_dir
    }

    pub fn lock_path(&self, key: &str) -> PathBuf {
        self.root
            .join("locks")
            .join(format!("{}.lock", normalize_cache_tag(key)))
    }

    fn ref_path(&self, key: &str) -> PathBuf {
        self.refs_dir.join(format!("{}.json", hash_string(key)))
    }

    fn blob_path(&self, digest: &str) -> PathBuf {
        self.blobs_dir.join(format!("{digest}.tar.zst"))
    }

    fn load_manifest(&self, key: &str) -> Result<Option<CacheManifest>> {
        let path = self.ref_path(key);
        if !path.exists() {
            return Ok(None);
        }
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let manifest = serde_json::from_str::<CacheManifest>(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(Some(manifest))
    }

    fn write_manifest(&self, key: &str, manifest: &CacheManifest) -> Result<()> {
        let path = self.ref_path(key);
        let mut manifest = manifest.clone();
        manifest
            .metadata
            .insert("ref_key".to_string(), key.to_string());
        let temp = NamedTempFile::new_in(&self.tmp_dir)?;
        fs::write(
            temp.path(),
            serde_json::to_vec_pretty(&manifest).context("failed to serialize cache manifest")?,
        )
        .with_context(|| format!("failed to write {}", temp.path().display()))?;
        temp.persist(&path)
            .map_err(|err| anyhow!("failed to persist {}: {}", path.display(), err.error))?;
        Ok(())
    }
}

impl CacheBackend for LocalCacheBackend {
    fn kind(&self) -> &'static str {
        "local"
    }

    fn detail(&self) -> String {
        self.root.display().to_string()
    }

    fn resolve_ref(&self, key: &str) -> Result<Option<CacheManifest>> {
        self.load_manifest(key)
    }

    fn publish_ref(&self, key: &str, manifest: &CacheManifest) -> Result<()> {
        self.write_manifest(key, manifest)
    }

    fn has_blob(&self, digest: &str) -> Result<bool> {
        Ok(self.blob_path(digest).exists())
    }

    fn fetch_blob(&self, digest: &str, destination: &Path) -> Result<()> {
        let blob_path = self.blob_path(digest);
        fs::copy(&blob_path, destination).with_context(|| {
            format!(
                "failed to copy local cache blob {} to {}",
                blob_path.display(),
                destination.display()
            )
        })?;
        Ok(())
    }

    fn store_blob(&self, digest: &str, source: &Path) -> Result<()> {
        let blob_path = self.blob_path(digest);
        if blob_path.exists() {
            return Ok(());
        }

        let temp = NamedTempFile::new_in(&self.tmp_dir)?;
        fs::copy(source, temp.path()).with_context(|| {
            format!(
                "failed to copy {} into local cache temp blob {}",
                source.display(),
                temp.path().display()
            )
        })?;
        match temp.persist(&blob_path) {
            Ok(_) => Ok(()),
            Err(_err) if blob_path.exists() => Ok(()),
            Err(err) => Err(anyhow!(
                "failed to persist {}: {}",
                blob_path.display(),
                err.error
            )),
        }
    }
}

impl CacheStore for LocalCacheStore {
    fn kind(&self) -> &'static str {
        self.backend.kind()
    }

    fn detail(&self) -> String {
        self.backend.detail()
    }

    fn restore(&self, entry: &CacheMount, destination: &Path) -> Result<CacheRestore> {
        for key in cache_restore_keys(entry) {
            let restored = restore_tree_cache(&self.backend, key, destination, Some(&entry.path))?;
            if restored.hit {
                return Ok(restored);
            }
        }
        Ok(CacheRestore {
            hit: false,
            digest: None,
            bytes: 0,
            content_hash: None,
        })
    }

    fn save(&self, entry: &CacheMount, source: &Path) -> Result<CacheSave> {
        save_tree_cache(&self.backend, &entry.key, source, Some(&entry.path))
    }

    fn backend(&self) -> &dyn crate::cache::backend::CacheBackend {
        &self.backend
    }

    fn lock(&self, entry: &CacheMount) -> Result<CacheLock> {
        acquire_cache_lock(&self.backend.lock_path(&entry.key))
    }
}

pub fn default_cache_dir(override_dir: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_dir {
        return Ok(path.to_path_buf());
    }

    let Some(home) = std::env::var_os("HOME") else {
        return Err(anyhow!("HOME is not set; pass --cache-dir explicitly"));
    };

    Ok(PathBuf::from(home).join(".boringbuilder").join("cache"))
}
fn hash_string(value: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use crate::cache::CacheStore;
    use crate::cache::backend::{CacheBackend, TREE_CACHE_ARTIFACT_KIND};
    use crate::schema::CacheMount;

    use super::{LocalCacheBackend, LocalCacheStore};

    #[test]
    fn round_trips_directory_contents() {
        let temp = tempdir().unwrap();
        let store = LocalCacheStore::open(temp.path().join("store").as_path()).unwrap();
        let source = temp.path().join("source");
        let dest = temp.path().join("dest");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested/file.txt"), "hello").unwrap();

        let entry = CacheMount {
            id: "bundle".to_string(),
            path: "/workspace/vendor/bundle".to_string(),
            key: "bundle-key".to_string(),
            restore_from: Vec::new(),
            mode: crate::schema::CacheMode::Shared,
        };

        let saved = store.save(&entry, &source).unwrap();
        assert!(saved.bytes > 0);

        let restored = store.restore(&entry, &dest).unwrap();
        assert!(restored.hit);
        assert_eq!(
            fs::read_to_string(dest.join("nested/file.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn saves_identical_directory_to_same_blob_digest() {
        let temp = tempdir().unwrap();
        let store = LocalCacheStore::open(temp.path().join("store").as_path()).unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested/file.txt"), "hello").unwrap();

        let entry = CacheMount {
            id: "bundle".to_string(),
            path: "/workspace/vendor/bundle".to_string(),
            key: "bundle-key".to_string(),
            restore_from: Vec::new(),
            mode: crate::schema::CacheMode::Shared,
        };

        let first = store.save(&entry, &source).unwrap();
        let second = store.save(&entry, &source).unwrap();

        assert_eq!(first.digest, second.digest);
        let blobs = fs::read_dir(store.root().join("blobs").join("sha256"))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(blobs.len(), 1);
    }

    #[test]
    fn publishes_manifest_refs_for_tree_caches() {
        let temp = tempdir().unwrap();
        let store = LocalCacheStore::open(temp.path().join("store").as_path()).unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), "hello").unwrap();

        let entry = CacheMount {
            id: "bundle".to_string(),
            path: "/workspace/vendor/bundle".to_string(),
            key: "bundle-key".to_string(),
            restore_from: Vec::new(),
            mode: crate::schema::CacheMode::Shared,
        };

        let saved = store.save(&entry, &source).unwrap();
        let backend = LocalCacheBackend::open(store.root()).unwrap();
        let manifest = backend.resolve_ref(&entry.key).unwrap().unwrap();

        assert_eq!(manifest.kind, TREE_CACHE_ARTIFACT_KIND);
        assert_eq!(manifest.blobs.len(), 1);
        assert_eq!(manifest.blobs[0].digest, saved.digest);
        assert_eq!(manifest.blobs[0].bytes, saved.bytes);
        assert!(manifest.metadata.contains_key("content_hash"));
    }

    #[test]
    fn rejects_legacy_ref_format() {
        let temp = tempdir().unwrap();
        let store = LocalCacheStore::open(temp.path().join("store").as_path()).unwrap();
        let source = temp.path().join("source");
        let dest = temp.path().join("dest");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested/file.txt"), "hello").unwrap();

        let entry = CacheMount {
            id: "bundle".to_string(),
            path: "/workspace/vendor/bundle".to_string(),
            key: "bundle-key".to_string(),
            restore_from: Vec::new(),
            mode: crate::schema::CacheMode::Shared,
        };

        let saved = store.save(&entry, &source).unwrap();
        let ref_path = store
            .root()
            .join("refs")
            .join(format!("{}.json", super::hash_string(&entry.key)));
        fs::write(
            &ref_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "digest": saved.digest.clone(),
                "bytes": saved.bytes,
                "content_hash": "legacy-hash",
            }))
            .unwrap(),
        )
        .unwrap();

        let error = store.restore(&entry, &dest).unwrap_err();
        assert!(
            error.to_string().contains("failed to parse"),
            "unexpected error: {error:#}"
        );
    }
}
