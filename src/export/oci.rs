use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::schema::Pipeline;
use crate::util::fs_tree::is_pseudo_fs;
use crate::util::oci_rootfs::unpack_layer;

pub const OCI_GZIP_LAYER_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
pub const OCI_ZSTD_LAYER_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar+zstd";
pub const BUILD_CACHE_SNAPSHOT_STATE_LAYER_MEDIA_TYPE: &str =
    "application/vnd.boringbuilder.cache.snapshot-state.v1+json";

pub struct OciLayerArchive {
    pub temp_file: tempfile::NamedTempFile,
    pub compressed_digest: String,
    pub diff_id: String,
    pub compressed_size: u64,
    pub media_type: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildCacheStepRecord {
    pub operation_index: usize,
    pub key: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildCacheLayout {
    pub steps: Vec<BuildCacheStepRecord>,
    pub layers: Vec<BuildCacheLayerDescriptor>,
    pub(crate) snapshot_states: BTreeMap<usize, BuildCacheSnapshotState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildCacheLayerDescriptor {
    pub media_type: String,
    pub digest: String,
    pub diff_id: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BuildCacheSnapshotStateDescriptor {
    pub digest: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuildCacheSnapshotState {
    pub descriptor: BuildCacheSnapshotStateDescriptor,
    pub state: Option<BTreeMap<PathBuf, SnapshotEntry>>,
}

pub fn export_pipeline_oci(
    pipeline: &Pipeline,
    output_path: &Path,
    base_image_dir: &Path,
    rootfs_dir: &Path,
) -> Result<PathBuf> {
    export_pipeline_oci_with_layer(
        pipeline,
        output_path,
        base_image_dir,
        LayerSource::Rootfs(rootfs_dir),
    )
}

pub fn export_pipeline_oci_from_overlay_upper(
    pipeline: &Pipeline,
    output_path: &Path,
    base_image_dir: &Path,
    upper_dir: &Path,
) -> Result<PathBuf> {
    export_pipeline_oci_with_layer(
        pipeline,
        output_path,
        base_image_dir,
        LayerSource::OverlayUpper(upper_dir),
    )
}

pub fn create_oci_layer_from_overlay_upper(upper_dir: &Path) -> Result<OciLayerArchive> {
    let (temp_file, compressed_digest, diff_id, compressed_size) =
        create_layer_from_overlay_upper(upper_dir)?;
    Ok(OciLayerArchive {
        temp_file,
        compressed_digest,
        diff_id,
        compressed_size,
        media_type: OCI_GZIP_LAYER_MEDIA_TYPE,
    })
}

pub fn create_oci_layer_from_fs_delta(
    previous_dir: Option<&Path>,
    current_dir: &Path,
) -> Result<OciLayerArchive> {
    let (temp_file, compressed_digest, diff_id, compressed_size) =
        create_layer_from_snapshot_delta(previous_dir, current_dir)?;
    Ok(OciLayerArchive {
        temp_file,
        compressed_digest,
        diff_id,
        compressed_size,
        media_type: OCI_ZSTD_LAYER_MEDIA_TYPE,
    })
}

#[cfg(test)]
pub(crate) fn snapshot_tree_with_state(source: &Path, destination: &Path) -> Result<()> {
    let mut state = BTreeMap::new();
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    if source.exists() {
        snapshot_tree_entry_with_state(source, source, destination, &mut state)?;
    }
    write_snapshot_state_manifest(destination, &state)?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn snapshot_tree_state_only(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    let state = load_or_collect_snapshot_state(source)?;
    write_snapshot_state_manifest(destination, &state)?;
    Ok(())
}

pub fn create_single_layer_oci_layout(
    output_path: &Path,
    platform: &str,
    layer: OciLayerArchive,
) -> Result<PathBuf> {
    let (platform_os, platform_arch) = platform
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("invalid platform '{}'", platform))?;
    let blobs_dir = output_path.join("blobs").join("sha256");
    fs::create_dir_all(&blobs_dir)
        .with_context(|| format!("failed to create {}", blobs_dir.display()))?;

    let layer_dest = blobs_dir.join(&layer.compressed_digest);
    fs::rename(layer.temp_file.path(), &layer_dest)
        .or_else(|_| fs::copy(layer.temp_file.path(), &layer_dest).map(|_| ()))
        .with_context(|| format!("failed to materialize {}", layer_dest.display()))?;

    let config = serde_json::json!({
        "architecture": platform_arch,
        "os": platform_os,
        "rootfs": {
            "type": "layers",
            "diff_ids": [format!("sha256:{}", layer.diff_id)],
        },
        "history": [{
            "created_by": "boringbuilder build-cache",
            "comment": "boringbuilder OCI build-cache layer",
        }],
    });
    let config_bytes = serde_json::to_vec_pretty(&config)?;
    let config_digest = sha256_bytes(&config_bytes);
    fs::write(blobs_dir.join(&config_digest), &config_bytes).with_context(|| {
        format!(
            "failed to write OCI config blob {}",
            blobs_dir.join(&config_digest).display()
        )
    })?;

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{config_digest}"),
            "size": config_bytes.len() as u64,
        },
        "layers": [{
            "mediaType": layer.media_type,
            "digest": format!("sha256:{}", layer.compressed_digest),
            "size": layer.compressed_size,
        }],
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let manifest_digest = sha256_bytes(&manifest_bytes);
    fs::write(blobs_dir.join(&manifest_digest), &manifest_bytes).with_context(|| {
        format!(
            "failed to write OCI manifest blob {}",
            blobs_dir.join(&manifest_digest).display()
        )
    })?;

    let index = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{manifest_digest}"),
            "size": manifest_bytes.len() as u64,
            "platform": {
                "architecture": platform_arch,
                "os": platform_os,
            }
        }]
    });
    fs::write(
        output_path.join("index.json"),
        serde_json::to_string_pretty(&index)?,
    )
    .with_context(|| {
        format!(
            "failed to write {}",
            output_path.join("index.json").display()
        )
    })?;
    fs::write(
        output_path.join("oci-layout"),
        r#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .with_context(|| {
        format!(
            "failed to write {}",
            output_path.join("oci-layout").display()
        )
    })?;

    Ok(output_path.to_path_buf())
}

pub fn append_build_cache_layer_to_layout(
    layout_dir: &Path,
    platform: &str,
    step: BuildCacheStepRecord,
    layer: OciLayerArchive,
) -> Result<usize> {
    let mut layout = load_build_cache_layout(layout_dir, platform)?.unwrap_or_default();
    let layer_descriptor = materialize_layer_blob(layout_dir, layer)?;
    layout.steps.push(step);
    layout.layers.push(layer_descriptor);
    let layer_count = layout.layers.len();
    write_build_cache_layout(layout_dir, platform, &layout)?;
    Ok(layer_count)
}

