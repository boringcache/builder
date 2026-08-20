use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::cache::archive::{archive_directory_with_digest, unpack_archive};
use crate::cache::backend::{
    CacheBackend, CacheManifest, CacheManifestBlob, single_blob_from_manifest,
};
use crate::ui;
use crate::util::fs::clear_directory;

const BASE_ROOTFS_ARCHIVE_ARTIFACT_KIND: &str = "base-rootfs-archive.v1";
const BASE_ROOTFS_PLATFORM_METADATA_KEY: &str = "platform";
const BASE_ROOTFS_MANIFEST_DIGEST_METADATA_KEY: &str = "manifest_digest";
const BASE_ROOTFS_IMAGE_METADATA_KEY: &str = "image";

#[derive(Debug)]
pub struct ImageCache {
    pub root: PathBuf,
}

impl ImageCache {
    pub fn open() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| anyhow!("HOME is not set; cannot determine image cache location"))?;
        let root = PathBuf::from(home).join(".boringbuilder").join("images");
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create image cache at {}", root.display()))?;
        fs::create_dir_all(root.join("tmp")).with_context(|| {
            format!(
                "failed to create image cache tmp dir at {}",
                root.join("tmp").display()
            )
        })?;
        Ok(Self { root })
    }

    pub fn cache_key(image: &str, platform: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(format!("{}\n{}", image, platform).as_bytes());
        hex::encode(hasher.finalize())
    }

    pub fn backend_rootfs_cache_key(platform: &str, manifest_digest: &str) -> String {
        crate::cache::cache_tag(&format!("base-rootfs-v1:{platform}:{manifest_digest}"))
    }

    pub fn cached_image_dir(&self, image: &str, platform: &str) -> Option<PathBuf> {
        let key = Self::cache_key(image, platform);
        let oci_dir = self.root.join("oci").join(&key);
        if oci_dir.join("manifest.json").exists() {
            println!(
                "{} {} {} ({})",
                ui::prefix(),
                ui::success("image cache hit"),
                image,
                platform
            );
            Some(oci_dir)
        } else {
            None
        }
    }

    pub fn pull_or_cached(
        &self,
        image: &str,
        platform: &str,
        platform_os: &str,
        platform_arch: &str,
    ) -> Result<PathBuf> {
        if let Some(oci_dir) = self.cached_image_dir(image, platform) {
            return Ok(oci_dir);
        }

        let key = Self::cache_key(image, platform);
        let oci_dir = self.root.join("oci").join(&key);
        ui::print_status(format!("pulling image {} ({})", image, platform));
        let pulled_dir = crate::registry::pull_image(image, platform_os, platform_arch, &self.root)
            .with_context(|| format!("failed to pull {}", image))?;

        fs::create_dir_all(oci_dir.parent().unwrap())?;
        match fs::rename(&pulled_dir, &oci_dir) {
            Ok(()) => {}
            Err(_) if oci_dir.join("manifest.json").exists() => {}
            Err(err) => {
                return Err(anyhow!(err))
                    .context(format!("failed to cache image at {}", oci_dir.display()));
            }
        }

        Ok(oci_dir)
    }

    pub fn unpack_or_cached(
        &self,
        image_dir: &Path,
        image: &str,
        platform: &str,
        backend: Option<&dyn CacheBackend>,
        unpack_fn: impl FnOnce(&Path, &Path) -> Result<()>,
    ) -> Result<PathBuf> {
        let key = Self::cache_key(image, platform);
        let rootfs_dir = self.root.join("rootfs").join(&key);
        let marker = rootfs_dir.join(".boringbuilder-unpacked");

        if marker.exists() {
            ui::print_status(format!("rootfs cache hit {image} ({platform})"));
            return Ok(rootfs_dir);
        }

        if let Some(backend) = backend {
            let manifest_digest = image_manifest_cache_digest(image_dir)?;
            if let Some(restored) =
                self.restore_rootfs_from_backend(backend, &manifest_digest, image, platform)?
            {
                return Ok(restored);
            }
        }

        ui::print_status("unpacking image rootfs");
        if rootfs_dir.exists() {
            fs::remove_dir_all(&rootfs_dir)?;
        }
        fs::create_dir_all(&rootfs_dir)?;
        unpack_fn(image_dir, &rootfs_dir)?;
        fs::write(&marker, "")?;

        if let Some(backend) = backend {
            let manifest_digest = image_manifest_cache_digest(image_dir)?;
            self.save_rootfs_to_backend(backend, &manifest_digest, image, platform, &rootfs_dir);
        }

        Ok(rootfs_dir)
    }

    pub fn restore_rootfs_from_backend(
        &self,
        backend: &dyn CacheBackend,
        manifest_digest: &str,
        image: &str,
        platform: &str,
    ) -> Result<Option<PathBuf>> {
        let key = Self::cache_key(image, platform);
        let rootfs_dir = self.root.join("rootfs").join(&key);
        let cache_key = Self::backend_rootfs_cache_key(platform, manifest_digest);
        ui::print_status(format!("restoring base rootfs cache {image} ({platform})"));
        match restore_rootfs_archive_cache(
            backend,
            &cache_key,
            &rootfs_dir,
            &self.root.join("tmp"),
            manifest_digest,
            image,
            platform,
        ) {
            Ok(Some(_blob)) => {
                fs::write(rootfs_dir.join(".boringbuilder-unpacked"), "")?;
                ui::print_status(format!("base rootfs cache hit {image} ({platform})"));
                Ok(Some(rootfs_dir))
            }
            Ok(None) => {
                ui::print_detail(format!("base rootfs cache miss {image} ({platform})"));
                Ok(None)
            }
            Err(error) => {
                ui::print_detail(format!(
                    "base rootfs cache restore skipped {image} ({platform}): {error:#}"
                ));
                Ok(None)
            }
        }
    }

    pub fn save_rootfs_to_backend(
        &self,
        backend: &dyn CacheBackend,
        manifest_digest: &str,
        image: &str,
        platform: &str,
        rootfs_dir: &Path,
    ) {
        let cache_key = Self::backend_rootfs_cache_key(platform, manifest_digest);
        match save_rootfs_archive_cache(
            backend,
            &cache_key,
            rootfs_dir,
            &self.root.join("tmp"),
            manifest_digest,
            image,
            platform,
        ) {
            Ok(blob) => {
                ui::print_detail(format!(
                    "base rootfs cache saved {image} ({platform}) digest={} bytes={}",
                    blob.digest, blob.bytes
                ));
            }
            Err(error) => {
                ui::print_detail(format!(
                    "base rootfs cache save skipped {image} ({platform}): {error:#}"
                ));
            }
        }
    }
}

