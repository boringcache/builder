use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

use crate::cache::archive::{archive_directory_with_digest, unpack_archive};
use crate::cache::backend::{
    CacheBackend, TREE_CACHE_MOUNT_PATH_METADATA_KEY, manifest_content_hash, manifest_mount_path,
    single_blob_from_manifest, tree_cache_manifest,
};
use crate::cache::{CacheRestore, CacheSave};
use crate::util::fs::{clear_directory, directory_content_hash};

const TREE_BLOB_FORMAT_METADATA_KEY: &str = "tree_blob_format";
const TREE_BLOB_FORMAT_DIRECTORY_V1: &str = "tree-dir.v1";

pub fn restore_tree_cache(
    backend: &dyn CacheBackend,
    key: &str,
    destination: &Path,
    mount_path: Option<&str>,
) -> Result<CacheRestore> {
    let Some(manifest) = backend.resolve_ref(key)? else {
        return Ok(CacheRestore {
            hit: false,
            digest: None,
            bytes: 0,
            content_hash: None,
        });
    };
    if mount_path_mismatch(&manifest, mount_path) {
        return Ok(CacheRestore {
            hit: false,
            digest: None,
            bytes: 0,
            content_hash: None,
        });
    }

    let blob = tree_blob_from_manifest(&manifest, backend.kind())?;
    if !backend.has_blob(&blob.digest)? {
        return Ok(CacheRestore {
            hit: false,
            digest: None,
            bytes: 0,
            content_hash: None,
        });
    }

    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    clear_directory(destination)?;
    if backend.supports_native_tree_blobs() && manifest_uses_native_tree_blob(&manifest) {
        backend.fetch_tree_blob(&blob.digest, destination)?;
    } else {
        let temp =
            NamedTempFile::new().context("failed to create temporary tree-cache blob file")?;
        match backend.fetch_blob(&blob.digest, temp.path()) {
            Ok(()) => unpack_archive(temp.path(), destination)?,
            Err(error)
                if backend.supports_native_tree_blobs()
                    && is_legacy_archive_blob_mismatch(&error) =>
            {
                clear_directory(destination)?;
                backend.fetch_tree_blob(&blob.digest, destination)?;
            }
            Err(error) => return Err(error),
        }
    }

    Ok(CacheRestore {
        hit: true,
        digest: Some(blob.digest),
        bytes: blob.bytes,
        content_hash: manifest_content_hash(&manifest).map(str::to_string),
    })
}

pub fn save_tree_cache(
    backend: &dyn CacheBackend,
    key: &str,
    source: &Path,
    mount_path: Option<&str>,
) -> Result<CacheSave> {
    fs::create_dir_all(source).with_context(|| format!("failed to create {}", source.display()))?;

    // Native-tree-blobs path (BoringCache): needs content_hash upfront for
    // materialization.  Separate tree walk is unavoidable here.
    if backend.supports_native_tree_blobs() {
        return save_tree_cache_native(backend, key, source, mount_path);
    }

    // Archive path (local / registry).
    // Check content_hash first to avoid archiving when content is unchanged.
    let content_hash = directory_content_hash(source)?;
    if let Some(existing) = backend.resolve_ref(key)?
        && !mount_path_mismatch(&existing, mount_path)
        && manifest_content_hash(&existing) == Some(content_hash.as_str())
        && let Ok(blob) = tree_blob_from_manifest(&existing, backend.kind())
        && backend.has_blob(&blob.digest)?
    {
        return Ok(CacheSave {
            digest: blob.digest,
            bytes: blob.bytes,
            content_hash,
        });
    }

    // Content changed — archive and store.
    let temp = NamedTempFile::new().context("failed to create temporary tree-cache archive")?;
    let (digest, bytes) = archive_directory_with_digest(source, temp.path())?;

    if !backend.has_blob(&digest)? {
        backend.store_blob(&digest, temp.path())?;
    }

    let mut manifest = tree_cache_manifest(digest.clone(), bytes, Some(content_hash.clone()));
    apply_mount_path_metadata(&mut manifest, mount_path);
    backend.publish_ref(key, &manifest)?;

    Ok(CacheSave {
        digest,
        bytes,
        content_hash,
    })
}

