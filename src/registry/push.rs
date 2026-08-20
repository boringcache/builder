use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::manifest::{
    IMAGE_MANIFEST_MEDIA_TYPE, ImageIndexEntry, OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MEDIA_TYPE,
    OciImageIndex, OciImageManifest, OciManifest, Platform,
};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use tokio::runtime::Builder as RuntimeBuilder;

use crate::boringcache_cli::{BoringCacheCli, BoringCacheTagScope};
use crate::util::process::run_streaming;

use super::{auth::resolve_auth, registry_reference_missing};

#[derive(Debug, Clone)]
pub struct ProxyPushOptions {
    pub workspace: Option<String>,
    pub binary: Option<PathBuf>,
    pub tag: String,
    pub metadata_hints: Vec<String>,
    pub scope: BoringCacheTagScope,
}

/// Push an OCI image layout directory to a remote registry.
pub fn push_image(oci_dir: &Path, target: &str, insecure: bool) -> Result<String> {
    let rt = RuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create tokio runtime for push")?;

    rt.block_on(push_image_async(oci_dir, target, insecure))
}

pub fn push_image_via_boringcache_proxy(oci_dir: &Path, options: &ProxyPushOptions) -> Result<()> {
    let boringbuilder_bin =
        std::env::current_exe().context("failed to resolve current boringbuilder binary")?;
    push_image_via_boringcache_proxy_with_binary(oci_dir, options, &boringbuilder_bin)
}

fn push_image_via_boringcache_proxy_with_binary(
    oci_dir: &Path,
    options: &ProxyPushOptions,
    boringbuilder_bin: &Path,
) -> Result<()> {
    let cli = BoringCacheCli::resolve(options.workspace.clone(), options.binary.clone())?;
    let oci_dir = oci_dir
        .canonicalize()
        .with_context(|| format!("failed to resolve OCI layout {}", oci_dir.display()))?;
    if options.tag.trim().is_empty() {
        bail!("--proxy-tag must not be empty");
    }

    let mut args = vec![
        "run".to_string(),
        cli.workspace().to_string(),
        "--proxy".to_string(),
        options.tag.clone(),
        "--skip-restore".to_string(),
        "--skip-save".to_string(),
        "--fail-on-cache-error".to_string(),
    ];
    options.scope.append_cli_args(&mut args);
    for hint in &options.metadata_hints {
        args.push("--metadata-hint".to_string());
        args.push(hint.clone());
    }
    args.push("--".to_string());
    args.push(boringbuilder_bin.display().to_string());
    args.push("push".to_string());
    args.push(oci_dir.display().to_string());
    args.push("{CACHE_REF}".to_string());
    args.push("--insecure".to_string());

    run_streaming(cli.binary(), &args)
}

async fn push_image_async(oci_dir: &Path, target: &str, insecure: bool) -> Result<String> {
    let reference: Reference = target
        .parse()
        .with_context(|| format!("invalid target image reference '{target}'"))?;

    let protocol = if insecure {
        ClientProtocol::Http
    } else {
        ClientProtocol::Https
    };
    let config = ClientConfig {
        protocol,
        ..Default::default()
    };
    let client = Client::new(config);

    let auth = resolve_auth(&reference);
    client
        .store_auth_if_needed(reference.resolve_registry(), &auth)
        .await;

    let local_entries = load_local_layout_entries(oci_dir)?;
    let pushed_entries = push_local_entries(&client, &reference, oci_dir, &local_entries).await?;
    push_merged_index(&client, &reference, &auth, &pushed_entries).await
}

#[derive(Debug, Clone)]
struct LocalLayoutEntry {
    descriptor: ImageIndexEntry,
    manifest: OciImageManifest,
    raw_manifest: Vec<u8>,
}

