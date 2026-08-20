//! Step slices: capture and restore per-step filesystem deltas.
//!
//! A slice is a tar or tar.zst archive of the files a step added or modified,
//! stored as a content-addressed OCI blob.  Slices are restored before
//! each step (warming the filesystem) and saved after (capturing changes).

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::cache::archive::unpack_archive_with_format;
use crate::cache::backend::{CacheBackend, CacheManifest, tree_cache_manifest};
use crate::util::fs_tree::is_pseudo_fs;
use crate::util::hashing::HashingWriter;

pub const STEP_STATE_IGNORED_PREFIXES: &[&str] = &[
    "var/cache/apt/archives",
    "var/lib/apt/lists",
    "var/lib/dpkg/lock",
    "var/lib/dpkg/lock-frontend",
    "var/lib/dpkg/triggers/Lock",
    "var/lib/dpkg/updates",
    "var/log/apt",
];
pub const STEP_SLICE_PRE_SNAPSHOT_FINGERPRINT_METADATA_KEY: &str = "pre_snapshot_fingerprint";
pub const STEP_SLICE_STATE_KEY_METADATA_KEY: &str = "step_state_key";
pub const STEP_SLICE_STATE_DEBUG_METADATA_KEY: &str = "step_state_debug";
pub const STEP_SLICE_ARCHIVE_FORMAT_METADATA_KEY: &str = "archive_format";
pub const STEP_SLICE_CAPTURE_ABI_METADATA_KEY: &str = "capture_abi";
pub const STEP_SLICE_ARCHIVE_FORMAT_TAR: &str = "tar";
pub const STEP_SLICE_ARCHIVE_FORMAT_TAR_ZST: &str = "tar.zst";

struct ArchivedSlice<'a> {
    archive: &'a Path,
    digest: &'a str,
    bytes: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SlicePublishMetadata<'a> {
    pub pre_snapshot_fingerprint: Option<&'a str>,
    pub step_state_key: Option<&'a str>,
    pub step_state_debug: Option<&'a StepStateDebugMetadata>,
    pub archive_format: Option<&'a str>,
    pub capture_abi: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StepStateDebugMetadata {
    #[serde(default)]
    pub inputs: Vec<StepStateInputDebug>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StepStateInputDebug {
    pub path: String,
    #[serde(default)]
    pub exclude: Vec<String>,
    pub hash: String,
}

/// Lightweight file metadata for delta detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    size: u64,
    mtime_ns: u64,
    // ctime catches in-place content changes when a tool restores the original
    // size and mtime (package managers commonly do this from archive metadata).
    // It is intentionally used for same-run diffing only, not stable cache keys.
    ctime_ns: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    is_dir: bool,
    is_symlink: bool,
}

/// Snapshot of a directory's file metadata (path → metadata).
pub type FsSnapshot = BTreeMap<PathBuf, FileMeta>;

/// Take a fast metadata snapshot of a directory tree.
/// Records path, timestamps, size, mode, and ownership without reading contents.
pub fn snapshot_metadata(root: &Path) -> Result<FsSnapshot> {
    let mut state = BTreeMap::new();
    if !root.exists() {
        return Ok(state);
    }

    let entries = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_pseudo_fs(root, e.path()));

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path == root {
            continue;
        }
        let relative = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        let metadata = fs::symlink_metadata(path)?;
        // Use Unix-specific mtime (never fails) instead of cross-platform modified().
        let mtime_ns = (metadata.mtime() as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(metadata.mtime_nsec() as u64);
        let ctime_ns = (metadata.ctime() as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(metadata.ctime_nsec() as u64);

        state.insert(
            relative,
            FileMeta {
                size: metadata.len(),
                mtime_ns,
                ctime_ns,
                mode: metadata.permissions().mode() & 0o7777,
                uid: metadata.uid(),
                gid: metadata.gid(),
                is_dir: metadata.is_dir(),
                is_symlink: metadata.file_type().is_symlink(),
            },
        );
    }

    Ok(state)
}