fn restore_rootfs_archive_cache(
    backend: &dyn CacheBackend,
    cache_key: &str,
    rootfs_dir: &Path,
    tmp_dir: &Path,
    manifest_digest: &str,
    image: &str,
    platform: &str,
) -> Result<Option<CacheManifestBlob>> {
    let Some(manifest) = backend.resolve_ref(cache_key)? else {
        return Ok(None);
    };
    if manifest.kind != BASE_ROOTFS_ARCHIVE_ARTIFACT_KIND {
        ui::print_detail(format!(
            "base rootfs cache ignored {image} ({platform}): stored format {}",
            manifest.kind
        ));
        return Ok(None);
    }
    if rootfs_manifest_mismatch(&manifest, manifest_digest, platform) {
        ui::print_detail(format!(
            "base rootfs cache ignored {image} ({platform}): metadata mismatch"
        ));
        return Ok(None);
    }

    let blob =
        single_blob_from_manifest(&manifest, BASE_ROOTFS_ARCHIVE_ARTIFACT_KIND, backend.kind())?;
    if !backend.has_blob(&blob.digest)? {
        return Ok(None);
    }

    fs::create_dir_all(tmp_dir)
        .with_context(|| format!("failed to create image cache tmp dir {}", tmp_dir.display()))?;
    let temp = NamedTempFile::new_in(tmp_dir)
        .context("failed to create temporary base-rootfs cache archive")?;
    ui::print_status(format!(
        "downloading base rootfs cache {image} ({platform})"
    ));
    backend.fetch_blob(&blob.digest, temp.path())?;

    fs::create_dir_all(rootfs_dir)
        .with_context(|| format!("failed to create {}", rootfs_dir.display()))?;
    clear_directory(rootfs_dir)?;
    ui::print_status(format!("unpacking base rootfs cache {image} ({platform})"));
    if let Err(error) = unpack_archive(temp.path(), rootfs_dir) {
        let _ = fs::remove_dir_all(rootfs_dir);
        return Err(error).with_context(|| {
            format!(
                "failed to unpack base rootfs cache {} into {}",
                blob.digest,
                rootfs_dir.display()
            )
        });
    }

    Ok(Some(blob))
}

