use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use oci_client::manifest::{OCI_IMAGE_INDEX_MEDIA_TYPE, OciImageIndex, OciImageManifest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache::backend::{CacheBackend, CacheManifest};
use crate::cache::{
    CacheLock, CacheRestore, CacheSave, CacheStore, acquire_cache_lock, normalize_cache_tag,
};
use crate::registry::pull_image_layout;
use crate::registry::push::push_image;
use crate::schema::CacheMount;
use crate::util::fs::{clear_directory, copy_tree, path_size};

const REGISTRY_CACHE_CONFIG_MEDIA_TYPE: &str = "application/vnd.boringbuilder.cache.config.v1+json";
const REGISTRY_CACHE_BLOB_MEDIA_TYPE: &str = "application/vnd.boringbuilder.cache.blob.v1.tar+zstd";
const REGISTRY_REF_KEY_METADATA: &str = "ref_key";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildCacheExportMode {
    Min,
    Max,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildCacheImportSpec {
    Registry { reference: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildCacheExportSpec {
    Registry {
        reference: String,
        mode: BuildCacheExportMode,
        ignore_error: bool,
    },
}

pub fn parse_build_cache_import_specs(raw_specs: &[String]) -> Result<Vec<BuildCacheImportSpec>> {
    raw_specs
        .iter()
        .map(|raw| match parse_registry_spec(raw)? {
            ParsedRegistrySpec::Registry(options) | ParsedRegistrySpec::RegistryExport(options) => {
                validate_registry_options(raw, &options)?;
                Ok(BuildCacheImportSpec::Registry {
                    reference: options.reference,
                })
            }
        })
        .collect()
}

pub fn parse_build_cache_export_specs(raw_specs: &[String]) -> Result<Vec<BuildCacheExportSpec>> {
    raw_specs
        .iter()
        .map(|raw| match parse_registry_spec(raw)? {
            ParsedRegistrySpec::Registry(options) | ParsedRegistrySpec::RegistryExport(options) => {
                validate_registry_options(raw, &options)?;
                Ok(BuildCacheExportSpec::Registry {
                    reference: options.reference,
                    mode: options.mode,
                    ignore_error: options.ignore_error,
                })
            }
        })
        .collect()
}

enum ParsedRegistrySpec {
    Registry(ParsedRegistryOptions),
    RegistryExport(ParsedRegistryOptions),
}

#[derive(Debug, Clone)]
struct ParsedRegistryOptions {
    reference: String,
    mode: BuildCacheExportMode,
    ignore_error: bool,
    oci_media_types: bool,
    image_manifest: bool,
    compression: BuildCacheCompression,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildCacheCompression {
    Gzip,
}

fn validate_registry_options(raw: &str, options: &ParsedRegistryOptions) -> Result<()> {
    if !options.oci_media_types {
        bail!(
            "build-cache spec '{raw}' uses oci-mediatypes=false, which is not supported; exported cache always uses OCI media types"
        );
    }
    let _ = options.image_manifest;
    match options.compression {
        BuildCacheCompression::Gzip => {}
    }
    Ok(())
}

impl BuildCacheCompression {
    fn parse(value: &str, raw: &str) -> Result<Self> {
        match value {
            "gzip" => Ok(Self::Gzip),
            _ => bail!(
                "unsupported build-cache compression '{value}' in '{raw}'; supported today: gzip"
            ),
        }
    }
}

fn parse_bool_field(value: &str, key: &str, raw: &str) -> Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!(
            "unsupported boolean value '{value}' for build-cache spec key '{key}' in '{raw}'; expected true or false"
        ),
    }
}

#[derive(Debug, Clone)]
pub struct RegistryCacheStore {
    backend: RegistryCacheBackend,
}

#[derive(Debug, Clone)]
pub struct RegistryCacheBackend {
    base_ref: String,
    platform_os: String,
    platform_arch: String,
    insecure: bool,
    root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RegistryCacheConfig {
    architecture: String,
    os: String,
    cache_manifest: CacheManifest,
}

impl RegistryCacheStore {
    pub fn open(
        cache_dir: Option<&Path>,
        base_ref: String,
        platform: &str,
        insecure: bool,
    ) -> Result<Self> {
        Ok(Self {
            backend: RegistryCacheBackend::open(cache_dir, base_ref, platform, insecure)?,
        })
    }

    fn reference(&self) -> Result<&str> {
        self.backend.reference()
    }
}

impl RegistryCacheBackend {
    pub fn open(
        cache_dir: Option<&Path>,
        base_ref: String,
        platform: &str,
        insecure: bool,
    ) -> Result<Self> {
        let (platform_os, platform_arch) = platform
            .split_once('/')
            .ok_or_else(|| anyhow!("invalid platform '{}'", platform))?;
        let root = default_registry_cache_dir(cache_dir)?;
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create {}", root.display()))?;
        let backend = Self {
            base_ref,
            platform_os: platform_os.to_string(),
            platform_arch: platform_arch.to_string(),
            insecure,
            root,
        };
        fs::create_dir_all(backend.tmp_dir())
            .with_context(|| format!("failed to create {}", backend.tmp_dir().display()))?;
        fs::create_dir_all(backend.staged_blob_dir())
            .with_context(|| format!("failed to create {}", backend.staged_blob_dir().display()))?;
        Ok(backend)
    }

    fn reference(&self) -> Result<&str> {
        let base_ref = self.base_ref.trim();
        if base_ref.is_empty() {
            bail!("build-cache registry reference must not be empty");
        }
        if base_ref.contains('@') {
            bail!(
                "build-cache registry reference must not use a digest: {}",
                self.base_ref
            );
        }
        Ok(base_ref)
    }

    fn root_scope(&self) -> PathBuf {
        self.root.join(hash_string(&format!(
            "{}:{}:{}",
            self.base_ref, self.platform_os, self.platform_arch
        )))
    }

    fn tmp_dir(&self) -> PathBuf {
        self.root_scope().join("tmp")
    }

    fn staged_blob_dir(&self) -> PathBuf {
        self.root_scope()
            .join("staged")
            .join("blobs")
            .join("sha256")
    }

    fn resolved_layout_dir(&self) -> PathBuf {
        self.root_scope().join("resolved")
    }

    fn publish_layout_dir(&self) -> PathBuf {
        self.root_scope().join("publish")
    }

    fn resolved_blob_path(&self, digest: &str) -> PathBuf {
        layout_blob_path(&self.resolved_layout_dir(), digest)
    }

    fn staged_blob_path(&self, digest: &str) -> PathBuf {
        digest_blob_path(&self.staged_blob_dir(), digest)
    }

    fn available_blob_path(&self, digest: &str) -> Option<PathBuf> {
        let staged = self.staged_blob_path(digest);
        if staged.exists() {
            return Some(staged);
        }
        let resolved = self.resolved_blob_path(digest);
        if resolved.exists() {
            return Some(resolved);
        }
        None
    }

    fn install_resolved_layout(&self, source: &Path) -> Result<PathBuf> {
        let destination = self.resolved_layout_dir();
        fs::create_dir_all(&destination)
            .with_context(|| format!("failed to create {}", destination.display()))?;
        clear_directory(&destination)?;
        copy_tree(source, &destination).with_context(|| {
            format!(
                "failed to copy registry cache layout {} into {}",
                source.display(),
                destination.display()
            )
        })?;
        Ok(destination)
    }

    fn pull_layout(&self) -> Result<Option<PathBuf>> {
        pull_image_layout(
            self.reference()?,
            &self.platform_os,
            &self.platform_arch,
            &self.root,
            self.insecure,
        )
        .with_context(|| format!("failed to restore build-cache ref {}", self.base_ref))
    }

    fn push_layout(&self, source: &Path) -> Result<String> {
        push_image(source, self.reference()?, self.insecure)
            .with_context(|| format!("failed to push build-cache ref {}", self.base_ref))
    }
}

impl CacheBackend for RegistryCacheBackend {
    fn kind(&self) -> &'static str {
        "registry"
    }

    fn detail(&self) -> String {
        format!("ref={}", self.base_ref)
    }

    fn resolve_ref(&self, key: &str) -> Result<Option<CacheManifest>> {
        let Some(layout_dir) = self.pull_layout()? else {
            return Ok(None);
        };
        let resolved_dir = self.install_resolved_layout(&layout_dir)?;
        let manifest = read_cache_manifest_from_layout(&resolved_dir)?;
        if let Some(stored_key) = manifest.metadata.get(REGISTRY_REF_KEY_METADATA)
            && stored_key != key
        {
            return Ok(None);
        }
        Ok(Some(manifest))
    }

    fn publish_ref(&self, key: &str, manifest: &CacheManifest) -> Result<()> {
        let manifest = manifest_with_ref_key(manifest, key);
        let layout_dir = self.publish_layout_dir();
        write_cache_layout(
            &layout_dir,
            &self.platform_os,
            &self.platform_arch,
            &manifest,
            |digest| {
                self.available_blob_path(digest).ok_or_else(|| {
                    anyhow!("missing registry cache blob {digest} for {}", self.base_ref)
                })
            },
        )?;
        self.push_layout(&layout_dir)?;
        self.install_resolved_layout(&layout_dir)?;
        Ok(())
    }

    fn has_blob(&self, digest: &str) -> Result<bool> {
        Ok(self.available_blob_path(digest).is_some())
    }

    fn fetch_blob(&self, digest: &str, destination: &Path) -> Result<()> {
        let source = self.available_blob_path(digest).ok_or_else(|| {
            anyhow!(
                "registry cache blob {digest} not found for {}",
                self.base_ref
            )
        })?;
        fs::copy(&source, destination).with_context(|| {
            format!(
                "failed to copy registry cache blob {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        Ok(())
    }

    fn store_blob(&self, digest: &str, source: &Path) -> Result<()> {
        let destination = self.staged_blob_path(digest);
        if destination.exists() {
            return Ok(());
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::copy(source, &destination).with_context(|| {
            format!(
                "failed to stage registry cache blob {} into {}",
                source.display(),
                destination.display()
            )
        })?;
        Ok(())
    }
}

impl CacheStore for RegistryCacheStore {
    fn kind(&self) -> &'static str {
        self.backend.kind()
    }

    fn detail(&self) -> String {
        self.backend.detail()
    }

    fn restore(&self, _entry: &CacheMount, destination: &Path) -> Result<CacheRestore> {
        let Some(layout_dir) = self.backend.pull_layout()? else {
            return Ok(CacheRestore {
                hit: false,
                digest: None,
                bytes: 0,
                content_hash: None,
            });
        };

        fs::create_dir_all(destination)
            .with_context(|| format!("failed to create {}", destination.display()))?;
        clear_directory(destination)?;
        copy_tree(&layout_dir, destination).with_context(|| {
            format!(
                "failed to copy restored build-cache layout {} into {}",
                layout_dir.display(),
                destination.display()
            )
        })?;

        Ok(CacheRestore {
            hit: true,
            digest: Some(self.reference()?.to_string()),
            bytes: path_size(destination).unwrap_or(0),
            content_hash: None,
        })
    }

    fn save(&self, _entry: &CacheMount, source: &Path) -> Result<CacheSave> {
        let digest = self.backend.push_layout(source)?;
        Ok(CacheSave {
            digest,
            bytes: path_size(source).unwrap_or(0),
            content_hash: String::new(),
        })
    }

    fn backend(&self) -> &dyn crate::cache::backend::CacheBackend {
        &self.backend
    }

    fn lock(&self, entry: &CacheMount) -> Result<CacheLock> {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| anyhow!("HOME is not set; cannot lock cache"))?;
        let scope = normalize_cache_tag(&self.backend.base_ref);
        let lock_path = PathBuf::from(home)
            .join(".boringbuilder")
            .join("locks")
            .join("registry")
            .join(scope)
            .join(format!("{}.lock", normalize_cache_tag(&entry.id)));
        acquire_cache_lock(&lock_path)
    }
}

fn manifest_with_ref_key(manifest: &CacheManifest, key: &str) -> CacheManifest {
    let mut manifest = manifest.clone();
    manifest
        .metadata
        .insert(REGISTRY_REF_KEY_METADATA.to_string(), key.to_string());
    manifest
}

fn write_cache_layout<F>(
    layout_dir: &Path,
    platform_os: &str,
    platform_arch: &str,
    manifest: &CacheManifest,
    mut resolve_blob_path: F,
) -> Result<()>
where
    F: FnMut(&str) -> Result<PathBuf>,
{
    fs::create_dir_all(layout_dir)
        .with_context(|| format!("failed to create {}", layout_dir.display()))?;
    clear_directory(layout_dir)?;

    let blobs_dir = layout_dir.join("blobs").join("sha256");
    fs::create_dir_all(&blobs_dir)
        .with_context(|| format!("failed to create {}", blobs_dir.display()))?;

    let config_bytes = serde_json::to_vec(&RegistryCacheConfig {
        architecture: platform_arch.to_string(),
        os: platform_os.to_string(),
        cache_manifest: manifest.clone(),
    })
    .context("failed to serialize registry cache config")?;
    let config_digest = hash_bytes(&config_bytes);
    write_layout_blob(&blobs_dir, &config_digest, &config_bytes)?;

    let layers = manifest
        .blobs
        .iter()
        .map(|blob| {
            let source = resolve_blob_path(&blob.digest)?;
            let actual_bytes = fs::metadata(&source)
                .with_context(|| format!("failed to stat {}", source.display()))?
                .len();
            if actual_bytes != blob.bytes {
                bail!(
                    "registry cache blob {} expected {} bytes but found {} at {}",
                    blob.digest,
                    blob.bytes,
                    actual_bytes,
                    source.display()
                );
            }
            let destination = digest_blob_path(&blobs_dir, &blob.digest);
            if !destination.exists() {
                fs::copy(&source, &destination).with_context(|| {
                    format!(
                        "failed to copy cache blob {} into OCI layout {}",
                        source.display(),
                        destination.display()
                    )
                })?;
            }
            Ok(serde_json::json!({
                "mediaType": REGISTRY_CACHE_BLOB_MEDIA_TYPE,
                "digest": blob.digest,
                "size": blob.bytes
            }))
        })
        .collect::<Result<Vec<_>>>()?;

    let raw_manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": oci_client::manifest::OCI_IMAGE_MEDIA_TYPE,
        "config": {
            "mediaType": REGISTRY_CACHE_CONFIG_MEDIA_TYPE,
            "digest": config_digest,
            "size": config_bytes.len() as u64
        },
        "layers": layers
    }))
    .context("failed to serialize registry cache image manifest")?;
    let manifest_digest = hash_bytes(&raw_manifest);
    write_layout_blob(&blobs_dir, &manifest_digest, &raw_manifest)?;

    fs::write(
        layout_dir.join("index.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_IMAGE_INDEX_MEDIA_TYPE,
            "manifests": [{
                "mediaType": oci_client::manifest::OCI_IMAGE_MEDIA_TYPE,
                "digest": manifest_digest,
                "size": raw_manifest.len() as u64,
                "platform": {
                    "os": platform_os,
                    "architecture": platform_arch
                }
            }]
        }))
        .context("failed to serialize registry cache index")?,
    )
    .with_context(|| {
        format!(
            "failed to write {}",
            layout_dir.join("index.json").display()
        )
    })?;
    fs::write(
        layout_dir.join("oci-layout"),
        r#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .with_context(|| {
        format!(
            "failed to write {}",
            layout_dir.join("oci-layout").display()
        )
    })?;

    Ok(())
}