pub fn load_build_cache_layout(
    layout_dir: &Path,
    platform: &str,
) -> Result<Option<BuildCacheLayout>> {
    let index_path = layout_dir.join("index.json");
    if !index_path.exists() {
        return Ok(None);
    }

    let index: serde_json::Value = serde_json::from_reader(
        File::open(&index_path)
            .with_context(|| format!("failed to open {}", index_path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", index_path.display()))?;

    let descriptor = index["manifests"]
        .as_array()
        .and_then(|manifests| manifests.first())
        .ok_or_else(|| anyhow::anyhow!("index.json missing manifests"))?;
    let manifest = load_cache_manifest(layout_dir, descriptor, platform)?;
    let config_digest = manifest["config"]["digest"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("manifest missing config digest"))?;
    let config_path = resolve_layout_blob_path(layout_dir, config_digest)?;
    let config: serde_json::Value = serde_json::from_reader(
        File::open(&config_path)
            .with_context(|| format!("failed to open {}", config_path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", config_path.display()))?;

    let steps: Vec<BuildCacheStepRecord> = serde_json::from_value(
        config
            .pointer("/boringbuilder/build_cache/steps")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([])),
    )
    .context("failed to parse build-cache steps metadata")?;
    let mut snapshot_states = load_build_cache_snapshot_state_refs(&config)
        .context("failed to parse build-cache snapshot-state metadata")?;
    if snapshot_states.is_empty() {
        snapshot_states = load_legacy_build_cache_snapshot_states(layout_dir)?;
    }
    let diff_ids = config["rootfs"]["diff_ids"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("build-cache config missing rootfs.diff_ids"))?;
    let layers = manifest["layers"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("build-cache manifest missing layers"))?;
    if diff_ids.len() > layers.len() {
        bail!(
            "build-cache layout {} has {} diff_ids but only {} layers",
            layout_dir.display(),
            diff_ids.len(),
            layers.len()
        );
    }
    if steps.len() != diff_ids.len() {
        bail!(
            "build-cache layout {} has {} recorded steps but {} rootfs layers",
            layout_dir.display(),
            steps.len(),
            diff_ids.len()
        );
    }

    let mut layer_descriptors = Vec::with_capacity(diff_ids.len());
    for (layer, diff_id) in layers.iter().take(diff_ids.len()).zip(diff_ids.iter()) {
        layer_descriptors.push(BuildCacheLayerDescriptor {
            media_type: layer["mediaType"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("layer missing mediaType"))?
                .to_string(),
            digest: layer["digest"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("layer missing digest"))?
                .to_string(),
            diff_id: diff_id
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("diff_id missing string value"))?
                .to_string(),
            size: layer["size"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("layer missing size"))?,
        });
    }

    Ok(Some(BuildCacheLayout {
        steps,
        layers: layer_descriptors,
        snapshot_states,
    }))
}

pub fn truncate_build_cache_layout(
    layout_dir: &Path,
    platform: &str,
    layer_count: usize,
) -> Result<()> {
    let Some(mut layout) = load_build_cache_layout(layout_dir, platform)? else {
        return Ok(());
    };
    if layer_count >= layout.layers.len() {
        return Ok(());
    }
    layout.steps.truncate(layer_count);
    layout.layers.truncate(layer_count);
    layout
        .snapshot_states
        .retain(|count, _| *count <= layer_count);
    write_build_cache_layout(layout_dir, platform, &layout)
}

pub fn store_build_cache_snapshot_state(
    layout_dir: &Path,
    layer_count: usize,
    snapshot_root: &Path,
) -> Result<()> {
    let platform = infer_build_cache_platform(layout_dir)?;
    let Some(mut layout) = load_build_cache_layout(layout_dir, &platform)? else {
        bail!(
            "build-cache layout {} is missing while storing snapshot state",
            layout_dir.display()
        );
    };
    let state = load_or_collect_snapshot_state(snapshot_root)?;
    layout.snapshot_states.insert(
        layer_count,
        build_cache_snapshot_state(state)
            .context("failed to prepare build-cache snapshot state")?,
    );
    write_build_cache_layout(layout_dir, &platform, &layout)
}

pub fn store_build_cache_snapshot_state_if_missing(
    layout_dir: &Path,
    layer_count: usize,
    snapshot_root: &Path,
) -> Result<bool> {
    let platform = infer_build_cache_platform(layout_dir)?;
    let Some(layout) = load_build_cache_layout(layout_dir, &platform)? else {
        return Ok(false);
    };
    if layout.snapshot_states.contains_key(&layer_count) {
        return Ok(false);
    }
    store_build_cache_snapshot_state(layout_dir, layer_count, snapshot_root)?;
    Ok(true)
}

pub fn restore_build_cache_snapshot_state(
    layout_dir: &Path,
    layer_count: usize,
    destination_root: &Path,
) -> Result<bool> {
    if let Some(state) = load_build_cache_snapshot_state(layout_dir, layer_count)? {
        fs::create_dir_all(destination_root)
            .with_context(|| format!("failed to create {}", destination_root.display()))?;
        let destination = snapshot_state_manifest_path(destination_root);
        fs::write(&destination, serde_json::to_vec(&state)?)
            .with_context(|| format!("failed to write {}", destination.display()))?;
        return Ok(true);
    }

    let source = build_cache_snapshot_state_path(layout_dir, layer_count);
    if !source.is_file() {
        return Ok(false);
    }

    fs::create_dir_all(destination_root)
        .with_context(|| format!("failed to create {}", destination_root.display()))?;
    let destination = snapshot_state_manifest_path(destination_root);
    fs::copy(&source, &destination).with_context(|| {
        format!(
            "failed to restore build-cache snapshot state {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(true)
}

pub(crate) fn load_build_cache_snapshot_state(
    layout_dir: &Path,
    layer_count: usize,
) -> Result<Option<BTreeMap<PathBuf, SnapshotEntry>>> {
    let platform = infer_build_cache_platform(layout_dir)?;
    if let Some(layout) = load_build_cache_layout(layout_dir, &platform)?
        && let Some(state) = layout.snapshot_states.get(&layer_count)
    {
        return load_snapshot_state_blob(layout_dir, state).map(Some);
    }

    let source = build_cache_snapshot_state_path(layout_dir, layer_count);
    if !source.is_file() {
        return Ok(None);
    }

    let state = serde_json::from_slice(
        &fs::read(&source).with_context(|| format!("failed to read {}", source.display()))?,
    )
    .with_context(|| format!("failed to parse {}", source.display()))?;
    Ok(Some(state))
}

fn infer_build_cache_platform(layout_dir: &Path) -> Result<String> {
    let index_path = layout_dir.join("index.json");
    let index: serde_json::Value = serde_json::from_reader(
        File::open(&index_path)
            .with_context(|| format!("failed to open {}", index_path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", index_path.display()))?;
    let descriptor = index["manifests"]
        .as_array()
        .and_then(|manifests| manifests.first())
        .ok_or_else(|| anyhow::anyhow!("index.json missing manifests"))?;
    let platform = descriptor["platform"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("index.json manifest missing platform"))?;
    let os = platform
        .get("os")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow::anyhow!("index.json manifest missing platform.os"))?;
    let arch = platform
        .get("architecture")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow::anyhow!("index.json manifest missing platform.architecture"))?;
    Ok(format!("{os}/{arch}"))
}

pub fn unpack_rootfs_from_build_cache_layout(
    layout_dir: &Path,
    platform: &str,
    rootfs_dir: &Path,
    layer_count: usize,
) -> Result<()> {
    let index_path = layout_dir.join("index.json");
    let index: serde_json::Value = serde_json::from_reader(
        File::open(&index_path)
            .with_context(|| format!("failed to open {}", index_path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", index_path.display()))?;

    let descriptor = index["manifests"]
        .as_array()
        .and_then(|manifests| manifests.first())
        .ok_or_else(|| anyhow::anyhow!("index.json missing manifests"))?;
    let manifest = load_cache_manifest(layout_dir, descriptor, platform)?;
    let layers = manifest["layers"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("manifest missing layers"))?;

    for layer in layers.iter().take(layer_count) {
        let digest = layer["digest"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("layer missing digest"))?;
        let media_type = layer["mediaType"].as_str();
        let layer_path = resolve_layout_blob_path(layout_dir, digest)?;
        unpack_layer(&layer_path, rootfs_dir, media_type)?;
    }

    Ok(())
}

fn materialize_layer_blob(
    layout_dir: &Path,
    layer: OciLayerArchive,
) -> Result<BuildCacheLayerDescriptor> {
    let blobs_dir = layout_dir.join("blobs").join("sha256");
    fs::create_dir_all(&blobs_dir)
        .with_context(|| format!("failed to create {}", blobs_dir.display()))?;
    let layer_dest = blobs_dir.join(&layer.compressed_digest);
    if !layer_dest.exists() {
        fs::rename(layer.temp_file.path(), &layer_dest)
            .or_else(|_| fs::copy(layer.temp_file.path(), &layer_dest).map(|_| ()))
            .with_context(|| format!("failed to materialize {}", layer_dest.display()))?;
    }

    Ok(BuildCacheLayerDescriptor {
        media_type: layer.media_type.to_string(),
        digest: format!("sha256:{}", layer.compressed_digest),
        diff_id: format!("sha256:{}", layer.diff_id),
        size: layer.compressed_size,
    })
}

pub(crate) fn write_build_cache_layout(
    layout_dir: &Path,
    platform: &str,
    layout: &BuildCacheLayout,
) -> Result<()> {
    let (platform_os, platform_arch) = platform
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("invalid platform '{}'", platform))?;
    let blobs_dir = layout_dir.join("blobs").join("sha256");
    fs::create_dir_all(&blobs_dir)
        .with_context(|| format!("failed to create {}", blobs_dir.display()))?;
    let snapshot_state_blobs =
        materialize_snapshot_state_blobs(layout_dir, &layout.snapshot_states)
            .context("failed to materialize build-cache snapshot-state blobs")?;

    let config = serde_json::json!({
        "architecture": platform_arch,
        "os": platform_os,
        "rootfs": {
            "type": "layers",
            "diff_ids": layout.layers.iter().map(|layer| layer.diff_id.clone()).collect::<Vec<_>>(),
        },
        "history": layout.steps.iter().map(|step| {
            serde_json::json!({
                "created_by": "boringbuilder build-cache",
                "comment": format!("boringbuilder build-cache operation {}", step.operation_index + 1),
            })
        }).collect::<Vec<_>>(),
        "boringbuilder": {
            "build_cache": {
                "format": "v4",
                "steps": layout.steps,
                "snapshot_state_blobs": snapshot_state_blobs,
            }
        }
    });
    let config_bytes = serde_json::to_vec_pretty(&config)?;
    let config_digest = sha256_bytes(&config_bytes);
    fs::write(blobs_dir.join(&config_digest), &config_bytes).with_context(|| {
        format!(
            "failed to write build-cache config {}",
            blobs_dir.join(&config_digest).display()
        )
    })?;

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{config_digest}"),
            "size": config_bytes.len() as u64,
        },
        "layers": layout
            .layers
            .iter()
            .map(|layer| {
                serde_json::json!({
                    "mediaType": layer.media_type,
                    "digest": layer.digest,
                    "size": layer.size,
                })
            })
            .chain(snapshot_state_blobs.values().map(|descriptor| {
                serde_json::json!({
                    "mediaType": BUILD_CACHE_SNAPSHOT_STATE_LAYER_MEDIA_TYPE,
                    "digest": descriptor.digest,
                    "size": descriptor.size,
                })
            }))
            .collect::<Vec<_>>(),
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let manifest_digest = sha256_bytes(&manifest_bytes);
    fs::write(blobs_dir.join(&manifest_digest), &manifest_bytes).with_context(|| {
        format!(
            "failed to write build-cache manifest {}",
            blobs_dir.join(&manifest_digest).display()
        )
    })?;

    let index = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{manifest_digest}"),
            "size": manifest_bytes.len() as u64,
            "platform": {
                "architecture": platform_arch,
                "os": platform_os,
            }
        }]
    });
    fs::write(
        layout_dir.join("index.json"),
        serde_json::to_string_pretty(&index)?,
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
    let legacy_snapshot_dir = layout_dir.join(BUILD_CACHE_SNAPSHOT_STATE_DIR);
    if legacy_snapshot_dir.exists() {
        fs::remove_dir_all(&legacy_snapshot_dir).with_context(|| {
            format!(
                "failed to remove legacy build-cache snapshot-state dir {}",
                legacy_snapshot_dir.display()
            )
        })?;
    }

    Ok(())
}

fn load_cache_manifest(
    layout_dir: &Path,
    descriptor: &serde_json::Value,
    _platform: &str,
) -> Result<serde_json::Value> {
    let digest = descriptor["digest"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("descriptor missing digest"))?;
    let manifest_path = resolve_layout_blob_path(layout_dir, digest)?;
    serde_json::from_reader(
        File::open(&manifest_path)
            .with_context(|| format!("failed to open {}", manifest_path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", manifest_path.display()))
}

fn export_pipeline_oci_with_layer(
    pipeline: &Pipeline,
    output_path: &Path,
    base_image_dir: &Path,
    layer_source: LayerSource<'_>,
) -> Result<PathBuf> {
    let manifest: serde_json::Value = serde_json::from_reader(
        File::open(base_image_dir.join("manifest.json"))
            .context("failed to open base image manifest")?,
    )
    .context("failed to parse base image manifest")?;

    let config_digest_raw = manifest["config"]["digest"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("manifest missing config.digest"))?;
    let config_path = resolve_blob(base_image_dir, config_digest_raw)?;
    let mut base_config: serde_json::Value = serde_json::from_reader(
        File::open(&config_path).context("failed to open base image config")?,
    )
    .context("failed to parse base image config")?;

    let base_layers = manifest["layers"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("manifest missing layers array"))?;

    let blobs_dir = output_path.join("blobs").join("sha256");
    fs::create_dir_all(&blobs_dir)?;

    let mut out_layers: Vec<serde_json::Value> = Vec::new();
    for layer in base_layers {
        let digest_raw = layer["digest"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("layer missing digest"))?;
        let digest_hex = digest_raw.strip_prefix("sha256:").unwrap_or(digest_raw);
        let source = resolve_blob(base_image_dir, digest_raw)?;
        let dest = blobs_dir.join(digest_hex);
        if !dest.exists() {
            fs::hard_link(&source, &dest).unwrap_or_else(|_| {
                fs::copy(&source, &dest).unwrap();
            });
        }
        out_layers.push(layer.clone());
    }

    let (layer_file, compressed_digest, diff_id, compressed_size) = match layer_source {
        LayerSource::Rootfs(rootfs_dir) => {
            let export_paths = resolve_export_paths(pipeline, rootfs_dir)?;
            create_layer_from_paths(&export_paths)?
        }
        LayerSource::OverlayUpper(upper_dir) => create_layer_from_overlay_upper(upper_dir)?,
    };
    let layer_dest = blobs_dir.join(&compressed_digest);
    fs::rename(layer_file.path(), &layer_dest)
        .or_else(|_| fs::copy(layer_file.path(), &layer_dest).map(|_| ()))?;

    out_layers.push(serde_json::json!({
        "mediaType": OCI_GZIP_LAYER_MEDIA_TYPE,
        "digest": format!("sha256:{compressed_digest}"),
        "size": compressed_size
    }));

    if let Some(diff_ids) = base_config
        .pointer_mut("/rootfs/diff_ids")
        .and_then(|v| v.as_array_mut())
    {
        diff_ids.push(serde_json::json!(format!("sha256:{diff_id}")));
    }

    if let Some(history) = base_config
        .pointer_mut("/history")
        .and_then(|v| v.as_array_mut())
    {
        history.push(serde_json::json!({
            "created_by": "boringbuilder",
            "comment": "boringbuilder execution outputs"
        }));
    }

    let env_list: Vec<String> = pipeline
        .env
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    if !env_list.is_empty()
        && let Some(cfg) = base_config.get_mut("config")
    {
        if let Some(existing) = cfg.get_mut("Env").and_then(|v| v.as_array_mut()) {
            for entry in &env_list {
                existing.push(serde_json::json!(entry));
            }
        } else {
            cfg["Env"] = serde_json::json!(env_list);
        }
    }
    if let Some(cfg) = base_config.get_mut("config") {
        cfg["WorkingDir"] = serde_json::json!(pipeline.workdir);
    }

    // Apply image metadata (entrypoint, cmd, user, labels, expose, volumes)
    if let Some(meta) = &pipeline.metadata
        && let Some(cfg) = base_config.get_mut("config")
    {
        if let Some(entrypoint) = &meta.entrypoint {
            cfg["Entrypoint"] = serde_json::json!(entrypoint);
        }
        if let Some(cmd) = &meta.cmd {
            cfg["Cmd"] = serde_json::json!(cmd);
        }
        if let Some(healthcheck) = &meta.healthcheck {
            cfg["Healthcheck"] = match healthcheck {
                crate::schema::ImageHealthcheck::None => {
                    serde_json::json!({ "Test": ["NONE"] })
                }
                crate::schema::ImageHealthcheck::Command {
                    test,
                    interval_nanos,
                    timeout_nanos,
                    start_period_nanos,
                    start_interval_nanos,
                    retries,
                } => {
                    let mut value = serde_json::json!({ "Test": test });
                    if let Some(interval_nanos) = interval_nanos {
                        value["Interval"] = serde_json::json!(interval_nanos);
                    }
                    if let Some(timeout_nanos) = timeout_nanos {
                        value["Timeout"] = serde_json::json!(timeout_nanos);
                    }
                    if let Some(start_period_nanos) = start_period_nanos {
                        value["StartPeriod"] = serde_json::json!(start_period_nanos);
                    }
                    if let Some(start_interval_nanos) = start_interval_nanos {
                        value["StartInterval"] = serde_json::json!(start_interval_nanos);
                    }
                    if let Some(retries) = retries {
                        value["Retries"] = serde_json::json!(retries);
                    }
                    value
                }
            };
        }
        if let Some(user) = &meta.user {
            cfg["User"] = serde_json::json!(user);
        }
        if let Some(signal) = &meta.stop_signal {
            cfg["StopSignal"] = serde_json::json!(signal);
        }
        if !meta.expose.is_empty() {
            let ports: serde_json::Map<String, serde_json::Value> = meta
                .expose
                .iter()
                .map(|p| {
                    let key = if p.contains('/') {
                        p.clone()
                    } else {
                        format!("{p}/tcp")
                    };
                    (key, serde_json::json!({}))
                })
                .collect();
            cfg["ExposedPorts"] = serde_json::Value::Object(ports);
        }
        if !meta.labels.is_empty() {
            cfg["Labels"] = serde_json::json!(meta.labels);
        }
        if !meta.volumes.is_empty() {
            let vols: serde_json::Map<String, serde_json::Value> = meta
                .volumes
                .iter()
                .map(|v| (v.clone(), serde_json::json!({})))
                .collect();
            cfg["Volumes"] = serde_json::Value::Object(vols);
        }
    }

    let config_bytes = serde_json::to_vec_pretty(&base_config)?;
    let config_digest = sha256_bytes(&config_bytes);
    fs::write(blobs_dir.join(&config_digest), &config_bytes)?;

    let new_manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{config_digest}"),
            "size": config_bytes.len() as u64
        },
        "layers": out_layers
    });
    let manifest_bytes = serde_json::to_vec_pretty(&new_manifest)?;
    let manifest_digest = sha256_bytes(&manifest_bytes);
    fs::write(blobs_dir.join(&manifest_digest), &manifest_bytes)?;

    let arch = pipeline.platform.split('/').nth(1).unwrap_or("amd64");

    let index = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{manifest_digest}"),
            "size": manifest_bytes.len() as u64,
            "platform": {
                "architecture": arch,
                "os": "linux"
            }
        }]
    });
    fs::write(
        output_path.join("index.json"),
        serde_json::to_string_pretty(&index)?,
    )?;
    fs::write(
        output_path.join("oci-layout"),
        r#"{"imageLayoutVersion":"1.0.0"}"#,
    )?;

    Ok(output_path.to_path_buf())
}

enum LayerSource<'a> {
    Rootfs(&'a Path),
    OverlayUpper(&'a Path),
}

fn resolve_blob(image_dir: &Path, digest: &str) -> Result<PathBuf> {
    let direct = image_dir.join(digest);
    if direct.exists() {
        return Ok(direct);
    }
    let hash_only = digest.split(':').nth(1).unwrap_or(digest);
    let alternate = image_dir.join(hash_only);
    if alternate.exists() {
        return Ok(alternate);
    }
    bail!("blob {} not found in {}", digest, image_dir.display())
}

fn resolve_layout_blob_path(layout_dir: &Path, digest: &str) -> Result<PathBuf> {
    let digest_only = digest.split(':').nth(1).unwrap_or(digest);
    let path = layout_dir.join("blobs").join("sha256").join(digest_only);
    if path.exists() {
        Ok(path)
    } else {
        bail!(
            "could not find OCI blob for digest {} in {}",
            digest,
            layout_dir.display()
        )
    }
}

struct ExportPath {
    host_path: PathBuf,
    container_prefix: PathBuf,
}

fn resolve_export_paths(pipeline: &Pipeline, rootfs_dir: &Path) -> Result<Vec<ExportPath>> {
    let container_paths: Vec<String> = if !pipeline.outputs.is_empty() {
        pipeline.outputs.clone()
    } else {
        pipeline
            .inputs
            .iter()
            .filter(|input| !input.readonly)
            .map(|input| input.dest.clone())
            .collect()
    };

    if container_paths.is_empty() {
        bail!("nothing to export; define outputs or use a writable input mount");
    }

    let mut paths = Vec::new();
    for container_path in container_paths {
        let relative = container_path.trim_start_matches('/');
        let host = rootfs_dir.join(relative);
        if host.exists() {
            paths.push(ExportPath {
                host_path: host,
                container_prefix: PathBuf::from(relative),
            });
        }
    }

    Ok(paths)
}

fn create_layer_from_paths(
    paths: &[ExportPath],
) -> Result<(tempfile::NamedTempFile, String, String, u64)> {
    let mut entries = Vec::new();
    for export in paths {
        collect_entries(&export.host_path, &export.container_prefix, &mut entries)?;
    }
    create_layer_from_entries(entries)
}

fn create_layer_from_overlay_upper(
    upper_dir: &Path,
) -> Result<(tempfile::NamedTempFile, String, String, u64)> {
    let mut entries = Vec::new();
    collect_overlay_entries(upper_dir, Path::new(""), &mut entries)?;
    create_layer_from_entries(entries)
}

fn create_layer_from_snapshot_delta(
    previous_dir: Option<&Path>,
    current_dir: &Path,
) -> Result<(tempfile::NamedTempFile, String, String, u64)> {
    if previous_dir.is_none() {
        let mut entries = Vec::new();
        collect_entries(current_dir, Path::new(""), &mut entries)?;
        return create_layer_from_entries_zstd(entries);
    }

    let previous = load_or_collect_snapshot_state(previous_dir.expect("checked above"))?;
    let current = load_or_collect_snapshot_state(current_dir)?;
    let mut entries = Vec::new();
    let mut removed_roots = BTreeSet::new();

    for previous_path in previous.keys() {
        if current.contains_key(previous_path)
            || has_removed_ancestor(previous_path, &removed_roots)
        {
            continue;
        }
        removed_roots.insert(previous_path.clone());
        entries.push(whiteout_entry(previous_path));
    }

    for (path, current_entry) in &current {
        let Some(previous_entry) = previous.get(path) else {
            entries.push(current_entry.to_layer_entry(current_dir, path));
            continue;
        };

        if previous_entry == current_entry {
            continue;
        }

        if previous_entry.kind != current_entry.kind {
            entries.push(whiteout_entry(path));
        }
        entries.push(current_entry.to_layer_entry(current_dir, path));
    }

    create_layer_from_entries_zstd(entries)
}

fn load_or_collect_snapshot_state(root: &Path) -> Result<BTreeMap<PathBuf, SnapshotEntry>> {
    if let Some(state) = load_snapshot_state_manifest(root)? {
        return Ok(state);
    }

    collect_snapshot_state(root)
}

fn create_layer_from_entries(
    entries: Vec<LayerEntry>,
) -> Result<(tempfile::NamedTempFile, String, String, u64)> {
    create_layer_from_entries_gzip(entries)
}

fn create_layer_from_entries_gzip(
    mut entries: Vec<LayerEntry>,
) -> Result<(tempfile::NamedTempFile, String, String, u64)> {
    entries.sort_by(|a, b| a.archive_path.cmp(&b.archive_path));
    entries.dedup_by(|a, b| a.archive_path == b.archive_path);

    let temp = tempfile::NamedTempFile::new()?;
    let file = File::create(temp.path())?;
    let compressed = HashingWriter::new(file);
    let gz = flate2::GzBuilder::new().write(compressed, flate2::Compression::fast());
    let uncompressed = HashingWriter::new(gz);
    let mut builder = tar::Builder::new(uncompressed);
    write_layer_entries(&mut builder, &entries)?;

    let uncompressed = builder.into_inner()?;
    let (gz, diff_hasher, _) = uncompressed.into_parts();
    let diff_id = hex::encode(diff_hasher.finalize());
    let compressed = finish_gzip(gz)?;
    let (file, compressed_hasher, compressed_size) = compressed.into_parts();
    drop(file);
    let compressed_digest = hex::encode(compressed_hasher.finalize());

    Ok((temp, compressed_digest, diff_id, compressed_size))
}

fn create_layer_from_entries_zstd(
    mut entries: Vec<LayerEntry>,
) -> Result<(tempfile::NamedTempFile, String, String, u64)> {
    entries.sort_by(|a, b| a.archive_path.cmp(&b.archive_path));
    entries.dedup_by(|a, b| a.archive_path == b.archive_path);

    let temp = tempfile::NamedTempFile::new()?;
    let file = File::create(temp.path())?;
    let compressed = HashingWriter::new(file);
    let zstd_enc = zstd::Encoder::new(compressed, 3)?;
    let uncompressed = HashingWriter::new(zstd_enc);
    let mut builder = tar::Builder::new(uncompressed);
    write_layer_entries(&mut builder, &entries)?;

    let uncompressed = builder.into_inner()?;
    let (zstd_enc, diff_hasher, _) = uncompressed.into_parts();
    let diff_id = hex::encode(diff_hasher.finalize());
    let compressed = zstd_enc.finish()?;
    let (file, compressed_hasher, compressed_size) = compressed.into_parts();
    drop(file);
    let compressed_digest = hex::encode(compressed_hasher.finalize());

    Ok((temp, compressed_digest, diff_id, compressed_size))
}

fn write_layer_entries<W: Write>(
    builder: &mut tar::Builder<W>,
    entries: &[LayerEntry],
) -> Result<()> {
    builder.follow_symlinks(false);
    for entry in entries {
        let mut header = tar::Header::new_gnu();
        header.set_uid(entry.uid);
        header.set_gid(entry.gid);
        header.set_mtime(0);
        header.set_mode(entry.mode);

        match &entry.kind {
            LayerEntryKind::Symlink(target) => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                builder.append_link(&mut header, &entry.archive_path, target)?;
            }
            LayerEntryKind::Directory => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_cksum();
                builder.append_data(&mut header, &entry.archive_path, io::empty())?;
            }
            LayerEntryKind::Whiteout => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(0);
                header.set_cksum();
                builder.append_data(&mut header, &entry.archive_path, io::empty())?;
            }
            LayerEntryKind::Regular => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(entry.size);
                header.set_cksum();
                let Some(host_path) = &entry.host_path else {
                    bail!("regular layer entry missing host path");
                };
                let mut input = match File::open(host_path) {
                    Ok(f) => f,
                    Err(e) if e.kind() == io::ErrorKind::PermissionDenied => continue,
                    Err(e) => return Err(e.into()),
                };
                builder.append_data(&mut header, &entry.archive_path, &mut input)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn snapshot_tree_entry_with_state(
    root: &Path,
    source: &Path,
    destination: &Path,
    state: &mut BTreeMap<PathBuf, SnapshotEntry>,
) -> Result<()> {
    if source != root && is_pseudo_fs(root, source) {
        return Ok(());
    }

    let relative =
        (source != root).then(|| source.strip_prefix(root).unwrap_or(source).to_path_buf());
    if relative.as_deref() == Some(Path::new(SNAPSHOT_STATE_MANIFEST_FILE)) {
        return Ok(());
    }

    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if is_ignorable_snapshot_error(&error) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mode = metadata.permissions().mode() & 0o7777;
    let uid = metadata.uid() as u64;
    let gid = metadata.gid() as u64;

    if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let target = fs::read_link(source)?;
            std::os::unix::fs::symlink(&target, destination)?;
            if let Some(relative) = relative {
                state.insert(
                    relative,
                    SnapshotEntry {
                        kind: SnapshotEntryKind::Symlink(target),
                        mode,
                        uid,
                        gid,
                    },
                );
            }
        }
        #[cfg(not(unix))]
        {
            anyhow::bail!("symlink-preserving cache snapshots are only supported on unix");
        }
        return Ok(());
    }

    if metadata.is_dir() {
        fs::create_dir_all(destination)?;
        if let Some(relative) = relative {
            state.insert(
                relative,
                SnapshotEntry {
                    kind: SnapshotEntryKind::Directory,
                    mode,
                    uid,
                    gid,
                },
            );
        }
        let mut children = Vec::new();
        for entry in fs::read_dir(source)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if is_ignorable_snapshot_error(&error) => continue,
                Err(error) => return Err(error.into()),
            };
            children.push(entry);
        }
        children.sort_by_key(|left| left.file_name());
        for child in children {
            let child_source = child.path();
            let child_destination = destination.join(child.file_name());
            snapshot_tree_entry_with_state(root, &child_source, &child_destination, state)?;
        }
        fs::set_permissions(destination, metadata.permissions())?;
        return Ok(());
    }

    if !metadata.is_file() {
        return Ok(());
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let digest = copy_file_with_digest(source, destination)?;
    fs::set_permissions(destination, metadata.permissions())?;
    if let Some(relative) = relative {
        let mtime_ns = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64);
        state.insert(
            relative,
            SnapshotEntry {
                kind: SnapshotEntryKind::Regular {
                    digest,
                    size: metadata.len(),
                    mtime_ns,
                },
                mode,
                uid,
                gid,
            },
        );
    }
    Ok(())
}

