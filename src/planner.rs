use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use indexmap::IndexMap;
use time::OffsetDateTime;
use time::macros::format_description;

use crate::cache::key::resolve_cache_key;
use crate::schema::{
    CacheKeyFile, CacheRestoreFromFile, DockerfileTargetFile, ExportConfig, HOST_RUNTIME_IMAGE,
    Input, InputFile, MultiTargetRecipe, MultiTargetRecipeFile, Operation, Pipeline, PipelineFile,
    PipelineOrMulti, PipelineRuntime, SetupSnapshot, SetupSnapshotFile, Step, StepBuildCacheInput,
    StepBuildCacheInputFile, StepBuildCacheInputsFile, StepCacheFile, StepCacheMountFile,
    StepCacheMountObjectFile, StepFile, StepRunMount, TargetFile,
};
use crate::util::platform::{default_host_platform, default_linux_platform};
use crate::util::process::{find_command, run_capture};
use crate::util::workspace::validate_workspace_ignore_patterns;

#[derive(Debug)]
struct ResolvedTargetGroup {
    targets: IndexMap<String, Pipeline>,
    order: Vec<String>,
}

pub fn load_pipeline(
    path: &Path,
    platform_override: Option<&str>,
    build_args: &[(String, String)],
) -> Result<PipelineOrMulti> {
    if crate::dockerfile::is_dockerfile(path) {
        let context_dir = path.parent().unwrap_or_else(|| Path::new("."));
        return crate::dockerfile::load_dockerfile(
            path,
            context_dir,
            platform_override,
            build_args,
            None,
        );
    }

    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read pipeline {}", path.display()))?;

    let value: serde_json::Value = yaml_serde::from_str(&raw)
        .with_context(|| format!("invalid YAML in {}", path.display()))?;

    if value.get("targets").is_some() || value.get("jobs").is_some() {
        let file: MultiTargetRecipeFile = yaml_serde::from_str(&raw)
            .with_context(|| format!("invalid multi-target YAML in {}", path.display()))?;
        Ok(PipelineOrMulti::Multi(resolve_multi_target(
            path,
            file,
            platform_override,
            build_args,
        )?))
    } else {
        let file: PipelineFile = yaml_serde::from_str(&raw)
            .with_context(|| format!("invalid YAML in {}", path.display()))?;
        let mut pipeline = resolve_pipeline(path, file, platform_override)?;
        apply_native_build_args(&mut pipeline, build_args);
        Ok(PipelineOrMulti::Single(pipeline))
    }
}

pub fn select_multi_target(
    multi: &MultiTargetRecipe,
    target_name: &str,
) -> Result<MultiTargetRecipe> {
    ensure!(
        multi.targets.contains_key(target_name),
        "unknown target '{target_name}'"
    );

    let mut required = BTreeSet::new();
    let mut pending = vec![target_name.to_string()];
    while let Some(target_name) = pending.pop() {
        if !required.insert(target_name.clone()) {
            continue;
        }
        let target = multi
            .targets
            .get(&target_name)
            .ok_or_else(|| anyhow!("target selection referenced unknown target '{target_name}'"))?;
        pending.extend(target.needs.iter().cloned());
    }

    let order = multi
        .order
        .iter()
        .filter(|target_name| required.contains(target_name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let targets = order
        .iter()
        .map(|target_name| {
            (
                target_name.clone(),
                multi
                    .targets
                    .get(target_name)
                    .expect("selected order must reference an existing target")
                    .clone(),
            )
        })
        .collect();

    Ok(MultiTargetRecipe {
        targets,
        order,
        base_dir: multi.base_dir.clone(),
    })
}

pub fn resolve_multi_target(
    path: &Path,
    file: MultiTargetRecipeFile,
    platform_override: Option<&str>,
    build_args: &[(String, String)],
) -> Result<MultiTargetRecipe> {
    let MultiTargetRecipeFile {
        definitions,
        includes,
        targets: file_targets,
    } = file;
    ensure!(
        !file_targets.is_empty(),
        "pipeline must contain at least one target"
    );

    let target_names: BTreeSet<&str> = file_targets.keys().map(String::as_str).collect();

    for (name, target) in &file_targets {
        for dep in &target.needs {
            ensure!(
                target_names.contains(dep.as_str()),
                "target '{name}' depends on unknown target '{dep}'"
            );
            ensure!(dep != name, "target '{name}' depends on itself");
        }
    }

    let order = topological_sort(&file_targets)?;

    let base_dir = resolve_pipeline_base_dir(path)?;
    let inherited_definitions = load_and_merge_definitions(&base_dir, definitions, &includes)?;

    let mut targets = IndexMap::new();
    let mut expanded_order = Vec::new();
    for name in &order {
        let target_file = file_targets.get(name).unwrap().clone();
        let resolved = resolve_target_group(
            &base_dir,
            name,
            target_file,
            platform_override,
            build_args,
            &inherited_definitions,
        )
        .with_context(|| format!("failed to resolve target '{name}'"))?;
        for internal_name in &resolved.order {
            ensure!(
                !targets.contains_key(internal_name),
                "target name collision after expansion: '{internal_name}'"
            );
            targets.insert(
                internal_name.clone(),
                resolved.targets.get(internal_name).unwrap().clone(),
            );
        }
        expanded_order.extend(resolved.order);
    }

    Ok(MultiTargetRecipe {
        targets,
        order: expanded_order,
        base_dir,
    })
}

fn resolve_target_group(
    base_dir: &Path,
    target_name: &str,
    target: TargetFile,
    platform_override: Option<&str>,
    build_args: &[(String, String)],
    inherited_definitions: &IndexMap<String, StepFile>,
) -> Result<ResolvedTargetGroup> {
    if let Some(dockerfile) = target.dockerfile.clone() {
        resolve_dockerfile_target_group(
            base_dir,
            target_name,
            target,
            dockerfile,
            platform_override,
            build_args,
        )
    } else {
        resolve_native_target_group(
            base_dir,
            target_name,
            target,
            platform_override,
            build_args,
            inherited_definitions,
        )
    }
}

fn resolve_native_target_group(
    base_dir: &Path,
    target_name: &str,
    target: TargetFile,
    platform_override: Option<&str>,
    build_args: &[(String, String)],
    inherited_definitions: &IndexMap<String, StepFile>,
) -> Result<ResolvedTargetGroup> {
    let TargetFile {
        image,
        runtime,
        host_lock,
        platform,
        workdir,
        env,
        inputs,
        definitions,
        includes,
        outputs,
        setup_snapshot,
        steps,
        dockerfile: _,
        export,
        metadata,
        needs,
    } = target;

    if runtime == PipelineRuntime::Container {
        ensure!(
            image
                .as_deref()
                .map(str::trim)
                .is_some_and(|image| !image.is_empty()),
            "target '{target_name}' must set image when dockerfile is not used"
        );
    }
    ensure!(
        !steps.is_empty(),
        "target '{target_name}' must contain at least one step"
    );

    let mut merged_definitions = inherited_definitions.clone();
    merged_definitions.extend(definitions);

    let file = PipelineFile {
        image,
        runtime,
        host_lock,
        platform,
        workdir,
        env,
        inputs,
        definitions: merged_definitions,
        includes,
        outputs,
        setup_snapshot,
        steps,
        export,
        metadata,
    };

    let pipeline_path = base_dir.join(format!("{target_name}.yml"));
    let mut pipeline = resolve_pipeline(&pipeline_path, file, platform_override)?;
    apply_native_build_args(&mut pipeline, build_args);
    pipeline.needs = needs;

    let mut targets = IndexMap::new();
    targets.insert(target_name.to_string(), pipeline);
    Ok(ResolvedTargetGroup {
        targets,
        order: vec![target_name.to_string()],
    })
}

fn resolve_dockerfile_target_group(
    base_dir: &Path,
    target_name: &str,
    target: TargetFile,
    dockerfile: DockerfileTargetFile,
    platform_override: Option<&str>,
    build_args: &[(String, String)],
) -> Result<ResolvedTargetGroup> {
    validate_dockerfile_target(target_name, &target)?;

    let dockerfile_path = base_dir.join(&dockerfile.file);
    let context_dir = dockerfile
        .context
        .as_ref()
        .map(|path| base_dir.join(path))
        .unwrap_or_else(|| {
            dockerfile_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| base_dir.to_path_buf())
        });

    let mut merged_build_args = dockerfile.build_args;
    for (key, value) in build_args {
        merged_build_args.insert(key.clone(), value.clone());
    }
    let merged_build_args = merged_build_args.into_iter().collect::<Vec<_>>();

    let selected_platform = match platform_override.or(target.platform.as_deref()) {
        Some(platform) => platform.to_string(),
        None => default_linux_platform()?,
    };
    let export = target
        .export
        .map(|export| resolve_export(base_dir, &selected_platform, export))
        .transpose()?;
    let loaded = crate::dockerfile::load_dockerfile(
        &dockerfile_path,
        &context_dir,
        Some(&selected_platform),
        &merged_build_args,
        export,
    )?;

    match loaded {
        PipelineOrMulti::Single(mut pipeline) => {
            pipeline.needs = target.needs;
            let mut targets = IndexMap::new();
            targets.insert(target_name.to_string(), pipeline);
            Ok(ResolvedTargetGroup {
                targets,
                order: vec![target_name.to_string()],
            })
        }
        PipelineOrMulti::Multi(multi) => {
            let mut rename_map = BTreeMap::new();
            let final_stage =
                multi.order.last().cloned().ok_or_else(|| {
                    anyhow!("Dockerfile target '{target_name}' produced no stages")
                })?;
            for stage_name in &multi.order {
                let resolved_name = if stage_name == &final_stage {
                    target_name.to_string()
                } else {
                    format!("{target_name}::{stage_name}")
                };
                rename_map.insert(stage_name.clone(), resolved_name);
            }

            let mut targets = IndexMap::new();
            let mut order = Vec::new();
            for stage_name in &multi.order {
                let mut pipeline = multi.targets.get(stage_name).unwrap().clone();
                for operation in &mut pipeline.operations {
                    if let Operation::CopyFromStage(copy) = operation
                        && let Some(resolved_stage) = rename_map.get(&copy.stage)
                    {
                        copy.stage = resolved_stage.clone();
                    }
                }
                pipeline.needs = pipeline
                    .needs
                    .iter()
                    .map(|need| {
                        rename_map
                            .get(need)
                            .cloned()
                            .unwrap_or_else(|| need.clone())
                    })
                    .collect();
                let resolved_name = rename_map.get(stage_name).unwrap().clone();
                order.push(resolved_name.clone());
                targets.insert(resolved_name, pipeline);
            }

            Ok(ResolvedTargetGroup { targets, order })
        }
    }
}

fn validate_dockerfile_target(target_name: &str, target: &TargetFile) -> Result<()> {
    ensure!(
        target.runtime == PipelineRuntime::Container,
        "target '{target_name}' cannot set runtime: host when dockerfile is used"
    );
    ensure!(
        target.image.is_none(),
        "target '{target_name}' cannot set image when dockerfile is used"
    );
    ensure!(
        target.workdir.is_none(),
        "target '{target_name}' cannot set workdir when dockerfile is used"
    );
    ensure!(
        target.env.is_empty(),
        "target '{target_name}' cannot set env when dockerfile is used"
    );
    ensure!(
        target.inputs.is_empty(),
        "target '{target_name}' cannot set inputs when dockerfile is used"
    );
    ensure!(
        target.definitions.is_empty(),
        "target '{target_name}' cannot set definitions when dockerfile is used"
    );
    ensure!(
        target.includes.is_empty(),
        "target '{target_name}' cannot set includes when dockerfile is used"
    );
    ensure!(
        target.outputs.is_empty(),
        "target '{target_name}' cannot set outputs when dockerfile is used"
    );
    ensure!(
        target.steps.is_empty(),
        "target '{target_name}' cannot set steps when dockerfile is used"
    );
    ensure!(
        target.setup_snapshot.is_none(),
        "target '{target_name}' cannot set setup_snapshot when dockerfile is used"
    );
    ensure!(
        target.metadata.is_none(),
        "target '{target_name}' cannot set metadata when dockerfile is used"
    );
    Ok(())
}

fn apply_native_build_args(pipeline: &mut Pipeline, build_args: &[(String, String)]) {
    for (key, value) in build_args {
        pipeline.env.insert(key.clone(), value.clone());
    }
}

fn topological_sort(targets: &IndexMap<String, TargetFile>) -> Result<Vec<String>> {
    let target_names: Vec<&str> = targets.keys().map(String::as_str).collect();
    let mut in_degree: BTreeMap<&str, usize> = BTreeMap::new();
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();

    for &name in &target_names {
        in_degree.insert(name, 0);
    }

    for (name, target) in targets.iter() {
        for dep in &target.needs {
            *in_degree.get_mut(name.as_str()).unwrap() += 1;
            dependents
                .entry(dep.as_str())
                .or_default()
                .push(name.as_str());
        }
    }

    let mut queue: Vec<&str> = in_degree
        .iter()
        .filter(|&(_, &deg)| deg == 0)
        .map(|(&name, _)| name)
        .collect();
    queue.sort();

    let mut result = Vec::new();
    while let Some(name) = queue.pop() {
        result.push(name.to_string());
        if let Some(deps) = dependents.get(name) {
            for &dep in deps {
                let deg = in_degree.get_mut(dep).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    queue.push(dep);
                    queue.sort();
                }
            }
        }
    }

    if result.len() != targets.len() {
        bail!("circular dependency detected in target graph");
    }

    Ok(result)
}

