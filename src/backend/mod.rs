pub mod detect;
pub mod image_cache;
pub mod linux_exec;
pub mod macos_container;
pub mod macos_guest_helper;
pub mod macos_host;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cache::CacheStoreConfig;
use crate::schema::{ExportFormat, Operation, Pipeline};
use crate::ui;

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub from_step: Option<String>,
    pub keep: bool,
    pub timings: bool,
    pub cache_explain: bool,
    pub export_format_override: Option<ExportFormat>,
    pub output_path_override: Option<PathBuf>,
    pub no_cache: bool,
    /// When true the backend should preserve the rootfs after execution so
    /// dependent stages can mount it.
    pub keep_rootfs: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunTimings {
    pub pull_ms: u128,
    pub unpack_ms: u128,
    pub prepare_ms: u128,
    pub operation_ms: u128,
    pub cache_restore_ms: u128,
    pub cache_save_ms: u128,
    pub export_ms: u128,
    pub total_ms: u128,
    pub export_bytes: u64,
    pub stage_rootfs_bytes: u64,
    pub cache: CacheMetrics,
    pub operations: Vec<OperationTiming>,
}

impl RunTimings {
    pub fn merge(&mut self, other: &RunTimings) {
        self.pull_ms += other.pull_ms;
        self.unpack_ms += other.unpack_ms;
        self.prepare_ms += other.prepare_ms;
        self.operation_ms += other.operation_ms;
        self.cache_restore_ms += other.cache_restore_ms;
        self.cache_save_ms += other.cache_save_ms;
        self.export_ms += other.export_ms;
        self.total_ms += other.total_ms;
        self.export_bytes += other.export_bytes;
        self.stage_rootfs_bytes += other.stage_rootfs_bytes;
        self.cache.merge(&other.cache);
        self.operations.extend(other.operations.iter().cloned());
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheMetrics {
    pub hit_count: u32,
    pub miss_count: u32,
    pub restore_bytes: u64,
    pub restore_phase_ms: CacheRestorePhaseMetrics,
    pub save_count: u32,
    pub save_bytes: u64,
}

impl CacheMetrics {
    pub fn merge(&mut self, other: &Self) {
        self.hit_count += other.hit_count;
        self.miss_count += other.miss_count;
        self.restore_bytes += other.restore_bytes;
        self.restore_phase_ms.merge(&other.restore_phase_ms);
        self.save_count += other.save_count;
        self.save_bytes += other.save_bytes;
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheRestorePhaseMetrics {
    pub clear_ms: u128,
    pub store_restore_ms: u128,
    pub hydrate_copy_ms: u128,
    pub fingerprint_ms: u128,
}

impl CacheRestorePhaseMetrics {
    pub fn merge(&mut self, other: &Self) {
        self.clear_ms += other.clear_ms;
        self.store_restore_ms += other.store_restore_ms;
        self.hydrate_copy_ms += other.hydrate_copy_ms;
        self.fingerprint_ms += other.fingerprint_ms;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationTiming {
    pub label: String,
    pub ms: u128,
    pub cached: bool,
}

pub struct RunSummary {
    pub container_name: Option<String>,
    pub export_path: Option<PathBuf>,
    /// Path to the preserved stage filesystem view (full rootfs or narrowed snapshot)
    /// when `keep_rootfs` was set.
    pub rootfs_dir: Option<PathBuf>,
    /// Prevents the temporary work directory from being deleted while
    /// downstream stages still reference the rootfs.  Dropping this handle
    /// unmounts the overlay and removes the temp directory.
    pub _keep_alive: Option<Arc<dyn Send + Sync>>,
    pub timings: RunTimings,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CachedOperationPrime {
    pub restore_ms: u128,
    pub complete: bool,
}

pub trait OperationCacheHooks {
    fn before_operation(
        &mut self,
        operation: &Operation,
        materialize_rootfs: Option<&Path>,
        snapshot_rootfs: Option<&Path>,
    ) -> Result<CachedOperationPrime>;
    fn after_operation(
        &mut self,
        operation: &Operation,
        materialize_rootfs: Option<&Path>,
        snapshot_rootfs: Option<&Path>,
    ) -> Result<u128>;
    fn on_cached_operation(&mut self, operation: &Operation) -> Result<u128>;
    fn prime_cached_operation(
        &mut self,
        operation: &Operation,
        materialize_rootfs: &Path,
        snapshot_rootfs: &Path,
    ) -> Result<CachedOperationPrime> {
        self.before_operation(operation, Some(materialize_rootfs), Some(snapshot_rootfs))
    }
    fn hydrate_export_caches(
        &mut self,
        pipeline: &Pipeline,
        materialize_rootfs: &Path,
        snapshot_rootfs: &Path,
    ) -> Result<CachedOperationPrime> {
        let _ = (pipeline, materialize_rootfs, snapshot_rootfs);
        Ok(CachedOperationPrime::default())
    }
}

pub trait ExecutionBackend {
    fn name(&self) -> &'static str;
    fn run(
        &self,
        pipeline: &Pipeline,
        options: &RunOptions,
        cache_config: &CacheStoreConfig,
        cache_hooks: Option<&mut dyn OperationCacheHooks>,
    ) -> Result<RunSummary>;
}

pub fn print_timing_summary(timings: &RunTimings) {
    println!();
    println!("{}", ui::section("timings"));
    for (label, ms) in [
        (ui::accent("resolve + pull"), timings.pull_ms),
        (ui::accent("unpack rootfs"), timings.unpack_ms),
        (ui::accent("prepare env"), timings.prepare_ms),
    ] {
        print_timing_ms_line(label, ms);
    }
    for op in &timings.operations {
        let cached = if op.cached {
            format!(" {}", ui::success("(cached)"))
        } else {
            String::new()
        };
        println!(
            "  {:<18} {:>6} ms",
            format!("{}{}", ui::bold(format!("step {}", op.label)), cached),
            op.ms
        );
    }
    if timings.cache_restore_ms > 0 {
        print_timing_ms_line(ui::accent("cache restore"), timings.cache_restore_ms);
        let phases = &timings.cache.restore_phase_ms;
        print_optional_timing_ms_lines([
            (ui::dim("  clear target"), phases.clear_ms),
            (ui::dim("  store restore"), phases.store_restore_ms),
            (ui::dim("  hydrate copy"), phases.hydrate_copy_ms),
            (ui::dim("  fingerprint"), phases.fingerprint_ms),
        ]);
    }
    if timings.cache_save_ms > 0 {
        print_timing_ms_line(ui::accent("cache save"), timings.cache_save_ms);
    }
    if timings.cache.hit_count > 0 || timings.cache.miss_count > 0 || timings.cache.save_count > 0 {
        println!(
            "  {:<18} hit={} miss={} save={}",
            ui::accent("cache ops"),
            timings.cache.hit_count,
            timings.cache.miss_count,
            timings.cache.save_count
        );
    }
    print_timing_ms_line(ui::accent("export"), timings.export_ms);
    print_timing_ms_line(ui::bold("total"), timings.total_ms);

    if timings.export_bytes > 0 || timings.stage_rootfs_bytes > 0 {
        println!();
        println!("{}", ui::section("storage"));
        if timings.export_bytes > 0 {
            println!(
                "  {:<18} {:>10}",
                ui::accent("export bytes"),
                timings.export_bytes
            );
        }
        if timings.stage_rootfs_bytes > 0 {
            println!(
                "  {:<18} {:>10}",
                ui::accent("stage rootfs"),
                timings.stage_rootfs_bytes
            );
        }
    }
}

fn print_timing_ms_line(label: String, value_ms: u128) {
    println!("  {:<18} {:>6} ms", label, value_ms);
}

fn print_optional_timing_ms_lines<const N: usize>(entries: [(String, u128); N]) {
    for (label, value_ms) in entries {
        if value_ms > 0 {
            print_timing_ms_line(label, value_ms);
        }
    }
}

/// Resolve the export format and output path from the pipeline config and CLI
/// overrides. Returns `None` when there is nothing to export (no pipeline
/// `export:` block and no CLI `-o` flag).
pub fn resolve_export(
    pipeline: &Pipeline,
    options: &RunOptions,
) -> Option<(ExportFormat, PathBuf)> {
    if let Some(export) = &pipeline.export {
        let format = options.export_format_override.unwrap_or(export.format);
        let path = options
            .output_path_override
            .clone()
            .unwrap_or_else(|| export.path.clone());
        Some((format, path))
    } else if let Some(path) = &options.output_path_override {
        // No export block in pipeline, but CLI passed -o (e.g. Dockerfile builds).
        // Default to OCI format unless overridden.
        let format = options.export_format_override.unwrap_or(ExportFormat::Oci);
        Some((format, path.clone()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{CacheMetrics, RunTimings};

    #[test]
    fn merge_combines_nested_metrics_without_duplicate_sources() {
        let mut left = RunTimings {
            cache: CacheMetrics {
                hit_count: 1,
                save_count: 1,
                ..CacheMetrics::default()
            },
            ..RunTimings::default()
        };
        let right = RunTimings {
            cache: CacheMetrics {
                miss_count: 2,
                save_count: 3,
                ..CacheMetrics::default()
            },
            ..RunTimings::default()
        };

        left.merge(&right);

        assert_eq!(left.cache.hit_count, 1);
        assert_eq!(left.cache.miss_count, 2);
        assert_eq!(left.cache.save_count, 4);
    }
}
