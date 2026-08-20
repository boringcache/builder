use std::collections::BTreeMap;
use std::path::{Component, Path};

use anyhow::{Result, bail, ensure};
use indexmap::IndexMap;

use crate::schema::{
    ContextCopyOp, DockerContext, ExportConfig, ImageHealthcheck, ImageMetadata, MultiTargetRecipe,
    Operation, Pipeline, PipelineOrMulti, RemoteAddOp, StageCopyOp, Step, StepRunBindSource,
    StepRunMount,
};

use super::context::{
    expand_sources as expand_context_sources, normalize_source as normalize_context_source,
};
use super::parse::{Instruction, RunMount};
use super::vars::substitute;

struct CopyOpAttrs<'a> {
    exclude: &'a [String],
    parents: bool,
    chown: Option<&'a str>,
    chmod: Option<&'a str>,
}

pub fn convert(
    instructions: Vec<Instruction>,
    docker_context: &DockerContext,
    platform_override: Option<&str>,
    build_args: &[(String, String)],
    export: Option<ExportConfig>,
) -> Result<PipelineOrMulti> {
    let explicit_stages = split_stages(instructions)?;
    ensure!(
        !explicit_stages.is_empty(),
        "Dockerfile must contain at least one FROM"
    );
    let explicit_stage_names = explicit_stages.iter().map(stage_name).collect::<Vec<_>>();
    let stages = materialize_external_copy_from_stages(explicit_stages, &explicit_stage_names);

    let mut global_args: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in build_args {
        global_args.insert(k.clone(), v.clone());
    }

    if stages.len() == 1 {
        let pipeline = convert_stage(
            &stages[0],
            docker_context,
            platform_override,
            &global_args,
            export,
        )?;
        Ok(PipelineOrMulti::Single(pipeline))
    } else {
        let multi = convert_multi_stage(
            &stages,
            docker_context,
            platform_override,
            &global_args,
            export,
        )?;
        Ok(PipelineOrMulti::Multi(multi))
    }
}

struct Stage {
    image: String,
    alias: Option<String>,
    platform: Option<String>,
    instructions: Vec<Instruction>,
    explicit_index: Option<usize>,
}

fn split_stages(instructions: Vec<Instruction>) -> Result<Vec<Stage>> {
    let mut stages = Vec::new();
    let mut current: Option<Stage> = None;
    let mut pre_from_args: Vec<Instruction> = Vec::new();
    let mut explicit_index = 0usize;

    for inst in instructions {
        match inst {
            Instruction::From {
                image,
                alias,
                platform,
            } => {
                if let Some(stage) = current.take() {
                    stages.push(stage);
                }
                let mut stage_instructions = Vec::new();
                for arg in &pre_from_args {
                    stage_instructions.push(arg.clone());
                }
                current = Some(Stage {
                    image,
                    alias,
                    platform,
                    instructions: stage_instructions,
                    explicit_index: Some(explicit_index),
                });
                explicit_index += 1;
            }
            Instruction::Arg { .. } if current.is_none() => {
                pre_from_args.push(inst);
            }
            _ => {
                if let Some(stage) = current.as_mut() {
                    stage.instructions.push(inst);
                } else {
                    bail!("instruction before FROM is not allowed (except ARG)");
                }
            }
        }
    }

    if let Some(stage) = current {
        stages.push(stage);
    }

    Ok(stages)
}

fn stage_name(stage: &Stage) -> String {
    stage.alias.clone().unwrap_or_else(|| {
        format!(
            "stage-{}",
            stage
                .explicit_index
                .expect("explicit stage index missing for unnamed stage")
        )
    })
}

fn is_explicit_stage_ref(value: &str, explicit_stage_names: &[String]) -> bool {
    if let Ok(idx) = value.parse::<usize>() {
        return idx < explicit_stage_names.len();
    }
    explicit_stage_names.iter().any(|name| name == value)
}

fn materialize_external_copy_from_stages(
    mut explicit_stages: Vec<Stage>,
    explicit_stage_names: &[String],
) -> Vec<Stage> {
    let mut external_refs: IndexMap<String, String> = IndexMap::new();

    for stage in &mut explicit_stages {
        for instruction in &mut stage.instructions {
            let Instruction::Copy {
                from: Some(from), ..
            } = instruction
            else {
                continue;
            };
            if is_explicit_stage_ref(from, explicit_stage_names) {
                continue;
            }

            let alias = if let Some(alias) = external_refs.get(from) {
                alias.clone()
            } else {
                let alias = format!("external-copy-from-{}", external_refs.len());
                external_refs.insert(from.clone(), alias.clone());
                alias
            };
            *from = alias;
        }
    }

    if external_refs.is_empty() {
        return explicit_stages;
    }

    let mut stages = external_refs
        .into_iter()
        .map(|(image, alias)| Stage {
            image,
            alias: Some(alias),
            platform: None,
            instructions: Vec::new(),
            explicit_index: None,
        })
        .collect::<Vec<_>>();
    stages.extend(explicit_stages);
    stages
}