pub fn resolve_pipeline(
    path: &Path,
    file: PipelineFile,
    platform_override: Option<&str>,
) -> Result<Pipeline> {
    let PipelineFile {
        image,
        runtime,
        host_lock: file_host_lock,
        platform: file_platform,
        workdir: file_workdir,
        mut env,
        inputs: file_inputs,
        definitions: file_definitions,
        includes: file_includes,
        outputs: file_outputs,
        setup_snapshot: file_setup_snapshot,
        steps: file_steps,
        export: file_export,
        metadata,
    } = file;
    ensure!(
        !file_steps.is_empty(),
        "pipeline must contain at least one step"
    );

    let base_dir = resolve_pipeline_base_dir(path)?;
    let definitions = load_and_merge_definitions(&base_dir, file_definitions, &file_includes)?;
    let file_steps = file_steps
        .into_iter()
        .map(|step| expand_step_uses(step, &definitions))
        .collect::<Result<Vec<_>>>()?;
    let workdir = file_workdir.unwrap_or_else(|| "/workspace".to_string());
    validate_container_path(&workdir, "workdir")?;
    let platform = platform_override
        .map(ToOwned::to_owned)
        .or(file_platform)
        .unwrap_or(match runtime {
            PipelineRuntime::Container => default_linux_platform()?,
            PipelineRuntime::Host => default_host_platform()?,
        });
    let image = match runtime {
        PipelineRuntime::Container => {
            ensure!(
                file_host_lock.is_none(),
                "host_lock is only supported for runtime: host pipelines"
            );
            let image = image.unwrap_or_default();
            ensure!(!image.trim().is_empty(), "pipeline image must not be empty");
            ensure!(
                image != HOST_RUNTIME_IMAGE,
                "pipeline image '{}' is reserved for runtime: host",
                HOST_RUNTIME_IMAGE
            );
            image
        }
        PipelineRuntime::Host => {
            let image = image.unwrap_or_else(|| HOST_RUNTIME_IMAGE.to_string());
            ensure!(
                image == HOST_RUNTIME_IMAGE,
                "runtime: host reserves image '{}'; omit image or set it exactly",
                HOST_RUNTIME_IMAGE
            );
            image
        }
    };
    if let Some(host_lock) = resolve_host_lock(runtime, file_host_lock)? {
        env.insert("BORINGBUILDER_HOST_LOCK".to_string(), host_lock);
    }

    let inputs = if file_inputs.is_empty() {
        vec![Input {
            source: canonicalize_existing(&base_dir)?,
            dest: workdir.clone(),
            readonly: false,
        }]
    } else {
        file_inputs
            .into_iter()
            .map(|input| resolve_input(&base_dir, input))
            .collect::<Result<Vec<_>>>()?
    };

    let outputs = if file_outputs.is_empty() {
        Vec::new()
    } else {
        file_outputs
            .into_iter()
            .map(|output| {
                validate_container_path(&output, "output path")?;
                Ok(output)
            })
            .collect::<Result<Vec<_>>>()?
    };

    let setup_snapshot =
        resolve_setup_snapshot(&base_dir, runtime, &platform, &env, file_setup_snapshot)?;

    let operations = file_steps
        .into_iter()
        .map(|step| resolve_step(&base_dir, runtime, &platform, &env, step).map(Operation::Exec))
        .collect::<Result<Vec<_>>>()?;

    let export = file_export
        .map(|export| resolve_export(&base_dir, &platform, export))
        .transpose()?;

    Ok(Pipeline {
        image,
        platform,
        workdir,
        env,
        inputs,
        outputs,
        setup_snapshot,
        operations,
        export,
        metadata,
        base_dir,
        needs: Vec::new(),
        stage_dependency_digests: BTreeMap::new(),
        stage_snapshot_follow_symlinks: Default::default(),
        docker_context: None,
    })
}