struct LayerEntry {
    host_path: Option<PathBuf>,
    archive_path: PathBuf,
    mode: u32,
    uid: u64,
    gid: u64,
    size: u64,
    kind: LayerEntryKind,
}

enum LayerEntryKind {
    Regular,
    Directory,
    Symlink(PathBuf),
    Whiteout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SnapshotEntry {
    kind: SnapshotEntryKind,
    mode: u32,
    uid: u64,
    gid: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum SnapshotEntryKind {
    Regular {
        digest: String,
        size: u64,
        /// Modification time in nanoseconds since epoch.  Used as a heuristic
        /// to skip content hashing when metadata is unchanged.  Optional for
        /// backward compatibility with cached state that predates this field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mtime_ns: Option<i64>,
    },
    Directory,
    Symlink(PathBuf),
}

impl PartialEq for SnapshotEntryKind {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Regular {
                    digest: d1,
                    size: s1,
                    ..
                },
                Self::Regular {
                    digest: d2,
                    size: s2,
                    ..
                },
            ) => d1 == d2 && s1 == s2,
            (Self::Directory, Self::Directory) => true,
            (Self::Symlink(a), Self::Symlink(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for SnapshotEntryKind {}

const SNAPSHOT_STATE_MANIFEST_FILE: &str = ".boringbuilder-snapshot-state-v1.json";

impl SnapshotEntry {
    fn to_layer_entry(&self, root: &Path, relative: &Path) -> LayerEntry {
        let kind = match &self.kind {
            SnapshotEntryKind::Regular { .. } => LayerEntryKind::Regular,
            SnapshotEntryKind::Directory => LayerEntryKind::Directory,
            SnapshotEntryKind::Symlink(target) => LayerEntryKind::Symlink(target.clone()),
        };
        LayerEntry {
            host_path: match self.kind {
                SnapshotEntryKind::Regular { .. }
                | SnapshotEntryKind::Directory
                | SnapshotEntryKind::Symlink(_) => Some(root.join(relative)),
            },
            archive_path: relative.to_path_buf(),
            mode: self.mode,
            uid: self.uid,
            gid: self.gid,
            size: match self.kind {
                SnapshotEntryKind::Regular { size, .. } => size,
                SnapshotEntryKind::Directory | SnapshotEntryKind::Symlink(_) => 0,
            },
            kind,
        }
    }
}

fn collect_snapshot_state(root: &Path) -> Result<BTreeMap<PathBuf, SnapshotEntry>> {
    let mut state = BTreeMap::new();
    if !root.exists() {
        return Ok(state);
    }

    let mut entries = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !is_pseudo_fs(root, entry.path()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by(|left, right| left.path().cmp(right.path()));

    for entry in entries {
        let path = entry.path();
        if path == root {
            continue;
        }
        let relative = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        if relative == Path::new(SNAPSHOT_STATE_MANIFEST_FILE) {
            continue;
        }
        let metadata = fs::symlink_metadata(path)?;
        let mode = metadata.permissions().mode() & 0o7777;
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            SnapshotEntryKind::Symlink(fs::read_link(path)?)
        } else if file_type.is_dir() {
            SnapshotEntryKind::Directory
        } else if file_type.is_file() {
            let mtime_ns = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64);
            SnapshotEntryKind::Regular {
                digest: hash_file_contents(path)?,
                size: metadata.len(),
                mtime_ns,
            }
        } else {
            continue;
        };
        state.insert(
            relative,
            SnapshotEntry {
                kind,
                mode,
                uid: metadata.uid() as u64,
                gid: metadata.gid() as u64,
            },
        );
    }

    Ok(state)
}

fn snapshot_state_manifest_path(root: &Path) -> PathBuf {
    root.join(SNAPSHOT_STATE_MANIFEST_FILE)
}

const BUILD_CACHE_SNAPSHOT_STATE_DIR: &str = ".boringbuilder-build-cache-snapshots";

fn build_cache_snapshot_state_path(layout_dir: &Path, layer_count: usize) -> PathBuf {
    layout_dir
        .join(BUILD_CACHE_SNAPSHOT_STATE_DIR)
        .join(format!("{layer_count}.json"))
}

fn build_cache_snapshot_state(
    state: BTreeMap<PathBuf, SnapshotEntry>,
) -> Result<BuildCacheSnapshotState> {
    let bytes =
        serde_json::to_vec(&state).context("failed to serialize build-cache snapshot state")?;
    Ok(BuildCacheSnapshotState {
        descriptor: BuildCacheSnapshotStateDescriptor {
            digest: format!("sha256:{}", sha256_bytes(&bytes)),
            size: bytes.len() as u64,
        },
        state: Some(state),
    })
}

fn load_build_cache_snapshot_state_refs(
    config: &serde_json::Value,
) -> Result<BTreeMap<usize, BuildCacheSnapshotState>> {
    let descriptors: BTreeMap<usize, BuildCacheSnapshotStateDescriptor> = serde_json::from_value(
        config
            .pointer("/boringbuilder/build_cache/snapshot_state_blobs")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
    )
    .context("failed to parse build-cache snapshot-state blob descriptors")?;
    if !descriptors.is_empty() {
        return Ok(descriptors
            .into_iter()
            .map(|(layer_count, descriptor)| {
                (
                    layer_count,
                    BuildCacheSnapshotState {
                        descriptor,
                        state: None,
                    },
                )
            })
            .collect());
    }

    let inline_states: BTreeMap<usize, BTreeMap<PathBuf, SnapshotEntry>> = serde_json::from_value(
        config
            .pointer("/boringbuilder/build_cache/snapshot_states")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
    )
    .context("failed to parse legacy inline build-cache snapshot states")?;
    inline_states
        .into_iter()
        .map(|(layer_count, state)| Ok((layer_count, build_cache_snapshot_state(state)?)))
        .collect()
}

fn materialize_snapshot_state_blobs(
    layout_dir: &Path,
    snapshot_states: &BTreeMap<usize, BuildCacheSnapshotState>,
) -> Result<BTreeMap<usize, BuildCacheSnapshotStateDescriptor>> {
    let blobs_dir = layout_dir.join("blobs").join("sha256");
    fs::create_dir_all(&blobs_dir)
        .with_context(|| format!("failed to create {}", blobs_dir.display()))?;

    snapshot_states
        .iter()
        .map(|(layer_count, snapshot_state)| {
            if snapshot_state_blob_path(layout_dir, &snapshot_state.descriptor.digest).is_file() {
                return Ok((*layer_count, snapshot_state.descriptor.clone()));
            }

            let Some(state) = snapshot_state.state.as_ref() else {
                bail!(
                    "build-cache snapshot-state blob {} is missing from {}",
                    snapshot_state.descriptor.digest,
                    layout_dir.display()
                );
            };
            let bytes = serde_json::to_vec(state)
                .context("failed to serialize build-cache snapshot state blob")?;
            let digest = format!("sha256:{}", sha256_bytes(&bytes));
            if digest != snapshot_state.descriptor.digest {
                bail!(
                    "build-cache snapshot-state digest mismatch: expected {}, got {}",
                    snapshot_state.descriptor.digest,
                    digest
                );
            }
            let destination = snapshot_state_blob_path(layout_dir, &digest);
            fs::write(&destination, &bytes)
                .with_context(|| format!("failed to write {}", destination.display()))?;
            Ok((*layer_count, snapshot_state.descriptor.clone()))
        })
        .collect()
}

fn snapshot_state_blob_path(layout_dir: &Path, digest: &str) -> PathBuf {
    layout_dir
        .join("blobs")
        .join("sha256")
        .join(digest.trim_start_matches("sha256:"))
}

fn load_legacy_build_cache_snapshot_states(
    layout_dir: &Path,
) -> Result<BTreeMap<usize, BuildCacheSnapshotState>> {
    let directory = layout_dir.join(BUILD_CACHE_SNAPSHOT_STATE_DIR);
    if !directory.is_dir() {
        return Ok(BTreeMap::new());
    }
    let mut states = BTreeMap::new();
    for entry in fs::read_dir(&directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(layer_count) = stem.parse::<usize>() else {
            continue;
        };
        let state: BTreeMap<PathBuf, SnapshotEntry> = serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?,
        )
        .with_context(|| format!("failed to parse {}", path.display()))?;
        states.insert(layer_count, build_cache_snapshot_state(state)?);
    }
    Ok(states)
}

fn load_snapshot_state_blob(
    layout_dir: &Path,
    snapshot_state: &BuildCacheSnapshotState,
) -> Result<BTreeMap<PathBuf, SnapshotEntry>> {
    if let Some(state) = snapshot_state.state.as_ref() {
        return Ok(state.clone());
    }

    let path = resolve_layout_blob_path(layout_dir, &snapshot_state.descriptor.digest)?;
    let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    if snapshot_state.descriptor.size != bytes.len() as u64 {
        bail!(
            "snapshot-state blob {} expected {} bytes but found {}",
            snapshot_state.descriptor.digest,
            snapshot_state.descriptor.size,
            bytes.len()
        );
    }
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {}", path.display()))
}

fn load_snapshot_state_manifest(root: &Path) -> Result<Option<BTreeMap<PathBuf, SnapshotEntry>>> {
    let path = snapshot_state_manifest_path(root);
    if !path.is_file() {
        return Ok(None);
    }

    let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let state = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(state))
}

#[cfg(test)]
fn write_snapshot_state_manifest(
    root: &Path,
    state: &BTreeMap<PathBuf, SnapshotEntry>,
) -> Result<()> {
    let path = snapshot_state_manifest_path(root);
    fs::write(&path, serde_json::to_vec(state)?)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn has_removed_ancestor(path: &Path, removed_roots: &BTreeSet<PathBuf>) -> bool {
    let mut ancestor = path.parent();
    while let Some(parent) = ancestor {
        if parent.as_os_str().is_empty() {
            return false;
        }
        if removed_roots.contains(parent) {
            return true;
        }
        ancestor = parent.parent();
    }
    false
}

fn whiteout_entry(path: &Path) -> LayerEntry {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let archive_path = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join(format!(".wh.{name}"));
    LayerEntry {
        host_path: None,
        archive_path,
        mode: 0o644,
        uid: 0,
        gid: 0,
        size: 0,
        kind: LayerEntryKind::Whiteout,
    }
}

fn collect_entries(
    path: &Path,
    archive_prefix: &Path,
    entries: &mut Vec<LayerEntry>,
) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let walker = WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_pseudo_fs(path, e.path()));

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err)
                if err
                    .io_error()
                    .is_some_and(|e| e.kind() == io::ErrorKind::PermissionDenied) =>
            {
                // Skip directories/files we cannot read – mirrors Docker
                // behaviour which silently omits unreadable overlay entries.
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        let is_symlink = entry.file_type().is_symlink();
        let link_target = if is_symlink {
            Some(fs::read_link(entry.path())?)
        } else {
            None
        };
        let metadata = if is_symlink {
            // For symlinks use symlink_metadata so we don't follow the link.
            match fs::symlink_metadata(entry.path()) {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => continue,
                Err(e) => return Err(e.into()),
            }
        } else {
            match entry.metadata() {
                Ok(m) => m,
                Err(err)
                    if err
                        .io_error()
                        .is_some_and(|e| e.kind() == io::ErrorKind::PermissionDenied) =>
                {
                    continue;
                }
                Err(err) => return Err(err.into()),
            }
        };
        let relative = entry.path().strip_prefix(path).unwrap_or(entry.path());
        let archive_path = if relative.as_os_str().is_empty() {
            // Root of the walked tree.  When archive_prefix is non-empty this
            // is a meaningful directory entry (e.g. "workspace"), but when
            // archive_prefix is empty (output is "/") the tar crate rejects
            // an empty path.  Skip it – the root directory is implicit.
            if archive_prefix.as_os_str().is_empty() {
                continue;
            }
            archive_prefix.to_path_buf()
        } else {
            archive_prefix.join(relative)
        };
        entries.push(LayerEntry {
            host_path: Some(entry.path().to_path_buf()),
            archive_path,
            mode: metadata.permissions().mode() & 0o7777,
            uid: metadata.uid() as u64,
            gid: metadata.gid() as u64,
            size: metadata.len(),
            kind: if let Some(target) = link_target {
                LayerEntryKind::Symlink(target)
            } else if metadata.is_dir() {
                LayerEntryKind::Directory
            } else if metadata.file_type().is_file() {
                LayerEntryKind::Regular
            } else {
                continue;
            },
        });
    }

    Ok(())
}