/// Save path for backends that support native tree blobs (BoringCache).
/// These don't archive to tar.zst — they save the directory tree directly.
fn save_tree_cache_native(
    backend: &dyn CacheBackend,
    key: &str,
    source: &Path,
    mount_path: Option<&str>,
) -> Result<CacheSave> {
    let content_hash = directory_content_hash(source)?;

    // Skip if existing ref matches.
    if let Some(existing) = backend.resolve_ref(key)?
        && !mount_path_mismatch(&existing, mount_path)
        && manifest_content_hash(&existing) == Some(content_hash.as_str())
        && let Ok(blob) = tree_blob_from_manifest(&existing, backend.kind())
        && backend.has_blob(&blob.digest)?
    {
        return Ok(CacheSave {
            digest: blob.digest,
            bytes: blob.bytes,
            content_hash: content_hash.clone(),
        });
    }

    if let Some(blob) = backend.materialize_native_tree_blob(&content_hash, source)? {
        let mut manifest =
            tree_cache_manifest(blob.digest.clone(), blob.bytes, Some(content_hash.clone()));
        apply_mount_path_metadata(&mut manifest, mount_path);
        manifest.metadata.insert(
            TREE_BLOB_FORMAT_METADATA_KEY.to_string(),
            TREE_BLOB_FORMAT_DIRECTORY_V1.to_string(),
        );
        backend.publish_ref(key, &manifest)?;
        return Ok(CacheSave {
            digest: blob.digest,
            bytes: blob.bytes,
            content_hash,
        });
    }

    // Fallback: archive + store as native tree blob.
    let temp = NamedTempFile::new().context("failed to create temporary tree-cache archive")?;
    let (digest, bytes) = archive_directory_with_digest(source, temp.path())?;

    if !backend.has_blob(&digest)? {
        backend.store_tree_blob(&digest, source)?;
    }

    let mut manifest = tree_cache_manifest(digest.clone(), bytes, Some(content_hash.clone()));
    apply_mount_path_metadata(&mut manifest, mount_path);
    manifest.metadata.insert(
        TREE_BLOB_FORMAT_METADATA_KEY.to_string(),
        TREE_BLOB_FORMAT_DIRECTORY_V1.to_string(),
    );
    backend.publish_ref(key, &manifest)?;

    Ok(CacheSave {
        digest,
        bytes,
        content_hash,
    })
}

pub fn tree_blob_from_manifest(
    manifest: &crate::cache::backend::CacheManifest,
    backend_kind: &str,
) -> Result<crate::cache::backend::CacheManifestBlob> {
    single_blob_from_manifest(
        manifest,
        crate::cache::backend::TREE_CACHE_ARTIFACT_KIND,
        backend_kind,
    )
}

fn manifest_uses_native_tree_blob(manifest: &crate::cache::backend::CacheManifest) -> bool {
    manifest
        .metadata
        .get(TREE_BLOB_FORMAT_METADATA_KEY)
        .is_some_and(|value| value == TREE_BLOB_FORMAT_DIRECTORY_V1)
}

fn is_legacy_archive_blob_mismatch(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains("payload.bin"))
}

fn apply_mount_path_metadata(
    manifest: &mut crate::cache::backend::CacheManifest,
    mount_path: Option<&str>,
) {
    if let Some(mount_path) = mount_path {
        manifest.metadata.insert(
            TREE_CACHE_MOUNT_PATH_METADATA_KEY.to_string(),
            mount_path.to_string(),
        );
    }
}