fn resolve_host_lock(
    runtime: PipelineRuntime,
    host_lock: Option<String>,
) -> Result<Option<String>> {
    let Some(host_lock) = host_lock else {
        return Ok(None);
    };
    ensure!(
        runtime == PipelineRuntime::Host,
        "host_lock is only supported for runtime: host pipelines"
    );
    let trimmed = host_lock.trim();
    ensure!(!trimmed.is_empty(), "host_lock must not be empty");
    ensure!(
        trimmed
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')),
        "host_lock must contain only ASCII letters, numbers, '-', '_', or '.'"
    );
    Ok(Some(trimmed.to_string()))
}

fn resolve_setup_snapshot(
    base_dir: &Path,
    runtime: PipelineRuntime,
    platform: &str,
    pipeline_env: &BTreeMap<String, String>,
    setup_snapshot: Option<SetupSnapshotFile>,
) -> Result<Option<SetupSnapshot>> {
    let Some(setup_snapshot) = setup_snapshot else {
        return Ok(None);
    };

    ensure!(
        runtime == PipelineRuntime::Container,
        "setup_snapshot is only supported for container pipelines"
    );
    validate_container_path(&setup_snapshot.path, "setup snapshot path")?;
    let key = resolve_cache_key(
        base_dir,
        platform,
        pipeline_env,
        &setup_snapshot.path,
        &setup_snapshot.key,
    )
    .context("invalid setup snapshot key")?;
    let restore_from = resolve_restore_from_keys(
        base_dir,
        platform,
        pipeline_env,
        &setup_snapshot.path,
        setup_snapshot.restore_from,
    )
    .context("invalid setup snapshot restore_from")?;

    Ok(Some(SetupSnapshot {
        path: setup_snapshot.path,
        key,
        restore_from,
    }))
}

fn resolve_restore_from_keys(
    base_dir: &Path,
    platform: &str,
    pipeline_env: &BTreeMap<String, String>,
    cache_path: &str,
    restore_from: Option<CacheRestoreFromFile>,
) -> Result<Vec<String>> {
    let keys = match restore_from {
        Some(CacheRestoreFromFile::Single(key)) => vec![key],
        Some(CacheRestoreFromFile::Multiple(keys)) => keys,
        None => return Ok(Vec::new()),
    };

    keys.into_iter()
        .map(|key| resolve_cache_key(base_dir, platform, pipeline_env, cache_path, &key))
        .collect()
}

pub fn render_plan(pipeline: &Pipeline) -> Result<String> {
    yaml_serde::to_string(pipeline).map_err(Into::into)
}

pub fn render_multi_plan(multi: &MultiTargetRecipe) -> Result<String> {
    let mut output = String::new();
    output.push_str("targets:\n");
    for name in &multi.order {
        let pipeline = &multi.targets[name];
        output.push_str(&format!("  {}:\n", name));
        let yaml = yaml_serde::to_string(pipeline)?;
        for line in yaml.lines() {
            output.push_str(&format!("    {}\n", line));
        }
    }
    Ok(output)
}

fn load_includes(base_dir: &Path, includes: &[PathBuf]) -> Result<IndexMap<String, StepFile>> {
    let mut definitions = IndexMap::new();
    for include_path in includes {
        let path = base_dir.join(include_path);
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read include file {}", path.display()))?;
        let file_definitions = yaml_serde::from_str::<IndexMap<String, StepFile>>(&raw)
            .with_context(|| format!("invalid YAML in include file {}", path.display()))?;
        definitions.extend(file_definitions);
    }
    Ok(definitions)
}

fn load_and_merge_definitions(
    base_dir: &Path,
    definitions: IndexMap<String, StepFile>,
    includes: &[PathBuf],
) -> Result<IndexMap<String, StepFile>> {
    let mut merged = load_includes(base_dir, includes)?;
    merged.extend(definitions);
    Ok(merged)
}

fn substitute_build_cache_inputs(
    build_cache_inputs: &mut Option<StepBuildCacheInputsFile>,
    vars: &BTreeMap<String, String>,
) {
    let Some(build_cache_inputs) = build_cache_inputs else {
        return;
    };

    match build_cache_inputs {
        StepBuildCacheInputsFile::Single(entry) => substitute_build_cache_input(entry, vars),
        StepBuildCacheInputsFile::Multiple(entries) => {
            for entry in entries {
                substitute_build_cache_input(entry, vars);
            }
        }
    }
}

fn substitute_step_cache(cache: &mut Option<StepCacheFile>, vars: &BTreeMap<String, String>) {
    let Some(cache) = cache else {
        return;
    };

    match cache {
        StepCacheFile::Single(entry) => substitute_step_cache_mount(entry, vars),
        StepCacheFile::Multiple(entries) => {
            for entry in entries {
                substitute_step_cache_mount(entry, vars);
            }
        }
    }
}

fn substitute_step_cache_mount(entry: &mut StepCacheMountFile, vars: &BTreeMap<String, String>) {
    match entry {
        StepCacheMountFile::Path(path) => {
            *path = crate::dockerfile::vars::substitute(path, vars);
        }
        StepCacheMountFile::Mount(spec) => {
            spec.path = crate::dockerfile::vars::substitute(&spec.path, vars);
            if let Some(key) = &mut spec.key {
                substitute_cache_key(key, vars);
            }
            substitute_restore_from(&mut spec.restore_from, vars);
        }
    }
}

fn substitute_restore_from(
    restore_from: &mut Option<CacheRestoreFromFile>,
    vars: &BTreeMap<String, String>,
) {
    let Some(restore_from) = restore_from else {
        return;
    };

    match restore_from {
        CacheRestoreFromFile::Single(key) => substitute_cache_key(key, vars),
        CacheRestoreFromFile::Multiple(keys) => {
            for key in keys {
                substitute_cache_key(key, vars);
            }
        }
    }
}

fn substitute_cache_key(key: &mut CacheKeyFile, vars: &BTreeMap<String, String>) {
    match key {
        CacheKeyFile::Literal(value) => {
            *value = crate::dockerfile::vars::substitute(value, vars);
        }
        CacheKeyFile::Structured(spec) => {
            if let Some(prefix) = &mut spec.prefix {
                *prefix = crate::dockerfile::vars::substitute(prefix, vars);
            }
        }
    }
}

fn substitute_build_cache_input(
    entry: &mut StepBuildCacheInputFile,
    vars: &BTreeMap<String, String>,
) {
    match entry {
        StepBuildCacheInputFile::Path(path) => {
            *path = crate::dockerfile::vars::substitute(path, vars);
        }
        StepBuildCacheInputFile::Structured(spec) => {
            spec.path = crate::dockerfile::vars::substitute(&spec.path, vars);
            for exclude in &mut spec.exclude {
                *exclude = crate::dockerfile::vars::substitute(exclude, vars);
            }
        }
    }
}