fn load_local_layout_entries(oci_dir: &Path) -> Result<Vec<LocalLayoutEntry>> {
    let index_path = oci_dir.join("index.json");
    if !index_path.exists() {
        bail!(
            "not a valid OCI image layout: {} missing",
            index_path.display()
        );
    }
    let index: OciImageIndex =
        serde_json::from_reader(File::open(&index_path).context("failed to open index.json")?)
            .context("failed to parse index.json")?;
    if index.manifests.is_empty() {
        bail!("index.json missing manifests");
    }

    let mut entries = Vec::with_capacity(index.manifests.len());
    for mut descriptor in index.manifests {
        if descriptor.platform.is_none() {
            bail!(
                "local OCI layout {} has a manifest without platform metadata",
                index_path.display()
            );
        }

        let manifest_path = blob_path(oci_dir, &descriptor.digest)?;
        let raw_manifest = fs::read(&manifest_path)
            .with_context(|| format!("failed to read manifest {}", manifest_path.display()))?;
        let manifest: OciImageManifest = serde_json::from_slice(&raw_manifest)
            .with_context(|| format!("failed to parse manifest {}", manifest_path.display()))?;
        if descriptor.media_type.trim().is_empty() {
            descriptor.media_type = manifest
                .media_type
                .clone()
                .unwrap_or_else(|| OCI_IMAGE_MEDIA_TYPE.to_string());
        }
        descriptor.size = raw_manifest.len() as i64;
        entries.push(LocalLayoutEntry {
            descriptor,
            manifest,
            raw_manifest,
        });
    }

    Ok(entries)
}

async fn push_local_entries(
    client: &Client,
    reference: &Reference,
    oci_dir: &Path,
    local_entries: &[LocalLayoutEntry],
) -> Result<Vec<ImageIndexEntry>> {
    let mut pushed_entries = Vec::with_capacity(local_entries.len());
    for entry in local_entries {
        push_config_blob(client, reference, oci_dir, &entry.manifest).await?;
        push_layer_blobs(client, reference, oci_dir, &entry.manifest).await?;
        println!("==> pushing manifest");
        let media_type = entry.descriptor.media_type.parse().with_context(|| {
            format!(
                "invalid manifest media type {}",
                entry.descriptor.media_type
            )
        })?;
        let manifest_reference = client
            .push_manifest_raw(reference, entry.raw_manifest.clone(), media_type)
            .await
            .context("failed to push manifest")?;
        let manifest_digest = normalize_manifest_reference(&manifest_reference)?;
        let digest_reference =
            register_manifest_by_digest_reference(reference, manifest_digest.clone());
        let digest_media_type = entry.descriptor.media_type.parse().with_context(|| {
            format!(
                "invalid manifest media type {}",
                entry.descriptor.media_type
            )
        })?;
        client
            .push_manifest_raw(
                &digest_reference,
                entry.raw_manifest.clone(),
                digest_media_type,
            )
            .await
            .with_context(|| {
                format!("failed to register manifest {} by digest", manifest_digest)
            })?;

        let mut descriptor = entry.descriptor.clone();
        descriptor.digest = manifest_digest;
        descriptor.size = entry.raw_manifest.len() as i64;
        pushed_entries.push(descriptor);
    }
    Ok(pushed_entries)
}

fn register_manifest_by_digest_reference(reference: &Reference, digest: String) -> Reference {
    reference.clone_with_digest(digest)
}

fn normalize_manifest_reference(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if looks_like_manifest_digest(trimmed) {
        return Ok(trimmed.to_string());
    }

    if let Some((_, digest)) = trimmed.rsplit_once("/manifests/") {
        let digest = digest.split(['?', '#']).next().unwrap_or(digest);
        if looks_like_manifest_digest(digest) {
            return Ok(digest.to_string());
        }
    }

    if let Some((_, digest)) = trimmed.rsplit_once('@')
        && looks_like_manifest_digest(digest)
    {
        return Ok(digest.to_string());
    }

    bail!("expected manifest digest or manifest location, got '{trimmed}'")
}

fn looks_like_manifest_digest(value: &str) -> bool {
    let Some((algorithm, encoded)) = value.split_once(':') else {
        return false;
    };
    !algorithm.is_empty()
        && !encoded.is_empty()
        && !value.contains('/')
        && !value.contains('?')
        && !value.contains('#')
}

fn normalize_index_entry_digest(entry: &ImageIndexEntry) -> Result<ImageIndexEntry> {
    let mut normalized = entry.clone();
    normalized.digest = normalize_manifest_reference(&normalized.digest)?;
    Ok(normalized)
}

async fn push_config_blob(
    client: &Client,
    reference: &Reference,
    oci_dir: &Path,
    manifest: &OciImageManifest,
) -> Result<()> {
    let config_blob_path = blob_path(oci_dir, &manifest.config.digest)?;
    let config_data = fs::read(&config_blob_path)
        .with_context(|| format!("failed to read config blob {}", config_blob_path.display()))?;
    println!(
        "==> pushing config {} ({} bytes)",
        &manifest.config.digest[..19],
        config_data.len()
    );
    client
        .push_blob(reference, config_data, &manifest.config.digest)
        .await
        .context("failed to push config blob")?;
    Ok(())
}

