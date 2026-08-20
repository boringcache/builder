use std::fs;
use std::fs::File;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::Result;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::util::fs_tree::is_pseudo_fs;

pub fn path_size(path: &Path) -> Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if metadata.file_type().is_symlink() {
        return Ok(0);
    }

    let mut total = 0u64;
    for entry in WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !is_pseudo_fs(path, entry.path()))
    {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            total += metadata.len();
        }
    }
    Ok(total)
}

pub fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;

    for entry in WalkDir::new(source).follow_links(false) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(source)?;
        if relative.as_os_str().is_empty() {
            continue;
        }

        let target = destination.join(relative);
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;

                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                symlink(fs::read_link(entry.path())?, &target)?;
            }
            #[cfg(not(unix))]
            {
                anyhow::bail!("symlink-preserving tree copy is only supported on unix");
            }
            continue;
        }

        if metadata.is_dir() {
            fs::create_dir_all(&target)?;
            fs::set_permissions(&target, metadata.permissions())?;
            continue;
        }

        if !metadata.is_file() {
            continue;
        }

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(entry.path(), &target)?;
        fs::set_permissions(&target, metadata.permissions())?;
    }

    Ok(())
}

pub fn copy_path(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            symlink(fs::read_link(source)?, destination)?;
        }
        #[cfg(not(unix))]
        {
            anyhow::bail!("symlink-preserving copy is only supported on unix");
        }
        return Ok(());
    }

    if metadata.is_dir() {
        copy_tree(source, destination)?;
        fs::set_permissions(destination, metadata.permissions())?;
        return Ok(());
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination)?;
    fs::set_permissions(destination, metadata.permissions())?;
    Ok(())
}

pub fn copy_path_dereferenced(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::metadata(source)?;
    if metadata.is_dir() {
        let resolved = fs::canonicalize(source)?;
        copy_tree(&resolved, destination)?;
        fs::set_permissions(destination, metadata.permissions())?;
        return Ok(());
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination)?;
    fs::set_permissions(destination, metadata.permissions())?;
    Ok(())
}

pub fn clear_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let target = entry.path();
        let metadata = fs::symlink_metadata(&target)?;
        if metadata.is_dir() {
            fs::remove_dir_all(&target)?;
        } else {
            fs::remove_file(&target)?;
        }
    }

    Ok(())
}

pub fn directory_content_hash(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"boringbuilder-directory-content-hash-v1\0");

    if !path.exists() {
        hasher.update(b"missing");
        return Ok(hex::encode(hasher.finalize()));
    }

    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_file() {
        hasher.update(b"file\0");
        hash_regular_file(path, &mut hasher)?;
        return Ok(hex::encode(hasher.finalize()));
    }
    if metadata.file_type().is_symlink() {
        hasher.update(b"symlink\0");
        hasher.update(fs::read_link(path)?.to_string_lossy().as_bytes());
        return Ok(hex::encode(hasher.finalize()));
    }
    if !metadata.is_dir() {
        hasher.update(b"special\0");
        #[cfg(unix)]
        hasher.update((metadata.permissions().mode() & 0o7777).to_le_bytes());
        return Ok(hex::encode(hasher.finalize()));
    }

    let mut entries = WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !is_pseudo_fs(path, entry.path()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by(|left, right| left.path().cmp(right.path()));

    for entry in entries {
        let entry_path = entry.path();
        if entry_path == path {
            continue;
        }

        let relative = entry_path.strip_prefix(path).unwrap_or(entry_path);
        let metadata = fs::symlink_metadata(entry_path)?;
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        #[cfg(unix)]
        hasher.update((metadata.permissions().mode() & 0o7777).to_le_bytes());

        if metadata.file_type().is_symlink() {
            hasher.update(b"symlink\0");
            hasher.update(fs::read_link(entry_path)?.to_string_lossy().as_bytes());
            continue;
        }

        if metadata.is_dir() {
            hasher.update(b"dir\0");
            continue;
        }

        if !metadata.is_file() {
            hasher.update(b"special\0");
            continue;
        }

        hasher.update(b"file\0");
        hash_regular_file(entry_path, &mut hasher)?;
    }

    Ok(hex::encode(hasher.finalize()))
}