fn substitute_step_definition(step: &mut StepFile, vars: &BTreeMap<String, String>) {
    if let Some(name) = &mut step.name {
        *name = crate::dockerfile::vars::substitute(name, vars);
    }
    if let Some(run) = &mut step.run {
        *run = crate::dockerfile::vars::substitute(run, vars);
    }
    for value in step.env.values_mut() {
        *value = crate::dockerfile::vars::substitute(value, vars);
    }
    if let Some(workdir) = &mut step.workdir {
        *workdir = crate::dockerfile::vars::substitute(workdir, vars);
    }
    if let Some(shell) = &mut step.shell {
        *shell = crate::dockerfile::vars::substitute(shell, vars);
    }
    substitute_step_cache(&mut step.cache, vars);
    substitute_build_cache_inputs(&mut step.build_cache_inputs, vars);
    if let Some(tag) = &mut step.tag {
        *tag = crate::dockerfile::vars::substitute(tag, vars);
    }
}

fn expand_step_uses(step: StepFile, definitions: &IndexMap<String, StepFile>) -> Result<StepFile> {
    let step_label = step.name.as_deref().unwrap_or("<unnamed>");
    let Some(reference) = step.uses.clone() else {
        ensure!(
            step.with.is_empty(),
            "step '{step_label}' cannot set 'with' without 'uses'"
        );
        ensure!(
            step.run.is_some(),
            "step '{step_label}' must have either 'run' or 'uses'"
        );
        return Ok(step);
    };

    ensure!(
        step.run.is_none(),
        "step cannot have both 'run' and 'uses' (uses: '{reference}')"
    );

    let mut expanded = definitions
        .get(&reference)
        .cloned()
        .ok_or_else(|| anyhow!("step references unknown definition '{reference}'"))?;
    ensure!(
        expanded.uses.is_none(),
        "definition '{reference}' cannot itself use 'uses'"
    );
    ensure!(
        expanded.with.is_empty(),
        "definition '{reference}' cannot set 'with'"
    );

    substitute_step_definition(&mut expanded, &step.with);

    if let Some(name) = step.name {
        expanded.name = Some(name);
    }
    if let Some(workdir) = step.workdir {
        expanded.workdir = Some(workdir);
    }
    if let Some(shell) = step.shell {
        expanded.shell = Some(shell);
    }
    if let Some(cache) = step.cache {
        expanded.cache = Some(cache);
    }
    if let Some(build_cache_inputs) = step.build_cache_inputs {
        expanded.build_cache_inputs = Some(build_cache_inputs);
    }
    if let Some(build_cache) = step.build_cache {
        expanded.build_cache = Some(build_cache);
    }
    if let Some(tag) = step.tag {
        expanded.tag = Some(tag);
    }
    expanded.env.extend(step.env);
    expanded.uses = None;
    expanded.with.clear();

    Ok(expanded)
}

fn resolve_input(base_dir: &Path, input: InputFile) -> Result<Input> {
    validate_container_path(&input.dest, "input destination")?;
    Ok(Input {
        source: canonicalize_existing(&base_dir.join(input.source))?,
        dest: input.dest,
        readonly: input.readonly,
    })
}

fn resolve_pipeline_base_dir(path: &Path) -> Result<PathBuf> {
    let file_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let in_conventional_config_dir = file_dir
        .file_name()
        .is_some_and(|name| name == std::ffi::OsStr::new("config"));
    if in_conventional_config_dir && let Some(git_root) = git_repo_root(&file_dir)? {
        return Ok(git_root);
    }
    Ok(file_dir)
}

fn git_repo_root(base_dir: &Path) -> Result<Option<PathBuf>> {
    let Some(git) = find_command("git") else {
        return Ok(None);
    };
    let output = run_capture(
        &git,
        &[
            "-C".to_string(),
            base_dir.display().to_string(),
            "rev-parse".to_string(),
            "--show-toplevel".to_string(),
        ],
    )?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(output.stdout.trim())))
}

#[allow(clippy::too_many_arguments)]
fn resolve_step(
    base_dir: &Path,
    runtime: PipelineRuntime,
    platform: &str,
    pipeline_env: &BTreeMap<String, String>,
    step: StepFile,
) -> Result<Step> {
    let step_label = step.name.as_deref().unwrap_or("<unnamed>");
    let run = step.run.as_deref().unwrap_or("");
    ensure!(
        !run.trim().is_empty(),
        "step '{}' has an empty run command",
        step_label
    );

    if let Some(step_workdir) = &step.workdir {
        validate_container_path(step_workdir, "step workdir")?;
    }

    let StepFile {
        name,
        run,
        uses: _,
        with: _,
        env: step_env,
        workdir,
        shell,
        cache,
        build_cache_inputs,
        build_cache,
        tag,
        privileged,
    } = step;
    let run = run.expect("run must be set before step resolution");
    let run_mounts = step_cache_run_mounts(base_dir, platform, pipeline_env, cache)?;
    ensure!(
        runtime == PipelineRuntime::Container || run_mounts.is_empty(),
        "step cache mounts are only supported for container pipelines today"
    );
    let build_cache_inputs = step_build_cache_inputs(build_cache_inputs)?;
    let step_env = if privileged {
        let mut env = step_env;
        env.insert(
            crate::schema::PRIVILEGED_STEP_ENV.to_string(),
            "1".to_string(),
        );
        env
    } else {
        step_env
    };

    Ok(Step {
        name,
        run,
        run_exec: None,
        run_mounts,
        env: step_env,
        workdir,
        shell,
        build_cache_inputs,
        build_cache,
        tag,
    })
}

fn step_cache_run_mounts(
    base_dir: &Path,
    platform: &str,
    pipeline_env: &BTreeMap<String, String>,
    cache: Option<StepCacheFile>,
) -> Result<Vec<StepRunMount>> {
    let entries = match cache {
        Some(StepCacheFile::Single(entry)) => vec![entry],
        Some(StepCacheFile::Multiple(entries)) => entries,
        None => return Ok(Vec::new()),
    };

    let mut targets = BTreeSet::new();
    let mut mounts = Vec::with_capacity(entries.len());
    for entry in entries {
        let spec = match entry {
            StepCacheMountFile::Path(path) => StepCacheMountObjectFile {
                path,
                key: None,
                restore_from: None,
                mode: Default::default(),
                read_only: false,
            },
            StepCacheMountFile::Mount(spec) => spec,
        };
        validate_container_path(&spec.path, "step cache path")?;
        ensure!(
            targets.insert(spec.path.clone()),
            "duplicate step cache path '{}'",
            spec.path
        );

        let default_key = default_step_cache_key(&spec.path);
        let key = resolve_cache_key(
            base_dir,
            platform,
            pipeline_env,
            &spec.path,
            spec.key.as_ref().unwrap_or(&default_key),
        )
        .with_context(|| format!("invalid step cache key for {}", spec.path))?;
        let restore_from = resolve_restore_from_keys(
            base_dir,
            platform,
            pipeline_env,
            &spec.path,
            spec.restore_from,
        )
        .with_context(|| format!("invalid step cache restore_from for {}", spec.path))?;

        mounts.push(StepRunMount::Cache {
            target: spec.path,
            id: key.clone(),
            key: Some(key),
            restore_from,
            readonly: spec.read_only,
            sharing: spec.mode,
        });
    }

    Ok(mounts)
}

fn default_step_cache_key(path: &str) -> CacheKeyFile {
    CacheKeyFile::Literal(format!(
        "cache-{{platform}}-{}",
        crate::cache::cache_tag(path)
    ))
}

fn step_build_cache_inputs(
    build_cache_inputs: Option<StepBuildCacheInputsFile>,
) -> Result<Option<Vec<StepBuildCacheInput>>> {
    let entries = match build_cache_inputs {
        Some(StepBuildCacheInputsFile::Single(entry)) => vec![entry],
        Some(StepBuildCacheInputsFile::Multiple(entries)) => entries,
        None => return Ok(None),
    };

    let mut unique = BTreeSet::new();
    for entry in entries {
        let input = match entry {
            StepBuildCacheInputFile::Path(path) => StepBuildCacheInput {
                path,
                exclude: Vec::new(),
            },
            StepBuildCacheInputFile::Structured(spec) => {
                validate_workspace_ignore_patterns(&spec.exclude)
                    .context("invalid step build-cache input exclude pattern")?;
                StepBuildCacheInput {
                    path: spec.path,
                    exclude: spec.exclude,
                }
            }
        };
        validate_container_path(&input.path, "step build-cache input path")?;
        unique.insert(input);
    }

    Ok(Some(unique.into_iter().collect()))
}