fn collect_overlay_entries(
    path: &Path,
    archive_prefix: &Path,
    entries: &mut Vec<LayerEntry>,
) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let walker = WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_pseudo_fs(path, e.path()));

    for entry in walker {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        let relative = entry.path().strip_prefix(path).unwrap_or(entry.path());
        if relative.as_os_str().is_empty() {
            continue;
        }
        let archive_path = archive_prefix.join(relative);
        let mode = metadata.permissions().mode() & 0o7777;

        if metadata.file_type().is_char_device() && metadata.rdev() == 0 {
            let Some(name) = archive_path.file_name() else {
                continue;
            };
            let whiteout = archive_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default()
                .join(format!(".wh.{}", name.to_string_lossy()));
            entries.push(LayerEntry {
                host_path: None,
                archive_path: whiteout,
                mode: 0o644,
                uid: 0,
                gid: 0,
                size: 0,
                kind: LayerEntryKind::Whiteout,
            });
            continue;
        }

        if metadata.is_dir() && dir_is_overlay_opaque(entry.path())? {
            entries.push(LayerEntry {
                host_path: None,
                archive_path: archive_path.join(".wh..wh..opq"),
                mode: 0o644,
                uid: 0,
                gid: 0,
                size: 0,
                kind: LayerEntryKind::Whiteout,
            });
        }

        let kind = if entry.file_type().is_symlink() {
            LayerEntryKind::Symlink(fs::read_link(entry.path())?)
        } else if metadata.is_dir() {
            LayerEntryKind::Directory
        } else if metadata.file_type().is_file() {
            LayerEntryKind::Regular
        } else {
            continue;
        };
        entries.push(LayerEntry {
            host_path: Some(entry.path().to_path_buf()),
            archive_path,
            mode,
            uid: metadata.uid() as u64,
            gid: metadata.gid() as u64,
            size: metadata.len(),
            kind,
        });
    }

    Ok(())
}