fn hash_regular_file(path: &Path, hasher: &mut Sha256) -> Result<()> {
    let metadata = fs::metadata(path)?;
    hasher.update(metadata.len().to_le_bytes());

    let mut file = File::open(path)?;
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
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;
    use std::process::Command;

    use tempfile::tempdir;

    use super::{
        clear_directory, copy_path, copy_path_dereferenced, copy_tree, directory_content_hash,
        path_size,
    };

    fn create_fifo(path: &std::path::Path) {
        let status = Command::new("mkfifo").arg(path).status().unwrap();
        assert!(
            status.success(),
            "mkfifo should succeed for {}",
            path.display()
        );
    }

    #[test]
    fn counts_file_tree_bytes() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("a.txt"), b"1234").unwrap();
        fs::create_dir_all(temp.path().join("nested")).unwrap();
        fs::write(temp.path().join("nested/b.txt"), b"hello").unwrap();

        assert_eq!(path_size(temp.path()).unwrap(), 9);
    }

    #[test]
    fn skips_pseudo_fs_subtrees() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("root.txt"), b"1234").unwrap();
        fs::create_dir_all(temp.path().join("proc/tty")).unwrap();
        fs::write(temp.path().join("proc/tty/driver"), b"should-not-count").unwrap();
        fs::set_permissions(
            temp.path().join("proc/tty/driver"),
            fs::Permissions::from_mode(0o0),
        )
        .unwrap();

        assert_eq!(path_size(temp.path()).unwrap(), 4);
    }

    #[test]
    fn copies_tree_with_symlinks() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("src");
        let destination = temp.path().join("dst");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested/file.txt"), b"hello").unwrap();
        symlink("nested/file.txt", source.join("current")).unwrap();

        copy_tree(&source, &destination).unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("nested/file.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            fs::read_link(destination.join("current")).unwrap(),
            std::path::PathBuf::from("nested/file.txt")
        );
    }

    #[test]
    fn copy_tree_skips_special_files() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("src");
        let destination = temp.path().join("dst");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested/file.txt"), b"hello").unwrap();
        create_fifo(&source.join("nested/cache.pipe"));

        copy_tree(&source, &destination).unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("nested/file.txt")).unwrap(),
            "hello"
        );
        assert!(
            !destination.join("nested/cache.pipe").exists(),
            "special files should be skipped during tree copies"
        );
    }

    #[test]
    fn copies_single_directory_file_and_symlink() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("src");
        let destination = temp.path().join("dst");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested/file.txt"), b"hello").unwrap();
        symlink("nested/file.txt", source.join("current")).unwrap();

        copy_path(&source.join("nested"), &destination.join("nested")).unwrap();
        copy_path(
            &source.join("nested/file.txt"),
            &destination.join("file.txt"),
        )
        .unwrap();
        copy_path(&source.join("current"), &destination.join("current")).unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("nested/file.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            fs::read_to_string(destination.join("file.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            fs::read_link(destination.join("current")).unwrap(),
            std::path::PathBuf::from("nested/file.txt")
        );
    }

    #[test]
    fn copies_dereferenced_symlink_as_file() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("src");
        let destination = temp.path().join("dst/file.txt");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("real.txt"), b"hello").unwrap();
        symlink("real.txt", source.join("current")).unwrap();

        copy_path_dereferenced(&source.join("current"), &destination).unwrap();

        assert_eq!(fs::read_to_string(&destination).unwrap(), "hello");
        assert!(
            !fs::symlink_metadata(&destination)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn clears_directory_contents_without_removing_root() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("cache");
        fs::create_dir_all(path.join("nested")).unwrap();
        fs::write(path.join("file.txt"), b"hello").unwrap();
        fs::write(path.join("nested/other.txt"), b"world").unwrap();

        clear_directory(&path).unwrap();

        assert!(path.exists());
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
    }

    #[test]
    fn directory_content_hash_ignores_mtime_changes() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("cache");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("file.txt"), "hello").unwrap();

        let first = directory_content_hash(&root).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        fs::write(root.join("file.txt"), "hello").unwrap();
        let second = directory_content_hash(&root).unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn directory_content_hash_changes_with_file_contents() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("cache");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("file.txt"), "hello").unwrap();

        let first = directory_content_hash(&root).unwrap();
        fs::write(root.join("file.txt"), "world").unwrap();
        let second = directory_content_hash(&root).unwrap();

        assert_ne!(first, second);
    }

    #[test]
    fn directory_content_hash_skips_special_files_without_failing() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("cache");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("file.txt"), "hello").unwrap();
        create_fifo(&root.join("cache.pipe"));

        let first = directory_content_hash(&root).unwrap();
        let second = directory_content_hash(&root).unwrap();

        assert_eq!(first, second);
    }
}
