use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use walkdir::WalkDir;

use crate::schema::{Input, Pipeline};
use crate::util::fs_tree::is_pseudo_fs;

#[derive(Debug)]
pub struct ArchiveEntry {
    pub host_path: PathBuf,
    pub archive_prefix: PathBuf,
}

#[derive(Debug, Clone)]
pub enum ArchiveNodeKind {
    Directory,
    File,
    Symlink(PathBuf),
}

#[derive(Debug)]
pub struct ArchiveNode {
    pub host_path: PathBuf,
    pub archive_path: PathBuf,
    pub metadata: fs::Metadata,
    pub kind: ArchiveNodeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymlinkMode {
    Error,
    Preserve,
}

pub fn resolve_archive_entries(
    pipeline: &Pipeline,
    rootfs_dir: Option<&Path>,
) -> Result<Vec<ArchiveEntry>> {
    let entries = if pipeline.outputs.is_empty() {
        match rootfs_dir {
            Some(rootfs) => pipeline
                .inputs
                .iter()
                .filter(|input| !input.readonly)
                .map(|input| default_rootfs_export_entry(rootfs, input))
                .collect::<Result<Vec<_>>>()?,
            None => pipeline
                .inputs
                .iter()
                .filter(|input| !input.readonly)
                .map(default_export_entry)
                .collect::<Result<Vec<_>>>()?,
        }
    } else {
        pipeline
            .outputs
            .iter()
            .map(|output| map_output_to_host(pipeline, output, rootfs_dir))
            .collect::<Result<Vec<_>>>()?
    };

    if entries.is_empty() {
        bail!("nothing to export; define outputs or use a writable input mount");
    }

    Ok(entries)
}

pub fn collect_archive_nodes(
    path: &Path,
    archive_prefix: &Path,
    symlink_mode: SymlinkMode,
    nodes: &mut Vec<ArchiveNode>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("export path does not exist: {}", path.display()))?;

    if metadata.file_type().is_symlink() {
        push_root_symlink(path, archive_prefix, metadata, symlink_mode, nodes)?;
        return Ok(());
    }

    if metadata.is_file() {
        nodes.push(ArchiveNode {
            host_path: path.to_path_buf(),
            archive_path: archive_prefix.to_path_buf(),
            metadata,
            kind: ArchiveNodeKind::File,
        });
        return Ok(());
    }

    if !archive_prefix.as_os_str().is_empty() {
        nodes.push(ArchiveNode {
            host_path: path.to_path_buf(),
            archive_path: archive_prefix.to_path_buf(),
            metadata,
            kind: ArchiveNodeKind::Directory,
        });
    }

    let walker = WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !is_pseudo_fs(path, entry.path()));

    for entry in walker {
        let entry = entry?;
        if entry.path() == path {
            continue;
        }

        let relative = entry.path().strip_prefix(path).map_err(|_| {
            anyhow!(
                "failed to compute export path for {}",
                entry.path().display()
            )
        })?;
        let archive_path = archive_prefix.join(relative);
        let metadata = fs::symlink_metadata(entry.path())?;

        let kind = if metadata.file_type().is_symlink() {
            match symlink_mode {
                SymlinkMode::Error => {
                    bail!(
                        "symlink export is not supported for this archive: {}",
                        entry.path().display()
                    );
                }
                SymlinkMode::Preserve => ArchiveNodeKind::Symlink(fs::read_link(entry.path())?),
            }
        } else if metadata.is_dir() {
            ArchiveNodeKind::Directory
        } else {
            ArchiveNodeKind::File
        };

        nodes.push(ArchiveNode {
            host_path: entry.path().to_path_buf(),
            archive_path,
            metadata,
            kind,
        });
    }

    Ok(())
}

pub fn sort_and_dedup_archive_nodes(nodes: &mut Vec<ArchiveNode>) {
    nodes.sort_by(|left, right| left.archive_path.cmp(&right.archive_path));
    nodes.dedup_by(|left, right| left.archive_path == right.archive_path);
}

fn default_export_entry(input: &Input) -> Result<ArchiveEntry> {
    Ok(ArchiveEntry {
        host_path: input.source.clone(),
        archive_prefix: PathBuf::from(input.dest.trim_start_matches('/')),
    })
}

fn default_rootfs_export_entry(rootfs_dir: &Path, input: &Input) -> Result<ArchiveEntry> {
    let prefix = input.dest.trim_start_matches('/');
    Ok(ArchiveEntry {
        host_path: if prefix.is_empty() {
            rootfs_dir.to_path_buf()
        } else {
            rootfs_dir.join(prefix)
        },
        archive_prefix: PathBuf::from(prefix),
    })
}

fn map_output_to_host(
    pipeline: &Pipeline,
    output: &str,
    rootfs_dir: Option<&Path>,
) -> Result<ArchiveEntry> {
    if let Some(rootfs) = rootfs_dir {
        let relative = output.trim_start_matches('/');
        return Ok(ArchiveEntry {
            host_path: if relative.is_empty() {
                rootfs.to_path_buf()
            } else {
                rootfs.join(relative)
            },
            archive_prefix: PathBuf::from(relative),
        });
    }

    let mut candidates = pipeline
        .inputs
        .iter()
        .filter(|input| !input.readonly)
        .collect::<Vec<_>>();
    candidates.sort_by_key(|input| std::cmp::Reverse(input.dest.len()));

    for input in candidates {
        let input_dest = Path::new(&input.dest);
        let output_path = Path::new(output);
        if let Ok(relative) = output_path.strip_prefix(input_dest) {
            let host_path = if relative.as_os_str().is_empty() {
                input.source.clone()
            } else {
                input.source.join(relative)
            };
            return Ok(ArchiveEntry {
                host_path,
                archive_prefix: PathBuf::from(output.trim_start_matches('/')),
            });
        }
    }

    Err(anyhow!(
        "output '{output}' is not contained within a writable input mount"
    ))
}