pub(crate) use crate::util::hashing::HashingWriter;

fn finish_gzip(gz: flate2::write::GzEncoder<HashingWriter<File>>) -> Result<HashingWriter<File>> {
    Ok(gz.finish()?)
}

fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn hash_file_contents(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
fn copy_file_with_digest(source: &Path, destination: &Path) -> Result<String> {
    let mut input =
        File::open(source).with_context(|| format!("failed to open {}", source.display()))?;
    let mut output = File::create(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
fn is_ignorable_snapshot_error(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(6))
}

fn dir_is_overlay_opaque(path: &Path) -> Result<bool> {
    let value = xattr::get(path, "trusted.overlay.opaque")
        .with_context(|| format!("failed to read overlay opaque xattr for {}", path.display()))?;
    Ok(matches!(value.as_deref(), Some(b"y")))
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use tempfile::tempdir;

    use super::*;
    use crate::util::fs::copy_tree;

    fn create_fifo(path: &Path) {
        let status = Command::new("mkfifo").arg(path).status().unwrap();
        assert!(
            status.success(),
            "mkfifo should succeed for {}",
            path.display()
        );
    }

    #[test]
    fn collect_entries_skips_unreadable_dirs() {
        // Root can read anything regardless of permissions, so this test only
        // exercises the permission-denied skip path when running as non-root.
        if nix::unistd::Uid::effective().is_root() {
            eprintln!("skipping: root bypasses permission checks");
            return;
        }

        let tmp = tempdir().unwrap();
        let root = tmp.path().join("rootfs");
        fs::create_dir_all(root.join("readable/child")).unwrap();
        fs::write(root.join("readable/child/file.txt"), "ok").unwrap();
        fs::create_dir_all(root.join("noperm")).unwrap();
        fs::write(root.join("noperm/secret.txt"), "hidden").unwrap();
        // Remove read+execute so WalkDir cannot enter the directory.
        fs::set_permissions(root.join("noperm"), fs::Permissions::from_mode(0o000)).unwrap();

        let mut entries = Vec::new();
        let prefix = PathBuf::from("out");
        collect_entries(&root, &prefix, &mut entries).unwrap();

        let paths: Vec<String> = entries
            .iter()
            .map(|e| e.archive_path.to_string_lossy().to_string())
            .collect();
        assert!(
            paths.iter().any(|p| p.contains("file.txt")),
            "readable file should be collected: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.contains("secret.txt")),
            "unreadable dir contents should be skipped: {paths:?}"
        );

        // Restore permissions for cleanup.
        fs::set_permissions(root.join("noperm"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn collect_entries_skips_root_when_prefix_empty() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("rootfs");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/hello"), "world").unwrap();

        let mut entries = Vec::new();
        collect_entries(&root, Path::new(""), &mut entries).unwrap();

        let paths: Vec<String> = entries
            .iter()
            .map(|e| e.archive_path.to_string_lossy().to_string())
            .collect();
        // The root entry (empty path) must not appear.
        assert!(
            !paths.contains(&String::new()),
            "empty archive path should be skipped: {paths:?}"
        );
        // But children should still be present.
        assert!(
            paths.iter().any(|p| p.contains("hello")),
            "child entries should be present: {paths:?}"
        );
    }

    #[test]
    fn collect_entries_skips_pseudo_fs() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("rootfs");
        fs::create_dir_all(root.join("proc/1")).unwrap();
        fs::create_dir_all(root.join("sys/kernel")).unwrap();
        fs::create_dir_all(root.join("dev/pts")).unwrap();
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        fs::write(root.join("usr/bin/true"), "").unwrap();

        let mut entries = Vec::new();
        collect_entries(&root, Path::new(""), &mut entries).unwrap();

        let paths: Vec<String> = entries
            .iter()
            .map(|e| e.archive_path.to_string_lossy().to_string())
            .collect();
        assert!(
            !paths.iter().any(|p| p.starts_with("proc")),
            "proc should be excluded: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.starts_with("sys")),
            "sys should be excluded: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.starts_with("dev")),
            "dev should be excluded: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.contains("true")),
            "usr/bin/true should be present: {paths:?}"
        );
    }

    #[test]
    fn collect_entries_and_snapshots_skip_special_files() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("rootfs");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/hello"), "world").unwrap();
        create_fifo(&root.join("bin/cache.pipe"));

        let mut entries = Vec::new();
        collect_entries(&root, Path::new(""), &mut entries).unwrap();
        let paths: Vec<String> = entries
            .iter()
            .map(|e| e.archive_path.to_string_lossy().to_string())
            .collect();
        assert!(
            paths.iter().any(|path| path == "bin/hello"),
            "regular files should still be exported: {paths:?}"
        );
        assert!(
            !paths.iter().any(|path| path == "bin/cache.pipe"),
            "special files should be skipped from exports: {paths:?}"
        );

        let snapshot = collect_snapshot_state(&root).unwrap();
        assert!(
            snapshot.contains_key(Path::new("bin/hello")),
            "regular files should still be snapshotted: {snapshot:?}"
        );
        assert!(
            !snapshot.contains_key(Path::new("bin/cache.pipe")),
            "special files should be skipped from snapshot state: {snapshot:?}"
        );
    }

    #[test]
    fn snapshot_tree_with_state_writes_manifest_and_preserves_symlinks() {
        let tmp = tempdir().unwrap();
        let source = tmp.path().join("source");
        let destination = tmp.path().join("snapshot");
        fs::create_dir_all(source.join("bin")).unwrap();
        fs::write(source.join("bin/hello"), "world").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("hello", source.join("bin/hello-link")).unwrap();
        create_fifo(&source.join("bin/cache.pipe"));

        snapshot_tree_with_state(&source, &destination).unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("bin/hello")).unwrap(),
            "world"
        );
        assert!(
            fs::symlink_metadata(destination.join("bin/hello-link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!destination.join("bin/cache.pipe").exists());

        let state = load_snapshot_state_manifest(&destination)
            .unwrap()
            .expect("snapshot state manifest should exist");
        assert!(state.contains_key(Path::new("bin")));
        assert!(state.contains_key(Path::new("bin/hello")));
        assert!(state.contains_key(Path::new("bin/hello-link")));
        assert!(!state.contains_key(Path::new("bin/cache.pipe")));
    }

    #[test]
    fn snapshot_tree_with_state_copies_files_from_read_only_directory() {
        let tmp = tempdir().unwrap();
        let source = tmp.path().join("source");
        let destination = tmp.path().join("snapshot");
        let read_only_dir = source.join("go/pkg/mod/example@v1.0.0");
        fs::create_dir_all(&read_only_dir).unwrap();
        fs::write(read_only_dir.join("Makefile"), "all:\n\t@echo ok\n").unwrap();

        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&read_only_dir).unwrap().permissions();
            permissions.set_mode(0o555);
            fs::set_permissions(&read_only_dir, permissions).unwrap();
        }

        snapshot_tree_with_state(&source, &destination).unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("go/pkg/mod/example@v1.0.0/Makefile")).unwrap(),
            "all:\n\t@echo ok\n"
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(destination.join("go/pkg/mod/example@v1.0.0"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o555
        );
    }

    #[test]
    fn snapshot_tree_state_only_writes_manifest_without_copying_files() {
        let tmp = tempdir().unwrap();
        let source = tmp.path().join("source");
        let destination = tmp.path().join("snapshot-state");
        fs::create_dir_all(source.join("bin")).unwrap();
        fs::write(source.join("bin/hello"), "world").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("hello", source.join("bin/hello-link")).unwrap();
        create_fifo(&source.join("bin/cache.pipe"));

        snapshot_tree_state_only(&source, &destination).unwrap();

        assert!(!destination.join("bin/hello").exists());
        assert!(!destination.join("bin/hello-link").exists());
        assert!(!destination.join("bin/cache.pipe").exists());

        let state = load_snapshot_state_manifest(&destination)
            .unwrap()
            .expect("snapshot state manifest should exist");
        assert!(state.contains_key(Path::new("bin")));
        assert!(state.contains_key(Path::new("bin/hello")));
        assert!(state.contains_key(Path::new("bin/hello-link")));
        assert!(!state.contains_key(Path::new("bin/cache.pipe")));
    }

    #[test]
    fn create_snapshot_delta_skips_special_files() {
        let tmp = tempdir().unwrap();
        let snapshot = tmp.path().join("snapshot");
        fs::create_dir_all(snapshot.join("bin")).unwrap();
        fs::write(snapshot.join("bin/hello"), "world").unwrap();
        create_fifo(&snapshot.join("bin/cache.pipe"));

        let layer = create_oci_layer_from_fs_delta(None, &snapshot).unwrap();
        let file = File::open(layer.temp_file.path()).unwrap();
        let decoder = zstd::Decoder::new(file).unwrap();
        let mut archive = tar::Archive::new(decoder);
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(
            names.iter().any(|name| name == "bin/hello"),
            "regular files should still be present: {names:?}"
        );
        assert!(
            !names.iter().any(|name| name == "bin/cache.pipe"),
            "special files should not be archived into OCI layers: {names:?}"
        );
    }

    #[test]
    fn create_snapshot_delta_archives_symlinks_with_long_targets() {
        let tmp = tempdir().unwrap();
        let snapshot = tmp.path().join("snapshot");
        fs::create_dir_all(&snapshot).unwrap();
        let long_target = format!("targets/{}", "nested/".repeat(40));
        #[cfg(unix)]
        std::os::unix::fs::symlink(&long_target, snapshot.join("current")).unwrap();

        let layer = create_oci_layer_from_fs_delta(None, &snapshot).unwrap();
        let file = File::open(layer.temp_file.path()).unwrap();
        let decoder = zstd::Decoder::new(file).unwrap();
        let mut archive = tar::Archive::new(decoder);
        let mut symlink_target = None;
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            if entry.path().unwrap() == Path::new("current") {
                symlink_target = entry
                    .link_name()
                    .unwrap()
                    .map(|target| target.to_string_lossy().to_string());
                break;
            }
        }

        assert_eq!(symlink_target.as_deref(), Some(long_target.as_str()));
    }

    #[test]
    fn create_layer_produces_valid_gzip() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("app");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("main.rs"), "fn main() {}").unwrap();

        let paths = vec![ExportPath {
            host_path: dir,
            container_prefix: PathBuf::from("app"),
        }];

        let (temp_file, compressed_digest, diff_id, compressed_size) =
            create_layer_from_paths(&paths).unwrap();

        assert!(!compressed_digest.is_empty());
        assert!(!diff_id.is_empty());
        assert!(compressed_size > 0);

        // Verify the file is valid gzip containing a tar with our file.
        let f = File::open(temp_file.path()).unwrap();
        let gz = flate2::read::GzDecoder::new(f);
        let mut archive = tar::Archive::new(gz);
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n.contains("main.rs")),
            "tar should contain main.rs: {names:?}"
        );
    }

    #[test]
    fn build_cache_layout_restores_prefix_layers() {
        let tmp = tempdir().unwrap();
        let snapshot1 = tmp.path().join("snapshot1");
        let snapshot2 = tmp.path().join("snapshot2");
        let layout = tmp.path().join("layout");
        let restored_prefix = tmp.path().join("restored-prefix");
        let restored_full = tmp.path().join("restored-full");

        fs::create_dir_all(snapshot1.join("bin")).unwrap();
        fs::write(snapshot1.join("bin/app"), "one").unwrap();
        copy_tree(&snapshot1, &snapshot2).unwrap();
        fs::write(snapshot2.join("bin/app"), "two").unwrap();
        fs::write(snapshot2.join("bin/helper"), "helper").unwrap();

        let first_layer_count = append_build_cache_layer_to_layout(
            &layout,
            "linux/amd64",
            BuildCacheStepRecord {
                operation_index: 0,
                key: "step-1".to_string(),
            },
            create_oci_layer_from_fs_delta(None, &snapshot1).unwrap(),
        )
        .unwrap();
        store_build_cache_snapshot_state(&layout, first_layer_count, &snapshot1).unwrap();
        let second_layer_count = append_build_cache_layer_to_layout(
            &layout,
            "linux/amd64",
            BuildCacheStepRecord {
                operation_index: 1,
                key: "step-2".to_string(),
            },
            create_oci_layer_from_fs_delta(Some(&snapshot1), &snapshot2).unwrap(),
        )
        .unwrap();
        store_build_cache_snapshot_state(&layout, second_layer_count, &snapshot2).unwrap();

        let loaded = load_build_cache_layout(&layout, "linux/amd64")
            .unwrap()
            .expect("layout should exist");
        assert_eq!(loaded.steps.len(), 2);
        assert_eq!(loaded.layers.len(), 2);

        fs::create_dir_all(&restored_prefix).unwrap();
        unpack_rootfs_from_build_cache_layout(&layout, "linux/amd64", &restored_prefix, 1).unwrap();
        assert_eq!(
            fs::read_to_string(restored_prefix.join("bin/app")).unwrap(),
            "one"
        );
        assert!(!restored_prefix.join("bin/helper").exists());

        fs::create_dir_all(&restored_full).unwrap();
        unpack_rootfs_from_build_cache_layout(&layout, "linux/amd64", &restored_full, 2).unwrap();
        assert_eq!(
            fs::read_to_string(restored_full.join("bin/app")).unwrap(),
            "two"
        );
        assert_eq!(
            fs::read_to_string(restored_full.join("bin/helper")).unwrap(),
            "helper"
        );
    }

    #[test]
    fn build_cache_snapshot_state_round_trips_and_truncates() {
        let tmp = tempdir().unwrap();
        let source1 = tmp.path().join("source1");
        let source2 = tmp.path().join("source2");
        let snapshot1 = tmp.path().join("snapshot1");
        let snapshot2 = tmp.path().join("snapshot2");
        let layout = tmp.path().join("layout");
        let restored = tmp.path().join("restored");

        fs::create_dir_all(source1.join("bin")).unwrap();
        fs::write(source1.join("bin/app"), "one").unwrap();
        copy_tree(&source1, &source2).unwrap();
        fs::write(source2.join("bin/app"), "two").unwrap();
        fs::write(source2.join("bin/helper"), "helper").unwrap();

        snapshot_tree_with_state(&source1, &snapshot1).unwrap();
        snapshot_tree_with_state(&source2, &snapshot2).unwrap();

        let first_layer_count = append_build_cache_layer_to_layout(
            &layout,
            "linux/amd64",
            BuildCacheStepRecord {
                operation_index: 0,
                key: "step-1".to_string(),
            },
            create_oci_layer_from_fs_delta(None, &snapshot1).unwrap(),
        )
        .unwrap();
        store_build_cache_snapshot_state(&layout, first_layer_count, &snapshot1).unwrap();
        let second_layer_count = append_build_cache_layer_to_layout(
            &layout,
            "linux/amd64",
            BuildCacheStepRecord {
                operation_index: 1,
                key: "step-2".to_string(),
            },
            create_oci_layer_from_fs_delta(Some(&snapshot1), &snapshot2).unwrap(),
        )
        .unwrap();
        store_build_cache_snapshot_state(&layout, second_layer_count, &snapshot2).unwrap();
        assert!(
            !layout.join(BUILD_CACHE_SNAPSHOT_STATE_DIR).exists(),
            "snapshot states should live in OCI metadata, not top-level sidecar dirs"
        );

        assert!(restore_build_cache_snapshot_state(&layout, 2, &restored).unwrap());
        assert!(
            restored
                .join(".boringbuilder-snapshot-state-v1.json")
                .is_file()
        );
        assert!(!restored.join("bin/app").exists());

        truncate_build_cache_layout(&layout, "linux/amd64", 1).unwrap();
        assert!(!restore_build_cache_snapshot_state(&layout, 2, &restored).unwrap());
        assert!(restore_build_cache_snapshot_state(&layout, 1, &restored).unwrap());
    }

    #[test]
    fn build_cache_snapshot_state_uses_blob_descriptors_in_config() {
        let tmp = tempdir().unwrap();
        let source = tmp.path().join("source");
        let snapshot = tmp.path().join("snapshot");
        let layout = tmp.path().join("layout");

        fs::create_dir_all(source.join("bin")).unwrap();
        fs::write(source.join("bin/app"), "hello").unwrap();

        snapshot_tree_with_state(&source, &snapshot).unwrap();
        let layer_count = append_build_cache_layer_to_layout(
            &layout,
            "linux/amd64",
            BuildCacheStepRecord {
                operation_index: 0,
                key: "step-1".to_string(),
            },
            create_oci_layer_from_fs_delta(None, &snapshot).unwrap(),
        )
        .unwrap();
        store_build_cache_snapshot_state(&layout, layer_count, &snapshot).unwrap();

        let index: serde_json::Value =
            serde_json::from_slice(&fs::read(layout.join("index.json")).unwrap()).unwrap();
        let manifest_digest = index["manifests"][0]["digest"]
            .as_str()
            .unwrap()
            .trim_start_matches("sha256:");
        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(layout.join("blobs/sha256").join(manifest_digest)).unwrap(),
        )
        .unwrap();
        let config_digest = manifest["config"]["digest"]
            .as_str()
            .unwrap()
            .trim_start_matches("sha256:");
        let config_bytes = fs::read(layout.join("blobs/sha256").join(config_digest)).unwrap();
        let config: serde_json::Value = serde_json::from_slice(&config_bytes).unwrap();

        assert!(
            config["boringbuilder"]["build_cache"]["snapshot_states"].is_null(),
            "snapshot state should not be embedded inline in the OCI config"
        );
        let descriptor = &config["boringbuilder"]["build_cache"]["snapshot_state_blobs"]["1"];
        let digest = descriptor["digest"].as_str().unwrap();
        assert!(
            !String::from_utf8_lossy(&config_bytes).contains("bin/app"),
            "snapshot state paths should live in separate blobs, not the OCI config"
        );

        let blob_bytes = fs::read(
            layout
                .join("blobs/sha256")
                .join(digest.trim_start_matches("sha256:")),
        )
        .unwrap();
        assert_eq!(
            descriptor["size"].as_u64().unwrap(),
            blob_bytes.len() as u64
        );
        let state: BTreeMap<PathBuf, SnapshotEntry> = serde_json::from_slice(&blob_bytes).unwrap();
        assert!(state.contains_key(Path::new("bin/app")));
        assert_eq!(
            load_build_cache_snapshot_state(&layout, 1)
                .unwrap()
                .unwrap(),
            state
        );
    }
}