async fn push_layer_blobs(
    client: &Client,
    reference: &Reference,
    oci_dir: &Path,
    manifest: &OciImageManifest,
) -> Result<()> {
    for layer in &manifest.layers {
        let layer_blob_path = blob_path(oci_dir, &layer.digest)?;
        let layer_data = fs::read(&layer_blob_path)
            .with_context(|| format!("failed to read layer blob {}", layer_blob_path.display()))?;
        println!(
            "==> pushing layer {} ({} bytes)",
            &layer.digest[..19],
            layer_data.len()
        );
        client
            .push_blob(reference, layer_data, &layer.digest)
            .await
            .with_context(|| format!("failed to push layer {}", layer.digest))?;
    }
    Ok(())
}

async fn push_merged_index(
    client: &Client,
    reference: &Reference,
    auth: &RegistryAuth,
    local_entries: &[ImageIndexEntry],
) -> Result<String> {
    const MAX_ATTEMPTS: usize = 3;

    for attempt in 0..MAX_ATTEMPTS {
        let existing_entries = fetch_remote_index_entries(client, reference, auth).await?;
        let merged_entries = merge_index_entries(&existing_entries, local_entries);
        println!("==> pushing index");
        let digest_location = client
            .push_manifest(
                reference,
                &OciManifest::ImageIndex(OciImageIndex {
                    schema_version: 2,
                    media_type: Some(OCI_IMAGE_INDEX_MEDIA_TYPE.to_string()),
                    manifests: merged_entries.clone(),
                    artifact_type: None,
                    annotations: None,
                }),
            )
            .await
            .context("failed to push image index")?;
        let digest = normalize_manifest_reference(&digest_location)?;

        let remote_entries = fetch_remote_index_entries(client, reference, auth).await?;
        if remote_entries_cover_local(&remote_entries, local_entries) {
            return Ok(digest);
        }

        if attempt + 1 < MAX_ATTEMPTS {
            println!("==> refreshing index after concurrent update");
        }
    }

    bail!(
        "failed to publish a converged multi-platform OCI index for {}",
        reference.whole()
    )
}

async fn fetch_remote_index_entries(
    client: &Client,
    reference: &Reference,
    auth: &RegistryAuth,
) -> Result<Vec<ImageIndexEntry>> {
    let accept = &[
        IMAGE_MANIFEST_MEDIA_TYPE,
        oci_client::manifest::IMAGE_MANIFEST_LIST_MEDIA_TYPE,
        OCI_IMAGE_MEDIA_TYPE,
        OCI_IMAGE_INDEX_MEDIA_TYPE,
    ];

    let (raw_manifest, manifest_digest) =
        match client.pull_manifest_raw(reference, auth, accept).await {
            Ok(result) => result,
            Err(error) => {
                let error = anyhow::Error::new(error).context("failed to pull manifest");
                if registry_reference_missing(&error) {
                    return Ok(Vec::new());
                }
                return Err(error);
            }
        };

    let parsed: OciManifest =
        serde_json::from_slice(&raw_manifest).context("failed to parse registry manifest JSON")?;
    match parsed {
        OciManifest::ImageIndex(index) => index
            .manifests
            .iter()
            .map(normalize_index_entry_digest)
            .collect(),
        OciManifest::Image(manifest) => Ok(vec![
            build_remote_single_manifest_entry(client, reference, &manifest, &manifest_digest)
                .await?,
        ]),
    }
}

async fn build_remote_single_manifest_entry(
    client: &Client,
    reference: &Reference,
    manifest: &OciImageManifest,
    manifest_digest: &str,
) -> Result<ImageIndexEntry> {
    let platform = fetch_manifest_platform(client, reference, &manifest.config.digest).await?;
    Ok(ImageIndexEntry {
        media_type: manifest
            .media_type
            .clone()
            .unwrap_or_else(|| OCI_IMAGE_MEDIA_TYPE.to_string()),
        digest: manifest_digest.to_string(),
        size: serde_json::to_vec(manifest)
            .context("failed to serialize manifest for size accounting")?
            .len() as i64,
        platform: Some(platform),
        annotations: None,
        artifact_type: None,
    })
}