/// Compute the set of files that changed between two snapshots.
/// Returns paths that are new, modified, or deleted.
pub fn diff_snapshots(before: &FsSnapshot, after: &FsSnapshot) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut changed = Vec::new();
    let mut deleted = Vec::new();

    // New or modified files.
    for (path, after_meta) in after {
        match before.get(path) {
            None => changed.push(path.clone()),
            Some(before_meta) if before_meta != after_meta => changed.push(path.clone()),
            _ => {}
        }
    }

    // Deleted files.
    for path in before.keys() {
        if !after.contains_key(path) {
            deleted.push(path.clone());
        }
    }

    (changed, deleted)
}

/// Stable fingerprint of a metadata snapshot.
pub fn snapshot_fingerprint(snapshot: &FsSnapshot) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"boringbuilder-step-pre-snapshot-v1\0");
    for (path, meta) in snapshot {
        hasher.update(path.as_os_str().as_bytes());
        hasher.update(b"\0");
        hasher.update(meta.size.to_le_bytes());
        hasher.update(meta.mtime_ns.to_le_bytes());
        hasher.update(meta.mode.to_le_bytes());
        hasher.update(meta.uid.to_le_bytes());
        hasher.update(meta.gid.to_le_bytes());
        hasher.update([meta.is_dir as u8, meta.is_symlink as u8]);
        hasher.update(b"\0");
    }
    hex::encode(hasher.finalize())
}

/// Archive the changed files from a rootfs into a slice blob.
/// Returns `(digest, bytes)` of the compressed archive.
pub fn archive_slice(
    rootfs: &Path,
    changed: &[PathBuf],
    deleted: &[PathBuf],
    destination: &Path,
) -> Result<(String, u64)> {
    let file = File::create(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    let hashing_file = HashingWriter::new(file);
    let encoder = zstd::Encoder::new(hashing_file, 3)?;
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);

    // Write changed/new files.
    for relative in changed {
        let absolute = rootfs.join(relative);
        let metadata = match fs::symlink_metadata(&absolute) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => continue,
            Err(e) => return Err(e.into()),
        };

        let mut header = tar::Header::new_gnu();
        header.set_uid(metadata.uid() as u64);
        header.set_gid(metadata.gid() as u64);
        header.set_mtime(0);
        header.set_mode(metadata.permissions().mode() & 0o7777);

        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&absolute)?;
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            builder.append_link(&mut header, relative, &target)?;
        } else if metadata.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_cksum();
            builder.append_data(&mut header, relative, io::empty())?;
        } else if metadata.is_file() {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(metadata.len());
            header.set_cksum();
            let mut input = File::open(&absolute)?;
            builder.append_data(&mut header, relative, &mut input)?;
        }
    }

    // Write whiteout entries for deleted files.
    for relative in deleted {
        let name = relative.file_name().unwrap_or_default().to_string_lossy();
        let whiteout_path = relative
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
            .join(format!(".wh.{name}"));
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(0);
        header.set_cksum();
        builder.append_data(&mut header, &whiteout_path, io::empty())?;
    }

    let encoder = builder.into_inner()?;
    let hashing_file = encoder.finish()?;
    let (_, hasher, bytes) = hashing_file.into_parts();
    Ok((hex::encode(hasher.finalize()), bytes))
}

/// Stable tag for a step's slice.
///
/// If `human_tag` is provided, uses it directly (enables cross-pipeline
/// sharing).  Otherwise derives a tag from image + platform + step index.
pub fn step_tag(image: &str, platform: &str, step_index: usize, human_tag: Option<&str>) -> String {
    if let Some(tag) = human_tag {
        return format!("step-slice-{tag}");
    }
    let mut hasher = Sha256::new();
    hasher.update(b"boringbuilder-step-slice-v1\0");
    hasher.update(image.as_bytes());
    hasher.update(b"\0");
    hasher.update(platform.as_bytes());
    hasher.update(b"\0");
    hasher.update(step_index.to_string().as_bytes());
    format!("step-slice-{}", hex::encode(hasher.finalize()))
}

/// Save a slice to the cache backend.
pub fn save_slice(
    backend: &dyn CacheBackend,
    tag: &str,
    rootfs: &Path,
    changed: &[PathBuf],
    deleted: &[PathBuf],
    metadata: SlicePublishMetadata<'_>,
) -> Result<u128> {
    let temp =
        tempfile::NamedTempFile::new().context("failed to create temporary slice archive")?;
    archive_slice(rootfs, changed, deleted, temp.path())?;
    publish_archived_slice(backend, tag, temp.path(), metadata)
}