pub(crate) fn read_cache_manifest_from_layout(layout_dir: &Path) -> Result<CacheManifest> {
    let index_path = layout_dir.join("index.json");
    let index: OciImageIndex = serde_json::from_slice(
        &fs::read(&index_path)
            .with_context(|| format!("failed to read {}", index_path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", index_path.display()))?;
    let descriptor = index.manifests.first().ok_or_else(|| {
        anyhow!(
            "registry cache layout {} is missing manifests",
            index_path.display()
        )
    })?;
    let manifest_path = layout_blob_path(layout_dir, &descriptor.digest);
    let image_manifest: OciImageManifest = serde_json::from_slice(
        &fs::read(&manifest_path)
            .with_context(|| format!("failed to read {}", manifest_path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    let config_path = layout_blob_path(layout_dir, &image_manifest.config.digest);
    let config: RegistryCacheConfig = serde_json::from_slice(
        &fs::read(&config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", config_path.display()))?;
    Ok(config.cache_manifest)
}

fn layout_blob_path(layout_dir: &Path, digest: &str) -> PathBuf {
    layout_dir
        .join("blobs")
        .join("sha256")
        .join(digest.strip_prefix("sha256:").unwrap_or(digest))
}

fn digest_blob_path(blobs_dir: &Path, digest: &str) -> PathBuf {
    blobs_dir.join(digest.strip_prefix("sha256:").unwrap_or(digest))
}

fn write_layout_blob(blobs_dir: &Path, digest: &str, bytes: &[u8]) -> Result<()> {
    let path = digest_blob_path(blobs_dir, digest);
    fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn hash_string(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn parse_registry_spec(raw: &str) -> Result<ParsedRegistrySpec> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("build-cache spec must not be empty");
    }
    if !raw.contains('=') {
        return Ok(ParsedRegistrySpec::Registry(ParsedRegistryOptions {
            reference: raw.to_string(),
            mode: BuildCacheExportMode::Min,
            ignore_error: false,
            oci_media_types: true,
            image_manifest: true,
            compression: BuildCacheCompression::Gzip,
        }));
    }

    let mut kind = None;
    let mut reference = None;
    let mut mode = BuildCacheExportMode::Min;
    let mut ignore_error = false;
    let mut oci_media_types = true;
    let mut image_manifest = true;
    let mut compression = BuildCacheCompression::Gzip;
    let mut explicit_export_options = false;

    for field in raw.split(',') {
        let (key, value) = field
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid build-cache spec field '{field}' in '{raw}'"))?;
        let key = key.trim();
        let value = value.trim();
        match key {
            "type" => kind = Some(value.to_string()),
            "ref" => reference = Some(value.to_string()),
            "mode" => {
                explicit_export_options = true;
                mode = match value {
                    "min" => BuildCacheExportMode::Min,
                    "max" => BuildCacheExportMode::Max,
                    _ => bail!(
                        "unsupported build-cache export mode '{value}' in '{raw}'; expected min or max"
                    ),
                }
            }
            "ignore-error" => {
                explicit_export_options = true;
                ignore_error = parse_bool_field(value, key, raw)?;
            }
            "oci-mediatypes" => {
                explicit_export_options = true;
                oci_media_types = parse_bool_field(value, key, raw)?;
            }
            "image-manifest" => {
                explicit_export_options = true;
                image_manifest = parse_bool_field(value, key, raw)?;
            }
            "compression" => {
                explicit_export_options = true;
                compression = BuildCacheCompression::parse(value, raw)?;
            }
            "src" | "dest" => {
                bail!(
                    "build-cache spec '{raw}' uses type=local semantics, which are not supported yet; use an OCI registry ref instead"
                )
            }
            _ => bail!("unsupported build-cache spec key '{key}' in '{raw}'"),
        }
    }

    match kind.as_deref() {
        Some("registry") => {
            let options = ParsedRegistryOptions {
                reference: reference.ok_or_else(|| {
                    anyhow!("registry build-cache spec '{raw}' must include ref=<image>")
                })?,
                mode,
                ignore_error,
                oci_media_types,
                image_manifest,
                compression,
            };
            if explicit_export_options {
                Ok(ParsedRegistrySpec::RegistryExport(options))
            } else {
                Ok(ParsedRegistrySpec::Registry(options))
            }
        }
        Some(other) => bail!(
            "unsupported build-cache backend type '{other}' in '{raw}'; supported today: registry"
        ),
        None => bail!(
            "invalid build-cache spec '{raw}'; use a bare OCI ref or an explicit type=registry,ref=... spec"
        ),
    }
}

fn default_registry_cache_dir(override_dir: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_dir {
        return Ok(path.join("build-cache-registry"));
    }

    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow!("HOME is not set; cannot determine registry cache location"))?;
    Ok(PathBuf::from(home)
        .join(".boringbuilder")
        .join("build-cache-registry"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use super::{
        BuildCacheExportMode, BuildCacheExportSpec, BuildCacheImportSpec, RegistryCacheBackend,
        RegistryCacheStore, hash_bytes, parse_build_cache_export_specs,
        parse_build_cache_import_specs, read_cache_manifest_from_layout, write_cache_layout,
    };
    use crate::cache::backend::{
        CacheBackend, CacheManifest, CacheManifestBlob, TREE_CACHE_ARTIFACT_KIND,
    };
    use tempfile::tempdir;

    fn tree_manifest_with_blob(bytes: &[u8]) -> (CacheManifest, String) {
        let digest = hash_bytes(bytes);
        (
            CacheManifest {
                version: 1,
                kind: TREE_CACHE_ARTIFACT_KIND.to_string(),
                blobs: vec![CacheManifestBlob {
                    digest: digest.clone(),
                    bytes: bytes.len() as u64,
                }],
                metadata: BTreeMap::from([
                    ("content_hash".to_string(), "content-hash".to_string()),
                    ("ref_key".to_string(), "cache-key".to_string()),
                ]),
            },
            digest,
        )
    }

    #[test]
    fn keeps_tagged_refs_stable() {
        let store = RegistryCacheStore::open(
            None,
            "ghcr.io/acme/cache:main".to_string(),
            "linux/amd64",
            false,
        )
        .unwrap();
        assert_eq!(store.reference().unwrap(), "ghcr.io/acme/cache:main");
    }

    #[test]
    fn keeps_untagged_refs_stable() {
        let store =
            RegistryCacheStore::open(None, "ghcr.io/acme/cache".to_string(), "linux/amd64", false)
                .unwrap();
        assert_eq!(store.reference().unwrap(), "ghcr.io/acme/cache");
    }

    #[test]
    fn parses_bare_registry_import_specs() {
        let specs =
            parse_build_cache_import_specs(&["ghcr.io/acme/cache:main".to_string()]).unwrap();
        assert_eq!(
            specs,
            vec![BuildCacheImportSpec::Registry {
                reference: "ghcr.io/acme/cache:main".to_string()
            }]
        );
    }

    #[test]
    fn parses_explicit_registry_export_specs() {
        let specs = parse_build_cache_export_specs(&[
            "type=registry,ref=ghcr.io/acme/cache:main".to_string()
        ])
        .unwrap();
        assert_eq!(
            specs,
            vec![BuildCacheExportSpec::Registry {
                reference: "ghcr.io/acme/cache:main".to_string(),
                mode: BuildCacheExportMode::Min,
                ignore_error: false,
            }]
        );
    }

    #[test]
    fn rejects_unsupported_local_specs() {
        let err =
            parse_build_cache_import_specs(&["type=local,src=/tmp/cache".to_string()]).unwrap_err();
        assert!(err.to_string().contains("type=local semantics"), "{}", err);
    }

    #[test]
    fn parses_registry_mode_min_and_ignore_error() {
        let specs = parse_build_cache_export_specs(&[
            "type=registry,ref=ghcr.io/acme/cache:main,mode=min,ignore-error=true".to_string(),
        ])
        .unwrap();
        assert_eq!(
            specs,
            vec![BuildCacheExportSpec::Registry {
                reference: "ghcr.io/acme/cache:main".to_string(),
                mode: BuildCacheExportMode::Min,
                ignore_error: true,
            }]
        );
    }

    #[test]
    fn rejects_non_oci_media_type_exports() {
        let err = parse_build_cache_export_specs(&[
            "type=registry,ref=ghcr.io/acme/cache:main,oci-mediatypes=false".to_string(),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("oci-mediatypes=false"), "{err}");
    }

    #[test]
    fn accepts_image_manifest_false_for_registry_exports() {
        let specs = parse_build_cache_export_specs(&[
            "type=registry,ref=ghcr.io/acme/cache:main,image-manifest=false".to_string(),
        ])
        .unwrap();
        assert_eq!(specs.len(), 1);
    }

    #[test]
    fn registry_cache_layout_round_trips_manifest_and_blobs() {
        let temp = tempdir().unwrap();
        let layout_dir = temp.path().join("layout");
        let source = temp.path().join("blob.tar.zst");
        let bytes = b"registry-cache-blob";
        fs::write(&source, bytes).unwrap();
        let (manifest, digest) = tree_manifest_with_blob(bytes);

        write_cache_layout(&layout_dir, "linux", "amd64", &manifest, |requested| {
            assert_eq!(requested, digest);
            Ok(source.clone())
        })
        .unwrap();

        let loaded = read_cache_manifest_from_layout(&layout_dir).unwrap();
        assert_eq!(loaded, manifest);
        assert_eq!(
            fs::read(
                layout_dir
                    .join("blobs")
                    .join("sha256")
                    .join(digest.strip_prefix("sha256:").unwrap())
            )
            .unwrap(),
            bytes
        );
    }

    #[test]
    fn registry_cache_backend_stages_and_fetches_blobs_locally() {
        let temp = tempdir().unwrap();
        let backend = RegistryCacheBackend::open(
            Some(temp.path()),
            "ghcr.io/acme/cache:main".to_string(),
            "linux/amd64",
            false,
        )
        .unwrap();
        let source = temp.path().join("source.tar.zst");
        let destination = temp.path().join("destination.tar.zst");
        let bytes = b"backend-blob";
        fs::write(&source, bytes).unwrap();
        let digest = hash_bytes(bytes);

        backend.store_blob(&digest, &source).unwrap();
        assert!(backend.has_blob(&digest).unwrap());
        backend.fetch_blob(&digest, &destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), bytes);
    }
}