async fn fetch_manifest_platform(
    client: &Client,
    reference: &Reference,
    config_digest: &str,
) -> Result<Platform> {
    let mut config_buf = Vec::new();
    client
        .pull_blob(reference, config_digest, &mut config_buf)
        .await
        .with_context(|| format!("failed to pull config blob {}", config_digest))?;
    let config: serde_json::Value =
        serde_json::from_slice(&config_buf).context("failed to parse image config JSON")?;
    let architecture = config["architecture"]
        .as_str()
        .ok_or_else(|| anyhow!("image config missing architecture"))?;
    let os = config["os"]
        .as_str()
        .ok_or_else(|| anyhow!("image config missing os"))?;
    Ok(Platform {
        architecture: architecture.into(),
        os: os.into(),
        os_version: config["os.version"].as_str().map(ToString::to_string),
        os_features: config["os.features"].as_array().map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToString::to_string))
                .collect::<Vec<_>>()
        }),
        variant: config["variant"].as_str().map(ToString::to_string),
        features: config["features"].as_array().map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToString::to_string))
                .collect::<Vec<_>>()
        }),
    })
}

fn merge_index_entries(
    existing: &[ImageIndexEntry],
    local: &[ImageIndexEntry],
) -> Vec<ImageIndexEntry> {
    let replacement_keys = local
        .iter()
        .filter_map(platform_key)
        .collect::<std::collections::BTreeSet<_>>();
    let mut merged = existing
        .iter()
        .filter(|entry| {
            platform_key(entry)
                .map(|key| !replacement_keys.contains(&key))
                .unwrap_or(true)
        })
        .cloned()
        .collect::<Vec<_>>();
    merged.extend(local.iter().cloned());
    merged
}

fn remote_entries_cover_local(remote: &[ImageIndexEntry], local: &[ImageIndexEntry]) -> bool {
    local.iter().all(|expected| {
        let Some(expected_key) = platform_key(expected) else {
            return false;
        };
        remote.iter().any(|candidate| {
            platform_key(candidate).as_deref() == Some(expected_key.as_str())
                && candidate.digest == expected.digest
        })
    })
}

fn platform_key(entry: &ImageIndexEntry) -> Option<String> {
    let platform = entry.platform.as_ref()?;
    Some(format!(
        "{}|{}|{}|{}|{}|{}",
        platform.os,
        platform.architecture,
        platform.variant.as_deref().unwrap_or(""),
        platform.os_version.as_deref().unwrap_or(""),
        platform
            .os_features
            .as_ref()
            .map(|items| items.join(","))
            .unwrap_or_default(),
        platform
            .features
            .as_ref()
            .map(|items| items.join(","))
            .unwrap_or_default()
    ))
}

