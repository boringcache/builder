use std::path::Path;

/// Directories that correspond to kernel-mounted pseudo-filesystems inside a
/// container. These must be skipped when hashing or exporting a physical rootfs
/// because they are mounted on top of the container filesystem and are not part
/// of the image layer content.
pub const PSEUDO_FS_DIRS: &[&str] = &["proc", "sys", "dev"];

pub fn is_pseudo_fs(root: &Path, entry_path: &Path) -> bool {
    let Ok(relative) = entry_path.strip_prefix(root) else {
        return false;
    };

    let Some(first) = relative.components().next() else {
        return false;
    };

    let name = first.as_os_str().to_str().unwrap_or("");
    PSEUDO_FS_DIRS.contains(&name)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::is_pseudo_fs;

    #[test]
    fn detects_pseudo_fs_descendants() {
        let root = Path::new("/tmp/rootfs");
        assert!(is_pseudo_fs(root, Path::new("/tmp/rootfs/proc/tty")));
        assert!(is_pseudo_fs(root, Path::new("/tmp/rootfs/sys/kernel")));
        assert!(is_pseudo_fs(root, Path::new("/tmp/rootfs/dev/null")));
        assert!(!is_pseudo_fs(root, Path::new("/tmp/rootfs/usr/bin/env")));
        assert!(!is_pseudo_fs(root, Path::new("/tmp/rootfs")));
    }
}