fn save_rootfs_archive_cache(
    backend: &dyn CacheBackend,
    cache_key: &str,
    rootfs_dir: &Path,
    tmp_dir: &Path,
    manifest_digest: &str,
    image: &str,
    platform: &str,
) -> Result<CacheManifestBlob> {
    if let Some(existing) = backend.resolve_ref(cache_key)?
        && existing.kind == BASE_ROOTFS_ARCHIVE_ARTIFACT_KIND
        && !rootfs_manifest_mismatch(&existing, manifest_digest, platform)
        && let Ok(blob) =
            single_blob_from_manifest(&existing, BASE_ROOTFS_ARCHIVE_ARTIFACT_KIND, backend.kind())
        && backend.has_blob(&blob.digest)?
    {
        return Ok(blob);
    }

    fs::create_dir_all(tmp_dir)
        .with_context(|| format!("failed to create image cache tmp dir {}", tmp_dir.display()))?;
    let temp = NamedTempFile::new_in(tmp_dir)
        .context("failed to create temporary base-rootfs cache archive")?;
    ui::print_status(format!("archiving base rootfs cache {image} ({platform})"));
    let (digest, bytes) = archive_directory_with_digest(rootfs_dir, temp.path())?;
    if !backend.has_blob(&digest)? {
        ui::print_status(format!("uploading base rootfs cache {image} ({platform})"));
        backend.store_blob(&digest, temp.path())?;
    }

    let blob = CacheManifestBlob {
        digest: digest.clone(),
        bytes,
    };
    let manifest = base_rootfs_archive_manifest(blob.clone(), manifest_digest, image, platform);
    ui::print_status(format!("publishing base rootfs cache {image} ({platform})"));
    backend.publish_ref(cache_key, &manifest)?;
    Ok(blob)
}

fn base_rootfs_archive_manifest(
    blob: CacheManifestBlob,
    manifest_digest: &str,
    image: &str,
    platform: &str,
) -> CacheManifest {
    let mut metadata = BTreeMap::new();
    metadata.insert(
        BASE_ROOTFS_MANIFEST_DIGEST_METADATA_KEY.to_string(),
        manifest_digest.to_string(),
    );
    metadata.insert(
        BASE_ROOTFS_PLATFORM_METADATA_KEY.to_string(),
        platform.to_string(),
    );
    metadata.insert(
        BASE_ROOTFS_IMAGE_METADATA_KEY.to_string(),
        image.to_string(),
    );

    CacheManifest {
        version: 1,
        kind: BASE_ROOTFS_ARCHIVE_ARTIFACT_KIND.to_string(),
        blobs: vec![blob],
        metadata,
    }
}

fn rootfs_manifest_mismatch(
    manifest: &CacheManifest,
    manifest_digest: &str,
    platform: &str,
) -> bool {
    manifest
        .metadata
        .get(BASE_ROOTFS_MANIFEST_DIGEST_METADATA_KEY)
        .is_none_or(|value| value != manifest_digest)
        || manifest
            .metadata
            .get(BASE_ROOTFS_PLATFORM_METADATA_KEY)
            .is_none_or(|value| value != platform)
}

