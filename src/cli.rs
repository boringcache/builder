use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};

use crate::backend::{RunOptions, detect::detect_for_loaded};
use crate::cache::{CacheStoreConfig, CacheStoreKind};
use crate::planner::{load_pipeline, render_multi_plan, render_plan, select_multi_target};
use crate::registry::push::push_image;
use crate::runtime::runner::{run_multi_target_recipe, run_pipeline};
use crate::schema::{ExportFormat, PipelineOrMulti};
use crate::ui;

const DEFAULT_RECIPE_FILES: &[&str] = &[
    "boringbuilder.yml",
    "boringbuilder.yaml",
    "config/boringbuilder.yml",
    "config/boringbuilder.yaml",
    "Dockerfile",
];

#[derive(Debug, Parser)]
#[command(name = "boringbuilder")]
#[command(about = "Build portable artifacts and OCI images without Docker")]
#[command(version)]
struct App {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build an artifact or image from a recipe or Dockerfile.
    Build(BuildArgs),
}

#[derive(Debug, clap::Args)]
struct BuildArgs {
    /// Recipe or Dockerfile. Defaults to boringbuilder.yml or Dockerfile.
    #[arg(short = 'f', long = "file")]
    recipe: Option<PathBuf>,
    /// Target platform, for example linux/amd64 or linux/arm64.
    #[arg(long)]
    platform: Option<String>,
    /// Build only this target and its dependencies.
    #[arg(long)]
    target: Option<String>,
    /// Override the recipe's export format.
    #[arg(long = "format")]
    export_format: Option<ExportFormat>,
    /// Override the recipe's output path.
    #[arg(short = 'o', long)]
    output: Option<PathBuf>,
    /// Set a build argument (KEY=VALUE).
    #[arg(long = "build-arg", short = 'e')]
    build_args: Vec<String>,
    /// Cache backend: local, boringcache, or an OCI registry reference.
    #[arg(long = "cache", default_value = "local")]
    cache: String,
    /// Local cache directory.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// BoringCache workspace.
    #[arg(long)]
    cache_workspace: Option<String>,
    /// BoringCache binary override.
    #[arg(long)]
    cache_bin: Option<PathBuf>,
    /// Use HTTP for an OCI registry cache.
    #[arg(long)]
    cache_insecure: bool,
    /// Disable cache restore and save.
    #[arg(long)]
    no_cache: bool,
    /// Explain cache hits and misses.
    #[arg(long)]
    cache_explain: bool,
    /// Print phase and step timings.
    #[arg(long)]
    timings: bool,
    /// Write timings as JSON.
    #[arg(long)]
    timings_json: Option<PathBuf>,
    /// Print the resolved build plan without executing it.
    #[arg(long)]
    dry_run: bool,
    /// Push the result as an OCI image.
    #[arg(long, value_name = "IMAGE")]
    push: Option<String>,
    /// Use HTTP when pushing the image.
    #[arg(long)]
    insecure: bool,
}

pub fn run() -> Result<()> {
    match App::parse().command {
        Command::Build(args) => build(args),
    }
}

fn build(args: BuildArgs) -> Result<()> {
    let recipe_path = resolve_recipe_path(args.recipe.clone())?;
    let build_args = parse_build_args(&args.build_args)?;
    let loaded = if crate::dockerfile::is_dockerfile(&recipe_path) {
        let context = recipe_path.parent().unwrap_or(Path::new("."));
        crate::dockerfile::load_dockerfile(
            &recipe_path,
            context,
            args.platform.as_deref(),
            &build_args,
            None,
        )?
    } else {
        load_pipeline(&recipe_path, args.platform.as_deref(), &build_args)?
    };
    let loaded = select_target(loaded, args.target.as_deref())?;

    if args.dry_run {
        let mut plan = loaded.clone();
        apply_plan_overrides(&mut plan, &args)?;
        print_plan(&plan)?;
        if let Some(target) = &args.push {
            println!("push: {target}");
        }
        return Ok(());
    }

    let mut push_output = None;
    let output = match (args.output.clone(), args.push.as_ref()) {
        (Some(path), _) => Some(path),
        (None, Some(_)) => {
            let directory = tempfile::Builder::new()
                .prefix("boringbuilder-push-")
                .tempdir()
                .context("failed to create temporary OCI output directory")?;
            let path = directory.path().join("image.oci");
            push_output = Some(directory);
            Some(path)
        }
        (None, None) => None,
    };

    let options = RunOptions {
        timings: args.timings,
        cache_explain: args.cache_explain,
        export_format_override: args
            .push
            .as_ref()
            .map(|_| ExportFormat::Oci)
            .or(args.export_format),
        output_path_override: output,
        no_cache: args.no_cache,
        ..RunOptions::default()
    };
    let cache = cache_config(&args)?;
    let backend = detect_for_loaded(&loaded)?;
    println!("{} {}", ui::accent("backend:"), backend.name());
    let summary = match loaded {
        PipelineOrMulti::Single(pipeline) => {
            run_pipeline(backend.as_ref(), &pipeline, &options, &cache)?
        }
        PipelineOrMulti::Multi(pipeline) => {
            run_multi_target_recipe(backend.as_ref(), &pipeline, &options, &cache)?
        }
    };

    let export = summary.export_path.as_deref().ok_or_else(|| {
        anyhow::anyhow!("build produced no output; declare export: in the recipe or pass -o")
    })?;
    println!("{} {}", ui::success("exported:"), export.display());

    if let Some(path) = args.timings_json {
        let payload = serde_json::to_string_pretty(&summary.timings)?;
        std::fs::write(&path, format!("{payload}\n"))
            .with_context(|| format!("failed to write {}", path.display()))?;
        println!("{} {}", ui::accent("timings:"), path.display());
    }

    if let Some(target) = args.push {
        ui::print_status(format!("pushing {target}"));
        let digest = push_image(export, &target, args.insecure)?;
        println!("{} {digest}", ui::success("pushed:"));
    }
    drop(push_output);
    Ok(())
}