fn convert_stage(
    stage: &Stage,
    docker_context: &DockerContext,
    platform_override: Option<&str>,
    global_args: &BTreeMap<String, String>,
    export: Option<ExportConfig>,
) -> Result<Pipeline> {
    let target_platform = platform_override.map(String::from).unwrap_or_else(|| {
        crate::util::platform::default_linux_platform()
            .unwrap_or_else(|_| "linux/amd64".to_string())
    });

    let mut vars = global_args.clone();
    vars.extend(docker_platform_vars(&target_platform)?);
    apply_leading_arg_defaults(&mut vars, &stage.instructions);
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut workdir = "/".to_string();
    let mut operations: Vec<Operation> = Vec::new();
    let mut metadata = ImageMetadata::default();
    let mut shell = vec!["/bin/sh".to_string(), "-c".to_string()];
    let mut outputs: Vec<String> = Vec::new();

    let image = substitute(&stage.image, &vars);
    let platform = stage
        .platform
        .as_deref()
        .map(|value| substitute(value, &vars))
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(target_platform);

    for inst in &stage.instructions {
        match inst {
            Instruction::Arg { key, default } => {
                if !vars.contains_key(key)
                    && let Some(def) = default
                {
                    vars.insert(key.clone(), substitute(def, &vars));
                }
            }
            Instruction::Env { entries } => {
                for (key, value) in entries {
                    let resolved = substitute(value, &vars);
                    vars.insert(key.clone(), resolved.clone());
                    env.insert(key.clone(), resolved);
                }
            }
            Instruction::Workdir { path } => {
                let resolved = substitute(path, &vars);
                if resolved.starts_with('/') {
                    workdir = resolved;
                } else {
                    workdir = format!("{}/{}", workdir.trim_end_matches('/'), resolved);
                }
            }
            Instruction::Run {
                command,
                exec,
                mounts,
            } => {
                let resolved = if exec.is_some() {
                    command.clone()
                } else {
                    substitute(command, &vars)
                };
                let resolved_exec = exec.clone();
                let resolved_mounts = mounts
                    .iter()
                    .map(|mount| convert_run_mount(mount, &workdir, &vars))
                    .collect::<Result<Vec<_>>>()?;
                let step_name = make_step_name(&resolved, operations.len());
                operations.push(Operation::Exec(Step {
                    name: Some(step_name),
                    run: resolved,
                    run_exec: resolved_exec,
                    run_mounts: resolved_mounts,
                    env: BTreeMap::new(),
                    workdir: Some(workdir.clone()),
                    shell: if exec.is_none() && shell != ["/bin/sh", "-c"] {
                        Some(shell.join(" "))
                    } else {
                        None
                    },
                    build_cache_inputs: None,
                    build_cache: None,
                    tag: None,
                }));
            }
            Instruction::Copy {
                from: None,
                sources,
                dest,
                exclude,
                parents,
                chown,
                chmod,
            } => {
                let resolved_dest = resolve_dest(&substitute(dest, &vars), &workdir);
                operations.push(Operation::CopyFromContext(make_context_copy_op(
                    docker_context,
                    sources,
                    &resolved_dest,
                    &vars,
                    operations.len(),
                    false,
                    CopyOpAttrs {
                        exclude,
                        parents: *parents,
                        chown: chown.as_deref(),
                        chmod: chmod.as_deref(),
                    },
                )?));
            }
            Instruction::Copy {
                from: Some(from),
                sources,
                dest,
                exclude,
                parents,
                chown,
                chmod,
            } => {
                let resolved_dest = resolve_dest(&substitute(dest, &vars), &workdir);
                operations.push(Operation::CopyFromStage(make_stage_copy_op(
                    from,
                    sources,
                    &resolved_dest,
                    &vars,
                    operations.len(),
                    CopyOpAttrs {
                        exclude,
                        parents: *parents,
                        chown: chown.as_deref(),
                        chmod: chmod.as_deref(),
                    },
                )?));
            }
            Instruction::Add {
                sources,
                dest,
                checksum,
            } => {
                let resolved_dest = resolve_dest(&substitute(dest, &vars), &workdir);
                let local_sources: Vec<String> = sources
                    .iter()
                    .filter(|source| !is_remote_source(source))
                    .cloned()
                    .collect();
                let remote_sources: Vec<String> = sources
                    .iter()
                    .filter(|source| is_remote_source(source))
                    .cloned()
                    .collect();
                if checksum.is_some() && (!local_sources.is_empty() || remote_sources.len() != 1) {
                    bail!("ADD --checksum is only supported for a single remote source");
                }
                if !local_sources.is_empty() {
                    operations.push(Operation::CopyFromContext(make_context_copy_op(
                        docker_context,
                        &local_sources,
                        &resolved_dest,
                        &vars,
                        operations.len(),
                        true,
                        CopyOpAttrs {
                            exclude: &[],
                            parents: false,
                            chown: None,
                            chmod: None,
                        },
                    )?));
                }
                for remote in &remote_sources {
                    operations.push(Operation::AddRemote(make_remote_add_op(
                        remote,
                        &resolved_dest,
                        &vars,
                        operations.len(),
                        checksum.as_deref(),
                    )));
                }
            }
            Instruction::Expose { port } => {
                metadata.expose.push(substitute(port, &vars));
            }
            Instruction::Label { key, value } => {
                metadata
                    .labels
                    .insert(key.clone(), substitute(value, &vars));
            }
            Instruction::User { user } => {
                metadata.user = Some(substitute(user, &vars));
            }
            Instruction::Entrypoint { exec } => {
                metadata.entrypoint = Some(exec.iter().map(|s| substitute(s, &vars)).collect());
            }
            Instruction::Cmd { exec } => {
                metadata.cmd = Some(exec.iter().map(|s| substitute(s, &vars)).collect());
            }
            Instruction::Healthcheck {
                test,
                interval_nanos,
                timeout_nanos,
                start_period_nanos,
                start_interval_nanos,
                retries,
            } => {
                metadata.healthcheck = Some(match test {
                    Some(test) => ImageHealthcheck::Command {
                        test: test.iter().map(|s| substitute(s, &vars)).collect(),
                        interval_nanos: *interval_nanos,
                        timeout_nanos: *timeout_nanos,
                        start_period_nanos: *start_period_nanos,
                        start_interval_nanos: *start_interval_nanos,
                        retries: *retries,
                    },
                    None => ImageHealthcheck::None,
                });
            }
            Instruction::Volume { paths } => {
                for p in paths {
                    metadata.volumes.push(substitute(p, &vars));
                }
            }
            Instruction::StopSignal { signal } => {
                metadata.stop_signal = Some(substitute(signal, &vars));
            }
            Instruction::Shell { exec } => {
                shell = exec.iter().map(|s| substitute(s, &vars)).collect();
            }
            Instruction::From { .. } => unreachable!(),
        }
    }

    if operations.is_empty() {
        operations.push(Operation::Exec(Step {
            name: Some("finalize".to_string()),
            run: "true".to_string(),
            run_exec: None,
            run_mounts: Vec::new(),
            env: BTreeMap::new(),
            workdir: Some(workdir.clone()),
            shell: None,
            build_cache_inputs: None,
            build_cache: None,
            tag: None,
        }));
    }

    // Dockerfile builds produce a full image; if no explicit outputs were
    // collected (e.g. single-stage with no COPY --from), default to the
    // entire rootfs so the exported image includes all installed packages
    // and filesystem changes (matching Docker's behavior).
    if outputs.is_empty() {
        outputs.push("/".to_string());
    }

    let has_metadata = metadata.entrypoint.is_some()
        || metadata.cmd.is_some()
        || metadata.healthcheck.is_some()
        || metadata.user.is_some()
        || !metadata.expose.is_empty()
        || !metadata.labels.is_empty()
        || !metadata.volumes.is_empty()
        || metadata.stop_signal.is_some();

    Ok(Pipeline {
        image,
        platform,
        workdir,
        env,
        inputs: Vec::new(),
        outputs,
        setup_snapshot: None,
        operations,
        export,
        metadata: if has_metadata { Some(metadata) } else { None },
        base_dir: docker_context.root.clone(),
        needs: Vec::new(),
        stage_dependency_digests: BTreeMap::new(),
        stage_snapshot_follow_symlinks: Default::default(),
        docker_context: Some(docker_context.clone()),
    })
}