fn resolve_export(
    base_dir: &Path,
    platform: &str,
    mut export: ExportConfig,
) -> Result<ExportConfig> {
    export.path = PathBuf::from(expand_export_path_template(
        &export.path.to_string_lossy(),
        platform,
    )?);
    if export.path.is_relative() {
        export.path = base_dir.join(export.path);
    }

    let parent = export.path.parent().ok_or_else(|| {
        anyhow!(
            "export path {} has no parent directory",
            export.path.display()
        )
    })?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create export directory {}", parent.display()))?;
    Ok(export)
}

fn expand_export_path_template(raw: &str, platform: &str) -> Result<String> {
    let (os, arch) = platform
        .split_once('/')
        .ok_or_else(|| anyhow!("invalid platform '{platform}', expected os/arch"))?;
    let timestamp = current_export_timestamp()?;
    Ok(raw
        .replace("{os}", os)
        .replace("{arch}", arch)
        .replace("{platform}", &format!("{os}-{arch}"))
        .replace("{timestamp}", &timestamp))
}

fn current_export_timestamp() -> Result<String> {
    const FORMAT: &[time::format_description::FormatItem<'static>] =
        format_description!("[year][month][day]-[hour][minute][second]");
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    now.format(FORMAT)
        .context("failed to format export timestamp")
}

fn canonicalize_existing(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("path does not exist: {}", path.display()))
}

fn validate_container_path(path: &str, field: &str) -> Result<()> {
    if !path.starts_with('/') {
        bail!("{field} must be an absolute container path, got '{path}'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use indexmap::IndexMap;
    use tempfile::tempdir;

    use crate::schema::{
        CacheKeyFile, CacheMode, CacheRestoreFromFile, ExportConfig, ExportFormat, InputFile,
        Operation, PipelineFile, PipelineOrMulti, PipelineRuntime, SetupSnapshotFile,
        StepBuildCacheInput, StepBuildCacheInputFile, StepBuildCacheInputsFile, StepFile,
        StepRunMount, TargetFile,
    };

    use super::resolve_pipeline;

    #[test]
    fn defaults_workspace_mount_to_pipeline_dir() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(&pipeline_path, "image: alpine\nsteps:\n  - run: echo hi\n").unwrap();

        let result = super::load_pipeline(&pipeline_path, None, &[]).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            PipelineOrMulti::Multi(_) => panic!("expected single pipeline"),
        };
        assert_eq!(pipeline.workdir, "/workspace");
        assert_eq!(pipeline.inputs.len(), 1);
        assert_eq!(pipeline.inputs[0].dest, "/workspace");
        assert!(!pipeline.inputs[0].readonly);
    }

    #[test]
    fn resolves_host_runtime_without_image() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            "runtime: host\nplatform: darwin/arm64\nsteps:\n  - run: echo hi\n",
        )
        .unwrap();

        let result = super::load_pipeline(&pipeline_path, None, &[]).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            PipelineOrMulti::Multi(_) => panic!("expected single pipeline"),
        };
        assert_eq!(pipeline.image, crate::schema::HOST_RUNTIME_IMAGE);
        assert_eq!(pipeline.platform, "darwin/arm64");
    }

    #[test]
    fn resolves_host_runtime_lock_key() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            "runtime: host\nhost_lock: windows-msvc-controller\nplatform: darwin/arm64\nsteps:\n  - run: echo hi\n",
        )
        .unwrap();

        let result = super::load_pipeline(&pipeline_path, None, &[]).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            PipelineOrMulti::Multi(_) => panic!("expected single pipeline"),
        };
        assert_eq!(
            pipeline
                .env
                .get("BORINGBUILDER_HOST_LOCK")
                .map(String::as_str),
            Some("windows-msvc-controller")
        );
    }

    #[test]
    fn rejects_host_runtime_lock_key_on_container_pipeline() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            "image: alpine\nhost_lock: windows-msvc-controller\nsteps:\n  - run: echo hi\n",
        )
        .unwrap();

        let error = super::load_pipeline(&pipeline_path, None, &[]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("host_lock is only supported for runtime: host pipelines")
        );
    }

    #[test]
    fn loads_repo_artifact_example() {
        let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");

        let artifact = super::load_pipeline(&examples.join("artifact.yml"), None, &[]).unwrap();
        let artifact = match artifact {
            PipelineOrMulti::Single(pipeline) => pipeline,
            PipelineOrMulti::Multi(_) => panic!("expected single pipeline"),
        };
        assert_eq!(artifact.inputs.len(), 1);
        assert_eq!(artifact.outputs, vec!["/out"]);
        assert_eq!(artifact.operations.len(), 1);
        assert_eq!(artifact.export.unwrap().format, ExportFormat::TarZst);
    }

    #[test]
    fn resolves_step_build_cache_inputs() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");

        let file = PipelineFile {
            image: Some("alpine:latest".to_string()),
            runtime: PipelineRuntime::Container,
            host_lock: None,
            platform: Some("linux/amd64".to_string()),
            workdir: Some("/workspace".to_string()),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            definitions: IndexMap::new(),
            includes: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: None,
            steps: vec![StepFile {
                name: Some("install".to_string()),
                run: Some("echo install".to_string()),
                uses: None,
                with: BTreeMap::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                cache: None,
                build_cache_inputs: Some(StepBuildCacheInputsFile::Multiple(vec![
                    StepBuildCacheInputFile::Path("/src/Gemfile.lock".to_string()),
                    StepBuildCacheInputFile::Path("/src/Gemfile".to_string()),
                    StepBuildCacheInputFile::Path("/src/Gemfile.lock".to_string()),
                ])),
                build_cache: None,
                tag: None,
                privileged: false,
            }],
            export: None,
            metadata: None,
        };

        let pipeline = resolve_pipeline(&pipeline_path, file, None).unwrap();
        let step = match &pipeline.operations[0] {
            Operation::Exec(step) => step,
            _ => panic!("expected exec step"),
        };
        assert_eq!(
            step.build_cache_inputs,
            Some(vec![
                StepBuildCacheInput {
                    path: "/src/Gemfile".to_string(),
                    exclude: Vec::new(),
                },
                StepBuildCacheInput {
                    path: "/src/Gemfile.lock".to_string(),
                    exclude: Vec::new(),
                },
            ])
        );
    }

    #[test]
    fn resolves_structured_step_build_cache_inputs() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");

        let file = PipelineFile {
            image: Some("alpine:latest".to_string()),
            runtime: PipelineRuntime::Container,
            host_lock: None,
            platform: Some("linux/amd64".to_string()),
            workdir: Some("/workspace".to_string()),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            definitions: IndexMap::new(),
            includes: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: None,
            steps: vec![StepFile {
                name: Some("sync".to_string()),
                run: Some("tar -C /src -cf - . | tar -C /workspace -xf -".to_string()),
                uses: None,
                with: BTreeMap::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                cache: None,
                build_cache_inputs: Some(StepBuildCacheInputsFile::Single(
                    StepBuildCacheInputFile::Structured(
                        crate::schema::StepBuildCacheInputObjectFile {
                            path: "/src".to_string(),
                            exclude: vec!["tmp".to_string(), "log".to_string()],
                        },
                    ),
                )),
                build_cache: None,
                tag: None,
                privileged: false,
            }],
            export: None,
            metadata: None,
        };

        let pipeline = resolve_pipeline(&pipeline_path, file, None).unwrap();
        let step = match &pipeline.operations[0] {
            Operation::Exec(step) => step,
            _ => panic!("expected exec step"),
        };
        assert_eq!(
            step.build_cache_inputs,
            Some(vec![StepBuildCacheInput {
                path: "/src".to_string(),
                exclude: vec!["tmp".to_string(), "log".to_string()],
            }])
        );
    }

    #[test]
    fn resolves_setup_snapshot_keys() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");

        let file = PipelineFile {
            image: Some("alpine:latest".to_string()),
            runtime: PipelineRuntime::Container,
            host_lock: None,
            platform: Some("linux/amd64".to_string()),
            workdir: Some("/workspace".to_string()),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            definitions: IndexMap::new(),
            includes: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: Some(SetupSnapshotFile {
                path: "/opt/setup".to_string(),
                key: CacheKeyFile::Literal("setup-{platform}".to_string()),
                restore_from: Some(CacheRestoreFromFile::Multiple(vec![
                    CacheKeyFile::Literal("setup-main".to_string()),
                    CacheKeyFile::Literal("setup-fallback-{os}".to_string()),
                ])),
            }),
            steps: vec![StepFile {
                name: Some("install".to_string()),
                run: Some("echo install".to_string()),
                uses: None,
                with: BTreeMap::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                cache: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: None,
                privileged: false,
            }],
            export: None,
            metadata: None,
        };

        let pipeline = resolve_pipeline(&pipeline_path, file, None).unwrap();
        let setup_snapshot = pipeline.setup_snapshot.expect("setup snapshot");
        assert_eq!(setup_snapshot.path, "/opt/setup");
        assert_eq!(setup_snapshot.key, "setup-linux-amd64");
        assert_eq!(
            setup_snapshot.restore_from,
            vec!["setup-main".to_string(), "setup-fallback-linux".to_string()]
        );
    }

    #[test]
    fn rejects_setup_snapshot_for_host_runtime() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");

        let file = PipelineFile {
            image: None,
            runtime: PipelineRuntime::Host,
            host_lock: None,
            platform: Some("darwin/arm64".to_string()),
            workdir: Some("/workspace".to_string()),
            env: BTreeMap::new(),
            inputs: Vec::new(),
            definitions: IndexMap::new(),
            includes: Vec::new(),
            outputs: Vec::new(),
            setup_snapshot: Some(SetupSnapshotFile {
                path: "/opt/setup".to_string(),
                key: CacheKeyFile::Literal("setup".to_string()),
                restore_from: None,
            }),
            steps: vec![StepFile {
                name: Some("install".to_string()),
                run: Some("echo install".to_string()),
                uses: None,
                with: BTreeMap::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                cache: None,
                build_cache_inputs: None,
                build_cache: None,
                tag: None,
                privileged: false,
            }],
            export: None,
            metadata: None,
        };

        let error = resolve_pipeline(&pipeline_path, file, None).unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("setup_snapshot is only supported for container pipelines")
        );
    }

    #[test]
    fn resolves_explicit_step_cache_mounts() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