pub fn publish_archived_slice(
    backend: &dyn CacheBackend,
    tag: &str,
    archive: &Path,
    metadata: SlicePublishMetadata<'_>,
) -> Result<u128> {
    let started = Instant::now();
    let digest = crate::cache::archive::hash_file(archive)?;
    let bytes = fs::metadata(archive)
        .with_context(|| format!("failed to stat archived slice {}", archive.display()))?
        .len();

    publish_archived_slice_with_digest(
        backend,
        tag,
        ArchivedSlice {
            archive,
            digest: &digest,
            bytes,
        },
        metadata,
        started,
    )
}

pub fn publish_archived_slice_prehashed(
    backend: &dyn CacheBackend,
    tag: &str,
    archive: &Path,
    digest: &str,
    bytes: u64,
    metadata: SlicePublishMetadata<'_>,
) -> Result<u128> {
    publish_archived_slice_with_digest(
        backend,
        tag,
        ArchivedSlice {
            archive,
            digest,
            bytes,
        },
        metadata,
        Instant::now(),
    )
}

pub fn publish_stored_slice_prehashed(
    backend: &dyn CacheBackend,
    tag: &str,
    digest: &str,
    bytes: u64,
    metadata: SlicePublishMetadata<'_>,
) -> Result<u128> {
    let started = Instant::now();
    ensure!(
        backend.has_blob(digest)?,
        "pre-stored slice blob {digest} was not found in {} cache",
        backend.kind()
    );
    publish_slice_manifest(backend, tag, digest.to_string(), bytes, metadata)?;
    Ok(started.elapsed().as_millis())
}

fn publish_archived_slice_with_digest(
    backend: &dyn CacheBackend,
    tag: &str,
    archived: ArchivedSlice<'_>,
    metadata: SlicePublishMetadata<'_>,
    started: Instant,
) -> Result<u128> {
    let digest = archived.digest.to_string();

    if !backend.has_blob(&digest)? {
        backend.store_blob(&digest, archived.archive)?;
    }

    publish_slice_manifest(backend, tag, digest, archived.bytes, metadata)?;
    Ok(started.elapsed().as_millis())
}

fn publish_slice_manifest(
    backend: &dyn CacheBackend,
    tag: &str,
    digest: String,
    bytes: u64,
    metadata: SlicePublishMetadata<'_>,
) -> Result<()> {
    let mut manifest = tree_cache_manifest(digest, bytes, None);
    if let Some(pre_snapshot_fingerprint) = metadata.pre_snapshot_fingerprint {
        manifest.metadata.insert(
            STEP_SLICE_PRE_SNAPSHOT_FINGERPRINT_METADATA_KEY.to_string(),
            pre_snapshot_fingerprint.to_string(),
        );
    }
    if let Some(step_state_key) = metadata.step_state_key {
        manifest.metadata.insert(
            STEP_SLICE_STATE_KEY_METADATA_KEY.to_string(),
            step_state_key.to_string(),
        );
    }
    if let Some(step_state_debug) = metadata.step_state_debug {
        manifest.metadata.insert(
            STEP_SLICE_STATE_DEBUG_METADATA_KEY.to_string(),
            serde_json::to_string(step_state_debug)
                .context("failed to serialize step slice state debug metadata")?,
        );
    }
    if let Some(archive_format) = metadata.archive_format {
        manifest.metadata.insert(
            STEP_SLICE_ARCHIVE_FORMAT_METADATA_KEY.to_string(),
            archive_format.to_string(),
        );
    }
    if let Some(capture_abi) = metadata.capture_abi {
        manifest.metadata.insert(
            STEP_SLICE_CAPTURE_ABI_METADATA_KEY.to_string(),
            capture_abi.to_string(),
        );
    }
    backend.publish_ref(tag, &manifest)?;
    Ok(())
}

pub fn manifest_archive_format(manifest: &CacheManifest) -> &str {
    manifest
        .metadata
        .get(STEP_SLICE_ARCHIVE_FORMAT_METADATA_KEY)
        .map(String::as_str)
        .unwrap_or(STEP_SLICE_ARCHIVE_FORMAT_TAR_ZST)
}