fn push_root_symlink(
    path: &Path,
    archive_prefix: &Path,
    metadata: fs::Metadata,
    symlink_mode: SymlinkMode,
    nodes: &mut Vec<ArchiveNode>,
) -> Result<()> {
    match symlink_mode {
        SymlinkMode::Error => {
            bail!(
                "symlink export is not supported for this archive: {}",
                path.display()
            );
        }
        SymlinkMode::Preserve => {
            nodes.push(ArchiveNode {
                host_path: path.to_path_buf(),
                archive_path: archive_prefix.to_path_buf(),
                metadata,
                kind: ArchiveNodeKind::Symlink(fs::read_link(path)?),
            });
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    use tempfile::tempdir;

    use crate::schema::{Input, Pipeline};

    use super::{
        ArchiveNodeKind, SymlinkMode, collect_archive_nodes, resolve_archive_entries,
        sort_and_dedup_archive_nodes,
    };

    fn test_pipeline(temp: &Path) -> Pipeline {
        Pipeline {
            image: "alpine".to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: vec![Input {
                source: temp.join("workspace"),
                dest: "/workspace".to_string(),
                readonly: false,
            }],
            outputs: vec!["/workspace/dist".to_string()],
            setup_snapshot: None,
            operations: Vec::new(),
            export: None,
            metadata: None,
            base_dir: temp.to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        }
    }

    #[test]
    fn resolves_outputs_from_rootfs_root() {
        let temp = tempdir().unwrap();
        let pipeline = Pipeline {
            outputs: vec!["/".to_string()],
            ..test_pipeline(temp.path())
        };
        let entries = resolve_archive_entries(&pipeline, Some(temp.path())).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].host_path, temp.path());
        assert!(entries[0].archive_prefix.as_os_str().is_empty());
    }

    #[test]
    fn falls_back_to_writable_inputs_without_outputs() {
        let temp = tempdir().unwrap();
        let pipeline = Pipeline {
            outputs: Vec::new(),
            ..test_pipeline(temp.path())
        };

        let entries = resolve_archive_entries(&pipeline, None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].host_path, temp.path().join("workspace"));
        assert_eq!(entries[0].archive_prefix, Path::new("workspace"));
    }

    #[test]
    fn prefers_declared_outputs_over_writable_inputs() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(workspace.join("dist")).unwrap();
        fs::write(workspace.join("dist/app.txt"), "ok").unwrap();

        let pipeline = test_pipeline(temp.path());
        let entries = resolve_archive_entries(&pipeline, None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].host_path, workspace.join("dist"));
        assert_eq!(entries[0].archive_prefix, Path::new("workspace/dist"));
    }

    #[test]
    fn collects_directories_files_and_symlinks() {
        let temp = tempdir().unwrap();
        let dist = temp.path().join("dist");
        fs::create_dir_all(dist.join("nested")).unwrap();
        fs::write(dist.join("nested/app.txt"), "ok").unwrap();
        symlink("nested/app.txt", dist.join("current")).unwrap();

        let mut nodes = Vec::new();
        collect_archive_nodes(
            &dist,
            Path::new("workspace/dist"),
            SymlinkMode::Preserve,
            &mut nodes,
        )
        .unwrap();
        sort_and_dedup_archive_nodes(&mut nodes);

        assert!(nodes.iter().any(|node| {
            node.archive_path == Path::new("workspace/dist")
                && matches!(node.kind, ArchiveNodeKind::Directory)
        }));
        assert!(nodes.iter().any(|node| {
            node.archive_path == Path::new("workspace/dist/nested")
                && matches!(node.kind, ArchiveNodeKind::Directory)
        }));
        assert!(nodes.iter().any(|node| {
            node.archive_path == Path::new("workspace/dist/nested/app.txt")
                && matches!(node.kind, ArchiveNodeKind::File)
        }));
        assert!(nodes.iter().any(|node| {
            node.archive_path == Path::new("workspace/dist/current")
                && matches!(node.kind, ArchiveNodeKind::Symlink(_))
        }));
    }

    #[test]
    fn dedups_nested_archive_nodes_from_overlapping_outputs() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(workspace.join("dist/nested")).unwrap();
        fs::write(workspace.join("dist/nested/app.txt"), "ok").unwrap();

        let mut nodes = Vec::new();
        collect_archive_nodes(
            &workspace.join("dist"),
            Path::new("workspace/dist"),
            SymlinkMode::Preserve,
            &mut nodes,
        )
        .unwrap();
        collect_archive_nodes(
            &workspace.join("dist/nested/app.txt"),
            Path::new("workspace/dist/nested/app.txt"),
            SymlinkMode::Preserve,
            &mut nodes,
        )
        .unwrap();
        sort_and_dedup_archive_nodes(&mut nodes);

        let app_paths = nodes
            .iter()
            .filter(|node| node.archive_path == Path::new("workspace/dist/nested/app.txt"))
            .count();
        assert_eq!(app_paths, 1);
    }
}