image: alpine:latest
platform: linux/amd64
env:
  RUBY_VERSION: "3.3"
steps:
  - name: build
    cache:
      - /workspace/vendor/bundle
      - path: /usr/local/bundle
        key: bundler-ruby-{env:RUBY_VERSION}-{platform}
        restore_from:
          - "bundler-ruby-{env:RUBY_VERSION}"
        mode: locked
        read_only: true
    run: echo hi
"#,
        )
        .unwrap();

        let pipeline = match super::load_pipeline(&pipeline_path, None, &[]).unwrap() {
            PipelineOrMulti::Single(pipeline) => pipeline,
            PipelineOrMulti::Multi(_) => panic!("expected single pipeline"),
        };
        let step = match &pipeline.operations[0] {
            Operation::Exec(step) => step,
            _ => panic!("expected exec step"),
        };

        assert_eq!(
            step.run_mounts,
            vec![
                StepRunMount::Cache {
                    target: "/workspace/vendor/bundle".to_string(),
                    id: "cache-linux-amd64-workspace-vendor-bundle".to_string(),
                    key: Some("cache-linux-amd64-workspace-vendor-bundle".to_string()),
                    restore_from: Vec::new(),
                    readonly: false,
                    sharing: CacheMode::Shared,
                },
                StepRunMount::Cache {
                    target: "/usr/local/bundle".to_string(),
                    id: "bundler-ruby-3.3-linux-amd64".to_string(),
                    key: Some("bundler-ruby-3.3-linux-amd64".to_string()),
                    restore_from: vec!["bundler-ruby-3.3".to_string()],
                    readonly: true,
                    sharing: CacheMode::Locked,
                },
            ]
        );
    }

    #[test]
    fn rejects_step_cache_mounts_on_host_runtime() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
runtime: host
platform: darwin/arm64
steps:
  - name: build
    cache: /tmp/boringbuilder-cache
    run: echo hi
"#,
        )
        .unwrap();

        let error = super::load_pipeline(&pipeline_path, None, &[]).unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("step cache mounts are only supported for container pipelines today")
        );
    }

    #[test]
    fn rejects_relative_step_build_cache_inputs() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
image: alpine:latest
steps:
  - name: build
    build_cache_inputs:
      - Gemfile.lock
    run: echo hi
"#,
        )
        .unwrap();

        let error = super::load_pipeline(&pipeline_path, None, &[]).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("step build-cache input path must be an absolute container path"));
    }

    #[test]
    fn rejects_invalid_step_build_cache_input_exclude_pattern() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
image: alpine:latest
steps:
  - name: build
    build_cache_inputs:
      path: /src
      exclude:
        - ""
    run: echo hi
"#,
        )
        .unwrap();

        let error = super::load_pipeline(&pipeline_path, None, &[]).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("invalid step build-cache input exclude pattern"));
    }

    #[test]
    fn parses_multi_target_pipeline() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
targets:
  setup:
    image: alpine:latest
    steps:
      - name: install
        run: echo setup
    outputs:
      - /workspace/out

  build:
    image: alpine:latest
    needs: [setup]
    steps:
      - name: compile
        run: echo build
"#,
        )
        .unwrap();

        let result = super::load_pipeline(&pipeline_path, None, &[]).unwrap();
        let multi = match result {
            PipelineOrMulti::Multi(m) => m,
            PipelineOrMulti::Single(_) => panic!("expected multi-target pipeline"),
        };

        assert_eq!(multi.order, vec!["setup", "build"]);
        assert_eq!(multi.targets.len(), 2);
        assert!(multi.targets.contains_key("setup"));
        assert!(multi.targets.contains_key("build"));
    }

    #[test]
    fn parses_tar_zst_export_in_target_pipeline() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
targets:
  build:
    image: alpine:latest
    steps:
      - run: echo hi
    outputs:
      - /workspace/dist
    export:
      format: tar.zst
      path: ./out/build.tar.zst
"#,
        )
        .unwrap();

        let result = super::load_pipeline(&pipeline_path, None, &[]).unwrap();
        let multi = match result {
            PipelineOrMulti::Multi(m) => m,
            PipelineOrMulti::Single(_) => panic!("expected multi-target pipeline"),
        };

        let export = multi.targets["build"]
            .export
            .as_ref()
            .expect("build target should have export config");
        assert_eq!(export.format, ExportFormat::TarZst);
        assert_eq!(export.path, temp.path().join("out/build.tar.zst"));
    }

    #[test]
    fn expands_export_path_placeholders() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
image: alpine:latest
platform: linux/arm64
steps:
  - run: echo hi
outputs:
  - /workspace/dist
export:
  format: tar.zst
  path: ./tmp/builds/boringcache-snapshot-{arch}-{timestamp}.tar.zst
"#,
        )
        .unwrap();

        let result = super::load_pipeline(&pipeline_path, None, &[]).unwrap();
        let pipeline = match result {
            PipelineOrMulti::Single(p) => p,
            PipelineOrMulti::Multi(_) => panic!("expected single pipeline"),
        };

        let export = pipeline
            .export
            .as_ref()
            .expect("pipeline should have export config");
        assert_eq!(
            export.path.parent().unwrap(),
            temp.path().join("tmp/builds")
        );
        let name = export.path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("boringcache-snapshot-arm64-"));
        assert!(name.ends_with(".tar.zst"));
        let timestamp = name
            .trim_start_matches("boringcache-snapshot-arm64-")
            .trim_end_matches(".tar.zst");
        assert_eq!(timestamp.len(), 15);
        assert_eq!(&timestamp[8..9], "-");
        assert!(timestamp[..8].chars().all(|c| c.is_ascii_digit()));
        assert!(timestamp[9..].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn detects_circular_deps() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
targets:
  a:
    image: alpine
    needs: [b]
    steps:
      - run: echo a
  b:
    image: alpine
    needs: [a]
    steps:
      - run: echo b
"#,
        )
        .unwrap();

        let result = super::load_pipeline(&pipeline_path, None, &[]);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("circular"),
            "expected circular error, got: {err}"
        );
    }

    #[test]
    fn orders_diamond_dag_correctly() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