fn blob_path(oci_dir: &Path, digest: &str) -> Result<PathBuf> {
    let hash = digest.strip_prefix("sha256:").unwrap_or(digest);
    let path = oci_dir.join("blobs").join("sha256").join(hash);
    if path.exists() {
        return Ok(path);
    }
    // Try flat layout (some dirs store blobs without blobs/sha256/ prefix)
    let flat = oci_dir.join(hash);
    if flat.exists() {
        return Ok(flat);
    }
    bail!(
        "blob not found for digest {digest} in {}",
        oci_dir.display()
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use crate::boringcache_cli::BoringCacheTagScope;

    use super::{
        OCI_IMAGE_MEDIA_TYPE, ProxyPushOptions, merge_index_entries, normalize_index_entry_digest,
        normalize_manifest_reference, push_image_via_boringcache_proxy_with_binary,
        register_manifest_by_digest_reference, remote_entries_cover_local,
    };
    use oci_client::manifest::{ImageIndexEntry, Platform};

    #[test]
    fn proxy_push_invokes_boringcache_run_with_inner_push_command() {
        let temp = tempdir().unwrap();
        let boringcache_bin = temp.path().join("boringcache");
        let boringbuilder_bin = temp.path().join("boringbuilder");
        let oci_dir = temp.path().join("oci");
        let log_path = temp.path().join("boringcache-args.log");
        fs::create_dir_all(&oci_dir).unwrap();

        fs::write(
            &boringcache_bin,
            format!(
                r#"#!/bin/sh
set -eu
printf '%s\n' "$@" > "{}"
"#,
                log_path.display()
            ),
        )
        .unwrap();
        let mut perms = fs::metadata(&boringcache_bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&boringcache_bin, perms).unwrap();

        fs::write(
            &boringbuilder_bin,
            r#"#!/bin/sh
set -eu
exit 0
"#,
        )
        .unwrap();
        let mut perms = fs::metadata(&boringbuilder_bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&boringbuilder_bin, perms).unwrap();

        let options = ProxyPushOptions {
            workspace: Some("boringcache/demo".to_string()),
            binary: Some(boringcache_bin.clone()),
            tag: "boringbuilder-proxy-e2e".to_string(),
            metadata_hints: vec!["tool=boringbuilder".to_string(), "phase=warm".to_string()],
            scope: BoringCacheTagScope::portable_no_git(),
        };

        push_image_via_boringcache_proxy_with_binary(&oci_dir, &options, &boringbuilder_bin)
            .unwrap();

        let args = fs::read_to_string(log_path)
            .unwrap()
            .lines()
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        let expected = vec![
            "run".to_string(),
            "boringcache/demo".to_string(),
            "--proxy".to_string(),
            "boringbuilder-proxy-e2e".to_string(),
            "--skip-restore".to_string(),
            "--skip-save".to_string(),
            "--fail-on-cache-error".to_string(),
            "--no-platform".to_string(),
            "--no-git".to_string(),
            "--metadata-hint".to_string(),
            "tool=boringbuilder".to_string(),
            "--metadata-hint".to_string(),
            "phase=warm".to_string(),
            "--".to_string(),
            boringbuilder_bin.display().to_string(),
            "push".to_string(),
            oci_dir.canonicalize().unwrap().display().to_string(),
            "{CACHE_REF}".to_string(),
            "--insecure".to_string(),
        ];

        assert_eq!(args, expected);
    }

    #[test]
    fn merge_index_entries_replaces_matching_platform_and_preserves_others() {
        let amd64_old = manifest_entry("sha256:old-amd64", "linux", "amd64");
        let arm64 = manifest_entry("sha256:arm64", "linux", "arm64");
        let amd64_new = manifest_entry("sha256:new-amd64", "linux", "amd64");

        let merged = merge_index_entries(
            &[amd64_old, arm64.clone()],
            std::slice::from_ref(&amd64_new),
        );

        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|entry| entry.digest == arm64.digest));
        assert!(merged.iter().any(|entry| entry.digest == amd64_new.digest));
        assert!(
            !merged
                .iter()
                .any(|entry| entry.digest == "sha256:old-amd64")
        );
    }

    #[test]
    fn remote_entries_cover_local_checks_platform_digest_pairs() {
        let remote = vec![
            manifest_entry("sha256:amd64", "linux", "amd64"),
            manifest_entry("sha256:arm64", "linux", "arm64"),
        ];
        let local = vec![manifest_entry("sha256:arm64", "linux", "arm64")];
        assert!(remote_entries_cover_local(&remote, &local));

        let local = vec![manifest_entry("sha256:other", "linux", "arm64")];
        assert!(!remote_entries_cover_local(&remote, &local));
    }

    #[test]
    fn registers_child_manifests_by_digest_reference() {
        let reference = oci_client::Reference::with_tag(
            "localhost:5000".to_string(),
            "boringbuilder/cache".to_string(),
            "main".to_string(),
        );
        let digest_ref =
            register_manifest_by_digest_reference(&reference, "sha256:deadbeef".to_string());
        assert_eq!(
            digest_ref.whole(),
            "localhost:5000/boringbuilder/cache@sha256:deadbeef"
        );
    }

    #[test]
    fn normalizes_manifest_location_to_digest() {
        let digest = normalize_manifest_reference(
            "http://localhost:5000/v2/boringbuilder/cache/manifests/sha256:deadbeef",
        )
        .unwrap();
        assert_eq!(digest, "sha256:deadbeef");
    }

    #[test]
    fn keeps_existing_manifest_digest() {
        let digest = normalize_manifest_reference("sha256:deadbeef").unwrap();
        assert_eq!(digest, "sha256:deadbeef");
    }

    #[test]
    fn normalizes_index_entry_digest_urls() {
        let entry = manifest_entry(
            "http://localhost:5000/v2/boringbuilder/cache/manifests/sha256:deadbeef",
            "linux",
            "amd64",
        );
        let normalized = normalize_index_entry_digest(&entry).unwrap();
        assert_eq!(normalized.digest, "sha256:deadbeef");
        assert_eq!(
            normalized
                .platform
                .as_ref()
                .unwrap()
                .architecture
                .to_string(),
            "amd64".to_string()
        );
    }
    fn manifest_entry(digest: &str, os: &str, arch: &str) -> ImageIndexEntry {
        ImageIndexEntry {
            media_type: OCI_IMAGE_MEDIA_TYPE.to_string(),
            digest: digest.to_string(),
            size: 42,
            platform: Some(Platform {
                os: os.into(),
                architecture: arch.into(),
                os_version: None,
                os_features: None,
                variant: None,
                features: None,
            }),
            annotations: None,
            artifact_type: None,
        }
    }
}
