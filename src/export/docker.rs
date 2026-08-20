use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use flate2::read::GzDecoder;

use crate::export::oci::{export_pipeline_oci, export_pipeline_oci_from_overlay_upper};
use crate::schema::Pipeline;

pub fn export_pipeline_docker(
    pipeline: &Pipeline,
    output_path: &Path,
    base_image_dir: &Path,
    rootfs_dir: &Path,
) -> Result<PathBuf> {
    let staging = tempfile::Builder::new()
        .prefix("boringbuilder-docker-export-")
        .tempdir()
        .context("failed to create staging directory for Docker export")?;

    let oci_dir = staging.path().join("oci");
    fs::create_dir_all(&oci_dir)?;
    export_pipeline_oci(pipeline, &oci_dir, base_image_dir, rootfs_dir)?;
    export_oci_layout_as_docker(&oci_dir, output_path)
}

pub fn export_pipeline_docker_from_overlay_upper(
    pipeline: &Pipeline,
    output_path: &Path,
    base_image_dir: &Path,
    upper_dir: &Path,
) -> Result<PathBuf> {
    let staging = tempfile::Builder::new()
        .prefix("boringbuilder-docker-export-")
        .tempdir()
        .context("failed to create staging directory for Docker export")?;

    let oci_dir = staging.path().join("oci");
    fs::create_dir_all(&oci_dir)?;
    export_pipeline_oci_from_overlay_upper(pipeline, &oci_dir, base_image_dir, upper_dir)?;
    export_oci_layout_as_docker(&oci_dir, output_path)
}

pub fn export_oci_layout_as_docker(oci_dir: &Path, output_path: &Path) -> Result<PathBuf> {
    let index: serde_json::Value =
        serde_json::from_reader(File::open(oci_dir.join("index.json"))?)?;
    let blobs_dir = oci_dir.join("blobs/sha256");
    let manifest_hex = resolve_manifest_hex(&blobs_dir, &index)?;
    let manifest: serde_json::Value =
        serde_json::from_reader(File::open(blobs_dir.join(&manifest_hex))?)?;

    let config_hex = manifest["config"]["digest"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("manifest missing config digest"))?
        .strip_prefix("sha256:")
        .unwrap_or("");

    let config: serde_json::Value =
        serde_json::from_reader(File::open(oci_dir.join("blobs/sha256").join(config_hex))?)?;
    let diff_ids = config
        .pointer("/rootfs/diff_ids")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("config missing rootfs.diff_ids"))?;

    let oci_layers = manifest["layers"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("manifest missing layers"))?;

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let file = File::create(output_path)
        .with_context(|| format!("failed to create {}", output_path.display()))?;
    let mut builder = tar::Builder::new(file);
    builder.follow_symlinks(false);

    let oci_layout = br#"{"imageLayoutVersion":"1.0.0"}"#;
    append_bytes(&mut builder, "oci-layout", oci_layout)?;

    let config_bytes = fs::read(blobs_dir.join(config_hex))?;
    append_dir(&mut builder, "blobs")?;
    append_dir(&mut builder, "blobs/sha256")?;
    append_bytes(
        &mut builder,
        &format!("blobs/sha256/{config_hex}"),
        &config_bytes,
    )?;

    let mut layer_paths = Vec::new();
    for (i, layer) in oci_layers.iter().enumerate() {
        let compressed_hex = layer["digest"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("layer missing digest"))?
            .strip_prefix("sha256:")
            .unwrap_or("");
        let diff_id_hex = diff_ids
            .get(i)
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing diff_id for layer {i}"))?
            .strip_prefix("sha256:")
            .unwrap_or("");

        let blob_path = blobs_dir.join(compressed_hex);
        let blob_file = File::open(&blob_path)?;

        let layer_data = if compressed_hex == diff_id_hex {
            let mut data = Vec::new();
            File::open(&blob_path)?.read_to_end(&mut data)?;
            data
        } else {
            let mut decoder = GzDecoder::new(blob_file);
            let mut data = Vec::new();
            decoder.read_to_end(&mut data)?;
            data
        };

        let tar_path = format!("blobs/sha256/{diff_id_hex}");
        append_bytes(&mut builder, &tar_path, &layer_data)?;
        layer_paths.push(tar_path);
    }

    let manifest_bytes = fs::read(blobs_dir.join(&manifest_hex))?;
    append_bytes(
        &mut builder,
        &format!("blobs/sha256/{manifest_hex}"),
        &manifest_bytes,
    )?;

    let docker_manifest = serde_json::json!([{
        "Config": format!("blobs/sha256/{config_hex}"),
        "RepoTags": serde_json::Value::Null,
        "Layers": layer_paths
    }]);
    let docker_manifest_bytes = serde_json::to_vec_pretty(&docker_manifest)?;
    append_bytes(&mut builder, "manifest.json", &docker_manifest_bytes)?;

    let index_bytes = fs::read(oci_dir.join("index.json"))?;
    append_bytes(&mut builder, "index.json", &index_bytes)?;

    builder.into_inner()?;
    Ok(output_path.to_path_buf())
}

fn resolve_manifest_hex(blobs_dir: &Path, index: &serde_json::Value) -> Result<String> {
    let manifest = index["manifests"]
        .as_array()
        .and_then(|manifests| manifests.first())
        .ok_or_else(|| anyhow::anyhow!("index.json missing manifests"))?;
    let digest = manifest["digest"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("index.json missing manifest digest"))?;
    let digest_hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    let media_type = manifest["mediaType"].as_str().unwrap_or_default();

    if media_type.contains("image.manifest") {
        return Ok(digest_hex.to_string());
    }

    if media_type.contains("image.index") {
        let nested: serde_json::Value =
            serde_json::from_reader(File::open(blobs_dir.join(digest_hex))?)?;
        return resolve_manifest_hex(blobs_dir, &nested);
    }

    let candidate: serde_json::Value =
        serde_json::from_reader(File::open(blobs_dir.join(digest_hex))?)?;
    if candidate.get("config").is_some() && candidate.get("layers").is_some() {
        return Ok(digest_hex.to_string());
    }
    if candidate.get("manifests").is_some() {
        return resolve_manifest_hex(blobs_dir, &candidate);
    }

    anyhow::bail!("unsupported OCI descriptor media type: {media_type}")
}

fn append_dir(builder: &mut tar::Builder<File>, name: &str) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_mode(0o755);
    header.set_entry_type(tar::EntryType::Directory);
    header.set_size(0);
    header.set_cksum();
    builder.append_data(&mut header, name, io::empty())?;
    Ok(())
}

fn append_bytes(builder: &mut tar::Builder<File>, name: &str, data: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(data.len() as u64);
    header.set_cksum();
    builder.append_data(&mut header, name, data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::resolve_manifest_hex;

    #[test]
    fn resolves_nested_oci_index_to_manifest() {
        let temp = tempdir().unwrap();
        let blobs = temp.path().join("blobs/sha256");
        fs::create_dir_all(&blobs).unwrap();

        let manifest_hex = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let nested_index_hex = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

        fs::write(
            blobs.join(nested_index_hex),
            serde_json::json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.index.v1+json",
                "manifests": [{
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": format!("sha256:{manifest_hex}"),
                    "size": 123
                }]
            })
            .to_string(),
        )
        .unwrap();

        let top_index = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [{
                "mediaType": "application/vnd.oci.image.index.v1+json",
                "digest": format!("sha256:{nested_index_hex}"),
                "size": 456
            }]
        });

        let resolved = resolve_manifest_hex(&blobs, &top_index).unwrap();
        assert_eq!(resolved, manifest_hex);
    }
}