targets:
  setup:
    image: alpine
    steps:
      - run: echo setup

  lint:
    image: alpine
    needs: [setup]
    steps:
      - run: echo lint

  test:
    image: alpine
    needs: [setup]
    steps:
      - run: echo test

  build:
    image: alpine
    needs: [lint, test]
    steps:
      - run: echo build
"#,
        )
        .unwrap();

        let result = super::load_pipeline(&pipeline_path, None, &[]).unwrap();
        let multi = match result {
            PipelineOrMulti::Multi(m) => m,
            PipelineOrMulti::Single(_) => panic!("expected multi-target pipeline"),
        };

        assert_eq!(multi.order[0], "setup");
        assert_eq!(*multi.order.last().unwrap(), "build");
        let lint_pos = multi.order.iter().position(|n| n == "lint").unwrap();
        let test_pos = multi.order.iter().position(|n| n == "test").unwrap();
        assert!(lint_pos > 0 && test_pos > 0);
        assert!(lint_pos < 3 && test_pos < 3);
    }

    #[test]
    fn rejects_unknown_dependency() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
targets:
  build:
    image: alpine
    needs: [nonexistent]
    steps:
      - run: echo hi
"#,
        )
        .unwrap();

        let result = super::load_pipeline(&pipeline_path, None, &[]);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("nonexistent"),
            "expected unknown target error, got: {err}"
        );
    }

    #[test]
    fn applies_build_args_to_native_targets() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
targets:
  build:
    image: alpine
    steps:
      - run: echo hi
"#,
        )
        .unwrap();

        let result = super::load_pipeline(
            &pipeline_path,
            None,
            &[("RAILS_ENV".to_string(), "production".to_string())],
        )
        .unwrap();
        let multi = match result {
            PipelineOrMulti::Multi(m) => m,
            PipelineOrMulti::Single(_) => panic!("expected multi-target pipeline"),
        };

        assert_eq!(
            multi.targets["build"].env.get("RAILS_ENV"),
            Some(&"production".to_string())
        );
    }

    #[test]
    fn selects_target_name_with_dependency_closure() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
targets:
  setup:
    image: alpine
    steps:
      - run: echo setup

  lint:
    image: alpine
    needs: [setup]
    steps:
      - run: echo lint

  test:
    image: alpine
    needs: [setup]
    steps:
      - run: echo test

  build:
    image: alpine
    needs: [lint, test]
    steps:
      - run: echo build
"#,
        )
        .unwrap();

        let result = super::load_pipeline(&pipeline_path, None, &[]).unwrap();
        let multi = match result {
            PipelineOrMulti::Multi(m) => m,
            PipelineOrMulti::Single(_) => panic!("expected multi-target pipeline"),
        };

        let selected = super::select_multi_target(&multi, "test").unwrap();
        assert_eq!(
            selected.order,
            vec!["setup".to_string(), "test".to_string()]
        );
        assert!(selected.targets.contains_key("setup"));
        assert!(selected.targets.contains_key("test"));
        assert!(!selected.targets.contains_key("lint"));
        assert!(!selected.targets.contains_key("build"));
    }

    #[test]
    fn rejects_unknown_target_name_selection() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
targets:
  build:
    image: alpine
    steps:
      - run: echo build
"#,
        )
        .unwrap();

        let result = super::load_pipeline(&pipeline_path, None, &[]).unwrap();
        let multi = match result {
            PipelineOrMulti::Multi(m) => m,
            PipelineOrMulti::Single(_) => panic!("expected multi-target pipeline"),
        };

        let err = super::select_multi_target(&multi, "test").unwrap_err();
        assert!(err.to_string().contains("unknown target 'test'"));
    }

    #[test]
    fn expands_dockerfile_target_stages_with_final_target_name() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("Dockerfile"),
            r#"
FROM golang:1.22 AS builder
WORKDIR /src
COPY main.go /src/
RUN go build -o /app main.go

FROM alpine:3.20
COPY --from=builder /app /app
CMD ["/app"]
"#,
        )
        .unwrap();
        fs::write(
            temp.path().join("main.go"),
            "package main\nfunc main() {}\n",
        )
        .unwrap();

        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
targets:
  image:
    dockerfile:
      file: ./Dockerfile
  verify:
    image: alpine
    needs: [image]
    steps:
      - run: test -e /boringbuilder-stages/image/app
"#,
        )
        .unwrap();

        let result = super::load_pipeline(&pipeline_path, None, &[]).unwrap();
        let multi = match result {
            PipelineOrMulti::Multi(m) => m,
            PipelineOrMulti::Single(_) => panic!("expected multi-target pipeline"),
        };

        assert_eq!(multi.order, vec!["image::builder", "image", "verify"]);
        assert!(multi.targets.contains_key("image::builder"));
        assert!(multi.targets.contains_key("image"));
        assert_eq!(multi.targets["image"].needs, vec!["image::builder"]);
        assert_eq!(multi.targets["verify"].needs, vec!["image"]);
        assert!(matches!(
            &multi.targets["image"].operations[0],
            Operation::CopyFromStage(op) if op.stage == "image::builder"
        ));
    }

    #[test]
    fn rejects_native_fields_on_dockerfile_targets() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("Dockerfile"), "FROM alpine\nRUN true\n").unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
targets:
  image:
    image: alpine
    dockerfile:
      file: ./Dockerfile
"#,
        )
        .unwrap();

        let result = super::load_pipeline(&pipeline_path, None, &[]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            format!("{err:#}").contains("cannot set image"),
            "expected dockerfile target validation failure, got: {err:#}"
        );
    }

    #[test]
    fn rejects_native_target_without_image() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
targets:
  build:
    steps:
      - run: echo hello
"#,
        )
        .unwrap();

        let result = super::load_pipeline(&pipeline_path, None, &[]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            format!("{err:#}").contains("must set image"),
            "expected missing image validation failure, got: {err:#}"
        );
    }

    #[test]
    fn rejects_native_target_without_steps() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
targets:
  build:
    image: alpine
"#,
        )
        .unwrap();

        let result = super::load_pipeline(&pipeline_path, None, &[]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            format!("{err:#}").contains("must contain at least one step"),
            "expected missing step validation failure, got: {err:#}"
        );
    }

    #[test]
    fn resolves_dockerfile_target_with_per_target_build_args() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join("Dockerfile"),
            "FROM alpine\nARG APP_ENV=dev\nRUN echo $APP_ENV\n",
        )
        .unwrap();

        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
targets:
  image:
    dockerfile:
      file: ./Dockerfile
      build_args:
        APP_ENV: production
"#,
        )
        .unwrap();

        let result = super::load_pipeline(&pipeline_path, None, &[]).unwrap();
        let multi = match result {
            PipelineOrMulti::Multi(m) => m,
            PipelineOrMulti::Single(_) => panic!("expected multi-target pipeline"),
        };

        let operation = &multi.targets["image"].operations[0];
        assert!(matches!(
            operation,
            Operation::Exec(step) if step.run.contains("production")
        ));
    }

    #[test]
    fn target_file_deserializes_dockerfile_targets() {
        let value = yaml_serde::from_str::<TargetFile>(
            r#"
dockerfile:
  file: ./Dockerfile
  context: .
  build_args:
    APP_ENV: production
needs: [setup]
"#,
        )
        .unwrap();

        assert!(value.image.is_none());
        assert!(value.steps.is_empty());
        let dockerfile = value.dockerfile.unwrap();
        assert_eq!(dockerfile.file, PathBuf::from("./Dockerfile"));
        assert_eq!(dockerfile.context, Some(PathBuf::from(".")));
        assert_eq!(
            dockerfile.build_args.get("APP_ENV"),
            Some(&"production".to_string())
        );
        assert_eq!(value.needs, vec!["setup"]);
    }

    #[test]
    fn uses_expands_definition() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