fn convert_multi_stage(
    stages: &[Stage],
    docker_context: &DockerContext,
    platform_override: Option<&str>,
    global_args: &BTreeMap<String, String>,
    export: Option<ExportConfig>,
) -> Result<MultiTargetRecipe> {
    let mut targets = IndexMap::new();
    let mut order = Vec::new();
    let stage_names = stages.iter().map(stage_name).collect::<Vec<_>>();
    let explicit_stage_names = stages
        .iter()
        .filter(|stage| stage.explicit_index.is_some())
        .map(stage_name)
        .collect::<Vec<_>>();

    for (idx, stage) in stages.iter().enumerate() {
        let name = &stage_names[idx];
        let is_final = stage.explicit_index == stages.iter().filter_map(|s| s.explicit_index).max();
        let stage_export = if is_final { export.clone() } else { None };

        let mut pipeline = convert_stage(
            stage,
            docker_context,
            platform_override,
            global_args,
            stage_export,
        )?;

        let mut needs = Vec::new();
        for operation in &mut pipeline.operations {
            match operation {
                Operation::CopyFromStage(copy) => {
                    let dep_name =
                        resolve_stage_ref(&copy.stage, &stage_names, &explicit_stage_names)?;
                    if !needs.contains(&dep_name) {
                        needs.push(dep_name.clone());
                    }
                    copy.stage = dep_name;
                }
                Operation::Exec(step) => {
                    for mount in &mut step.run_mounts {
                        let StepRunMount::Bind {
                            source: StepRunBindSource::Stage { stage, .. },
                            ..
                        } = mount
                        else {
                            continue;
                        };
                        let dep_name =
                            resolve_stage_ref(stage, &stage_names, &explicit_stage_names)?;
                        if !needs.contains(&dep_name) {
                            needs.push(dep_name.clone());
                        }
                        *stage = dep_name;
                    }
                }
                Operation::CopyFromContext(_) | Operation::AddRemote(_) => {}
            }
        }

        if !is_final && pipeline.outputs.is_empty() {
            pipeline.outputs.push(pipeline.workdir.clone());
        }

        pipeline.needs = needs;
        order.push(name.clone());
        targets.insert(name.clone(), pipeline);
    }

    Ok(MultiTargetRecipe {
        targets,
        order,
        base_dir: docker_context.root.clone(),
    })
}

fn resolve_stage_ref(
    name: &str,
    stage_names: &[String],
    explicit_stage_names: &[String],
) -> Result<String> {
    if let Ok(idx) = name.parse::<usize>() {
        if idx < explicit_stage_names.len() {
            return Ok(explicit_stage_names[idx].clone());
        }
        bail!("stage index {idx} out of range");
    }
    if stage_names.contains(&name.to_string()) {
        Ok(name.to_string())
    } else {
        bail!("unknown build stage: {name}");
    }
}

fn apply_leading_arg_defaults(vars: &mut BTreeMap<String, String>, instructions: &[Instruction]) {
    for instruction in instructions {
        let Instruction::Arg { key, default } = instruction else {
            break;
        };
        if !vars.contains_key(key)
            && let Some(default) = default
        {
            vars.insert(key.clone(), substitute(default, vars));
        }
    }
}

fn docker_platform_vars(target_platform: &str) -> Result<BTreeMap<String, String>> {
    let (target_os, target_arch) = target_platform
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("invalid platform '{target_platform}'"))?;
    Ok(BTreeMap::from([
        ("TARGETPLATFORM".to_string(), target_platform.to_string()),
        ("TARGETOS".to_string(), target_os.to_string()),
        ("TARGETARCH".to_string(), target_arch.to_string()),
        // boringbuilder currently executes stages on the selected target platform,
        // so build and target platform are the same until we support distinct
        // build-vs-target execution semantics.
        ("BUILDPLATFORM".to_string(), target_platform.to_string()),
        ("BUILDOS".to_string(), target_os.to_string()),
        ("BUILDARCH".to_string(), target_arch.to_string()),
    ]))
}

