use anyhow::{Result, bail};

use crate::schema::{HOST_RUNTIME_IMAGE, Pipeline, PipelineOrMulti};

use super::{
    ExecutionBackend, linux_exec::LinuxExecBackend, macos_container::MacosContainerBackend,
    macos_host::MacosHostBackend,
};

pub fn detect_for_loaded(loaded: &PipelineOrMulti) -> Result<Box<dyn ExecutionBackend>> {
    match pipeline_runtime_kind(loaded)? {
        RuntimeKind::Container => detect_container_backend(),
        RuntimeKind::Host => detect_host_backend(),
    }
}

fn detect_container_backend() -> Result<Box<dyn ExecutionBackend>> {
    if cfg!(target_os = "macos") {
        return Ok(Box::new(MacosContainerBackend::default()));
    }

    if cfg!(target_os = "linux") {
        return Ok(Box::new(LinuxExecBackend));
    }

    bail!("unsupported host OS: {}", std::env::consts::OS)
}

fn detect_host_backend() -> Result<Box<dyn ExecutionBackend>> {
    if cfg!(target_os = "macos") {
        return Ok(Box::new(MacosHostBackend));
    }
    bail!("runtime: host is only supported for trusted native macOS artifact builds")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeKind {
    Container,
    Host,
}

fn pipeline_runtime_kind(loaded: &PipelineOrMulti) -> Result<RuntimeKind> {
    let mut kinds = loaded
        .pipelines()
        .map(pipeline_runtime_kind_for_pipeline)
        .collect::<Vec<_>>();
    kinds.dedup();
    match kinds.as_slice() {
        [kind] => Ok(*kind),
        [] => bail!("recipe does not contain any targets"),
        _ => bail!("mixed runtime kinds are not supported in one run yet"),
    }
}

fn pipeline_runtime_kind_for_pipeline(pipeline: &Pipeline) -> RuntimeKind {
    if pipeline.image == HOST_RUNTIME_IMAGE {
        RuntimeKind::Host
    } else {
        RuntimeKind::Container
    }
}

trait PipelineCollection<'a> {
    type Iter: Iterator<Item = &'a Pipeline>;
    fn pipelines(&'a self) -> Self::Iter;
}

impl<'a> PipelineCollection<'a> for PipelineOrMulti {
    type Iter = Box<dyn Iterator<Item = &'a Pipeline> + 'a>;

    fn pipelines(&'a self) -> Self::Iter {
        match self {
            PipelineOrMulti::Single(pipeline) => Box::new(std::iter::once(pipeline)),
            PipelineOrMulti::Multi(multi) => Box::new(multi.targets.values()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    use indexmap::IndexMap;

    use crate::schema::{Input, Operation, Pipeline, PipelineOrMulti, Step};

    use super::{RuntimeKind, pipeline_runtime_kind, pipeline_runtime_kind_for_pipeline};

    fn sample_pipeline(image: &str) -> Pipeline {
        Pipeline {
            image: image.to_string(),
            platform: "linux/amd64".to_string(),
            workdir: "/workspace".to_string(),
            env: BTreeMap::new(),
            inputs: vec![Input {
                source: PathBuf::from("."),
                dest: "/workspace".to_string(),
                readonly: false,
            }],
            outputs: Vec::new(),
            setup_snapshot: None,
            operations: vec![Operation::Exec(Step {
                name: Some("step".to_string()),
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
            export: None,
            metadata: None,
            base_dir: PathBuf::from("."),
            needs: Vec::new(),
            stage_dependency_digests: BTreeMap::new(),
            stage_snapshot_follow_symlinks: BTreeSet::new(),
            docker_context: None,
        }
    }

    #[test]
    fn detects_host_runtime_from_reserved_image() {
        let pipeline = sample_pipeline(crate::schema::HOST_RUNTIME_IMAGE);
        assert_eq!(
            pipeline_runtime_kind_for_pipeline(&pipeline),
            RuntimeKind::Host
        );
    }

    #[test]
    fn rejects_mixed_runtime_runs() {
        let mut targets = IndexMap::new();
        targets.insert(
            "host".to_string(),
            sample_pipeline(crate::schema::HOST_RUNTIME_IMAGE),
        );
        targets.insert("container".to_string(), sample_pipeline("debian:bookworm"));
        let loaded = PipelineOrMulti::Multi(crate::schema::MultiTargetRecipe {
            targets,
            order: vec!["host".to_string(), "container".to_string()],
            base_dir: PathBuf::from("."),
        });
        let error = pipeline_runtime_kind(&loaded).unwrap_err().to_string();
        assert!(error.contains("mixed runtime kinds"));
    }
}