pub fn manifest_pre_snapshot_fingerprint(manifest: &CacheManifest) -> Option<&str> {
    manifest
        .metadata
        .get(STEP_SLICE_PRE_SNAPSHOT_FINGERPRINT_METADATA_KEY)
        .map(String::as_str)
}

pub fn manifest_step_state_key(manifest: &CacheManifest) -> Option<&str> {
    manifest
        .metadata
        .get(STEP_SLICE_STATE_KEY_METADATA_KEY)
        .map(String::as_str)
}

pub fn manifest_capture_abi(manifest: &CacheManifest) -> Option<&str> {
    manifest
        .metadata
        .get(STEP_SLICE_CAPTURE_ABI_METADATA_KEY)
        .map(String::as_str)
}

pub fn manifest_step_state_debug(manifest: &CacheManifest) -> Option<StepStateDebugMetadata> {
    manifest
        .metadata
        .get(STEP_SLICE_STATE_DEBUG_METADATA_KEY)
        .and_then(|value| serde_json::from_str(value).ok())
}

/// Restore a slice into the rootfs. Returns ms elapsed, or None if no slice exists.
pub fn restore_slice(backend: &dyn CacheBackend, tag: &str, rootfs: &Path) -> Result<Option<u128>> {
    let started = Instant::now();
    let Some(manifest) = backend.resolve_ref(tag)? else {
        return Ok(None);
    };
    let blob = crate::cache::backend::single_blob_from_manifest(
        &manifest,
        crate::cache::backend::TREE_CACHE_ARTIFACT_KIND,
        backend.kind(),
    )?;
    if !backend.has_blob(&blob.digest)? {
        return Ok(None);
    }

    // Fetch and unpack — does NOT clear rootfs, applies on top.
    let temp =
        tempfile::NamedTempFile::new().context("failed to create temporary slice restore file")?;
    backend.fetch_blob(&blob.digest, temp.path())?;
    unpack_archive_with_format(temp.path(), rootfs, manifest_archive_format(&manifest))?;

    Ok(Some(started.elapsed().as_millis()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn snapshot_captures_files_and_dirs() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/file.txt"), "hello").unwrap();
        symlink("file.txt", root.join("sub/link")).unwrap();

        let snap = snapshot_metadata(root).unwrap();
        assert!(snap.contains_key(Path::new("sub")));
        assert!(snap.contains_key(Path::new("sub/file.txt")));
        assert!(snap.contains_key(Path::new("sub/link")));
        assert!(snap[Path::new("sub")].is_dir);
        assert!(snap[Path::new("sub/link")].is_symlink);
    }

    #[test]
    fn diff_detects_new_modified_deleted() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("keep.txt"), "same").unwrap();
        fs::write(root.join("modify.txt"), "before").unwrap();
        fs::write(root.join("delete.txt"), "gone").unwrap();

        let before = snapshot_metadata(root).unwrap();

        fs::write(root.join("modify.txt"), "after-changed").unwrap();
        fs::remove_file(root.join("delete.txt")).unwrap();
        fs::write(root.join("new.txt"), "fresh").unwrap();

        let after = snapshot_metadata(root).unwrap();
        let (changed, deleted) = diff_snapshots(&before, &after);

        assert!(changed.contains(&PathBuf::from("modify.txt")));
        assert!(changed.contains(&PathBuf::from("new.txt")));
        assert!(!changed.contains(&PathBuf::from("keep.txt")));
        assert_eq!(deleted, vec![PathBuf::from("delete.txt")]);
    }

    #[test]
    fn diff_detects_same_size_content_changes_with_restored_mtime() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("same-metadata.txt");
        fs::write(&path, "before").unwrap();
        let original_mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let before = snapshot_metadata(tmp.path()).unwrap();

        fs::write(&path, "after!").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(original_mtime))
            .unwrap();

        let after = snapshot_metadata(tmp.path()).unwrap();
        let (changed, deleted) = diff_snapshots(&before, &after);
        assert_eq!(changed, vec![PathBuf::from("same-metadata.txt")]);
        assert!(deleted.is_empty());
    }

    #[test]
    fn diff_detects_ownership_changes() {
        let path = PathBuf::from("owned.txt");
        let original = FileMeta {
            size: 4,
            mtime_ns: 0,
            ctime_ns: 0,
            mode: 0o644,
            uid: 0,
            gid: 0,
            is_dir: false,
            is_symlink: false,
        };
        let mut before = BTreeMap::new();
        before.insert(path.clone(), original.clone());

        let mut after = before.clone();
        after.get_mut(&path).unwrap().uid = 1000;
        assert_eq!(diff_snapshots(&before, &after).0, vec![path.clone()]);

        after.insert(path.clone(), original);
        after.get_mut(&path).unwrap().gid = 1000;
        assert_eq!(diff_snapshots(&before, &after).0, vec![path]);
    }

    #[test]
    fn archive_and_restore_round_trips() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("rootfs");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/app"), "binary").unwrap();

        let changed = vec![PathBuf::from("bin"), PathBuf::from("bin/app")];
        let deleted = vec![];

        let archive = tmp.path().join("slice.tar.zst");
        let (digest, bytes) = archive_slice(&root, &changed, &deleted, &archive).unwrap();
        assert!(!digest.is_empty());
        assert!(bytes > 0);

        // Restore into a different directory.
        let dest = tmp.path().join("restored");
        fs::create_dir_all(&dest).unwrap();
        crate::cache::archive::unpack_archive(&archive, &dest).unwrap();
        assert_eq!(fs::read_to_string(dest.join("bin/app")).unwrap(), "binary");
    }

    #[test]
    fn snapshot_preserves_package_manager_outputs() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("var/cache/apt/archives/partial")).unwrap();
        fs::create_dir_all(root.join("var/lib/apt/lists/partial")).unwrap();
        fs::create_dir_all(root.join("var/lib/dpkg/updates")).unwrap();
        fs::create_dir_all(root.join("var/log/apt")).unwrap();
        fs::create_dir_all(root.join("workspace/cache")).unwrap();
        fs::write(root.join("var/cache/apt/archives/pkg.deb"), "deb").unwrap();
        fs::write(root.join("var/lib/apt/lists/lock"), "lock").unwrap();
        fs::write(root.join("var/lib/dpkg/lock-frontend"), "lock").unwrap();
        fs::write(root.join("var/log/apt/history.log"), "history").unwrap();
        fs::write(root.join("workspace/cache/app.txt"), "keep").unwrap();

        let snap = snapshot_metadata(root).unwrap();

        assert!(snap.contains_key(Path::new("var/lib/apt/lists")));
        assert!(snap.contains_key(Path::new("var/lib/apt/lists/lock")));
        assert!(snap.contains_key(Path::new("var/lib/apt/lists/partial")));
        assert!(snap.contains_key(Path::new("var/cache/apt/archives")));
        assert!(snap.contains_key(Path::new("var/cache/apt/archives/pkg.deb")));
        assert!(snap.contains_key(Path::new("var/cache/apt/archives/partial")));
        assert!(snap.contains_key(Path::new("var/lib/dpkg/lock-frontend")));
        assert!(snap.contains_key(Path::new("var/lib/dpkg/updates")));
        assert!(snap.contains_key(Path::new("var/log/apt")));
        assert!(snap.contains_key(Path::new("var/log/apt/history.log")));
        assert!(snap.contains_key(Path::new("workspace/cache")));
        assert!(snap.contains_key(Path::new("workspace/cache/app.txt")));
    }

    #[test]
    fn diff_captures_package_manager_changes() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("var/cache/apt")).unwrap();
        fs::create_dir_all(root.join("var/lib/apt")).unwrap();
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::write(root.join("workspace/app.txt"), "before").unwrap();
        let before = snapshot_metadata(root).unwrap();

        fs::create_dir_all(root.join("var/cache/apt/archives/partial")).unwrap();
        fs::write(root.join("var/cache/apt/archives/pkg.deb"), "deb").unwrap();
        fs::create_dir_all(root.join("var/lib/apt/lists/partial")).unwrap();
        fs::write(root.join("var/lib/apt/lists/lock"), "lock").unwrap();
        fs::write(root.join("workspace/app.txt"), "after").unwrap();

        let after = snapshot_metadata(root).unwrap();
        let (changed, deleted) = diff_snapshots(&before, &after);

        assert!(changed.contains(&PathBuf::from("workspace/app.txt")));
        assert!(changed.contains(&PathBuf::from("var/cache/apt/archives")));
        assert!(changed.contains(&PathBuf::from("var/cache/apt/archives/pkg.deb")));
        assert!(changed.contains(&PathBuf::from("var/cache/apt/archives/partial")));
        assert!(changed.contains(&PathBuf::from("var/lib/apt/lists")));
        assert!(changed.contains(&PathBuf::from("var/lib/apt/lists/lock")));
        assert!(changed.contains(&PathBuf::from("var/lib/apt/lists/partial")));
        assert!(deleted.is_empty());
    }

    #[test]
    fn save_slice_publishes_empty_manifest_for_unchanged_step() {
        let tmp = tempdir().unwrap();
        let rootfs = tmp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("workspace")).unwrap();
        fs::write(rootfs.join("workspace/keep.txt"), "keep").unwrap();

        let backend =
            crate::cache::local::LocalCacheBackend::open(&tmp.path().join("store")).unwrap();

        let _elapsed = save_slice(
            &backend,
            "step-slice-noop",
            &rootfs,
            &[],
            &[],
            SlicePublishMetadata {
                pre_snapshot_fingerprint: Some("pre-fingerprint"),
                step_state_key: Some("step-state-key"),
                capture_abi: Some("test-capture-abi"),
                step_state_debug: Some(&StepStateDebugMetadata {
                    inputs: vec![StepStateInputDebug {
                        path: "/workspace".to_string(),
                        exclude: vec!["tmp".to_string()],
                        hash: "workspace-hash".to_string(),
                    }],
                }),
                ..Default::default()
            },
        )
        .unwrap();

        let manifest = backend.resolve_ref("step-slice-noop").unwrap().unwrap();
        let blob = crate::cache::backend::single_blob_from_manifest(
            &manifest,
            crate::cache::backend::TREE_CACHE_ARTIFACT_KIND,
            backend.kind(),
        )
        .unwrap();
        assert!(blob.bytes > 0);
        assert!(backend.has_blob(&blob.digest).unwrap());
        assert_eq!(
            manifest_pre_snapshot_fingerprint(&manifest),
            Some("pre-fingerprint")
        );
        assert_eq!(manifest_step_state_key(&manifest), Some("step-state-key"));
        assert_eq!(manifest_capture_abi(&manifest), Some("test-capture-abi"));
        assert_eq!(
            manifest_step_state_debug(&manifest),
            Some(StepStateDebugMetadata {
                inputs: vec![StepStateInputDebug {
                    path: "/workspace".to_string(),
                    exclude: vec!["tmp".to_string()],
                    hash: "workspace-hash".to_string(),
                }],
            })
        );

        let restore_root = tmp.path().join("restore");
        fs::create_dir_all(&restore_root).unwrap();
        assert!(
            restore_slice(&backend, "step-slice-noop", &restore_root)
                .unwrap()
                .is_some()
        );
        assert!(!restore_root.join("workspace").exists());
    }

    #[test]
    fn publish_stored_slice_prehashed_only_publishes_manifest() {
        let tmp = tempdir().unwrap();
        let backend =
            crate::cache::local::LocalCacheBackend::open(&tmp.path().join("store")).unwrap();
        let digest = "c".repeat(64);
        let source = tmp.path().join("stored.tar.zst");
        fs::write(&source, b"already stored").unwrap();
        backend.store_blob(&digest, &source).unwrap();

        publish_stored_slice_prehashed(
            &backend,
            "step-slice-stored",
            &digest,
            14,
            SlicePublishMetadata {
                step_state_key: Some("stored-state-key"),
                ..Default::default()
            },
        )
        .unwrap();

        let manifest = backend.resolve_ref("step-slice-stored").unwrap().unwrap();
        let blob = crate::cache::backend::single_blob_from_manifest(
            &manifest,
            crate::cache::backend::TREE_CACHE_ARTIFACT_KIND,
            backend.kind(),
        )
        .unwrap();
        assert_eq!(blob.digest, digest);
        assert_eq!(blob.bytes, 14);
        assert_eq!(manifest_step_state_key(&manifest), Some("stored-state-key"));
    }
}