fn mount_path_mismatch(
    manifest: &crate::cache::backend::CacheManifest,
    mount_path: Option<&str>,
) -> bool {
    match mount_path {
        Some(expected) => manifest_mount_path(manifest) != Some(expected),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::*;
    use crate::cache::backend::{CacheBackend, CacheManifest, CacheManifestBlob};

    struct NativeFallbackBackend {
        fetch_blob_calls: Cell<usize>,
        fetch_tree_blob_calls: Cell<usize>,
    }

    impl NativeFallbackBackend {
        fn manifest() -> CacheManifest {
            CacheManifest {
                version: 1,
                kind: crate::cache::backend::TREE_CACHE_ARTIFACT_KIND.to_string(),
                blobs: vec![CacheManifestBlob {
                    digest: "sha256:test".to_string(),
                    bytes: 123,
                }],
                metadata: BTreeMap::new(),
            }
        }
    }

    impl CacheBackend for NativeFallbackBackend {
        fn kind(&self) -> &'static str {
            "test"
        }

        fn detail(&self) -> String {
            "test".to_string()
        }

        fn resolve_ref(&self, _key: &str) -> Result<Option<CacheManifest>> {
            Ok(Some(Self::manifest()))
        }

        fn publish_ref(&self, _key: &str, _manifest: &CacheManifest) -> Result<()> {
            unreachable!()
        }

        fn has_blob(&self, _digest: &str) -> Result<bool> {
            Ok(true)
        }

        fn fetch_blob(&self, _digest: &str, _destination: &Path) -> Result<()> {
            self.fetch_blob_calls.set(self.fetch_blob_calls.get() + 1);
            anyhow::bail!(
                "failed to copy restored BoringCache blob /tmp/foo/payload.bin into /tmp/bar: No such file or directory"
            );
        }

        fn store_blob(&self, _digest: &str, _source: &Path) -> Result<()> {
            unreachable!()
        }

        fn supports_native_tree_blobs(&self) -> bool {
            true
        }

        fn fetch_tree_blob(&self, _digest: &str, destination: &Path) -> Result<()> {
            self.fetch_tree_blob_calls
                .set(self.fetch_tree_blob_calls.get() + 1);
            fs::create_dir_all(destination.join("cache"))?;
            fs::write(destination.join("cache/ok.txt"), "ok")?;
            Ok(())
        }

        fn store_tree_blob(&self, _digest: &str, _source: &Path) -> Result<()> {
            unreachable!()
        }
    }

    #[test]
    fn restore_tree_cache_falls_back_to_native_blob_on_payload_mismatch() {
        let backend = NativeFallbackBackend {
            fetch_blob_calls: Cell::new(0),
            fetch_tree_blob_calls: Cell::new(0),
        };
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("restore");

        let restore = restore_tree_cache(&backend, "test", &destination, None).unwrap();

        assert!(restore.hit);
        assert_eq!(backend.fetch_blob_calls.get(), 1);
        assert_eq!(backend.fetch_tree_blob_calls.get(), 1);
        assert_eq!(
            fs::read_to_string(destination.join("cache/ok.txt")).unwrap(),
            "ok"
        );
    }

    #[test]
    fn restore_tree_cache_does_not_fallback_for_unrelated_fetch_errors() {
        struct FailingBackend;

        impl CacheBackend for FailingBackend {
            fn kind(&self) -> &'static str {
                "test"
            }

            fn detail(&self) -> String {
                "test".to_string()
            }

            fn resolve_ref(&self, _key: &str) -> Result<Option<CacheManifest>> {
                Ok(Some(NativeFallbackBackend::manifest()))
            }

            fn publish_ref(&self, _key: &str, _manifest: &CacheManifest) -> Result<()> {
                unreachable!()
            }

            fn has_blob(&self, _digest: &str) -> Result<bool> {
                Ok(true)
            }

            fn fetch_blob(&self, _digest: &str, _destination: &Path) -> Result<()> {
                anyhow::bail!("some other fetch failure")
            }

            fn store_blob(&self, _digest: &str, _source: &Path) -> Result<()> {
                unreachable!()
            }

            fn supports_native_tree_blobs(&self) -> bool {
                true
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("restore");
        let error = restore_tree_cache(&FailingBackend, "test", &destination, None).unwrap_err();
        assert!(error.to_string().contains("some other fetch failure"));
    }

    #[test]
    fn save_tree_cache_uses_native_blob_materialization_when_available() {
        struct NativeMaterializeBackend {
            materialize_calls: Cell<usize>,
            publish_calls: Cell<usize>,
        }

        impl CacheBackend for NativeMaterializeBackend {
            fn kind(&self) -> &'static str {
                "test"
            }

            fn detail(&self) -> String {
                "test".to_string()
            }

            fn resolve_ref(&self, _key: &str) -> Result<Option<CacheManifest>> {
                Ok(None)
            }

            fn publish_ref(&self, _key: &str, manifest: &CacheManifest) -> Result<()> {
                self.publish_calls.set(self.publish_calls.get() + 1);
                assert_eq!(
                    manifest
                        .metadata
                        .get(TREE_BLOB_FORMAT_METADATA_KEY)
                        .map(String::as_str),
                    Some(TREE_BLOB_FORMAT_DIRECTORY_V1)
                );
                Ok(())
            }

            fn has_blob(&self, _digest: &str) -> Result<bool> {
                Ok(false)
            }

            fn fetch_blob(&self, _digest: &str, _destination: &Path) -> Result<()> {
                unreachable!()
            }

            fn store_blob(&self, _digest: &str, _source: &Path) -> Result<()> {
                unreachable!()
            }

            fn supports_native_tree_blobs(&self) -> bool {
                true
            }

            fn materialize_native_tree_blob(
                &self,
                content_hash: &str,
                _source: &Path,
            ) -> Result<Option<CacheManifestBlob>> {
                self.materialize_calls.set(self.materialize_calls.get() + 1);
                Ok(Some(CacheManifestBlob {
                    digest: format!("sha256:{content_hash}"),
                    bytes: 42,
                }))
            }
        }

        let backend = NativeMaterializeBackend {
            materialize_calls: Cell::new(0),
            publish_calls: Cell::new(0),
        };
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("tree");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file.txt"), "hello").unwrap();

        let saved = save_tree_cache(&backend, "key", &source, None).unwrap();

        assert_eq!(backend.materialize_calls.get(), 1);
        assert_eq!(backend.publish_calls.get(), 1);
        assert_eq!(saved.bytes, 42);
        assert!(saved.digest.starts_with("sha256:"));
    }

    #[test]
    fn restore_tree_cache_misses_when_mount_path_metadata_is_missing() {
        struct LegacyBackend;

        impl CacheBackend for LegacyBackend {
            fn kind(&self) -> &'static str {
                "test"
            }

            fn detail(&self) -> String {
                "test".to_string()
            }

            fn resolve_ref(&self, _key: &str) -> Result<Option<CacheManifest>> {
                Ok(Some(tree_cache_manifest(
                    "sha256:test".to_string(),
                    123,
                    Some("hash".to_string()),
                )))
            }

            fn publish_ref(&self, _key: &str, _manifest: &CacheManifest) -> Result<()> {
                unreachable!()
            }

            fn has_blob(&self, _digest: &str) -> Result<bool> {
                Ok(true)
            }

            fn fetch_blob(&self, _digest: &str, _destination: &Path) -> Result<()> {
                unreachable!()
            }

            fn store_blob(&self, _digest: &str, _source: &Path) -> Result<()> {
                unreachable!()
            }
        }

        let destination = tempfile::tempdir().unwrap();
        let restored = restore_tree_cache(
            &LegacyBackend,
            "test",
            destination.path(),
            Some("/opt/mise/installs"),
        )
        .unwrap();

        assert!(!restored.hit);
    }

    #[test]
    fn save_tree_cache_records_mount_path_metadata() {
        use std::cell::RefCell;

        struct RecordingBackend {
            manifest: RefCell<Option<CacheManifest>>,
        }

        impl CacheBackend for RecordingBackend {
            fn kind(&self) -> &'static str {
                "test"
            }

            fn detail(&self) -> String {
                "test".to_string()
            }

            fn resolve_ref(&self, _key: &str) -> Result<Option<CacheManifest>> {
                Ok(None)
            }

            fn publish_ref(&self, _key: &str, manifest: &CacheManifest) -> Result<()> {
                self.manifest.replace(Some(manifest.clone()));
                Ok(())
            }

            fn has_blob(&self, _digest: &str) -> Result<bool> {
                Ok(false)
            }

            fn fetch_blob(&self, _digest: &str, _destination: &Path) -> Result<()> {
                unreachable!()
            }

            fn store_blob(&self, _digest: &str, _source: &Path) -> Result<()> {
                Ok(())
            }
        }

        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("ok.txt"), "ok").unwrap();
        let backend = RecordingBackend {
            manifest: RefCell::new(None),
        };

        save_tree_cache(&backend, "key", source.path(), Some("/opt/mise/installs")).unwrap();

        let manifest = backend.manifest.borrow().clone().unwrap();
        assert_eq!(
            manifest.metadata.get(TREE_CACHE_MOUNT_PATH_METADATA_KEY),
            Some(&"/opt/mise/installs".to_string())
        );
    }
}
