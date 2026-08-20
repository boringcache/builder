use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use indexmap::IndexMap;
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

pub const HOST_RUNTIME_IMAGE: &str = "host://local";

pub const PRIVILEGED_STEP_ENV: &str = "BORINGBUILDER_PRIVILEGED";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineFile {
    pub image: Option<String>,
    #[serde(default)]
    pub runtime: PipelineRuntime,
    #[serde(default)]
    pub host_lock: Option<String>,
    pub platform: Option<String>,
    pub workdir: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub inputs: Vec<InputFile>,
    #[serde(default)]
    pub definitions: IndexMap<String, StepFile>,
    #[serde(default)]
    pub includes: Vec<PathBuf>,
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(default)]
    pub setup_snapshot: Option<SetupSnapshotFile>,
    #[serde(default)]
    pub steps: Vec<StepFile>,
    pub export: Option<ExportConfig>,
    #[serde(default)]
    pub metadata: Option<ImageMetadata>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InputFile {
    pub source: PathBuf,
    pub dest: String,
    #[serde(default)]
    pub readonly: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepCacheMountObjectFile {
    pub path: String,
    #[serde(default)]
    pub key: Option<CacheKeyFile>,
    #[serde(default)]
    pub restore_from: Option<CacheRestoreFromFile>,
    #[serde(default)]
    pub mode: CacheMode,
    #[serde(default, alias = "readonly")]
    pub read_only: bool,
}

#[derive(Debug, Clone)]
pub enum StepCacheMountFile {
    Path(String),
    Mount(StepCacheMountObjectFile),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum CacheRestoreFromFile {
    Single(CacheKeyFile),
    Multiple(Vec<CacheKeyFile>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupSnapshotFile {
    pub path: String,
    pub key: CacheKeyFile,
    #[serde(default)]
    pub restore_from: Option<CacheRestoreFromFile>,
}

#[derive(Debug, Clone)]
pub enum StepCacheFile {
    Single(StepCacheMountFile),
    Multiple(Vec<StepCacheMountFile>),
}

impl<'de> Deserialize<'de> for StepCacheMountFile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::String(path) => Ok(Self::Path(path)),
            serde_json::Value::Object(_) => {
                let mount = serde_json::from_value(value).map_err(de::Error::custom)?;
                Ok(Self::Mount(mount))
            }
            _ => Err(de::Error::custom(
                "cache entry must be a string reference/path or a mapping",
            )),
        }
    }
}

impl<'de> Deserialize<'de> for StepCacheFile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Array(entries) => {
                let mut resolved = Vec::with_capacity(entries.len());
                for entry in entries {
                    let cache = serde_json::from_value(entry).map_err(de::Error::custom)?;
                    resolved.push(cache);
                }
                Ok(Self::Multiple(resolved))
            }
            other => {
                let cache = serde_json::from_value(other).map_err(de::Error::custom)?;
                Ok(Self::Single(cache))
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepBuildCacheInputObjectFile {
    pub path: String,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StepBuildCacheInputFile {
    Path(String),
    Structured(StepBuildCacheInputObjectFile),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StepBuildCacheInputsFile {
    Single(StepBuildCacheInputFile),
    Multiple(Vec<StepBuildCacheInputFile>),
}

#[derive(Debug, Clone)]
pub enum CacheKeyFile {
    Literal(String),
    Structured(CacheKeySpecFile),
}

impl<'de> Deserialize<'de> for CacheKeyFile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::String(value) => Ok(Self::Literal(value)),
            serde_json::Value::Object(_) => {
                let spec = serde_json::from_value(value).map_err(de::Error::custom)?;
                Ok(Self::Structured(spec))
            }
            _ => Err(de::Error::custom("cache key must be a string or a mapping")),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CacheKeySpecFile {
    pub prefix: Option<String>,
    #[serde(default)]
    pub files: Vec<PathBuf>,
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepFile {
    pub name: Option<String>,
    pub run: Option<String>,
    #[serde(default)]
    pub uses: Option<String>,
    #[serde(default)]
    pub with: BTreeMap<String, String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub workdir: Option<String>,
    pub shell: Option<String>,
    #[serde(default)]
    pub cache: Option<StepCacheFile>,
    /// Retained for YAML backward compatibility with the older
    /// build-cache naming. These inputs now define the step-slice
    /// invalidation boundary when you need something narrower or more
    /// explicit than the default filesystem snapshot.
    pub build_cache_inputs: Option<StepBuildCacheInputsFile>,
    /// Retained for YAML backward compatibility. When false, this step
    /// opts out of automatic step-slice reuse.
    #[serde(default)]
    pub build_cache: Option<bool>,
    /// Human-readable tag for the step's filesystem slice.  Pipelines
    /// sharing the same tag reuse each other's cached delta.
    pub tag: Option<String>,
    #[serde(default)]
    pub privileged: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Step {
    pub name: Option<String>,
    pub run: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_exec: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub run_mounts: Vec<StepRunMount>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub workdir: Option<String>,
    pub shell: Option<String>,
    /// Retained for YAML backward compatibility with the older
    /// build-cache naming. These inputs now define the step-slice
    /// invalidation boundary when you need something narrower or more
    /// explicit than the default filesystem snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_cache_inputs: Option<Vec<StepBuildCacheInput>>,
    /// Retained for YAML backward compatibility. When false, this step
    /// opts out of automatic step-slice reuse.
    pub build_cache: Option<bool>,
    /// Human-readable tag for cross-pipeline slice reuse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct StepBuildCacheInput {
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum CacheMode {
    #[default]
    Shared,
    Locked,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepRunMount {
    Cache {
        target: String,
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        restore_from: Vec<String>,
        #[serde(default)]
        readonly: bool,
        #[serde(default)]
        sharing: CacheMode,
    },
    Bind {
        target: String,
        source: StepRunBindSource,
        #[serde(default = "default_true")]
        readonly: bool,
    },
    Tmpfs {
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<String>,
    },
    Secret {
        target: String,
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<String>,
        #[serde(default)]
        required: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uid: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gid: Option<u32>,
    },
    Ssh {
        target: String,
        id: String,
        #[serde(default)]
        required: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uid: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gid: Option<u32>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepRunBindSource {
    Context { path: String },
    Stage { stage: String, path: String },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", content = "spec", rename_all = "snake_case")]
pub enum Operation {
    Exec(Step),
    CopyFromContext(ContextCopyOp),
    CopyFromStage(StageCopyOp),
    AddRemote(RemoteAddOp),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContextCopyOp {
    pub name: Option<String>,
    pub sources: Vec<String>,
    pub dest: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub extract_archives: bool,
    #[serde(default)]
    pub preserve_parents: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chown: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chmod: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StageCopyOp {
    pub name: Option<String>,
    pub stage: String,
    pub sources: Vec<String>,
    pub dest: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub preserve_parents: bool,
    #[serde(default = "default_true")]
    pub follow_symlinks: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chown: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chmod: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RemoteAddOp {
    pub name: Option<String>,
    pub url: String,
    pub dest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExportConfig {
    pub format: ExportFormat,
    pub path: PathBuf,
    #[serde(default = "default_true")]
    pub reproducible: bool,
}

/// Image metadata applied to the OCI config on export.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ImageMetadata {
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<ImageHealthcheck>,
    pub user: Option<String>,
    #[serde(default)]
    pub expose: Vec<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub volumes: Vec<String>,
    pub stop_signal: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageHealthcheck {
    None,
    Command {
        test: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interval_nanos: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_nanos: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start_period_nanos: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start_interval_nanos: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retries: Option<u32>,
    },
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, clap::ValueEnum)]
pub enum ExportFormat {
    #[serde(rename = "oci")]
    #[value(name = "oci")]
    Oci,
    #[serde(rename = "docker")]
    #[value(name = "docker")]
    Docker,
    #[serde(rename = "tar")]
    #[value(name = "tar")]
    Tar,
    #[serde(rename = "tar.zst", alias = "tar-zst", alias = "tar_zst")]
    #[value(name = "tar.zst", alias = "tar-zst", alias = "tar_zst")]
    TarZst,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiTargetRecipeFile {
    #[serde(default)]
    pub definitions: IndexMap<String, StepFile>,
    #[serde(default)]
    pub includes: Vec<PathBuf>,
    #[serde(alias = "jobs")]
    pub targets: IndexMap<String, TargetFile>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetFile {
    pub image: Option<String>,
    #[serde(default)]
    pub runtime: PipelineRuntime,
    #[serde(default)]
    pub host_lock: Option<String>,
    pub platform: Option<String>,
    pub workdir: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub inputs: Vec<InputFile>,
    #[serde(default)]
    pub definitions: IndexMap<String, StepFile>,
    #[serde(default)]
    pub includes: Vec<PathBuf>,
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(default)]
    pub setup_snapshot: Option<SetupSnapshotFile>,
    #[serde(default)]
    pub steps: Vec<StepFile>,
    pub dockerfile: Option<DockerfileTargetFile>,
    pub export: Option<ExportConfig>,
    #[serde(default)]
    pub metadata: Option<ImageMetadata>,
    #[serde(default)]
    pub needs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DockerfileTargetFile {
    pub file: PathBuf,
    pub context: Option<PathBuf>,
    #[serde(default)]
    pub build_args: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MultiTargetRecipe {
    pub targets: IndexMap<String, Pipeline>,
    pub order: Vec<String>,
    pub base_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "kind", content = "pipeline", rename_all = "snake_case")]
pub enum PipelineOrMulti {
    Single(Pipeline),
    Multi(MultiTargetRecipe),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Pipeline {
    pub image: String,
    pub platform: String,
    pub workdir: String,
    pub env: BTreeMap<String, String>,
    pub inputs: Vec<Input>,
    pub outputs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_snapshot: Option<SetupSnapshot>,
    pub operations: Vec<Operation>,
    pub export: Option<ExportConfig>,
    pub metadata: Option<ImageMetadata>,
    pub base_dir: PathBuf,
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default)]
    pub stage_dependency_digests: BTreeMap<String, String>,
    #[serde(default)]
    pub stage_snapshot_follow_symlinks: BTreeSet<String>,
    #[serde(default)]
    pub docker_context: Option<DockerContext>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PipelineRuntime {
    #[default]
    Container,
    Host,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Input {
    pub source: PathBuf,
    pub dest: String,
    pub readonly: bool,
}

pub type CacheDefinitionFile = StepCacheMountObjectFile;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CacheMount {
    pub id: String,
    pub path: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restore_from: Vec<String>,
    pub mode: CacheMode,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SetupSnapshot {
    pub path: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restore_from: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DockerContext {
    pub root: PathBuf,
    pub ignore_patterns: Vec<String>,
}

impl Pipeline {
    pub fn step_start_index(&self, selector: Option<&str>) -> anyhow::Result<usize> {
        let Some(selector) = selector.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(0);
        };

        if let Ok(index) = selector.parse::<usize>() {
            anyhow::ensure!(
                index > 0 && index <= self.operations.len(),
                "step index {index} is out of range (1..={})",
                self.operations.len()
            );
            return Ok(index - 1);
        }

        self.operations
            .iter()
            .position(|operation| operation.name() == Some(selector))
            .ok_or_else(|| anyhow::anyhow!("step '{selector}' was not found"))
    }
}

impl Operation {
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Exec(step) => step.name.as_deref(),
            Self::CopyFromContext(op) => op.name.as_deref(),
            Self::CopyFromStage(op) => op.name.as_deref(),
            Self::AddRemote(op) => op.name.as_deref(),
        }
    }

    pub fn cache_enabled(&self) -> bool {
        match self {
            Self::Exec(step) => step.build_cache != Some(false),
            Self::CopyFromContext(_) | Self::CopyFromStage(_) => true,
            Self::AddRemote(_) => false,
        }
    }
}

fn default_true() -> bool {
    true
}
