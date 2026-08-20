mod auth;
pub mod push;

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::manifest::OciImageManifest;
use oci_client::{Client, Reference};
use tokio::runtime::Builder as RuntimeBuilder;

use self::auth::resolve_auth;

#[derive(Debug, Clone)]
pub struct ResolvedImageMetadata {
    pub manifest_digest: String,
    pub config: serde_json::Value,
}

pub fn resolve_image_metadata(
    image: &str,
    platform_os: &str,
    platform_arch: &str,
) -> Result<ResolvedImageMetadata> {
    let rt = RuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create tokio runtime for registry client")?;

    rt.block_on(resolve_image_metadata_async(
        image,
        platform_os,
        platform_arch,
        false,
    ))
}

pub fn pull_image(
    image: &str,
    platform_os: &str,
    platform_arch: &str,
    cache_dir: &Path,
) -> Result<PathBuf> {
    let rt = RuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create tokio runtime for registry client")?;

    rt.block_on(pull_image_async(
        image,
        platform_os,
        platform_arch,
        cache_dir,
        false,
    ))
}

pub fn pull_image_layout(
    image: &str,
    platform_os: &str,
    platform_arch: &str,
    cache_dir: &Path,
    insecure: bool,
) -> Result<Option<PathBuf>> {
    let rt = RuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create tokio runtime for registry client")?;

    rt.block_on(pull_image_layout_async(
        image,
        platform_os,
        platform_arch,
        cache_dir,
        insecure,
    ))
}

async fn resolve_image_metadata_async(
    image: &str,
    platform_os: &str,
    platform_arch: &str,
    insecure: bool,
) -> Result<ResolvedImageMetadata> {
    let config = ClientConfig {
        protocol: if insecure {
            ClientProtocol::Http
        } else {
            ClientProtocol::Https
        },
        ..Default::default()
    };
    let client = Client::new(config);

    let reference: Reference = image
        .parse()
        .with_context(|| format!("invalid image reference '{image}'"))?;

    let auth = resolve_auth(&reference);
    client
        .store_auth_if_needed(reference.resolve_registry(), &auth)
        .await;

    let manifest =
        resolve_platform_manifest(&client, &reference, &auth, platform_os, platform_arch).await?;

    let mut config_buf = Vec::new();
    client
        .pull_blob(
            &reference,
            manifest.image.config.digest.as_str(),
            &mut config_buf,
        )
        .await
        .context("failed to pull config blob")?;
    let config = serde_json::from_slice(&config_buf).context("failed to parse config blob JSON")?;

    Ok(ResolvedImageMetadata {
        manifest_digest: manifest.manifest_digest,
        config,
    })
}

async fn pull_image_async(
    image: &str,
    platform_os: &str,
    platform_arch: &str,
    cache_dir: &Path,
    insecure: bool,
) -> Result<PathBuf> {
    let config = ClientConfig {
        protocol: if insecure {
            ClientProtocol::Http
        } else {
            ClientProtocol::Https
        },
        ..Default::default()
    };
    let client = Client::new(config);

    let reference: Reference = image
        .parse()
        .with_context(|| format!("invalid image reference '{image}'"))?;

    let auth = resolve_auth(&reference);
    client
        .store_auth_if_needed(reference.resolve_registry(), &auth)
        .await;

    let manifest =
        resolve_platform_manifest(&client, &reference, &auth, platform_os, platform_arch).await?;

    let staging = cache_dir.join("tmp");
    fs::create_dir_all(&staging)?;
    let temp_dir = tempfile::Builder::new()
        .prefix("pull-")
        .tempdir_in(&staging)?;
    let dest = temp_dir.path();

    let mut config_buf = Vec::new();
    client
        .pull_blob(
            &reference,
            manifest.image.config.digest.as_str(),
            &mut config_buf,
        )
        .await
        .context("failed to pull config blob")?;
    write_blob(dest, &manifest.image.config.digest, &config_buf)?;

    for layer in &manifest.image.layers {
        let existing = blob_path(dest, &layer.digest);
        if existing.exists() {
            let meta = fs::metadata(&existing)?;
            if meta.len() == layer.size as u64 {
                continue;
            }
        }

        let mut layer_buf = Vec::with_capacity(layer.size as usize);
        client
            .pull_blob(&reference, layer.digest.as_str(), &mut layer_buf)
            .await
            .with_context(|| format!("failed to pull layer {}", layer.digest))?;
        write_blob(dest, &layer.digest, &layer_buf)?;
    }

    let dir_manifest = build_dir_manifest(&manifest.image, &manifest.manifest_digest);
    let manifest_json = serde_json::to_vec_pretty(&dir_manifest)?;
    fs::write(dest.join("manifest.json"), &manifest_json)?;

    let kept = temp_dir.keep();
    Ok(kept)
}