fn apply_plan_overrides(loaded: &mut PipelineOrMulti, args: &BuildArgs) -> Result<()> {
    let pipeline = match loaded {
        PipelineOrMulti::Single(pipeline) => pipeline,
        PipelineOrMulti::Multi(multi) => {
            let name = multi
                .order
                .last()
                .context("multi-target recipe has no final target")?;
            multi
                .targets
                .get_mut(name)
                .context("multi-target recipe is missing its final target")?
        }
    };

    if let Some(export) = &mut pipeline.export {
        if let Some(format) = args.export_format {
            export.format = format;
        }
        if let Some(path) = &args.output {
            export.path = path.clone();
        }
    } else if let Some(path) = &args.output {
        pipeline.export = Some(crate::schema::ExportConfig {
            format: args.export_format.unwrap_or(ExportFormat::Oci),
            path: path.clone(),
            reproducible: true,
        });
    }

    if args.push.is_some() {
        let export = pipeline.export.get_or_insert(crate::schema::ExportConfig {
            format: ExportFormat::Oci,
            path: PathBuf::from("<temporary-oci-layout>"),
            reproducible: true,
        });
        export.format = ExportFormat::Oci;
    }
    Ok(())
}

fn select_target(loaded: PipelineOrMulti, target: Option<&str>) -> Result<PipelineOrMulti> {
    match (loaded, target) {
        (PipelineOrMulti::Single(_), Some(target)) => {
            bail!("--target {target} requires a recipe with multiple targets")
        }
        (PipelineOrMulti::Multi(pipeline), Some(target)) => Ok(PipelineOrMulti::Multi(
            select_multi_target(&pipeline, target)?,
        )),
        (loaded, None) => Ok(loaded),
    }
}

fn print_plan(loaded: &PipelineOrMulti) -> Result<()> {
    match loaded {
        PipelineOrMulti::Single(pipeline) => print!("{}", render_plan(pipeline)?),
        PipelineOrMulti::Multi(pipeline) => print!("{}", render_multi_plan(pipeline)?),
    }
    Ok(())
}

fn cache_config(args: &BuildArgs) -> Result<CacheStoreConfig> {
    let cache_store = match args.cache.as_str() {
        "local" => CacheStoreKind::Local,
        "boringcache" => CacheStoreKind::BoringCache,
        reference if reference.contains('/') || reference.contains('.') => {
            CacheStoreKind::Registry {
                reference: reference.to_string(),
                insecure: args.cache_insecure,
            }
        }
        value => {
            bail!("unknown cache backend '{value}'; use local, boringcache, or an OCI reference")
        }
    };
    Ok(CacheStoreConfig {
        cache_dir: args.cache_dir.clone(),
        cache_store,
        cache_store_explicit: args.cache != "local",
        cache_workspace: args.cache_workspace.clone(),
        cache_bin: args.cache_bin.clone(),
        platform: args.platform.clone(),
    })
}

fn resolve_recipe_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    DEFAULT_RECIPE_FILES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no build recipe found; create boringbuilder.yml or pass a file with -f"
            )
        })
}

fn parse_build_args(args: &[String]) -> Result<Vec<(String, String)>> {
    args.iter()
        .map(|arg| {
            let (key, value) = arg.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("invalid build argument '{arg}', expected KEY=VALUE")
            })?;
            ensure!(!key.is_empty(), "build argument key cannot be empty");
            Ok((key.to_string(), value.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parse_build_args;

    #[test]
    fn parses_build_arguments() {
        assert_eq!(
            parse_build_args(&["VERSION=1.2.3".to_string()]).unwrap(),
            vec![("VERSION".to_string(), "1.2.3".to_string())]
        );
    }
}