fn image_manifest_cache_digest(image_dir: &Path) -> Result<String> {
    let manifest_path = image_dir.join("manifest.json");
    let manifest_bytes = fs::read(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    if let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&manifest_bytes)
        && let Some(digest) = manifest
            .pointer("/x-boringbuilder/manifestDigest")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
    {
        return Ok(digest.to_string());
    }

    let mut hasher = Sha256::new();
    hasher.update(b"boringbuilder-dir-manifest-v1\0");
    hasher.update(&manifest_bytes);
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use crate::cache::backend::CacheBackend;
    use crate::cache::local::LocalCacheBackend;

    use super::{BASE_ROOTFS_ARCHIVE_ARTIFACT_KIND, ImageCache, image_manifest_cache_digest};

    #[test]
    fn backend_rootfs_cache_key_uses_platform_and_manifest_digest() {
        assert_eq!(
            ImageCache::backend_rootfs_cache_key("linux/arm64", "sha256:abc123"),
            "base-rootfs-v1-linux-arm64-sha256-abc123"
        );
    }

    #[test]
    fn image_manifest_digest_prefers_resolved_manifest_metadata() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("manifest.json"),
            r#"{"x-boringbuilder":{"manifestDigest":"sha256:resolved"}}"#,
        )
        .unwrap();

        assert_eq!(
            image_manifest_cache_digest(temp.path()).unwrap(),
            "sha256:resolved"
        );
    }

    #[test]
    fn backend_rootfs_cache_round_trips_through_archive_blob() {
        let temp = tempfile::tempdir().unwrap();
        let images = temp.path().join("images");
        let cache = ImageCache { root: images };
        let backend = LocalCacheBackend::open(&temp.path().join("cache")).unwrap();
        let rootfs = cache.root.join("rootfs").join(ImageCache::cache_key(
            "example.test/base:latest",
            "linux/amd64",
        ));
        fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
        fs::write(rootfs.join("usr/bin/tool"), "ok").unwrap();
        fs::write(rootfs.join(".boringbuilder-unpacked"), "").unwrap();

        cache.save_rootfs_to_backend(
            &backend,
            "sha256:manifest",
            "example.test/base:latest",
            "linux/amd64",
            &rootfs,
        );
        let manifest = backend
            .resolve_ref(&ImageCache::backend_rootfs_cache_key(
                "linux/amd64",
                "sha256:manifest",
            ))
            .unwrap()
            .unwrap();
        assert_eq!(manifest.kind, BASE_ROOTFS_ARCHIVE_ARTIFACT_KIND);
        fs::remove_dir_all(&rootfs).unwrap();

        let restored = cache
            .restore_rootfs_from_backend(
                &backend,
                "sha256:manifest",
                "example.test/base:latest",
                "linux/amd64",
            )
            .unwrap()
            .unwrap();

        assert_eq!(
            fs::read_to_string(restored.join("usr/bin/tool")).unwrap(),
            "ok"
        );
        assert!(restored.join(".boringbuilder-unpacked").exists());
    }

    #[cfg(unix)]
    #[test]
    fn backend_rootfs_cache_preserves_absolute_symlink_targets() {
        let temp = tempfile::tempdir().unwrap();
        let images = temp.path().join("images");
        let cache = ImageCache { root: images };
        let backend = LocalCacheBackend::open(&temp.path().join("cache")).unwrap();
        let rootfs = cache.root.join("rootfs").join(ImageCache::cache_key(
            "example.test/base:latest",
            "linux/amd64",
        ));
        fs::create_dir_all(rootfs.join("etc/alternatives")).unwrap();
        fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
        fs::write(rootfs.join("usr/bin/mawk"), "ok").unwrap();
        symlink("/usr/bin/mawk", rootfs.join("etc/alternatives/awk")).unwrap();
        fs::write(rootfs.join(".boringbuilder-unpacked"), "").unwrap();

        cache.save_rootfs_to_backend(
            &backend,
            "sha256:absolute-symlink",
            "example.test/base:latest",
            "linux/amd64",
            &rootfs,
        );
        fs::remove_dir_all(&rootfs).unwrap();

        let restored = cache
            .restore_rootfs_from_backend(
                &backend,
                "sha256:absolute-symlink",
                "example.test/base:latest",
                "linux/amd64",
            )
            .unwrap()
            .unwrap();

        assert_eq!(
            fs::read_link(restored.join("etc/alternatives/awk")).unwrap(),
            std::path::PathBuf::from("/usr/bin/mawk")
        );
    }
}