image: alpine:latest
definitions:
  greet:
    run: echo hello
steps:
  - uses: greet
"#,
        )
        .unwrap();

        let pipeline = match super::load_pipeline(&pipeline_path, None, &[]).unwrap() {
            PipelineOrMulti::Single(pipeline) => pipeline,
            PipelineOrMulti::Multi(_) => panic!("expected single pipeline"),
        };
        let step = match &pipeline.operations[0] {
            Operation::Exec(step) => step,
            _ => panic!("expected exec step"),
        };
        assert_eq!(step.run, "echo hello");
    }

    #[test]
    fn with_substitutes_into_definition_fields() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
image: alpine:latest
definitions:
  setup:
    name: setup-${tool}
    run: install ${tool} ${version:-latest}
    workdir: /workspace/${tool}
    tag: deps-${tool}
    env:
      TOOL: ${tool}
steps:
  - uses: setup
    with:
      tool: rust
      version: "1.80"
"#,
        )
        .unwrap();

        let pipeline = match super::load_pipeline(&pipeline_path, None, &[]).unwrap() {
            PipelineOrMulti::Single(pipeline) => pipeline,
            PipelineOrMulti::Multi(_) => panic!("expected single pipeline"),
        };
        let step = match &pipeline.operations[0] {
            Operation::Exec(step) => step,
            _ => panic!("expected exec step"),
        };
        assert_eq!(step.name.as_deref(), Some("setup-rust"));
        assert_eq!(step.run, "install rust 1.80");
        assert_eq!(step.workdir.as_deref(), Some("/workspace/rust"));
        assert_eq!(step.tag.as_deref(), Some("deps-rust"));
        assert_eq!(step.env.get("TOOL"), Some(&"rust".to_string()));
    }

    #[test]
    fn uses_step_can_override_fields() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
image: alpine:latest
definitions:
  base:
    name: base-step
    workdir: /original
    run: echo hi
    tag: base-tag
steps:
  - uses: base
    name: custom-name
    workdir: /custom
    tag: custom-tag
"#,
        )
        .unwrap();

        let pipeline = match super::load_pipeline(&pipeline_path, None, &[]).unwrap() {
            PipelineOrMulti::Single(pipeline) => pipeline,
            PipelineOrMulti::Multi(_) => panic!("expected single pipeline"),
        };
        let step = match &pipeline.operations[0] {
            Operation::Exec(step) => step,
            _ => panic!("expected exec step"),
        };
        assert_eq!(step.name.as_deref(), Some("custom-name"));
        assert_eq!(step.workdir.as_deref(), Some("/custom"));
        assert_eq!(step.tag.as_deref(), Some("custom-tag"));
    }

    #[test]
    fn rejects_both_run_and_uses() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
image: alpine:latest
definitions:
  greet:
    run: echo hello
steps:
  - run: echo world
    uses: greet
"#,
        )
        .unwrap();

        let err = super::load_pipeline(&pipeline_path, None, &[]).unwrap_err();
        assert!(format!("{err:#}").contains("both 'run' and 'uses'"));
    }

    #[test]
    fn rejects_with_without_uses() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
image: alpine:latest
steps:
  - run: echo hi
    with:
      tool: rust
"#,
        )
        .unwrap();

        let err = super::load_pipeline(&pipeline_path, None, &[]).unwrap_err();
        assert!(format!("{err:#}").contains("cannot set 'with' without 'uses'"));
    }

    #[test]
    fn rejects_uses_referencing_missing_definition() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
image: alpine:latest
steps:
  - uses: nonexistent
"#,
        )
        .unwrap();

        let err = super::load_pipeline(&pipeline_path, None, &[]).unwrap_err();
        assert!(format!("{err:#}").contains("unknown definition 'nonexistent'"));
    }

    #[test]
    fn rejects_step_without_run_or_uses() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
image: alpine:latest
steps:
  - name: empty
"#,
        )
        .unwrap();

        let err = super::load_pipeline(&pipeline_path, None, &[]).unwrap_err();
        assert!(format!("{err:#}").contains("must have either 'run' or 'uses'"));
    }

    #[test]
    fn includes_loads_definitions_from_external_file() {
        let temp = tempdir().unwrap();
        let steps_dir = temp.path().join("steps");
        fs::create_dir_all(&steps_dir).unwrap();
        fs::write(
            steps_dir.join("common.yml"),
            r#"
greet:
  run: echo hello ${who:-world}
"#,
        )
        .unwrap();

        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
image: alpine:latest
includes:
  - steps/common.yml
steps:
  - uses: greet
    with:
      who: boringbuilder
"#,
        )
        .unwrap();

        let pipeline = match super::load_pipeline(&pipeline_path, None, &[]).unwrap() {
            PipelineOrMulti::Single(pipeline) => pipeline,
            PipelineOrMulti::Multi(_) => panic!("expected single pipeline"),
        };
        let step = match &pipeline.operations[0] {
            Operation::Exec(step) => step,
            _ => panic!("expected exec step"),
        };
        assert_eq!(step.run, "echo hello boringbuilder");
    }

    #[test]
    fn multi_target_outer_definitions_visible_to_targets() {
        let temp = tempdir().unwrap();
        let pipeline_path = temp.path().join("pipeline.yml");
        fs::write(
            &pipeline_path,
            r#"
definitions:
  greet:
    run: echo hello
targets:
  build:
    image: alpine:latest
    steps:
      - uses: greet
"#,
        )
        .unwrap();

        let result = super::load_pipeline(&pipeline_path, None, &[]).unwrap();
        let multi = match result {
            PipelineOrMulti::Multi(multi) => multi,
            PipelineOrMulti::Single(_) => panic!("expected multi-target pipeline"),
        };
        let step = match &multi.targets["build"].operations[0] {
            Operation::Exec(step) => step,
            _ => panic!("expected exec step"),
        };
        assert_eq!(step.run, "echo hello");
    }

    #[test]
    fn config_pipeline_uses_git_repo_root_as_workspace_base() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let config_dir = root.join("config");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(root.join("mise.toml"), "min_version = \"2026.1.0\"\n").unwrap();

        let Some(git) = crate::util::process::find_command("git") else {
            return;
        };
        crate::util::process::run_capture(
            &git,
            &[
                "-C".to_string(),
                root.display().to_string(),
                "init".to_string(),
            ],
        )
        .unwrap();

        let pipeline_path = config_dir.join("boringbuilder.yml");
        let file = PipelineFile {
            image: Some("alpine:latest".to_string()),
            runtime: PipelineRuntime::Container,
            host_lock: None,
            platform: Some("linux/amd64".to_string()),
            workdir: Some("/workspace".to_string()),
            env: BTreeMap::new(),
            inputs: vec![InputFile {
                source: PathBuf::from("."),
                dest: "/src".to_string(),
                readonly: true,
            }],
            definitions: IndexMap::new(),
            includes: Vec::new(),
            outputs: vec!["/workspace/out".to_string()],
            setup_snapshot: None,
            steps: vec![StepFile {
                name: Some("sync".to_string()),
                run: Some("install -m 644 /src/mise.toml /workspace/mise.toml".to_string()),
                uses: None,
                with: BTreeMap::new(),
                env: BTreeMap::new(),
                workdir: None,
                shell: None,
                cache: None,
                build_cache_inputs: Some(StepBuildCacheInputsFile::Single(
                    StepBuildCacheInputFile::Path("/src/mise.toml".to_string()),
                )),
                build_cache: None,
                tag: None,
                privileged: false,
            }],
            export: Some(ExportConfig {
                format: crate::schema::ExportFormat::TarZst,
                path: PathBuf::from("./tmp/builds/out.tar.zst"),
                reproducible: true,
            }),
            metadata: None,
        };

        let pipeline = resolve_pipeline(&pipeline_path, file, None).unwrap();
        let root = root.canonicalize().unwrap();

        assert_eq!(pipeline.base_dir, root);
        assert_eq!(pipeline.inputs[0].source, pipeline.base_dir);
        assert_eq!(
            pipeline
                .export
                .unwrap()
                .path
                .strip_prefix(&pipeline.base_dir)
                .unwrap(),
            Path::new("tmp/builds/out.tar.zst")
        );
    }
}
