use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub const TREE_CACHE_ARTIFACT_KIND: &str = "tree.v1";
pub const SETUP_SNAPSHOT_ARTIFACT_KIND: &str = "setup-snapshot.v1";
pub const BUILD_CACHE_LAYOUT_ARTIFACT_KIND: &str = "build-cache-layout.v1";
pub const TREE_CACHE_MOUNT_PATH_METADATA_KEY: &str = "mount_path";
pub const CACHE_REF_KEY_METADATA_KEY: &str = "ref_key";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheManifest {
    #[serde(default = "default_cache_manifest_version")]
    pub version: u32,
    pub kind: String,
    #[serde(default)]
    pub blobs: Vec<CacheManifestBlob>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheManifestBlob {
    pub digest: String,
    pub bytes: u64,
}

pub trait CacheBackend {
    fn kind(&self) -> &'static str;
    fn detail(&self) -> String;
    fn resolve_ref(&self, key: &str) -> Result<Option<CacheManifest>>;
    fn publish_ref(&self, key: &str, manifest: &CacheManifest) -> Result<()>;
    fn batch_has_refs(&self, _keys: &[String]) -> Result<Option<Vec<bool>>> {
        Ok(None)
    }
    fn has_blob(&self, digest: &str) -> Result<bool>;
    fn fetch_blob(&self, digest: &str, destination: &Path) -> Result<()>;
    fn store_blob(&self, digest: &str, source: &Path) -> Result<()>;

    fn supports_native_tree_blobs(&self) -> bool {
        false
    }

    fn fetch_tree_blob(&self, _digest: &str, _destination: &Path) -> Result<()> {
        bail!("{} does not support native tree blob restore", self.kind())
    }

    fn store_tree_blob(&self, _digest: &str, _source: &Path) -> Result<()> {
        bail!("{} does not support native tree blob save", self.kind())
    }

    fn materialize_native_tree_blob(
        &self,
        _content_hash: &str,
        _source: &Path,
    ) -> Result<Option<CacheManifestBlob>> {
        Ok(None)
    }

    fn supports_native_build_cache_layout_blobs(&self) -> bool {
        false
    }

    fn fetch_build_cache_layout_blob(&self, _digest: &str, _destination: &Path) -> Result<()> {
        bail!(
            "{} does not support native build-cache layout blob restore",
            self.kind()
        )
    }

    fn store_build_cache_layout_blob(&self, _digest: &str, _source: &Path) -> Result<()> {
        bail!(
            "{} does not support native build-cache layout blob save",
            self.kind()
        )
    }
}

pub fn tree_cache_manifest(
    digest: String,
    bytes: u64,
    content_hash: Option<String>,
) -> CacheManifest {
    single_blob_manifest(TREE_CACHE_ARTIFACT_KIND, digest, bytes, content_hash)
}

pub fn setup_snapshot_manifest(
    digest: String,
    bytes: u64,
    content_hash: Option<String>,
) -> CacheManifest {
    single_blob_manifest(SETUP_SNAPSHOT_ARTIFACT_KIND, digest, bytes, content_hash)
}

pub fn build_cache_layout_manifest(
    digest: String,
    bytes: u64,
    content_hash: Option<String>,
) -> CacheManifest {
    single_blob_manifest(
        BUILD_CACHE_LAYOUT_ARTIFACT_KIND,
        digest,
        bytes,
        content_hash,
    )
}

pub fn single_blob_from_manifest(
    manifest: &CacheManifest,
    expected_kind: &str,
    backend_kind: &str,
) -> Result<CacheManifestBlob> {
    if manifest.kind != expected_kind {
        anyhow::bail!(
            "unsupported {backend_kind} cache manifest kind '{}'; expected {}",
            manifest.kind,
            expected_kind
        );
    }
    if manifest.blobs.len() != 1 {
        anyhow::bail!(
            "unsupported {backend_kind} cache manifest with {} blobs; expected 1",
            manifest.blobs.len()
        );
    }
    Ok(manifest.blobs[0].clone())
}

fn single_blob_manifest(
    kind: &str,
    digest: String,
    bytes: u64,
    content_hash: Option<String>,
) -> CacheManifest {
    let mut metadata = BTreeMap::new();
    if let Some(content_hash) = content_hash {
        metadata.insert("content_hash".to_string(), content_hash);
    }
    CacheManifest {
        version: default_cache_manifest_version(),
        kind: kind.to_string(),
        blobs: vec![CacheManifestBlob { digest, bytes }],
        metadata,
    }
}

pub fn manifest_content_hash(manifest: &CacheManifest) -> Option<&str> {
    manifest.metadata.get("content_hash").map(String::as_str)
}

pub fn manifest_mount_path(manifest: &CacheManifest) -> Option<&str> {
    manifest
        .metadata
        .get(TREE_CACHE_MOUNT_PATH_METADATA_KEY)
        .map(String::as_str)
}

pub fn manifest_ref_key(manifest: &CacheManifest) -> Option<&str> {
    manifest
        .metadata
        .get(CACHE_REF_KEY_METADATA_KEY)
        .map(String::as_str)
}

fn default_cache_manifest_version() -> u32 {
    1
}
