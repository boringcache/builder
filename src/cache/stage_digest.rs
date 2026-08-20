use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::cache::key::hash_str;
use crate::util::fs_tree::is_pseudo_fs;

pub fn digest_stage_rootfs(rootfs: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    hash_str(&mut hasher, "boringbuilder-stage-rootfs-v1");

    let walker = WalkDir::new(rootfs)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !is_pseudo_fs(rootfs, entry.path()));

    let mut paths = Vec::new();
    for entry in walker {
        match entry {
            Ok(entry) => paths.push(entry.into_path()),
            Err(err)
                if err
                    .io_error()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied) =>
            {
                continue;
            }
            Err(err) => return Err(err.into()),
        }
    }
    paths.sort();

    for path in paths {
        if path == rootfs {
            continue;
        }

        let relative = path.strip_prefix(rootfs).with_context(|| {
            format!(
                "failed to compute stage rootfs relative path for {}",
                path.display()
            )
        })?;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => continue,
            Err(err) => return Err(err.into()),
        };

        hash_str(&mut hasher, &relative.to_string_lossy());
        hash_metadata(&mut hasher, &metadata);

        if metadata.file_type().is_symlink() {
            hash_str(&mut hasher, "symlink");
            let target = fs::read_link(&path)
                .with_context(|| format!("failed to read symlink {}", path.display()))?;
            hash_str(&mut hasher, &target.to_string_lossy());
            continue;
        }

        if metadata.is_dir() {
            hash_str(&mut hasher, "dir");
            continue;
        }

        if metadata.is_file() {
            hash_str(&mut hasher, "file");
            hash_file_contents(&mut hasher, &path)?;
            continue;
        }

        hash_str(&mut hasher, "special");
        hasher.update(metadata.rdev().to_le_bytes());
    }

    Ok(hex::encode(hasher.finalize()))
}

fn hash_metadata(hasher: &mut Sha256, metadata: &fs::Metadata) {
    hash_str(hasher, "mode");
    hasher.update((metadata.permissions().mode() & 0o7777).to_le_bytes());
    hash_str(hasher, "uid");
    hasher.update(metadata.uid().to_le_bytes());
    hash_str(hasher, "gid");
    hasher.update(metadata.gid().to_le_bytes());
}

fn hash_file_contents(hasher: &mut Sha256, path: &Path) -> Result<()> {
    let mut file =
        fs::File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::digest_stage_rootfs;

    #[test]
    fn digest_changes_when_rootfs_content_changes() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("app")).unwrap();
        fs::write(rootfs.join("app/file.txt"), "one").unwrap();
        let digest1 = digest_stage_rootfs(&rootfs).unwrap();

        fs::write(rootfs.join("app/file.txt"), "two").unwrap();
        let digest2 = digest_stage_rootfs(&rootfs).unwrap();

        assert_ne!(digest1, digest2);
    }

    #[test]
    fn digest_skips_pseudo_filesystems_and_hashes_symlinks() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("app")).unwrap();
        fs::create_dir_all(rootfs.join("proc/tty")).unwrap();
        fs::write(rootfs.join("app/file.txt"), "payload").unwrap();
        symlink("file.txt", rootfs.join("app/link.txt")).unwrap();

        let digest = digest_stage_rootfs(&rootfs).unwrap();

        fs::write(rootfs.join("proc/tty/driver"), "ephemeral").unwrap();
        let digest_after_proc_change = digest_stage_rootfs(&rootfs).unwrap();

        assert_eq!(digest, digest_after_proc_change);
    }
}
