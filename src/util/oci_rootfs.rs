use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use serde::Deserialize;
use tar::{Archive, EntryType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Plain,
    Gzip,
    Zstd,
}

#[derive(Debug, Deserialize)]
struct DirManifest {
    layers: Vec<LayerDescriptor>,
}

#[derive(Debug, Deserialize)]
struct LayerDescriptor {
    #[serde(rename = "mediaType")]
    media_type: Option<String>,
    digest: String,
}

#[derive(Debug)]
struct HardLinkEntry {
    path: PathBuf,
    target: PathBuf,
}

pub fn unpack_rootfs_from_dir_manifest(image_dir: &Path, rootfs_dir: &Path) -> Result<()> {
    let manifest_path = image_dir.join("manifest.json");
    let manifest: DirManifest = serde_json::from_reader(
        File::open(&manifest_path)
            .with_context(|| format!("failed to open {}", manifest_path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", manifest_path.display()))?;

    for layer in manifest.layers {
        let layer_path = resolve_layer_path(image_dir, &layer.digest)?;
        unpack_layer(&layer_path, rootfs_dir, layer.media_type.as_deref())?;
    }

    Ok(())
}

pub fn unpack_rootfs_from_oci_layout(
    layout_dir: &Path,
    platform: &str,
    rootfs_dir: &Path,
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
        .ok_or_else(|| anyhow!("index.json missing manifests"))?;

    let manifest = load_oci_manifest(layout_dir, descriptor, platform)?;
    let layers = manifest["layers"]
        .as_array()
        .ok_or_else(|| anyhow!("manifest missing layers"))?;

    for layer in layers {
        let digest = layer["digest"]
            .as_str()
            .ok_or_else(|| anyhow!("layer missing digest"))?;
        let media_type = layer["mediaType"].as_str();
        let layer_path = resolve_layout_blob_path(layout_dir, digest)?;
        unpack_layer(&layer_path, rootfs_dir, media_type)?;
    }

    Ok(())
}

pub fn unpack_layer(layer_path: &Path, rootfs_dir: &Path, media_type: Option<&str>) -> Result<()> {
    let file = File::open(layer_path)
        .with_context(|| format!("failed to open layer {}", layer_path.display()))?;

    let compression = compression_for(layer_path, media_type)?;
    match compression {
        Compression::Gzip => unpack_archive(Archive::new(GzDecoder::new(file)), rootfs_dir),
        Compression::Zstd => unpack_archive(Archive::new(zstd::Decoder::new(file)?), rootfs_dir),
        Compression::Plain => unpack_archive(Archive::new(file), rootfs_dir),
    }
}

pub fn normalize_archive_path(path: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => bail!("layer contained parent directory traversal"),
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

pub fn compression_for(path: &Path, media_type: Option<&str>) -> Result<Compression> {
    if let Some(media_type) = media_type {
        if media_type.contains("zstd") {
            return Ok(Compression::Zstd);
        }
        if media_type.contains("gzip") {
            return Ok(Compression::Gzip);
        }
        if media_type.ends_with(".tar") || media_type.contains("layer.v1.tar") {
            return Ok(Compression::Plain);
        }
    }

    let mut file = File::open(path)?;
    let mut header = [0u8; 4];
    let read = file.read(&mut header)?;
    if read >= 4 && header == [0x28, 0xB5, 0x2F, 0xFD] {
        return Ok(Compression::Zstd);
    }
    if read >= 2 && header[0] == 0x1F && header[1] == 0x8B {
        return Ok(Compression::Gzip);
    }

    Ok(Compression::Plain)
}

fn resolve_layer_path(image_dir: &Path, digest: &str) -> Result<PathBuf> {
    let direct = image_dir.join(digest);
    if direct.exists() {
        return Ok(direct);
    }

    let digest_only = digest.split(':').nth(1).unwrap_or(digest);
    let alternate = image_dir.join(digest_only);
    if alternate.exists() {
        return Ok(alternate);
    }

    bail!(
        "could not find layer blob for digest {} in {}",
        digest,
        image_dir.display()
    )
}

fn resolve_layout_blob_path(layout_dir: &Path, digest: &str) -> Result<PathBuf> {
    let digest_only = digest.split(':').nth(1).unwrap_or(digest);
    let path = layout_dir.join("blobs/sha256").join(digest_only);
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

fn load_oci_manifest(
    layout_dir: &Path,
    descriptor: &serde_json::Value,
    platform: &str,
) -> Result<serde_json::Value> {
    let media_type = descriptor["mediaType"].as_str().unwrap_or("");
    let digest = descriptor["digest"]
        .as_str()
        .ok_or_else(|| anyhow!("descriptor missing digest"))?;
    let descriptor_path = resolve_layout_blob_path(layout_dir, digest)?;
    let value: serde_json::Value = serde_json::from_reader(
        File::open(&descriptor_path)
            .with_context(|| format!("failed to open {}", descriptor_path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", descriptor_path.display()))?;

    if media_type.contains("manifest.v1+json") || value.get("layers").is_some() {
        return Ok(value);
    }

    let (os, arch) = platform
        .split_once('/')
        .ok_or_else(|| anyhow!("invalid platform '{}'", platform))?;
    let manifests = value["manifests"]
        .as_array()
        .ok_or_else(|| anyhow!("manifest list missing manifests"))?;
    for manifest in manifests {
        let manifest_os = manifest["platform"]["os"].as_str().unwrap_or("");
        let manifest_arch = manifest["platform"]["architecture"].as_str().unwrap_or("");
        if manifest_os == os && manifest_arch == arch {
            return load_oci_manifest(layout_dir, manifest, platform);
        }
    }

    Err(anyhow!(
        "no OCI manifest found for platform {} in {}",
        platform,
        layout_dir.display()
    ))
}

fn unpack_archive<R: Read>(mut archive: Archive<R>, rootfs_dir: &Path) -> Result<()> {
    let mut pending_links = Vec::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = normalize_archive_path(entry.path()?.as_ref())?;
        if path.as_os_str().is_empty() {
            continue;
        }

        let filename = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        if filename == ".wh..wh..opq" {
            let dir = rootfs_dir.join(path.parent().unwrap_or_else(|| Path::new("")));
            remove_dir_children(&dir)?;
            continue;
        }
        if let Some(target_name) = filename.strip_prefix(".wh.") {
            let target = rootfs_dir
                .join(path.parent().unwrap_or_else(|| Path::new("")))
                .join(target_name);
            remove_path(&target)?;
            continue;
        }

        let destination = rootfs_dir.join(&path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        if destination.exists() && entry.header().entry_type() == EntryType::Directory {
            continue;
        }

        match entry.header().entry_type() {
            EntryType::Directory => {
                fs::create_dir_all(&destination)?;
            }
            EntryType::Link => {
                let Some(target) = entry.link_name()? else {
                    bail!("hard link entry missing target: {}", path.display());
                };
                pending_links.push(HardLinkEntry {
                    path: path.clone(),
                    target: normalize_archive_path(&target)?,
                });
            }
            _ => {
                if destination.exists() && entry.header().entry_type() != EntryType::Directory {
                    remove_path(&destination)?;
                }
                entry.unpack(&destination).with_context(|| {
                    format!(
                        "failed to unpack {} into {}",
                        path.display(),
                        destination.display()
                    )
                })?;
            }
        }
    }

    for link in pending_links {
        let destination = rootfs_dir.join(&link.path);
        let target = rootfs_dir.join(&link.target);
        if !target.exists() {
            bail!(
                "hard link target {} for {} does not exist after unpack",
                link.target.display(),
                link.path.display()
            );
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        if destination.exists() {
            remove_path(&destination)?;
        }
        fs::hard_link(&target, &destination).with_context(|| {
            format!(
                "failed to create hard link {} -> {}",
                destination.display(),
                target.display()
            )
        })?;
    }

    Ok(())
}

fn remove_dir_children(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        remove_path(&entry.path())?;
    }

    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    if !path.exists() && !path.is_symlink() {
        return Ok(());
    }

    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::fs::File;
    use std::path::{Path, PathBuf};

    use tar::Builder;
    use tempfile::tempdir;

    use super::{Compression, compression_for, normalize_archive_path, unpack_layer};

    #[test]
    fn normalizes_archive_paths() {
        assert_eq!(
            normalize_archive_path(Path::new("/usr/bin/env")).unwrap(),
            PathBuf::from("usr/bin/env")
        );
        assert!(normalize_archive_path(Path::new("../escape")).is_err());
    }

    #[test]
    fn detects_gzip_magic() {
        let temp = tempdir().unwrap();
        let layer = temp.path().join("layer.tar.gz");
        let mut encoder = flate2::write::GzEncoder::new(
            File::create(&layer).unwrap(),
            flate2::Compression::default(),
        );
        use std::io::Write as _;
        encoder.write_all(b"hello").unwrap();
        encoder.finish().unwrap();

        assert_eq!(compression_for(&layer, None).unwrap(), Compression::Gzip);
    }

    #[test]
    fn applies_whiteout_entries() {
        let temp = tempdir().unwrap();
        let layer_path = temp.path().join("layer.tar");
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
        fs::write(rootfs.join("usr/bin/old"), "stale").unwrap();

        let file = File::create(&layer_path).unwrap();
        let mut tar = Builder::new(file);

        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_cksum();
        tar.append_data(&mut header, "usr/bin/.wh.old", std::io::empty())
            .unwrap();
        tar.finish().unwrap();

        unpack_layer(
            &layer_path,
            &rootfs,
            Some("application/vnd.oci.image.layer.v1.tar"),
        )
        .unwrap();
        assert!(!rootfs.join("usr/bin/old").exists());
    }
}
