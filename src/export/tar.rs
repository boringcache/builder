use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::export::archive::{
    ArchiveNodeKind, SymlinkMode, collect_archive_nodes, resolve_archive_entries,
    sort_and_dedup_archive_nodes,
};
use crate::schema::Pipeline;

pub fn export_pipeline_tar(pipeline: &Pipeline, output_path: &Path) -> Result<PathBuf> {
    export_pipeline_tar_with_rootfs(pipeline, output_path, None)
}

pub fn export_pipeline_tar_from_rootfs(
    pipeline: &Pipeline,
    output_path: &Path,
    rootfs_dir: &Path,
) -> Result<PathBuf> {
    export_pipeline_tar_with_rootfs(pipeline, output_path, Some(rootfs_dir))
}

pub fn export_pipeline_tar_zst(pipeline: &Pipeline, output_path: &Path) -> Result<PathBuf> {
    export_pipeline_tar_zst_with_rootfs(pipeline, output_path, None)
}

pub fn export_pipeline_tar_zst_from_rootfs(
    pipeline: &Pipeline,
    output_path: &Path,
    rootfs_dir: &Path,
) -> Result<PathBuf> {
    export_pipeline_tar_zst_with_rootfs(pipeline, output_path, Some(rootfs_dir))
}

fn export_pipeline_tar_with_rootfs(
    pipeline: &Pipeline,
    output_path: &Path,
    rootfs_dir: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    export_pipeline_tar_with_writer(
        pipeline,
        output_path,
        rootfs_dir,
        File::create(output_path)
            .with_context(|| format!("failed to create {}", output_path.display()))?,
    )
}

fn export_pipeline_tar_zst_with_rootfs(
    pipeline: &Pipeline,
    output_path: &Path,
    rootfs_dir: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let file = File::create(output_path)
        .with_context(|| format!("failed to create {}", output_path.display()))?;
    let mut encoder = zstd::Encoder::new(file, 1)?;
    encoder.multithread(4)?;
    export_pipeline_tar_with_writer(pipeline, output_path, rootfs_dir, encoder)
}

fn export_pipeline_tar_with_writer<W: Write + ArchiveWriterFinish>(
    pipeline: &Pipeline,
    output_path: &Path,
    rootfs_dir: Option<&Path>,
    writer: W,
) -> Result<PathBuf> {
    let export_paths = resolve_archive_entries(pipeline, rootfs_dir)?;

    let mut builder = ::tar::Builder::new(writer);
    builder.follow_symlinks(false);

    let mut nodes = Vec::new();
    for entry in export_paths {
        collect_archive_nodes(
            &entry.host_path,
            &entry.archive_prefix,
            SymlinkMode::Preserve,
            &mut nodes,
        )?;
    }
    sort_and_dedup_archive_nodes(&mut nodes);

    for node in nodes {
        let mut header = ::tar::Header::new_gnu();
        header.set_uid(node.metadata.uid() as u64);
        header.set_gid(node.metadata.gid() as u64);
        header.set_mtime(0);
        header.set_mode(node.metadata.permissions().mode() & 0o7777);

        match node.kind {
            ArchiveNodeKind::Directory => {
                header.set_entry_type(::tar::EntryType::Directory);
                header.set_size(0);
                header.set_cksum();
                builder.append_data(&mut header, &node.archive_path, io::empty())?;
            }
            ArchiveNodeKind::Symlink(target) => {
                header.set_entry_type(::tar::EntryType::Symlink);
                header.set_size(0);
                builder.append_link(&mut header, &node.archive_path, target)?;
            }
            ArchiveNodeKind::File => {
                header.set_entry_type(::tar::EntryType::Regular);
                header.set_size(node.metadata.len());
                header.set_cksum();
                let mut input = File::open(&node.host_path)
                    .with_context(|| format!("failed to open {}", node.host_path.display()))?;
                builder.append_data(&mut header, &node.archive_path, &mut input)?;
            }
        }
    }

    let writer = builder.into_inner()?;
    finish_archive_writer(writer)?;
    Ok(output_path.to_path_buf())
}