async fn pull_image_layout_async(
    image: &str,
    platform_os: &str,
    platform_arch: &str,
    cache_dir: &Path,
    insecure: bool,
) -> Result<Option<PathBuf>> {
    let config = ClientConfig {
        protocol: if insecure {
            ClientProtocol::Http
        } else {
            ClientProtocol::Https
        },
        ..Default::default()
    };
    let client = Client::new(config);

    let reference: Reference = image
        .parse()
        .with_context(|| format!("invalid image reference '{image}'"))?;

    let auth = resolve_auth(&reference);
    client
        .store_auth_if_needed(reference.resolve_registry(), &auth)
        .await;

    let manifest =
        match resolve_platform_manifest(&client, &reference, &auth, platform_os, platform_arch)
            .await
        {
            Ok(manifest) => manifest,
            Err(error) if registry_reference_missing(&error) => return Ok(None),
            Err(error) => return Err(error),
        };

    let staging = cache_dir.join("tmp");
    fs::create_dir_all(&staging)?;
    let temp_dir = tempfile::Builder::new()
        .prefix("pull-layout-")
        .tempdir_in(&staging)?;
    let dest = temp_dir.path();
    let blobs_dir = dest.join("blobs").join("sha256");
    fs::create_dir_all(&blobs_dir)?;

    let mut config_buf = Vec::new();
    client
        .pull_blob(
            &reference,
            manifest.image.config.digest.as_str(),
            &mut config_buf,
        )
        .await
        .context("failed to pull config blob")?;
    write_blob(&blobs_dir, &manifest.image.config.digest, &config_buf)?;

    for layer in &manifest.image.layers {
        let mut layer_buf = Vec::with_capacity(layer.size as usize);
        client
            .pull_blob(&reference, layer.digest.as_str(), &mut layer_buf)
            .await
            .with_context(|| format!("failed to pull layer {}", layer.digest))?;
        write_blob(&blobs_dir, &layer.digest, &layer_buf)?;
    }

    write_blob(
        &blobs_dir,
        &manifest.manifest_digest,
        &manifest.raw_manifest,
    )?;

    let index = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": manifest.manifest_digest,
            "size": manifest.raw_manifest.len() as u64,
            "platform": {
                "os": platform_os,
                "architecture": platform_arch,
            }
        }]
    });
    fs::write(dest.join("index.json"), serde_json::to_vec_pretty(&index)?)
        .with_context(|| format!("failed to write {}", dest.join("index.json").display()))?;
    fs::write(dest.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#)
        .with_context(|| format!("failed to write {}", dest.join("oci-layout").display()))?;

    let kept = temp_dir.keep();
    Ok(Some(kept))
}

async fn resolve_platform_manifest(
    client: &Client,
    reference: &Reference,
    auth: &oci_client::secrets::RegistryAuth,
    platform_os: &str,
    platform_arch: &str,
) -> Result<ResolvedPlatformManifest> {
    let accept = &[
        oci_client::manifest::IMAGE_MANIFEST_MEDIA_TYPE,
        oci_client::manifest::IMAGE_MANIFEST_LIST_MEDIA_TYPE,
        oci_client::manifest::OCI_IMAGE_MEDIA_TYPE,
        oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE,
    ];

    let (raw_manifest, manifest_digest) = client
        .pull_manifest_raw(reference, auth, accept)
        .await
        .context("failed to pull manifest")?;

    let parsed: serde_json::Value =
        serde_json::from_slice(&raw_manifest).context("failed to parse manifest JSON")?;

    if is_manifest_list(&parsed) {
        let platform_digest = find_platform_digest(&parsed, platform_os, platform_arch)?;
        let platform_ref = build_digest_reference(reference, &platform_digest)?;
        let (platform_raw_manifest, platform_manifest_digest) = client
            .pull_manifest_raw(
                &platform_ref,
                auth,
                &[
                    oci_client::manifest::IMAGE_MANIFEST_MEDIA_TYPE,
                    oci_client::manifest::OCI_IMAGE_MEDIA_TYPE,
                ],
            )
            .await
            .context("failed to pull platform-specific manifest")?;
        let image_manifest: OciImageManifest = serde_json::from_slice(&platform_raw_manifest)
            .context("failed to parse platform-specific image manifest")?;
        Ok(ResolvedPlatformManifest {
            image: image_manifest,
            manifest_digest: platform_manifest_digest,
            raw_manifest: platform_raw_manifest.to_vec(),
        })
    } else {
        let image_manifest: OciImageManifest =
            serde_json::from_slice(&raw_manifest).context("failed to parse image manifest")?;
        Ok(ResolvedPlatformManifest {
            image: image_manifest,
            manifest_digest,
            raw_manifest: raw_manifest.to_vec(),
        })
    }
}

