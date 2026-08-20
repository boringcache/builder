use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

use crate::cache::archive::{archive_directory_with_digest, unpack_archive};
use crate::cache::backend::{
    CacheBackend, CacheManifest, CacheManifestBlob, SETUP_SNAPSHOT_ARTIFACT_KIND,
    manifest_content_hash, setup_snapshot_manifest, single_blob_from_manifest,
};
use crate::cache::{CacheRestore, CacheSave};
use crate::util::fs::{clear_directory, directory_content_hash};

const TREE_BLOB_FORMAT_METADATA_KEY: &str = "tree_blob_format";
const TREE_BLOB_FORMAT_DIRECTORY_V1: &str = "tree-dir.v1";
const SETUP_SNAPSHOT_PLATFORM_METADATA_KEY: &str = "setup_snapshot_platform";
const SETUP_SNAPSHOT_BASE_IMAGE_METADATA_KEY: &str = "setup_snapshot_base_image";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetupSnapshotMetadata {
    pub platform: Option<String>,
    pub base_image: Option<String>,
}

pub fn restore_setup_snapshot(
    backend: &dyn CacheBackend,
    key: &str,
    destination: &Path,
) -> Result<CacheRestore> {
    let Some(manifest) = backend.resolve_ref(key)? else {
        return Ok(CacheRestore {
            hit: false,
            digest: None,
            bytes: 0,
            content_hash: None,
        });
    };

    let blob = setup_snapshot_blob_from_manifest(&manifest, backend.kind())?;
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
            NamedTempFile::new().context("failed to create temporary setup snapshot blob file")?;
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

pub fn save_setup_snapshot(
    backend: &dyn CacheBackend,
    key: &str,
    source: &Path,
    metadata: &SetupSnapshotMetadata,
) -> Result<CacheSave> {
    fs::create_dir_all(source).with_context(|| format!("failed to create {}", source.display()))?;

    if backend.supports_native_tree_blobs() {
        return save_setup_snapshot_native(backend, key, source, metadata);
    }

    let content_hash = directory_content_hash(source)?;
    if let Some(existing) = backend.resolve_ref(key)?
        && manifest_content_hash(&existing) == Some(content_hash.as_str())
        && let Ok(blob) = setup_snapshot_blob_from_manifest(&existing, backend.kind())
        && backend.has_blob(&blob.digest)?
    {
        return Ok(CacheSave {
            digest: blob.digest,
            bytes: blob.bytes,
            content_hash,
        });
    }

    let temp = NamedTempFile::new().context("failed to create temporary setup snapshot archive")?;
    let (digest, bytes) = archive_directory_with_digest(source, temp.path())?;
    if !backend.has_blob(&digest)? {
        backend.store_blob(&digest, temp.path())?;
    }

    let mut manifest = setup_snapshot_manifest(digest.clone(), bytes, Some(content_hash.clone()));
    apply_setup_snapshot_metadata(&mut manifest, metadata);
    backend.publish_ref(key, &manifest)?;

    Ok(CacheSave {
        digest,
        bytes,
        content_hash,
    })
}

fn save_setup_snapshot_native(
    backend: &dyn CacheBackend,
    key: &str,
    source: &Path,
    metadata: &SetupSnapshotMetadata,
) -> Result<CacheSave> {
    let content_hash = directory_content_hash(source)?;
    if let Some(existing) = backend.resolve_ref(key)?
        && manifest_content_hash(&existing) == Some(content_hash.as_str())
        && let Ok(blob) = setup_snapshot_blob_from_manifest(&existing, backend.kind())
        && backend.has_blob(&blob.digest)?
    {
        return Ok(CacheSave {
            digest: blob.digest,
            bytes: blob.bytes,
            content_hash,
        });
    }

    if let Some(blob) = backend.materialize_native_tree_blob(&content_hash, source)? {
        let mut manifest =
            setup_snapshot_manifest(blob.digest.clone(), blob.bytes, Some(content_hash.clone()));
        manifest.metadata.insert(
            TREE_BLOB_FORMAT_METADATA_KEY.to_string(),
            TREE_BLOB_FORMAT_DIRECTORY_V1.to_string(),
        );
        apply_setup_snapshot_metadata(&mut manifest, metadata);
        backend.publish_ref(key, &manifest)?;
        return Ok(CacheSave {
            digest: blob.digest,
            bytes: blob.bytes,
            content_hash,
        });
    }

    let temp = NamedTempFile::new().context("failed to create temporary setup snapshot archive")?;
    let (digest, bytes) = archive_directory_with_digest(source, temp.path())?;
    if !backend.has_blob(&digest)? {
        backend.store_tree_blob(&digest, source)?;
    }

    let mut manifest = setup_snapshot_manifest(digest.clone(), bytes, Some(content_hash.clone()));
    manifest.metadata.insert(
        TREE_BLOB_FORMAT_METADATA_KEY.to_string(),
        TREE_BLOB_FORMAT_DIRECTORY_V1.to_string(),
    );
    apply_setup_snapshot_metadata(&mut manifest, metadata);
    backend.publish_ref(key, &manifest)?;

    Ok(CacheSave {
        digest,
        bytes,
        content_hash,
    })
}

pub fn setup_snapshot_blob_from_manifest(
    manifest: &CacheManifest,
    backend_kind: &str,
) -> Result<CacheManifestBlob> {
    single_blob_from_manifest(manifest, SETUP_SNAPSHOT_ARTIFACT_KIND, backend_kind)
}

pub fn manifest_setup_snapshot_platform(manifest: &CacheManifest) -> Option<&str> {
    manifest
        .metadata
        .get(SETUP_SNAPSHOT_PLATFORM_METADATA_KEY)
        .map(String::as_str)
}

pub fn manifest_setup_snapshot_base_image(manifest: &CacheManifest) -> Option<&str> {
    manifest
        .metadata
        .get(SETUP_SNAPSHOT_BASE_IMAGE_METADATA_KEY)
        .map(String::as_str)
}

fn apply_setup_snapshot_metadata(manifest: &mut CacheManifest, metadata: &SetupSnapshotMetadata) {
    if let Some(platform) = &metadata.platform {
        manifest.metadata.insert(
            SETUP_SNAPSHOT_PLATFORM_METADATA_KEY.to_string(),
            platform.clone(),
        );
    }
    if let Some(base_image) = &metadata.base_image {
        manifest.metadata.insert(
            SETUP_SNAPSHOT_BASE_IMAGE_METADATA_KEY.to_string(),
            base_image.clone(),
        );
    }
}

fn manifest_uses_native_tree_blob(manifest: &CacheManifest) -> bool {
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::cell::RefCell;
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::cache::backend::CacheBackend;
    use crate::cache::local::LocalCacheBackend;

    #[test]
    fn local_backend_round_trips_setup_snapshot() {
        let temp = tempdir().unwrap();
        let backend = LocalCacheBackend::open(&temp.path().join("store")).unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("toolchain/bin")).unwrap();
        fs::write(source.join("toolchain/bin/mise"), "ok").unwrap();

        let metadata = SetupSnapshotMetadata {
            platform: Some("linux/amd64".to_string()),
            base_image: Some("ubuntu-24.04".to_string()),
        };
        let saved =
            save_setup_snapshot(&backend, "setup-snapshot-check", &source, &metadata).unwrap();
        assert!(saved.bytes > 0);

        let manifest = backend
            .resolve_ref("setup-snapshot-check")
            .unwrap()
            .unwrap();
        assert_eq!(manifest.kind, SETUP_SNAPSHOT_ARTIFACT_KIND);
        assert_eq!(
            manifest_setup_snapshot_platform(&manifest),
            Some("linux/amd64")
        );
        assert_eq!(
            manifest_setup_snapshot_base_image(&manifest),
            Some("ubuntu-24.04")
        );

        let destination = temp.path().join("restore");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("stale.txt"), "remove-me").unwrap();
        let restored =
            restore_setup_snapshot(&backend, "setup-snapshot-check", &destination).unwrap();
        assert!(restored.hit);
        assert!(!destination.join("stale.txt").exists());
        assert_eq!(
            fs::read_to_string(destination.join("toolchain/bin/mise")).unwrap(),
            "ok"
        );
    }

    #[test]
    fn native_tree_backend_materializes_setup_snapshot() {
        struct NativeMaterializeBackend {
            materialize_calls: Cell<usize>,
            publish_manifest: RefCell<Option<CacheManifest>>,
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
                self.publish_manifest.replace(Some(manifest.clone()));
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
            publish_manifest: RefCell::new(None),
        };
        let temp = tempdir().unwrap();
        let source = temp.path().join("snapshot");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("ok.txt"), "ok").unwrap();

        let saved = save_setup_snapshot(
            &backend,
            "setup-snapshot-native",
            &source,
            &SetupSnapshotMetadata {
                platform: Some("linux/arm64".to_string()),
                base_image: Some("debian-12".to_string()),
            },
        )
        .unwrap();

        assert_eq!(backend.materialize_calls.get(), 1);
        assert_eq!(saved.bytes, 42);
        let manifest = backend.publish_manifest.borrow().clone().unwrap();
        assert_eq!(manifest.kind, SETUP_SNAPSHOT_ARTIFACT_KIND);
        assert_eq!(
            manifest.metadata.get(TREE_BLOB_FORMAT_METADATA_KEY),
            Some(&TREE_BLOB_FORMAT_DIRECTORY_V1.to_string())
        );
        assert_eq!(
            manifest_setup_snapshot_platform(&manifest),
            Some("linux/arm64")
        );
        assert_eq!(
            manifest_setup_snapshot_base_image(&manifest),
            Some("debian-12")
        );
    }
}