trait ArchiveWriterFinish {
    fn finish_archive(self) -> Result<()>;
}

impl ArchiveWriterFinish for File {
    fn finish_archive(self) -> Result<()> {
        drop(self);
        Ok(())
    }
}

impl<'a, W: Write> ArchiveWriterFinish for zstd::Encoder<'a, W> {
    fn finish_archive(self) -> Result<()> {
        self.finish()?;
        Ok(())
    }
}

fn finish_archive_writer<W: ArchiveWriterFinish>(writer: W) -> Result<()> {
    writer.finish_archive()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs::{self, File};
    use std::io::Read;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use crate::schema::{ExportConfig, ExportFormat, Input, Operation, Pipeline, Step};

    use super::{export_pipeline_tar, export_pipeline_tar_zst_from_rootfs};

    #[test]
    fn exports_tar_with_symlinks_and_directories() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let dist = workspace.join("dist");
        fs::create_dir_all(dist.join("nested")).unwrap();
        fs::write(dist.join("nested/app.txt"), "ok").unwrap();
        symlink("nested/app.txt", dist.join("current")).unwrap();

        let pipeline = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/arm64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: vec![Input {
                source: workspace.clone(),
                dest: "/workspace".to_string(),
                readonly: false,
            }],
            outputs: vec!["/workspace/dist".to_string()],
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("build".to_string()),
                run: "echo hi".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: None,
            })],
            export: Some(ExportConfig {
                format: ExportFormat::Tar,
                path: temp.path().join("out.tar"),
                reproducible: true,
            }),
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let archive = export_pipeline_tar(&pipeline, &temp.path().join("out.tar")).unwrap();
        let file = File::open(archive).unwrap();
        let mut archive = ::tar::Archive::new(file);
        let mut names = Vec::new();
        let mut symlink_ok = false;
        for entry_result in archive.entries().unwrap() {
            let entry = entry_result.unwrap();
            let path = entry.path().unwrap().to_path_buf();
            if path == Path::new("workspace/dist/current")
                && entry.header().entry_type().is_symlink()
            {
                symlink_ok = true;
            }
            names.push(path);
        }
        assert!(names.contains(&PathBuf::from("workspace/dist")));
        assert!(names.contains(&PathBuf::from("workspace/dist/nested")));
        assert!(names.contains(&PathBuf::from("workspace/dist/nested/app.txt")));
        assert!(symlink_ok);
    }

    #[test]
    fn exports_tar_zst_from_rootfs() {
        let temp = tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let dist = rootfs.join("workspace/dist");
        fs::create_dir_all(&dist).unwrap();
        fs::write(dist.join("app.js"), "console.log('ok');").unwrap();

        let pipeline = Pipeline {
            image: "alpine".to_string(),
            platform: "linux/arm64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: vec![Input {
                source: temp.path().join("readonly-src"),
                dest: "/src".to_string(),
                readonly: true,
            }],
            outputs: vec!["/workspace/dist".to_string()],
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("build".to_string()),
                run: "echo hi".to_string(),
                run_exec: None,
                run_mounts: Vec::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: None,
            })],
            export: Some(ExportConfig {
                format: ExportFormat::TarZst,
                path: temp.path().join("out.tar.zst"),
                reproducible: true,
            }),
            metadata: None,
            base_dir: temp.path().to_path_buf(),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: Default::default(),
            docker_context: None,
        };

        let archive = export_pipeline_tar_zst_from_rootfs(
            &pipeline,
            &temp.path().join("out.tar.zst"),
            &rootfs,
        )
        .unwrap();
        let file = File::open(archive).unwrap();
        let decoder = zstd::Decoder::new(file).unwrap();
        let mut archive = ::tar::Archive::new(decoder);
        let mut content = String::new();
        for entry_result in archive.entries().unwrap() {
            let mut entry = entry_result.unwrap();
            if entry.path().unwrap() == Path::new("workspace/dist/app.js") {
                entry.read_to_string(&mut content).unwrap();
            }
        }
        assert_eq!(content, "console.log('ok');");
    }
}