struct ResolvedPlatformManifest {
    image: OciImageManifest,
    manifest_digest: String,
    raw_manifest: Vec<u8>,
}

fn is_manifest_list(parsed: &serde_json::Value) -> bool {
    let media_type = parsed["mediaType"].as_str().unwrap_or("");
    media_type == oci_client::manifest::IMAGE_MANIFEST_LIST_MEDIA_TYPE
        || media_type == oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE
        || (parsed.get("manifests").is_some() && parsed.get("config").is_none())
}

fn find_platform_digest(index: &serde_json::Value, os: &str, arch: &str) -> Result<String> {
    let manifests = index["manifests"]
        .as_array()
        .ok_or_else(|| anyhow!("manifest list missing 'manifests' array"))?;

    for entry in manifests {
        let platform = &entry["platform"];
        let entry_os = platform["os"].as_str().unwrap_or("");
        let entry_arch = platform["architecture"].as_str().unwrap_or("");

        if entry_os == os && entry_arch == arch {
            return entry["digest"]
                .as_str()
                .map(ToString::to_string)
                .ok_or_else(|| anyhow!("manifest entry missing digest"));
        }
    }

    Err(anyhow!("no manifest found for platform {os}/{arch}"))
}

fn build_digest_reference(base: &Reference, digest: &str) -> Result<Reference> {
    let ref_str = format!("{}/{}@{}", base.registry(), base.repository(), digest);
    ref_str
        .parse()
        .with_context(|| format!("failed to build digest reference: {ref_str}"))
}

fn blob_path(dir: &Path, digest: &str) -> PathBuf {
    let hash = digest.strip_prefix("sha256:").unwrap_or(digest);
    dir.join(hash)
}

fn write_blob(dir: &Path, digest: &str, data: &[u8]) -> Result<()> {
    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create blob directory {}", dir.display()))?;
    let path = blob_path(dir, digest);
    let temp_path = dir.join(format!(
        ".tmp-{}",
        &digest[digest.len().saturating_sub(12)..]
    ));
    let mut file = File::create(&temp_path)
        .with_context(|| format!("failed to create blob file {}", temp_path.display()))?;
    file.write_all(data)?;
    file.flush()?;
    fs::rename(&temp_path, &path)?;
    Ok(())
}

pub(crate) fn registry_reference_missing(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let message = cause.to_string().to_ascii_lowercase();
        message.contains("manifest unknown")
            || message.contains("name unknown")
            || message.contains("404")
            || message.contains("not found")
    })
}

fn build_dir_manifest(manifest: &OciImageManifest, manifest_digest: &str) -> serde_json::Value {
    let layers: Vec<serde_json::Value> = manifest
        .layers
        .iter()
        .map(|layer| {
            serde_json::json!({
                "mediaType": layer.media_type,
                "digest": layer.digest,
                "size": layer.size
            })
        })
        .collect();

    serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "config": {
            "mediaType": manifest.config.media_type,
            "digest": manifest.config.digest,
            "size": manifest.config.size
        },
        "layers": layers,
        "x-boringbuilder": {
            "manifestDigest": manifest_digest
        }
    })
}

#[cfg(test)]
mod tests {
    use oci_client::manifest::{OciDescriptor, OciImageManifest};
    use tempfile::tempdir;

    use super::{build_dir_manifest, registry_reference_missing, write_blob};

    #[test]
    fn write_blob_creates_missing_directory() {
        let temp = tempdir().unwrap();
        let dir = temp.path().join("missing");
        write_blob(&dir, "sha256:abc123", b"hello").unwrap();
        assert!(dir.join("abc123").exists());
    }

    #[test]
    fn detects_missing_registry_reference_through_error_context_chain() {
        let error = anyhow::anyhow!(
            "OCI API errors: [OCI API error: cache:build-cache-deadbeef 404 Not Found]"
        )
        .context("Registry error")
        .context("failed to pull manifest");

        assert!(registry_reference_missing(&error));
    }

    #[test]
    fn dir_manifest_records_resolved_manifest_digest() {
        let manifest = OciImageManifest {
            schema_version: 2,
            media_type: None,
            config: OciDescriptor {
                media_type: "application/vnd.oci.image.config.v1+json".to_string(),
                digest: "sha256:config".to_string(),
                size: 10,
                annotations: None,
                urls: None,
                artifact_type: None,
            },
            layers: Vec::new(),
            subject: None,
            artifact_type: None,
            annotations: None,
        };

        let dir_manifest = build_dir_manifest(&manifest, "sha256:manifest");

        assert_eq!(
            dir_manifest.pointer("/x-boringbuilder/manifestDigest"),
            Some(&serde_json::Value::String("sha256:manifest".to_string()))
        );
    }
}