fn make_context_copy_op(
    docker_context: &DockerContext,
    sources: &[String],
    dest: &str,
    vars: &BTreeMap<String, String>,
    op_index: usize,
    extract_archives: bool,
    attrs: CopyOpAttrs<'_>,
) -> Result<ContextCopyOp> {
    let mut resolved_sources = Vec::new();
    for source in sources {
        let source = substitute(source, vars);
        if attrs.parents {
            resolved_sources.extend(expand_parent_preserving_context_sources(
                docker_context,
                &source,
            )?);
        } else {
            resolved_sources.extend(expand_context_sources(docker_context, &source)?);
        }
    }
    let resolved_exclude = attrs
        .exclude
        .iter()
        .map(|pattern| substitute(pattern, vars))
        .collect();
    let src_display: Vec<&str> = sources.iter().map(|source| source.as_str()).collect();
    Ok(ContextCopyOp {
        name: Some(format!(
            "step-{}: copy {} → {}",
            op_index + 1,
            src_display.join(", "),
            dest
        )),
        sources: resolved_sources,
        dest: dest.to_string(),
        exclude: resolved_exclude,
        extract_archives,
        preserve_parents: attrs.parents,
        chown: attrs.chown.map(|value| substitute(value, vars)),
        chmod: attrs.chmod.map(|value| substitute(value, vars)),
    })
}

