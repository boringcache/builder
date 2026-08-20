use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::util::fs_tree::is_pseudo_fs;

/// Archive a directory to a tar.zst file.
#[cfg(test)]
pub fn archive_directory(source: &Path, destination: &Path) -> Result<()> {
    let file = File::create(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    let encoder = zstd::Encoder::new(file, 3)?;
    let mut builder = tar::Builder::new(encoder);
    write_sorted_tar_entries(&mut builder, source)?;
    let encoder = builder.into_inner()?;
    let _ = encoder.finish()?;
    Ok(())
}

/// Archive a directory and compute the archive's SHA256 digest.
///
/// Returns `(archive_digest, archive_bytes)`. The archive digest is computed
/// by streaming the compressed output through a `HashingWriter`, avoiding a
/// separate read of the archive file.
pub fn archive_directory_with_digest(source: &Path, destination: &Path) -> Result<(String, u64)> {
    let file = File::create(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    let hashing_file = HashingWriter::new(file);
    let encoder = zstd::Encoder::new(hashing_file, 3)?;
    let mut builder = tar::Builder::new(encoder);
    write_sorted_tar_entries(&mut builder, source)?;
    let encoder = builder.into_inner()?;
    let hashing_file = encoder.finish()?;
    let (_, archive_hasher, archive_bytes) = hashing_file.into_parts();
    Ok((hex::encode(archive_hasher.finalize()), archive_bytes))
}

pub fn unpack_archive(archive_path: &Path, destination: &Path) -> Result<()> {
    let file = File::open(archive_path)
        .with_context(|| format!("failed to open {}", archive_path.display()))?;
    let decoder = zstd::Decoder::new(file)
        .with_context(|| format!("failed to decode {}", archive_path.display()))?;
    let mut archive = tar::Archive::new(decoder);
    configure_unpack(&mut archive);
    archive
        .unpack(destination)
        .with_context(|| format!("failed to restore {}", destination.display()))?;
    Ok(())
}

pub fn unpack_archive_with_format(
    archive_path: &Path,
    destination: &Path,
    archive_format: &str,
) -> Result<()> {
    match archive_format {
        crate::cache::slice::STEP_SLICE_ARCHIVE_FORMAT_TAR_ZST => {
            unpack_archive(archive_path, destination)
        }
        crate::cache::slice::STEP_SLICE_ARCHIVE_FORMAT_TAR => {
            let file = File::open(archive_path)
                .with_context(|| format!("failed to open {}", archive_path.display()))?;
            let mut archive = tar::Archive::new(file);
            configure_unpack(&mut archive);
            archive
                .unpack(destination)
                .with_context(|| format!("failed to restore {}", destination.display()))?;
            Ok(())
        }
        other => bail!("unsupported archive format: {other}"),
    }
}

fn configure_unpack<R: Read>(archive: &mut tar::Archive<R>) {
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(nix::unistd::Uid::effective().is_root());
}

pub fn hash_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
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

// ---------------------------------------------------------------------------
// Internal: sorted tar writing with optional content hashing
// ---------------------------------------------------------------------------

fn write_sorted_tar_entries<W: Write>(builder: &mut tar::Builder<W>, source: &Path) -> Result<()> {
    builder.follow_symlinks(false);
    for (path, relative, metadata) in sorted_entries(source)? {
        append_entry(builder, &path, &relative, &metadata)?;
    }
    Ok(())
}

fn sorted_entries(
    source: &Path,
) -> Result<Vec<(std::path::PathBuf, std::path::PathBuf, fs::Metadata)>> {
    let mut raw_entries = WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !is_pseudo_fs(source, entry.path()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    raw_entries.sort_by(|left, right| left.path().cmp(right.path()));

    let mut result = Vec::with_capacity(raw_entries.len());
    for entry in raw_entries {
        let path = entry.path().to_path_buf();
        if path == source {
            continue;
        }
        let relative = path
            .strip_prefix(source)
            .with_context(|| format!("failed to compute archive path for {}", path.display()))?
            .to_path_buf();
        let metadata = fs::symlink_metadata(&path)?;
        result.push((path, relative, metadata));
    }
    Ok(result)
}

fn append_entry<W: Write>(
    builder: &mut tar::Builder<W>,
    path: &Path,
    relative: &Path,
    metadata: &fs::Metadata,
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_mode(metadata.permissions().mode() & 0o7777);

    if metadata.file_type().is_symlink() {
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder.append_link(&mut header, relative, fs::read_link(path)?)?;
        return Ok(());
    }

    if metadata.is_dir() {
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_cksum();
        builder.append_data(&mut header, relative, io::empty())?;
        return Ok(());
    }

    if !metadata.is_file() {
        return Ok(());
    }

    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(metadata.len());
    header.set_cksum();
    let mut input =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    builder.append_data(&mut header, relative, &mut input)?;
    Ok(())
}

use crate::util::hashing::HashingWriter;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use tempfile::tempdir;

    use super::{archive_directory, unpack_archive_with_format};

    #[test]
    fn archives_symlinks_with_long_targets() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested/app.txt"), "ok").unwrap();
        let long_target = format!("nested/{}/app.txt", "deep".repeat(40));
        let long_target_parent = source.join("nested").join("deep".repeat(40));
        fs::create_dir_all(&long_target_parent).unwrap();
        fs::write(long_target_parent.join("app.txt"), "ok").unwrap();
        symlink(&long_target, source.join("current")).unwrap();

        let archive = temp.path().join("cache.tar.zst");
        archive_directory(&source, &archive).unwrap();
        assert!(archive.exists());
    }

    #[test]
    fn unpack_preserves_extended_mode_and_root_ownership() {
        let temp = tempdir().unwrap();
        let archive_path = temp.path().join("owned.tar");
        let file = fs::File::create(&archive_path).unwrap();
        let mut builder = tar::Builder::new(file);
        let mut header = tar::Header::new_gnu();
        let expected_mode = if nix::unistd::Uid::effective().is_root() {
            0o4755
        } else {
            // macOS can clear setuid on files created by an unprivileged user;
            // the sticky bit still proves extended mode preservation.
            0o1755
        };
        header.set_uid(123);
        header.set_gid(456);
        header.set_mode(expected_mode);
        header.set_mtime(0);
        header.set_size(4);
        header.set_cksum();
        builder
            .append_data(&mut header, "owned", "data".as_bytes())
            .unwrap();
        builder.finish().unwrap();

        let destination = temp.path().join("destination");
        fs::create_dir(&destination).unwrap();
        unpack_archive_with_format(
            &archive_path,
            &destination,
            crate::cache::slice::STEP_SLICE_ARCHIVE_FORMAT_TAR,
        )
        .unwrap();

        let metadata = fs::metadata(destination.join("owned")).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o7777, expected_mode);
        if nix::unistd::Uid::effective().is_root() {
            assert_eq!(metadata.uid(), 123);
            assert_eq!(metadata.gid(), 456);
        }
    }
}