fn make_stage_copy_op(
    stage: &str,
    sources: &[String],
    dest: &str,
    vars: &BTreeMap<String, String>,
    op_index: usize,
    attrs: CopyOpAttrs<'_>,
) -> Result<StageCopyOp> {
    let resolved_sources = sources
        .iter()
        .map(|src| {
            let source = substitute(src, vars);
            if attrs.parents {
                encode_parent_preserving_stage_source(&source)
            } else {
                Ok(source)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let resolved_exclude = attrs
        .exclude
        .iter()
        .map(|pattern| substitute(pattern, vars))
        .collect();
    let src_display: Vec<&str> = sources.iter().map(|source| source.as_str()).collect();
    Ok(StageCopyOp {
        name: Some(format!(
            "step-{}: copy {} from {} → {}",
            op_index + 1,
            src_display.join(", "),
            stage,
            dest
        )),
        stage: stage.to_string(),
        sources: resolved_sources,
        dest: dest.to_string(),
        exclude: resolved_exclude,
        preserve_parents: attrs.parents,
        follow_symlinks: true,
        chown: attrs.chown.map(|value| substitute(value, vars)),
        chmod: attrs.chmod.map(|value| substitute(value, vars)),
    })
}

fn make_remote_add_op(
    source: &str,
    dest: &str,
    vars: &BTreeMap<String, String>,
    op_index: usize,
    checksum: Option<&str>,
) -> RemoteAddOp {
    let resolved_source = substitute(source, vars);
    RemoteAddOp {
        name: Some(format!("step-{}: add {} → {}", op_index + 1, source, dest)),
        url: resolved_source,
        dest: dest.to_string(),
        checksum: checksum.map(|value| substitute(value, vars)),
    }
}

fn resolve_dest(dest: &str, workdir: &str) -> String {
    if dest.starts_with('/') {
        dest.to_string()
    } else {
        format!("{}/{}", workdir.trim_end_matches('/'), dest)
    }
}

fn make_step_name(command: &str, index: usize) -> String {
    let first_line = command.lines().next().unwrap_or(command);
    let trimmed = first_line.trim();
    let short = if trimmed.len() > 60 {
        format!("{}...", &trimmed[..57])
    } else {
        trimmed.to_string()
    };
    format!("step-{}: {}", index + 1, short)
}

fn convert_run_mount(
    mount: &RunMount,
    workdir: &str,
    vars: &BTreeMap<String, String>,
) -> Result<StepRunMount> {
    match mount {
        RunMount::Cache {
            target,
            id,
            readonly,
            sharing,
        } => {
            let target = resolve_dest(&substitute(target, vars), workdir);
            let id = id
                .as_deref()
                .map(|value| substitute(value, vars))
                .unwrap_or_else(|| target.clone());
            Ok(StepRunMount::Cache {
                target,
                id,
                key: None,
                restore_from: Vec::new(),
                readonly: *readonly,
                sharing: *sharing,
            })
        }
        RunMount::Bind {
            target,
            source,
            from,
            readonly,
        } => {
            let target = resolve_dest(&substitute(target, vars), workdir);
            let source = match from.as_deref().map(|value| substitute(value, vars)) {
                Some(from) if from != "context" => {
                    let path = source
                        .as_deref()
                        .map(|value| substitute(value, vars))
                        .unwrap_or_else(|| "/".to_string());
                    StepRunBindSource::Stage {
                        stage: from,
                        path: normalize_stage_mount_source(&path)?,
                    }
                }
                _ => {
                    let path = source
                        .as_deref()
                        .map(|value| substitute(value, vars))
                        .unwrap_or_else(|| ".".to_string());
                    StepRunBindSource::Context {
                        path: normalize_context_bind_source(&path)?,
                    }
                }
            };
            Ok(StepRunMount::Bind {
                target,
                source,
                readonly: *readonly,
            })
        }
        RunMount::Tmpfs { target, size } => Ok(StepRunMount::Tmpfs {
            target: resolve_dest(&substitute(target, vars), workdir),
            size: size.as_ref().map(|value| substitute(value, vars)),
        }),
        RunMount::Secret {
            target,
            id,
            env,
            required,
            mode,
            uid,
            gid,
        } => Ok(StepRunMount::Secret {
            target: resolve_dest(&substitute(target, vars), workdir),
            id: substitute(id, vars),
            env: env.as_ref().map(|value| substitute(value, vars)),
            required: *required,
            mode: *mode,
            uid: *uid,
            gid: *gid,
        }),
        RunMount::Ssh {
            target,
            id,
            required,
            mode,
            uid,
            gid,
        } => Ok(StepRunMount::Ssh {
            target: resolve_dest(&substitute(target, vars), workdir),
            id: substitute(id, vars),
            required: *required,
            mode: *mode,
            uid: *uid,
            gid: *gid,
        }),
    }
}

fn normalize_context_bind_source(source: &str) -> Result<String> {
    let normalized = normalize_context_source(source)?;
    let rendered = normalized.to_string_lossy().replace('\\', "/");
    Ok(if rendered.is_empty() {
        ".".to_string()
    } else {
        rendered
    })
}

fn normalize_stage_mount_source(source: &str) -> Result<String> {
    let mut normalized = String::new();
    for component in Path::new(source).components() {
        match component {
            Component::Prefix(_) => bail!("stage mount source is invalid: {source}"),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => bail!("stage mount source escapes its root: {source}"),
            Component::Normal(part) => {
                if !normalized.is_empty() {
                    normalized.push('/');
                }
                normalized.push_str(&part.to_string_lossy());
            }
        }
    }

    Ok(if normalized.is_empty() {
        "/".to_string()
    } else {
        format!("/{normalized}")
    })
}

fn expand_parent_preserving_context_sources(
    docker_context: &DockerContext,
    source: &str,
) -> Result<Vec<String>> {
    let (lookup_source, parent_marker) = split_copy_parent_marker(source)?;
    let expanded = expand_context_sources(docker_context, &lookup_source)?;
    Ok(expanded
        .into_iter()
        .map(|resolved| reapply_copy_parent_marker(&resolved, parent_marker.as_deref()))
        .collect())
}

fn encode_parent_preserving_stage_source(source: &str) -> Result<String> {
    let (lookup_source, parent_marker) = split_copy_parent_marker(source)?;
    let normalized = normalize_stage_mount_source(&lookup_source)?;
    Ok(reapply_copy_parent_marker(
        &normalized,
        parent_marker.as_deref(),
    ))
}

fn split_copy_parent_marker(source: &str) -> Result<(String, Option<String>)> {
    let trimmed = source.trim();
    ensure!(!trimmed.is_empty(), "COPY source must not be empty");
    if let Some(rest) = trimmed.strip_prefix("/./") {
        ensure!(!rest.is_empty(), "COPY source cut point must keep a suffix");
        return Ok((format!("/{rest}"), Some("/".to_string())));
    }
    if let Some(rest) = trimmed.strip_prefix("./") {
        ensure!(!rest.is_empty(), "COPY source cut point must keep a suffix");
        return Ok((rest.to_string(), Some(String::new())));
    }
    if let Some((prefix, suffix)) = trimmed.split_once("/./") {
        ensure!(
            !suffix.is_empty(),
            "COPY source cut point must keep a suffix"
        );
        let lookup = if prefix == "/" {
            format!("/{suffix}")
        } else {
            format!("{prefix}/{suffix}")
        };
        return Ok((lookup, Some(prefix.to_string())));
    }
    Ok((trimmed.to_string(), None))
}

fn reapply_copy_parent_marker(resolved: &str, marker: Option<&str>) -> String {
    match marker {
        None => resolved.to_string(),
        Some("") => {
            format!("./{}", resolved.trim_start_matches('/'))
        }
        Some("/") => format!("/./{}", resolved.trim_start_matches('/')),
        Some(marker) => {
            let marker = marker.trim_end_matches('/');
            let normalized_resolved = resolved.trim_start_matches('/');
            let normalized_marker = marker.trim_start_matches('/');
            let suffix = normalized_resolved
                .strip_prefix(normalized_marker)
                .and_then(|rest| rest.strip_prefix('/'))
                .unwrap_or(normalized_resolved);
            format!("{marker}/./{suffix}")
        }
    }
}

fn is_remote_source(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dockerfile::parse::parse;
    use crate::schema::{DockerContext, Operation};
    use tempfile::tempdir;

    #[test]
    fn converts_simple_dockerfile() {
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("app.py"), "print('hi')").unwrap();

        let instructions = parse(
            "FROM python:3.12\nWORKDIR /app\nCOPY app.py /app/\nRUN pip install flask\nCMD [\"python\", \"app.py\"]\n",
        ).unwrap();
        let context = DockerContext {
            root: temp.path().to_path_buf(),
            ignore_patterns: Vec::new(),
        };

        let result = convert(instructions, &context, None, &[], None).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            _ => panic!("expected single"),
        };

        assert_eq!(pipeline.image, "python:3.12");
        assert_eq!(pipeline.workdir, "/app");
        assert_eq!(pipeline.operations.len(), 2);
        assert!(matches!(
            &pipeline.operations[0],
            Operation::CopyFromContext(op)
                if op.sources == vec!["app.py".to_string()]
                    && op.dest == "/app/"
                    && op.chown.is_none()
                    && op.chmod.is_none()
        ));
        assert!(matches!(
            &pipeline.operations[1],
            Operation::Exec(step) if step.run.contains("pip install flask") && step.run_exec.is_none()
        ));
        assert!(pipeline.inputs.is_empty());
        assert_eq!(
            pipeline.metadata.as_ref().unwrap().cmd,
            Some(vec!["python".to_string(), "app.py".to_string()])
        );
    }

    #[test]
    fn converts_multi_stage() {
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("main.go"), "package main").unwrap();

        let instructions = parse(
            r#"
FROM golang:1.22 AS builder
WORKDIR /src
COPY main.go /src/
RUN go build -o /app

FROM alpine:3.19
COPY --from=builder /app /app
CMD ["/app"]
"#,
        )
        .unwrap();
        let context = DockerContext {
            root: temp.path().to_path_buf(),
            ignore_patterns: Vec::new(),
        };

        let result = convert(instructions, &context, None, &[], None).unwrap();
        let multi = match result {
            PipelineOrMulti::Multi(m) => m,
            _ => panic!("expected multi"),
        };

        assert_eq!(multi.order, vec!["builder", "stage-1"]);
        assert_eq!(multi.targets["builder"].image, "golang:1.22");
        assert_eq!(multi.targets["stage-1"].image, "alpine:3.19");
        assert!(matches!(
            &multi.targets["builder"].operations[0],
            Operation::CopyFromContext(op)
                if op.sources == vec!["main.go".to_string()]
                    && op.dest == "/src/"
                    && op.chown.is_none()
                    && op.chmod.is_none()
        ));
        assert!(matches!(
            &multi.targets["builder"].operations[1],
            Operation::Exec(step) if step.run.contains("go build") && step.run_exec.is_none()
        ));
        assert!(matches!(
            &multi.targets["stage-1"].operations[0],
            Operation::CopyFromStage(op)
                if op.stage == "builder"
                    && op.sources == vec!["/app".to_string()]
                    && op.dest == "/app"
                    && op.chown.is_none()
                    && op.chmod.is_none()
        ));
    }

    #[test]
    fn converts_external_image_copy_from_into_synthetic_stage() {
        let temp = tempdir().unwrap();
        let instructions = parse(
            r#"
FROM alpine:3.19
COPY --from=busybox:1.36 /bin/busybox /bin/busybox
"#,
        )
        .unwrap();
        let context = DockerContext {
            root: temp.path().to_path_buf(),
            ignore_patterns: Vec::new(),
        };

        let result = convert(instructions, &context, None, &[], None).unwrap();
        let multi = match result {
            PipelineOrMulti::Multi(m) => m,
            _ => panic!("expected multi"),
        };

        assert_eq!(multi.order.len(), 2);
        let external = &multi.order[0];
        assert_eq!(multi.targets[external].image, "busybox:1.36");
        assert_eq!(multi.targets["stage-0"].image, "alpine:3.19");
        assert_eq!(multi.targets["stage-0"].needs, vec![external.clone()]);
        assert!(matches!(
            &multi.targets["stage-0"].operations[0],
            Operation::CopyFromStage(op)
                if op.stage == *external
                    && op.sources == vec!["/bin/busybox".to_string()]
                    && op.dest == "/bin/busybox"
        ));
    }

    #[test]
    fn expands_copy_glob_sources_from_context() {
        let temp = tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src/a.rb"), "puts 'a'\n").unwrap();
        std::fs::write(temp.path().join("src/b.rb"), "puts 'b'\n").unwrap();
        std::fs::write(temp.path().join("src/c.py"), "print('c')\n").unwrap();

        let instructions = parse("FROM alpine\nCOPY src/*.rb /app/\n").unwrap();
        let context = DockerContext {
            root: temp.path().to_path_buf(),
            ignore_patterns: Vec::new(),
        };

        let result = convert(instructions, &context, None, &[], None).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            _ => panic!("expected single"),
        };

        assert!(matches!(
            &pipeline.operations[0],
            Operation::CopyFromContext(op)
                if op.sources == vec!["src/a.rb".to_string(), "src/b.rb".to_string()]
                    && op.dest == "/app/"
        ));
    }

    #[test]
    fn keeps_numeric_stage_refs_stable_when_external_copy_sources_exist() {
        let temp = tempdir().unwrap();
        let instructions = parse(
            r#"
FROM busybox:1.36 AS builder
RUN echo hi
FROM alpine:3.19
COPY --from=0 /bin/sh /bin/sh
COPY --from=busybox:1.36 /bin/busybox /bin/busybox
"#,
        )
        .unwrap();
        let context = DockerContext {
            root: temp.path().to_path_buf(),
            ignore_patterns: Vec::new(),
        };

        let result = convert(instructions, &context, None, &[], None).unwrap();
        let multi = match result {
            PipelineOrMulti::Multi(m) => m,
            _ => panic!("expected multi"),
        };

        let final_target = &multi.targets["stage-1"];
        assert!(final_target.needs.contains(&"builder".to_string()));
        let external_dep = final_target
            .needs
            .iter()
            .find(|name| *name != "builder")
            .expect("external image dependency should be present");
        assert!(matches!(
            &final_target.operations[0],
            Operation::CopyFromStage(op) if op.stage == "builder"
        ));
        assert!(matches!(
            &final_target.operations[1],
            Operation::CopyFromStage(op) if op.stage == *external_dep
        ));
        assert_eq!(multi.targets[external_dep].image, "busybox:1.36");
    }

    #[test]
    fn converts_copy_parents_and_extended_run_mounts() {
        let temp = tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src/lib")).unwrap();
        std::fs::write(temp.path().join("src/lib/app.rb"), "puts :ok").unwrap();
        let instructions = parse(
            r#"
FROM alpine
COPY --parents src/./lib/app.rb /workspace/
RUN --mount=type=tmpfs,target=/tmp/build,size=64m \
    --mount=type=secret,id=npmrc,target=/root/.npmrc,env=NPMRC,required=true,mode=0400 \
    --mount=type=ssh,id=git,target=/run/ssh-agent \
    echo hi
"#,
        )
        .unwrap();
        let context = DockerContext {
            root: temp.path().to_path_buf(),
            ignore_patterns: Vec::new(),
        };

        let result = convert(instructions, &context, None, &[], None).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            _ => panic!("expected single"),
        };

        assert!(matches!(
            &pipeline.operations[0],
            Operation::CopyFromContext(op)
                if op.preserve_parents
                    && op.sources == vec!["src/./lib/app.rb".to_string()]
                    && op.dest == "/workspace/"
        ));
        assert!(matches!(
            &pipeline.operations[1],
            Operation::Exec(step)
                if step.run_mounts == vec![
                    StepRunMount::Tmpfs {
                        target: "/tmp/build".to_string(),
                        size: Some("64m".to_string()),
                    },
                    StepRunMount::Secret {
                        target: "/root/.npmrc".to_string(),
                        id: "npmrc".to_string(),
                        env: Some("NPMRC".to_string()),
                        required: true,
                        mode: Some(0o400),
                        uid: None,
                        gid: None,
                    },
                    StepRunMount::Ssh {
                        target: "/run/ssh-agent".to_string(),
                        id: "git".to_string(),
                        required: false,
                        mode: None,
                        uid: None,
                        gid: None,
                    },
                ]
        ));
    }

    #[test]
    fn supports_from_platform_and_predefined_platform_args() {
        let temp = tempdir().unwrap();
        let instructions = parse(
            "ARG BASE=alpine:3.19\nFROM --platform=$TARGETPLATFORM ${BASE} AS base\nRUN echo hi\n",
        )
        .unwrap();
        let context = DockerContext {
            root: temp.path().to_path_buf(),
            ignore_patterns: Vec::new(),
        };

        let result = convert(instructions, &context, Some("linux/arm64"), &[], None).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            _ => panic!("expected single"),
        };

        assert_eq!(pipeline.image, "alpine:3.19");
        assert_eq!(pipeline.platform, "linux/arm64");
    }

    #[test]
    fn applies_env_and_arg_substitution() {
        let temp = tempdir().unwrap();
        let instructions = parse(
            "FROM alpine\nARG VERSION=1.0\nENV APP_VERSION=$VERSION\nRUN echo $APP_VERSION\n",
        )
        .unwrap();
        let context = DockerContext {
            root: temp.path().to_path_buf(),
            ignore_patterns: Vec::new(),
        };

        let result = convert(instructions, &context, None, &[], None).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            _ => panic!("expected single"),
        };

        assert_eq!(pipeline.env.get("APP_VERSION").unwrap(), "1.0");
        assert!(matches!(
            &pipeline.operations[0],
            Operation::Exec(step) if step.run.contains("1.0")
        ));
    }

    #[test]
    fn applies_multiple_env_pairs_and_substitutes_later_values() {
        let temp = tempdir().unwrap();
        let instructions = parse(
            "FROM alpine\nENV APP_HOME=/app PATH=$APP_HOME/bin:/usr/bin MODE=prod\nRUN echo $PATH $MODE\n",
        )
        .unwrap();
        let context = DockerContext {
            root: temp.path().to_path_buf(),
            ignore_patterns: Vec::new(),
        };

        let result = convert(instructions, &context, None, &[], None).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            _ => panic!("expected single"),
        };

        assert_eq!(pipeline.env.get("APP_HOME").unwrap(), "/app");
        assert_eq!(pipeline.env.get("PATH").unwrap(), "/app/bin:/usr/bin");
        assert_eq!(pipeline.env.get("MODE").unwrap(), "prod");
        assert!(matches!(
            &pipeline.operations[0],
            Operation::Exec(step)
                if step.run.contains("/app/bin:/usr/bin") && step.run.contains("prod")
        ));
    }

    #[test]
    fn converts_exec_form_run_without_shell_expansion_mode() {
        let temp = tempdir().unwrap();
        let instructions = parse("FROM alpine\nSHELL [\"/bin/bash\", \"-lc\"]\nRUN [\"echo\", \"$HOME\", \"hello world\"]\n").unwrap();
        let context = DockerContext {
            root: temp.path().to_path_buf(),
            ignore_patterns: Vec::new(),
        };

        let result = convert(instructions, &context, None, &[], None).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            _ => panic!("expected single"),
        };

        assert!(matches!(
            &pipeline.operations[0],
            Operation::Exec(step)
                if step.run_exec
                    == Some(vec![
                        "echo".to_string(),
                        "$HOME".to_string(),
                        "hello world".to_string(),
                    ])
                    && step.shell.is_none()
        ));
    }

    #[test]
    fn build_arg_override() {
        let temp = tempdir().unwrap();
        let instructions = parse("FROM alpine\nARG VERSION=1.0\nRUN echo $VERSION\n").unwrap();

        let result = convert(
            instructions,
            &DockerContext {
                root: temp.path().to_path_buf(),
                ignore_patterns: Vec::new(),
            },
            None,
            &[("VERSION".to_string(), "2.0".to_string())],
            None,
        )
        .unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            _ => panic!("expected single"),
        };

        assert!(matches!(
            &pipeline.operations[0],
            Operation::Exec(step) if step.run.contains("2.0")
        ));
    }

    #[test]
    fn metadata_fields_populated() {
        let temp = tempdir().unwrap();
        let instructions = parse(
            r#"
FROM alpine
EXPOSE 8080
LABEL maintainer="test"
USER 1000
HEALTHCHECK --interval=30s CMD ["curl", "-f", "http://localhost"]
VOLUME ["/data"]
STOPSIGNAL SIGTERM
ENTRYPOINT ["/bin/sh"]
CMD ["-c", "echo hi"]
"#,
        )
        .unwrap();
        let context = DockerContext {
            root: temp.path().to_path_buf(),
            ignore_patterns: Vec::new(),
        };

        let result = convert(instructions, &context, None, &[], None).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            _ => panic!("expected single"),
        };

        let meta = pipeline.metadata.unwrap();
        assert_eq!(meta.expose, vec!["8080"]);
        assert_eq!(meta.labels.get("maintainer").unwrap(), "test");
        assert_eq!(meta.user, Some("1000".to_string()));
        assert_eq!(
            meta.healthcheck,
            Some(ImageHealthcheck::Command {
                test: vec![
                    "CMD".to_string(),
                    "curl".to_string(),
                    "-f".to_string(),
                    "http://localhost".to_string(),
                ],
                interval_nanos: Some(30_000_000_000),
                timeout_nanos: None,
                start_period_nanos: None,
                start_interval_nanos: None,
                retries: None,
            })
        );
        assert_eq!(meta.volumes, vec!["/data"]);
        assert_eq!(meta.stop_signal, Some("SIGTERM".to_string()));
        assert_eq!(meta.entrypoint, Some(vec!["/bin/sh".to_string()]));
        assert_eq!(
            meta.cmd,
            Some(vec!["-c".to_string(), "echo hi".to_string()])
        );
    }

    #[test]
    fn empty_run_gets_finalize_step() {
        let temp = tempdir().unwrap();
        let instructions = parse("FROM alpine\nCMD [\"/bin/sh\"]\n").unwrap();
        let context = DockerContext {
            root: temp.path().to_path_buf(),
            ignore_patterns: Vec::new(),
        };

        let result = convert(instructions, &context, None, &[], None).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            _ => panic!("expected single"),
        };

        assert_eq!(pipeline.operations.len(), 1);
        assert!(matches!(
            &pipeline.operations[0],
            Operation::Exec(step) if step.run == "true"
        ));
    }

    #[test]
    fn workdir_relative_resolution() {
        let temp = tempdir().unwrap();
        let instructions = parse("FROM alpine\nWORKDIR /app\nWORKDIR src\nRUN echo hi\n").unwrap();
        let context = DockerContext {
            root: temp.path().to_path_buf(),
            ignore_patterns: Vec::new(),
        };

        let result = convert(instructions, &context, None, &[], None).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            _ => panic!("expected single"),
        };

        assert_eq!(pipeline.workdir, "/app/src");
        assert!(matches!(
            &pipeline.operations[0],
            Operation::Exec(step) if step.workdir == Some("/app/src".to_string())
        ));
    }

    #[test]
    fn supports_copy_chown_and_chmod_flags() {
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("app.py"), "print('hi')").unwrap();
        let instructions =
            parse("FROM python:3.12\nCOPY --chown=1000:1001 --chmod=755 app.py /app/\n").unwrap();
        let context = DockerContext {
            root: temp.path().to_path_buf(),
            ignore_patterns: Vec::new(),
        };

        let result = convert(instructions, &context, None, &[], None).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            _ => panic!("expected single"),
        };

        assert!(matches!(
            &pipeline.operations[0],
            Operation::CopyFromContext(op)
                if op.chown.as_deref() == Some("1000:1001")
                    && op.exclude.is_empty()
                    && op.chmod.as_deref() == Some("755")
        ));
    }

    #[test]
    fn converts_copy_exclude_flags() {
        let temp = tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("app")).unwrap();
        std::fs::write(temp.path().join("app/app.js"), "console.log('hi')").unwrap();
        let instructions =
            parse("FROM alpine\nCOPY --exclude=*.map --exclude=app/**/*.tmp app /workspace/\n")
                .unwrap();
        let context = DockerContext {
            root: temp.path().to_path_buf(),
            ignore_patterns: Vec::new(),
        };

        let result = convert(instructions, &context, None, &[], None).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            _ => panic!("expected single"),
        };

        assert!(matches!(
            &pipeline.operations[0],
            Operation::CopyFromContext(op)
                if op.exclude == vec!["*.map".to_string(), "app/**/*.tmp".to_string()]
        ));
    }

    #[test]
    fn converts_remote_add_to_explicit_op() {
        let temp = tempdir().unwrap();
        let instructions =
            parse("FROM alpine\nADD https://example.com/archive.tgz /tmp/\n").unwrap();
        let context = DockerContext {
            root: temp.path().to_path_buf(),
            ignore_patterns: Vec::new(),
        };

        let result = convert(instructions, &context, None, &[], None).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            _ => panic!("expected single"),
        };

        assert!(matches!(
            &pipeline.operations[0],
            Operation::AddRemote(op)
                if op.url == "https://example.com/archive.tgz"
                    && op.dest == "/tmp/"
                    && op.checksum.is_none()
        ));
    }

    #[test]
    fn converts_remote_add_checksum_to_explicit_op() {
        let temp = tempdir().unwrap();
        let instructions = parse(
            "FROM alpine\nADD --checksum=sha256:deadbeef https://example.com/archive.tgz /tmp/\n",
        )
        .unwrap();
        let context = DockerContext {
            root: temp.path().to_path_buf(),
            ignore_patterns: Vec::new(),
        };

        let result = convert(instructions, &context, None, &[], None).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            _ => panic!("expected single"),
        };

        assert!(matches!(
            &pipeline.operations[0],
            Operation::AddRemote(op)
                if op.url == "https://example.com/archive.tgz"
                    && op.dest == "/tmp/"
                    && op.checksum.as_deref() == Some("sha256:deadbeef")
        ));
    }

    #[test]
    fn local_add_marks_archive_extraction() {
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join("archive.tar.gz"), "placeholder").unwrap();
        let instructions = parse("FROM alpine\nADD archive.tar.gz /opt/\n").unwrap();
        let context = DockerContext {
            root: temp.path().to_path_buf(),
            ignore_patterns: Vec::new(),
        };

        let result = convert(instructions, &context, None, &[], None).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            _ => panic!("expected single"),
        };

        assert!(matches!(
            &pipeline.operations[0],
            Operation::CopyFromContext(op)
                if op.extract_archives
                    && op.sources == vec!["archive.tar.gz".to_string()]
                    && op.dest == "/opt/"
                    && op.chown.is_none()
                    && op.chmod.is_none()
        ));
    }
}
