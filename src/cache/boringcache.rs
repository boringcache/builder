use std::fs;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tempfile::tempdir;

use crate::boringcache_cli::{BoringCacheCli, BoringCacheTagScope};
use crate::cache::backend::{CacheBackend, CacheManifest, manifest_ref_key};
use crate::cache::cache_tag;
use crate::cache::tree::{restore_tree_cache, save_tree_cache};
use crate::cache::{
    CacheLock, CacheRestore, CacheSave, CacheStore, acquire_cache_lock, cache_restore_keys,
    normalize_cache_tag,
};
use crate::schema::CacheMount;
use crate::util::fs::path_size;
use crate::util::process::display_command;

#[derive(Debug, Clone)]
pub(crate) struct BoringCacheStore {
    backend: BoringCacheBackend,
}

#[derive(Debug, Clone)]
pub(crate) struct BoringCacheBackend {
    cli: BoringCacheCli,
    ref_scope: BoringCacheTagScope,
    blob_scope: BoringCacheTagScope,
}

impl BoringCacheStore {
    pub(crate) fn open(workspace: Option<String>, binary: Option<PathBuf>) -> Result<Self> {
        Self::open_with_scope(
            workspace,
            binary,
            BoringCacheTagScope::platform_scoped_no_git(),
        )
    }

    pub(crate) fn open_with_scope(
        workspace: Option<String>,
        binary: Option<PathBuf>,
        scope: BoringCacheTagScope,
    ) -> Result<Self> {
        Ok(Self {
            backend: BoringCacheBackend::open(workspace, binary, scope)?,
        })
    }

    fn workspace(&self) -> &str {
        self.backend.workspace()
    }
}

impl BoringCacheBackend {
    pub(crate) fn open(
        workspace: Option<String>,
        binary: Option<PathBuf>,
        ref_scope: BoringCacheTagScope,
    ) -> Result<Self> {
        Ok(Self {
            cli: BoringCacheCli::resolve(workspace, binary)?,
            ref_scope,
            blob_scope: BoringCacheTagScope::portable_no_git(),
        })
    }

    pub(crate) fn workspace(&self) -> &str {
        self.cli.workspace()
    }

    pub(crate) fn binary(&self) -> &Path {
        self.cli.binary()
    }

    fn ref_tag(key: &str) -> String {
        cache_tag(key)
    }

    fn blob_tag(digest: &str) -> String {
        cache_tag(&format!("cache-blob-{digest}"))
    }

    fn check_tags(&self, tags: &[String], scope: BoringCacheTagScope) -> Result<Vec<CheckResult>> {
        let max_attempts = pending_check_retry_attempts();
        let mut attempt = 0u32;
        loop {
            let results = self.run_check_tags(tags, scope)?;
            if attempt >= max_attempts || !results.iter().any(CheckResult::is_retryable) {
                return Ok(results);
            }
            sleep(pending_check_retry_delay(attempt));
            attempt += 1;
        }
    }

    fn run_check_tags(
        &self,
        tags: &[String],
        scope: BoringCacheTagScope,
    ) -> Result<Vec<CheckResult>> {
        if tags.is_empty() {
            return Ok(Vec::new());
        }

        let mut args = vec![
            "check".to_string(),
            self.workspace().to_string(),
            tags.join(","),
            "--json".to_string(),
        ];
        scope.append_cli_args(&mut args);
        let output = self.cli.run_capture(&args).with_context(|| {
            format!(
                "failed to run boringcache check for tags {}",
                tags.join(",")
            )
        })?;

        if !output.status.success() {
            bail!(
                "boringcache check failed for {}: {}",
                tags.join(","),
                if output.stderr.trim().is_empty() {
                    output.stdout.trim()
                } else {
                    output.stderr.trim()
                }
            );
        }

        let summary: CheckSummary = serde_json::from_str(&output.stdout).with_context(|| {
            format!(
                "failed to parse boringcache check JSON (stdout: {}; stderr: {})",
                summarize_output(&output.stdout),
                summarize_output(&output.stderr)
            )
        })?;
        if summary.results.len() != tags.len() {
            bail!(
                "boringcache check returned {} results for {} tags",
                summary.results.len(),
                tags.len()
            );
        }
        Ok(summary.results)
    }

    fn check_tag(&self, tag: &str, scope: BoringCacheTagScope) -> Result<CheckResult> {
        self.check_tags(&[tag.to_string()], scope)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("boringcache check returned no results for tag {tag}"))
    }

    pub(crate) fn batch_has_ref_tags(&self, tags: &[String]) -> Result<Vec<bool>> {
        let checks = self.check_tags(tags, self.ref_scope)?;
        Ok(checks
            .into_iter()
            .map(|result| result.status == "hit")
            .collect())
    }

    fn save_directory(&self, tag: &str, source: &Path, scope: BoringCacheTagScope) -> Result<()> {
        let path_tag = format!("{tag}:{}", source.display());
        let max_attempts = save_retry_attempts();
        let mut attempt = 0u32;
        loop {
            let mut args = vec![
                "save".to_string(),
                self.workspace().to_string(),
                path_tag.clone(),
                "--fail-on-cache-error".to_string(),
            ];
            scope.append_cli_args(&mut args);
            let output = self
                .cli
                .run_capture(&args)
                .with_context(|| format!("failed to save BoringCache tag {tag}"))?;
            if output.status.success() {
                return Ok(());
            }

            if attempt < max_attempts && save_output_is_retryable(&output.stdout, &output.stderr) {
                sleep(save_retry_delay(attempt));
                attempt += 1;
                continue;
            }

            bail!(
                "{} failed with status {}{}{}",
                display_command(self.binary(), &args),
                output
                    .status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".to_string()),
                if output.stdout.trim().is_empty() {
                    ""
                } else {
                    "\nstdout:\n"
                },
                if output.stdout.trim().is_empty() {
                    output.stderr.trim().to_string()
                } else if output.stderr.trim().is_empty() {
                    output.stdout.trim().to_string()
                } else {
                    format!(
                        "{}\nstderr:\n{}",
                        output.stdout.trim(),
                        output.stderr.trim()
                    )
                }
            );
        }
    }

    fn restore_directory(
        &self,
        tag: &str,
        destination: &Path,
        scope: BoringCacheTagScope,
    ) -> Result<()> {
        let max_attempts = restore_retry_attempts();
        let mut attempt = 0u32;
        loop {
            fs::create_dir_all(destination)
                .with_context(|| format!("failed to create {}", destination.display()))?;
            let tag_path = format!("{tag}:{}", destination.display());
            let mut args = vec![
                "restore".to_string(),
                self.workspace().to_string(),
                tag_path,
                "--fail-on-cache-error".to_string(),
            ];
            scope.append_cli_args(&mut args);
            let output = self.cli.run_capture(&args).with_context(|| {
                format!(
                    "failed to run boringcache restore for tag {} into {}",
                    tag,
                    destination.display()
                )
            })?;
            if output.status.success() {
                return Ok(());
            }

            if attempt < max_attempts && restore_output_is_retryable(&output.stdout, &output.stderr)
            {
                sleep(restore_retry_delay(attempt));
                attempt += 1;
                continue;
            }

            bail!(
                "{} failed with status {}{}{}",
                display_command(self.binary(), &args),
                output
                    .status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".to_string()),
                if output.stdout.trim().is_empty() {
                    ""
                } else {
                    "\nstdout:\n"
                },
                if output.stdout.trim().is_empty() {
                    output.stderr.trim().to_string()
                } else if output.stderr.trim().is_empty() {
                    output.stdout.trim().to_string()
                } else {
                    format!(
                        "{}\nstderr:\n{}",
                        output.stdout.trim(),
                        output.stderr.trim()
                    )
                }
            );
        }
    }

    pub(crate) fn resolve_ref_tag(
        &self,
        tag: &str,
        expected_key: Option<&str>,
    ) -> Result<Option<CacheManifest>> {
        let check = self.check_tag(tag, self.ref_scope)?;
        if check.status != "hit" {
            return Ok(None);
        }

        let temp = tempdir().context("failed to create temporary BoringCache ref restore dir")?;
        self.restore_directory(tag, temp.path(), self.ref_scope)?;
        let manifest_path = temp.path().join("manifest.json");
        if !manifest_path.exists() {
            return Ok(None);
        }
        let manifest: CacheManifest = serde_json::from_slice(
            &fs::read(&manifest_path)
                .with_context(|| format!("failed to read {}", manifest_path.display()))?,
        )
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
        if let Some(expected_key) = expected_key
            && let Some(stored_key) = manifest_ref_key(&manifest)
            && stored_key != expected_key
        {
            return Ok(None);
        }
        Ok(Some(manifest))
    }

    pub(crate) fn publish_ref_tag(
        &self,
        tag: &str,
        logical_key: &str,
        manifest: &CacheManifest,
    ) -> Result<()> {
        let temp = tempdir().context("failed to create temporary BoringCache ref publish dir")?;
        let manifest_path = temp.path().join("manifest.json");
        let mut manifest = manifest.clone();
        manifest
            .metadata
            .insert("ref_key".to_string(), logical_key.to_string());
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).context("failed to serialize cache manifest")?,
        )
        .with_context(|| format!("failed to write {}", manifest_path.display()))?;
        self.save_directory(tag, temp.path(), self.ref_scope)?;
        self.wait_for_saved_ref_visibility(tag)
    }

    fn wait_for_saved_ref_visibility(&self, tag: &str) -> Result<()> {
        let max_attempts = saved_ref_visibility_retry_attempts();
        let mut attempt = 0u32;
        loop {
            let check = self.check_tag(tag, self.ref_scope)?;
            if check.status == "hit" {
                return Ok(());
            }
            if check.status != "miss" || attempt >= max_attempts {
                return Ok(());
            }
            sleep(saved_ref_visibility_retry_delay(attempt));
            attempt += 1;
        }
    }
}

impl CacheBackend for BoringCacheBackend {
    fn kind(&self) -> &'static str {
        "boringcache"
    }

    fn detail(&self) -> String {
        self.cli.detail()
    }

    fn resolve_ref(&self, key: &str) -> Result<Option<CacheManifest>> {
        self.resolve_ref_tag(&Self::ref_tag(key), Some(key))
    }

    fn publish_ref(&self, key: &str, manifest: &CacheManifest) -> Result<()> {
        self.publish_ref_tag(&Self::ref_tag(key), key, manifest)
    }

    fn batch_has_refs(&self, keys: &[String]) -> Result<Option<Vec<bool>>> {
        let tags = keys
            .iter()
            .map(|key| Self::ref_tag(key))
            .collect::<Vec<_>>();
        Ok(Some(self.batch_has_ref_tags(&tags)?))
    }

    fn has_blob(&self, digest: &str) -> Result<bool> {
        let tag = Self::blob_tag(digest);
        let check = self.check_tag(&tag, self.blob_scope)?;
        Ok(check.status == "hit")
    }

    fn supports_native_tree_blobs(&self) -> bool {
        true
    }

    fn supports_native_build_cache_layout_blobs(&self) -> bool {
        true
    }

    fn fetch_blob(&self, digest: &str, destination: &Path) -> Result<()> {
        let tag = Self::blob_tag(digest);
        let temp = tempdir().context("failed to create temporary BoringCache blob restore dir")?;
        self.restore_directory(&tag, temp.path(), self.blob_scope)?;
        let payload_path = temp.path().join("payload.bin");
        fs::copy(&payload_path, destination).with_context(|| {
            format!(
                "failed to copy restored BoringCache blob {} into {}",
                payload_path.display(),
                destination.display()
            )
        })?;
        Ok(())
    }

    fn fetch_tree_blob(&self, digest: &str, destination: &Path) -> Result<()> {
        let tag = Self::blob_tag(digest);
        self.restore_directory(&tag, destination, self.blob_scope)
            .with_context(|| format!("failed to restore native tree cache blob {digest}"))
    }

    fn fetch_build_cache_layout_blob(&self, digest: &str, destination: &Path) -> Result<()> {
        let tag = Self::blob_tag(digest);
        self.restore_directory(&tag, destination, self.blob_scope)
            .with_context(|| format!("failed to restore OCI build-cache layout blob {digest}"))
    }

    fn store_blob(&self, digest: &str, source: &Path) -> Result<()> {
        if self.has_blob(digest)? {
            return Ok(());
        }

        let tag = Self::blob_tag(digest);
        let temp = tempdir().context("failed to create temporary BoringCache blob publish dir")?;
        let payload_path = temp.path().join("payload.bin");
        fs::copy(source, &payload_path).with_context(|| {
            format!(
                "failed to stage {} into {}",
                source.display(),
                payload_path.display()
            )
        })?;
        self.save_directory(&tag, temp.path(), self.blob_scope)
    }

    fn store_tree_blob(&self, digest: &str, source: &Path) -> Result<()> {
        if self.has_blob(digest)? {
            return Ok(());
        }
        self.save_directory(&Self::blob_tag(digest), source, self.blob_scope)
            .with_context(|| format!("failed to save native tree cache blob {digest}"))
    }

    fn materialize_native_tree_blob(
        &self,
        content_hash: &str,
        source: &Path,
    ) -> Result<Option<crate::cache::backend::CacheManifestBlob>> {
        let digest = format!("sha256:{content_hash}");
        if !self.has_blob(&digest)? {
            self.store_tree_blob(&digest, source)?;
        }
        Ok(Some(crate::cache::backend::CacheManifestBlob {
            digest,
            bytes: path_size(source)?,
        }))
    }

    fn store_build_cache_layout_blob(&self, digest: &str, source: &Path) -> Result<()> {
        if self.has_blob(digest)? {
            return Ok(());
        }
        if !source.join("index.json").is_file()
            || !source.join("oci-layout").is_file()
            || !source.join("blobs").join("sha256").is_dir()
        {
            bail!(
                "build-cache layout {} is not an OCI layout directory",
                source.display()
            );
        }

        let tag = Self::blob_tag(digest);
        self.save_directory(&tag, source, self.blob_scope)
            .with_context(|| format!("failed to save OCI build-cache layout blob {digest}"))
    }
}

impl CacheStore for BoringCacheStore {
    fn kind(&self) -> &'static str {
        self.backend.kind()
    }

    fn detail(&self) -> String {
        self.backend.detail()
    }

    fn restore(&self, entry: &CacheMount, destination: &Path) -> Result<CacheRestore> {
        for key in cache_restore_keys(entry) {
            let restored = restore_tree_cache(&self.backend, key, destination, Some(&entry.path))?;
            if restored.hit {
                return Ok(restored);
            }
        }
        Ok(CacheRestore {
            hit: false,
            digest: None,
            bytes: 0,
            content_hash: None,
        })
    }

    fn save(&self, entry: &CacheMount, source: &Path) -> Result<CacheSave> {
        save_tree_cache(&self.backend, &entry.key, source, Some(&entry.path))
    }

    fn backend(&self) -> &dyn crate::cache::backend::CacheBackend {
        &self.backend
    }

    fn lock(&self, entry: &CacheMount) -> Result<CacheLock> {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| anyhow!("HOME is not set; cannot lock cache"))?;
        let lock_path = PathBuf::from(home)
            .join(".boringbuilder")
            .join("locks")
            .join("boringcache")
            .join(normalize_cache_tag(self.workspace()))
            .join(format!("{}.lock", normalize_cache_tag(&entry.key)));
        acquire_cache_lock(&lock_path)
    }
}

#[derive(Debug, Deserialize)]
struct CheckSummary {
    results: Vec<CheckResult>,
}

#[derive(Debug, Deserialize)]
struct CheckResult {
    status: String,
}

impl CheckResult {
    fn is_retryable(&self) -> bool {
        matches!(self.status.as_str(), "pending" | "uploading")
    }
}

fn pending_check_retry_attempts() -> u32 {
    if cfg!(test) { 1 } else { 5 }
}

fn pending_check_retry_delay(attempt: u32) -> Duration {
    if cfg!(test) {
        Duration::from_millis(10 * 2u64.pow(attempt.min(3)))
    } else {
        Duration::from_millis(500 * 2u64.pow(attempt.min(4)))
    }
}

fn restore_retry_attempts() -> u32 {
    pending_check_retry_attempts()
}

fn restore_retry_delay(attempt: u32) -> Duration {
    pending_check_retry_delay(attempt)
}

fn save_retry_attempts() -> u32 {
    pending_check_retry_attempts()
}

fn save_retry_delay(attempt: u32) -> Duration {
    pending_check_retry_delay(attempt)
}

fn saved_ref_visibility_retry_attempts() -> u32 {
    if cfg!(test) { 2 } else { 8 }
}

fn saved_ref_visibility_retry_delay(attempt: u32) -> Duration {
    pending_check_retry_delay(attempt)
}

fn restore_output_is_retryable(stdout: &str, stderr: &str) -> bool {
    let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    combined.contains("manifest request failed")
        || combined.contains("404 not found")
        || combined.contains("pending")
        || combined.contains("uploading")
        || combined.contains("awaiting_blob_visibility")
}

fn save_output_is_retryable(stdout: &str, stderr: &str) -> bool {
    let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    combined.contains("cache upload in progress")
        || combined.contains("upload in progress")
        || combined.contains("pending")
        || combined.contains("uploading")
        || combined.contains("awaiting_blob_visibility")
}

fn summarize_output(output: &str) -> String {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        "<empty>".to_string()
    } else {
        let compact = trimmed.replace('\n', "\\n");
        if compact.len() > 240 {
            format!("{}...", &compact[..240])
        } else {
            compact
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use anyhow::Context;
    use tar::Archive;
    use tempfile::tempdir;
    use zstd::Decoder;

    use crate::boringcache_cli::BoringCacheTagScope;
    use crate::cache::CacheStore;
    use crate::cache::backend::{CacheBackend, tree_cache_manifest};
    use crate::cache::slice::STEP_SLICE_STATE_KEY_METADATA_KEY;
    use crate::export::oci::{
        BuildCacheStepRecord, append_build_cache_layer_to_layout, create_oci_layer_from_fs_delta,
        load_build_cache_layout, load_build_cache_snapshot_state, snapshot_tree_with_state,
        store_build_cache_snapshot_state,
    };
    use crate::schema::CacheMount;

    use super::{BoringCacheBackend, BoringCacheStore};

    fn write_fake_boringcache_binary(dir: &Path, script: impl AsRef<[u8]>) -> PathBuf {
        let binary = dir.join("boringcache");
        let mut staged = tempfile::NamedTempFile::new_in(dir).unwrap();
        staged.write_all(script.as_ref()).unwrap();
        staged.as_file().sync_all().unwrap();
        let mut perms = staged.as_file().metadata().unwrap().permissions();
        perms.set_mode(0o755);
        staged.as_file().set_permissions(perms).unwrap();
        let staged = staged.into_temp_path();
        staged.persist(&binary).unwrap();
        binary
    }

    #[test]
    fn ref_tag_uses_logical_key_directly() {
        assert_eq!(
            BoringCacheBackend::ref_tag("bootsnap-ec05991b4103ae45"),
            "bootsnap-ec05991b4103ae45"
        );
    }

    #[test]
    fn open_with_scope_passes_explicit_tag_scope_flags() {
        let temp = tempdir().unwrap();
        let args_log = temp.path().join("args.log");
        let binary = write_fake_boringcache_binary(
            temp.path(),
            format!(
                r#"#!/bin/sh
set -eu
printf '%s\n' "$@" >> "{}"
cmd="$1"
shift
case "$cmd" in
  check)
    echo '{{"results":[{{"status":"miss"}}]}}'
    ;;
  *)
    echo "unexpected command: $cmd" >&2
    exit 1
    ;;
esac
"#,
                args_log.display()
            ),
        );

        let store = BoringCacheStore::open_with_scope(
            Some("boringcache/demo".to_string()),
            Some(binary),
            BoringCacheTagScope::portable_no_git(),
        )
        .unwrap();
        let destination = temp.path().join("restore");
        let entry = CacheMount {
            id: "toolchain".to_string(),
            path: "/usr/local/bin/boringbuilder".to_string(),
            key: "toolchain-linux-amd64-sha123".to_string(),
            restore_from: Vec::new(),
            mode: crate::schema::CacheMode::Shared,
        };

        let restore = store.restore(&entry, &destination).unwrap();
        assert!(!restore.hit);

        let logged = fs::read_to_string(&args_log).unwrap();
        assert!(logged.contains("--no-platform"));
        assert!(logged.contains("--no-git"));
    }

    #[test]
    fn resolve_ref_retries_pending_check_results() {
        let temp = tempdir().unwrap();
        let state = temp.path().join("check-count");
        let binary = write_fake_boringcache_binary(
            temp.path(),
            format!(
                r#"#!/bin/sh
set -eu
STATE="{}"
cmd="$1"
shift
workspace="$1"
shift
case "$cmd" in
  check)
    count=0
    if [ -f "$STATE" ]; then
      count="$(cat "$STATE")"
    fi
    count=$((count + 1))
    printf '%s' "$count" > "$STATE"
    if [ "$count" -eq 1 ]; then
      echo '{{"results":[{{"status":"pending"}}]}}'
    else
      echo '{{"results":[{{"status":"hit"}}]}}'
    fi
    ;;
  restore)
    pair="$1"
    path="${{pair#*:}}"
    mkdir -p "$path"
    printf '{{"version":1,"kind":"tree.v1","blobs":[{{"digest":"sha256:test","bytes":1}}],"metadata":{{}}}}' > "$path/manifest.json"
    ;;
  *)
    echo "unexpected command: $cmd" >&2
    exit 1
    ;;
esac
"#,
                state.display()
            ),
        );

        let backend = BoringCacheBackend::open(
            Some("boringcache/demo".to_string()),
            Some(binary),
            BoringCacheTagScope::portable_no_git(),
        )
        .unwrap();

        let manifest = backend.resolve_ref("runtime").unwrap().unwrap();
        assert_eq!(manifest.kind, "tree.v1");
        assert_eq!(fs::read_to_string(&state).unwrap(), "2");
    }

    #[test]
    fn has_blob_retries_uploading_check_results() {
        let temp = tempdir().unwrap();
        let state = temp.path().join("check-count");
        let binary = write_fake_boringcache_binary(
            temp.path(),
            format!(
                r#"#!/bin/sh
set -eu
STATE="{}"
cmd="$1"
shift
workspace="$1"
shift
case "$cmd" in
  check)
    count=0
    if [ -f "$STATE" ]; then
      count="$(cat "$STATE")"
    fi
    count=$((count + 1))
    printf '%s' "$count" > "$STATE"
    if [ "$count" -eq 1 ]; then
      echo '{{"results":[{{"status":"uploading"}}]}}'
    else
      echo '{{"results":[{{"status":"hit"}}]}}'
    fi
    ;;
  *)
    echo "unexpected command: $cmd" >&2
    exit 1
    ;;
esac
"#,
                state.display()
            ),
        );

        let backend = BoringCacheBackend::open(
            Some("boringcache/demo".to_string()),
            Some(binary),
            BoringCacheTagScope::portable_no_git(),
        )
        .unwrap();

        assert!(backend.has_blob("sha256:test").unwrap());
        assert_eq!(fs::read_to_string(&state).unwrap(), "2");
    }

    #[test]
    fn resolve_ref_retries_manifest_visibility_restore_failures() {
        let temp = tempdir().unwrap();
        let state = temp.path().join("restore-count");
        let binary = write_fake_boringcache_binary(
            temp.path(),
            format!(
                r#"#!/bin/sh
set -eu
STATE="{}"
cmd="$1"
shift
workspace="$1"
shift
case "$cmd" in
  check)
    echo '{{"results":[{{"status":"hit"}}]}}'
    ;;
  restore)
    pair="$1"
    path="${{pair#*:}}"
    count=0
    if [ -f "$STATE" ]; then
      count="$(cat "$STATE")"
    fi
    count=$((count + 1))
    printf '%s' "$count" > "$STATE"
    if [ "$count" -eq 1 ]; then
      echo 'warning: Restore failed: Manifest request failed: HTTP status client error (404 Not Found)' >&2
      exit 1
    fi
    mkdir -p "$path"
    printf '{{"version":1,"kind":"tree.v1","blobs":[{{"digest":"sha256:test","bytes":1}}],"metadata":{{}}}}' > "$path/manifest.json"
    ;;
  *)
    echo "unexpected command: $cmd" >&2
    exit 1
    ;;
esac
"#,
                state.display()
            ),
        );

        let backend = BoringCacheBackend::open(
            Some("boringcache/demo".to_string()),
            Some(binary),
            BoringCacheTagScope::portable_no_git(),
        )
        .unwrap();

        let manifest = backend.resolve_ref("runtime").unwrap().unwrap();
        assert_eq!(manifest.kind, "tree.v1");
        assert_eq!(fs::read_to_string(&state).unwrap(), "2");
    }

    #[test]
    fn native_build_cache_blob_save_uses_oci_layout_directory_directly() {
        let temp = tempdir().unwrap();
        let args_log = temp.path().join("args.log");
        let binary = write_fake_boringcache_binary(
            temp.path(),
            format!(
                r#"#!/bin/sh
set -eu
printf '%s\n' "$@" >> "{}"
cmd="$1"
shift
case "$cmd" in
  check)
    echo '{{"results":[{{"status":"miss"}}]}}'
    ;;
  save)
    exit 0
    ;;
  *)
    echo "unexpected command: $cmd" >&2
    exit 1
    ;;
esac
"#,
                args_log.display()
            ),
        );

        let layout = temp.path().join("oci-layout");
        fs::create_dir_all(layout.join("blobs").join("sha256")).unwrap();
        fs::write(layout.join("index.json"), "{}").unwrap();
        fs::write(
            layout.join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();

        let backend = BoringCacheBackend::open(
            Some("boringcache/demo".to_string()),
            Some(binary),
            BoringCacheTagScope::portable_no_git(),
        )
        .unwrap();
        backend
            .store_build_cache_layout_blob("sha256:test-layout", &layout)
            .unwrap();

        let logged = fs::read_to_string(&args_log).unwrap();
        assert!(
            logged.contains(&format!(
                "save\nboringcache/demo\ncache-blob-sha256-test-layout:{}",
                layout.display()
            )),
            "expected build-cache blob save to point at the OCI layout dir directly; log was: {logged}"
        );
        assert!(!logged.contains("payload.bin"));
    }

    #[test]
    fn native_build_cache_blob_restore_targets_layout_directory_directly() {
        let temp = tempdir().unwrap();
        let args_log = temp.path().join("args.log");
        let binary = write_fake_boringcache_binary(
            temp.path(),
            format!(
                r#"#!/bin/sh
set -eu
printf '%s\n' "$@" >> "{}"
cmd="$1"
shift
case "$cmd" in
  restore)
    exit 0
    ;;
  *)
    echo "unexpected command: $cmd" >&2
    exit 1
    ;;
esac
"#,
                args_log.display()
            ),
        );

        let destination = temp.path().join("restore-layout");
        let backend = BoringCacheBackend::open(
            Some("boringcache/demo".to_string()),
            Some(binary),
            BoringCacheTagScope::portable_no_git(),
        )
        .unwrap();
        backend
            .fetch_build_cache_layout_blob("sha256:test-layout", &destination)
            .unwrap();

        let logged = fs::read_to_string(&args_log).unwrap();
        assert!(
            logged.contains(&format!(
                "restore\nboringcache/demo\ncache-blob-sha256-test-layout:{}",
                destination.display()
            )),
            "expected build-cache blob restore to target the OCI layout dir directly; log was: {logged}"
        );
    }

    #[test]
    fn native_build_cache_blob_round_trip_preserves_snapshot_state_metadata() {
        let temp = tempdir().unwrap();
        let backend_store = temp.path().join("backend-store");
        fs::create_dir_all(&backend_store).unwrap();
        let binary = write_fake_boringcache_binary(
            temp.path(),
            format!(
                r#"#!/bin/sh
set -eu
STORE_DIR="{}"
cmd="$1"
shift
workspace="$1"
shift
case "$cmd" in
  check)
    tag="$1"
    if [ -d "$STORE_DIR/$tag" ]; then
      echo '{{"results":[{{"status":"hit","size":5,"compressed_size":3}}]}}'
    else
      echo '{{"results":[{{"status":"miss"}}]}}'
    fi
    ;;
  save)
    pair="$1"
    tag="${{pair%%:*}}"
    path="${{pair#*:}}"
    rm -rf "$STORE_DIR/$tag"
    mkdir -p "$STORE_DIR/$tag"
    cp -R "$path"/. "$STORE_DIR/$tag"/
    ;;
  restore)
    pair="$1"
    tag="${{pair%%:*}}"
    path="${{pair#*:}}"
    mkdir -p "$path"
    cp -R "$STORE_DIR/$tag"/. "$path"/
    ;;
  *)
    echo "unexpected command: $cmd" >&2
    exit 1
    ;;
esac
"#,
                backend_store.display()
            ),
        );

        let source = temp.path().join("source");
        let snapshot = temp.path().join("snapshot");
        let layout = temp.path().join("layout");
        let restored = temp.path().join("restored-layout");
        fs::create_dir_all(source.join("bin")).unwrap();
        fs::write(source.join("bin/app"), "hello").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("app", source.join("bin/app-link")).unwrap();
        snapshot_tree_with_state(&source, &snapshot).unwrap();
        let layer_count = append_build_cache_layer_to_layout(
            &layout,
            "linux/amd64",
            BuildCacheStepRecord {
                operation_index: 0,
                key: "step-1".to_string(),
            },
            create_oci_layer_from_fs_delta(None, &snapshot).unwrap(),
        )
        .unwrap();
        store_build_cache_snapshot_state(&layout, layer_count, &snapshot).unwrap();

        let backend = BoringCacheBackend::open(
            Some("boringcache/demo".to_string()),
            Some(binary),
            BoringCacheTagScope::portable_no_git(),
        )
        .unwrap();
        backend
            .store_build_cache_layout_blob("sha256:test-layout", &layout)
            .unwrap();
        backend
            .fetch_build_cache_layout_blob("sha256:test-layout", &restored)
            .unwrap();

        let restored_layout = load_build_cache_layout(&restored, "linux/amd64")
            .unwrap()
            .unwrap();
        assert!(restored_layout.snapshot_states.contains_key(&1));
        let restored_state = load_build_cache_snapshot_state(&restored, 1)
            .unwrap()
            .expect("missing restored snapshot state");
        assert!(
            restored_state.contains_key(Path::new("bin/app")),
            "restored OCI config metadata should retain file snapshot state"
        );
        assert!(
            restored_state.contains_key(Path::new("bin/app-link")),
            "restored OCI config metadata should retain symlink snapshot state"
        );
        assert!(
            !restored
                .join(".boringbuilder-build-cache-snapshots")
                .exists(),
            "round-tripped layout should not depend on legacy top-level snapshot sidecars"
        );
    }

    #[test]
    fn native_tree_blob_save_uses_directory_directly() {
        let temp = tempdir().unwrap();
        let args_log = temp.path().join("args.log");
        let binary = write_fake_boringcache_binary(
            temp.path(),
            format!(
                r#"#!/bin/sh
set -eu
printf '%s\n' "$@" >> "{}"
cmd="$1"
shift
case "$cmd" in
  check)
    echo '{{"results":[{{"status":"miss"}}]}}'
    ;;
  save)
    exit 0
    ;;
  *)
    echo "unexpected command: $cmd" >&2
    exit 1
    ;;
esac
"#,
                args_log.display()
            ),
        );

        let tree = temp.path().join("tree");
        fs::create_dir_all(tree.join("nested")).unwrap();
        fs::write(tree.join("nested").join("file.txt"), "hello").unwrap();

        let backend = BoringCacheBackend::open(
            Some("boringcache/demo".to_string()),
            Some(binary),
            BoringCacheTagScope::portable_no_git(),
        )
        .unwrap();
        backend.store_tree_blob("sha256:test-tree", &tree).unwrap();

        let logged = fs::read_to_string(&args_log).unwrap();
        assert!(
            logged.contains(&format!(
                "save\nboringcache/demo\ncache-blob-sha256-test-tree:{}",
                tree.display()
            )),
            "expected tree blob save to point at the directory directly; log was: {logged}"
        );
        assert!(!logged.contains("payload.bin"));
    }

    #[test]
    fn native_tree_blob_restore_targets_directory_directly() {
        let temp = tempdir().unwrap();
        let args_log = temp.path().join("args.log");
        let binary = write_fake_boringcache_binary(
            temp.path(),
            format!(
                r#"#!/bin/sh
set -eu
printf '%s\n' "$@" >> "{}"
cmd="$1"
shift
case "$cmd" in
  restore)
    exit 0
    ;;
  *)
    echo "unexpected command: $cmd" >&2
    exit 1
    ;;
esac
"#,
                args_log.display()
            ),
        );

        let destination = temp.path().join("restore-tree");
        let backend = BoringCacheBackend::open(
            Some("boringcache/demo".to_string()),
            Some(binary),
            BoringCacheTagScope::portable_no_git(),
        )
        .unwrap();
        backend
            .fetch_tree_blob("sha256:test-tree", &destination)
            .unwrap();

        let logged = fs::read_to_string(&args_log).unwrap();
        assert!(
            logged.contains(&format!(
                "restore\nboringcache/demo\ncache-blob-sha256-test-tree:{}",
                destination.display()
            )),
            "expected tree blob restore to target the directory directly; log was: {logged}"
        );
    }

    #[test]
    fn round_trips_via_fake_boringcache_binary() {
        let temp = tempdir().unwrap();
        let backend_store = temp.path().join("backend-store");
        fs::create_dir_all(&backend_store).unwrap();
        let binary = write_fake_boringcache_binary(
            temp.path(),
            format!(
                r#"#!/bin/sh
set -eu
STORE_DIR="{}"
cmd="$1"
shift
workspace="$1"
shift
case "$cmd" in
  check)
    tag="$1"
    if [ -d "$STORE_DIR/$tag" ]; then
      echo '{{"results":[{{"status":"hit","size":5,"compressed_size":3}}]}}'
    else
      echo '{{"results":[{{"status":"miss"}}]}}'
    fi
    ;;
  save)
    pair="$1"
    tag="${{pair%%:*}}"
    path="${{pair#*:}}"
    rm -rf "$STORE_DIR/$tag"
    mkdir -p "$STORE_DIR/$tag"
    cp -R "$path"/. "$STORE_DIR/$tag"/
    ;;
  restore)
    pair="$1"
    tag="${{pair%%:*}}"
    path="${{pair#*:}}"
    mkdir -p "$path"
    cp -R "$STORE_DIR/$tag"/. "$path"/
    ;;
  *)
    echo "unexpected command: $cmd" >&2
    exit 1
    ;;
esac
"#,
                backend_store.display()
            ),
        );

        let store =
            BoringCacheStore::open(Some("org/workspace".to_string()), Some(binary)).unwrap();
        let entry = CacheMount {
            id: "bundle".to_string(),
            path: "/workspace/vendor/bundle".to_string(),
            key: "bundle-key".to_string(),
            restore_from: Vec::new(),
            mode: crate::schema::CacheMode::Shared,
        };
        let source = temp.path().join("source");
        let restored = temp.path().join("restored");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("Gemfile.lock"), "deps").unwrap();

        let saved = store.save(&entry, &source).unwrap();
        assert!(saved.bytes > 0);

        let mut stored_tags = fs::read_dir(&backend_store)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        stored_tags.sort();
        assert_eq!(stored_tags.len(), 2);
        assert!(stored_tags.iter().any(|tag| tag == "bundle-key"));
        assert!(stored_tags.iter().any(|tag| tag.starts_with("cache-blob-")));

        let hit = store.restore(&entry, &restored).unwrap();
        assert!(hit.hit);
        assert_eq!(
            fs::read_to_string(restored.join("Gemfile.lock")).unwrap(),
            "deps"
        );
    }

    #[test]
    fn publish_ref_preserves_manifest_metadata() {
        let temp = tempdir().unwrap();
        let backend_store = temp.path().join("backend-store");
        fs::create_dir_all(&backend_store).unwrap();
        let binary = write_fake_boringcache_binary(
            temp.path(),
            format!(
                r#"#!/bin/sh
set -eu
STORE_DIR="{}"
cmd="$1"
shift
workspace="$1"
shift
case "$cmd" in
  check)
    tag="$1"
    if [ -d "$STORE_DIR/$tag" ]; then
      echo '{{"results":[{{"status":"hit","size":5,"compressed_size":3}}]}}'
    else
      echo '{{"results":[{{"status":"miss"}}]}}'
    fi
    ;;
  save)
    pair="$1"
    tag="${{pair%%:*}}"
    path="${{pair#*:}}"
    rm -rf "$STORE_DIR/$tag"
    mkdir -p "$STORE_DIR/$tag"
    cp -R "$path"/. "$STORE_DIR/$tag"/
    ;;
  restore)
    pair="$1"
    tag="${{pair%%:*}}"
    path="${{pair#*:}}"
    mkdir -p "$path"
    cp -R "$STORE_DIR/$tag"/. "$path"/
    ;;
  *)
    echo "unexpected command: $cmd" >&2
    exit 1
    ;;
esac
"#,
                backend_store.display()
            ),
        );

        let backend = BoringCacheBackend::open(
            Some("org/workspace".to_string()),
            Some(binary),
            BoringCacheTagScope::portable_no_git(),
        )
        .unwrap();

        let mut manifest = tree_cache_manifest("sha256:test".to_string(), 123, None);
        manifest.metadata.insert(
            STEP_SLICE_STATE_KEY_METADATA_KEY.to_string(),
            "state-key-123".to_string(),
        );

        backend.publish_ref("step-slice-demo", &manifest).unwrap();
        let resolved = backend.resolve_ref("step-slice-demo").unwrap().unwrap();
        assert_eq!(
            resolved
                .metadata
                .get(STEP_SLICE_STATE_KEY_METADATA_KEY)
                .map(String::as_str),
            Some("state-key-123")
        );
    }

    #[test]
    fn publish_ref_retries_initial_post_save_miss_until_visible() {
        let temp = tempdir().unwrap();
        let backend_store = temp.path().join("backend-store");
        let check_count = temp.path().join("check-count");
        fs::create_dir_all(&backend_store).unwrap();
        let binary = write_fake_boringcache_binary(
            temp.path(),
            format!(
                r#"#!/bin/sh
set -eu
STORE_DIR="{}"
CHECK_COUNT="{}"
cmd="$1"
shift
workspace="$1"
shift
case "$cmd" in
  check)
    count=0
    if [ -f "$CHECK_COUNT" ]; then
      count="$(cat "$CHECK_COUNT")"
    fi
    count=$((count + 1))
    printf '%s' "$count" > "$CHECK_COUNT"
    tag="$1"
    if [ -d "$STORE_DIR/$tag" ] && [ "$count" -ge 2 ]; then
      echo '{{"results":[{{"status":"hit","size":5,"compressed_size":3}}]}}'
    else
      echo '{{"results":[{{"status":"miss"}}]}}'
    fi
    ;;
  save)
    pair="$1"
    tag="${{pair%%:*}}"
    path="${{pair#*:}}"
    rm -rf "$STORE_DIR/$tag"
    mkdir -p "$STORE_DIR/$tag"
    cp -R "$path"/. "$STORE_DIR/$tag"/
    ;;
  *)
    echo "unexpected command: $cmd" >&2
    exit 1
    ;;
esac
"#,
                backend_store.display(),
                check_count.display()
            ),
        );

        let backend = BoringCacheBackend::open(
            Some("org/workspace".to_string()),
            Some(binary),
            BoringCacheTagScope::portable_no_git(),
        )
        .unwrap();

        let manifest = tree_cache_manifest("sha256:test".to_string(), 123, None);
        backend.publish_ref("step-slice-demo", &manifest).unwrap();
        assert_eq!(fs::read_to_string(check_count).unwrap(), "2");
    }

    #[test]
    fn returns_miss_without_restore() {
        let temp = tempdir().unwrap();
        let binary = write_fake_boringcache_binary(
            temp.path(),
            r#"#!/bin/sh
set -eu
cmd="$1"
shift
workspace="$1"
shift
case "$cmd" in
  check)
    echo '{"results":[{"status":"miss"}]}'
    ;;
  restore)
    echo "restore should not be called on miss" >&2
    exit 1
    ;;
  *)
    exit 0
    ;;
esac
"#,
        );

        let store =
            BoringCacheStore::open(Some("org/workspace".to_string()), Some(binary)).unwrap();
        let entry = CacheMount {
            id: "cache".to_string(),
            path: "/workspace/cache".to_string(),
            key: "cache-key".to_string(),
            restore_from: Vec::new(),
            mode: crate::schema::CacheMode::Shared,
        };
        let restored = temp.path().join("restored");
        let result = store.restore(&entry, &restored).unwrap();
        assert!(!result.hit);
        assert_eq!(result.bytes, 0);
    }

    #[test]
    fn misses_legacy_directory_tags_without_manifests() {
        let temp = tempdir().unwrap();
        let backend_store = temp.path().join("backend-store");
        let legacy_tag = backend_store.join("apt-archives-bookworm-build");
        fs::create_dir_all(legacy_tag.join("archives")).unwrap();
        fs::write(legacy_tag.join("archives").join("pkg.deb"), "deb").unwrap();
        let binary = write_fake_boringcache_binary(
            temp.path(),
            format!(
                r#"#!/bin/sh
set -eu
STORE_DIR="{}"
cmd="$1"
shift
workspace="$1"
shift
case "$cmd" in
  check)
    tag="$1"
    if [ -d "$STORE_DIR/$tag" ]; then
      echo '{{"results":[{{"status":"hit","size":5,"compressed_size":3}}]}}'
    else
      echo '{{"results":[{{"status":"miss"}}]}}'
    fi
    ;;
  save)
    pair="$1"
    tag="${{pair%%:*}}"
    path="${{pair#*:}}"
    rm -rf "$STORE_DIR/$tag"
    mkdir -p "$STORE_DIR/$tag"
    cp -R "$path"/. "$STORE_DIR/$tag"/
    ;;
  restore)
    pair="$1"
    tag="${{pair%%:*}}"
    path="${{pair#*:}}"
    mkdir -p "$path"
    cp -R "$STORE_DIR/$tag"/. "$path"/
    ;;
  *)
    echo "unexpected command: $cmd" >&2
    exit 1
    ;;
esac
"#,
                backend_store.display()
            ),
        );

        let store =
            BoringCacheStore::open(Some("org/workspace".to_string()), Some(binary)).unwrap();
        let entry = CacheMount {
            id: "apt".to_string(),
            path: "/var/cache/apt".to_string(),
            key: "apt-archives-bookworm-build".to_string(),
            restore_from: Vec::new(),
            mode: crate::schema::CacheMode::Shared,
        };

        let restored = temp.path().join("restored");
        let result = store.restore(&entry, &restored).unwrap();
        assert!(!result.hit);
        assert!(!restored.join("archives").join("pkg.deb").exists());
        assert!(
            !backend_store
                .join("apt-archives-bookworm-build")
                .join("manifest.json")
                .exists()
        );
    }

    #[test]
    fn batch_has_refs_checks_multiple_tags_in_one_cli_call() {
        let temp = tempdir().unwrap();
        let args_log = temp.path().join("args.log");
        let binary = write_fake_boringcache_binary(
            temp.path(),
            format!(
                r#"#!/bin/sh
set -eu
printf '%s\n' "$@" >> "{}"
cmd="$1"
shift
case "$cmd" in
  check)
    echo '{{"results":[{{"status":"miss"}},{{"status":"hit"}},{{"status":"hit"}}]}}'
    ;;
  *)
    echo "unexpected command: $cmd" >&2
    exit 1
    ;;
esac
"#,
                args_log.display()
            ),
        );

        let backend = BoringCacheBackend::open(
            Some("boringcache/demo".to_string()),
            Some(binary),
            BoringCacheTagScope::portable_no_git(),
        )
        .unwrap();

        let hits = backend
            .batch_has_refs(&[
                "build-cache-step-one".to_string(),
                "build-cache-step-two".to_string(),
                "build-cache-scope".to_string(),
            ])
            .unwrap()
            .expect("expected batch result");

        assert_eq!(hits, vec![false, true, true]);

        let logged = fs::read_to_string(&args_log).unwrap();
        assert!(logged.contains(
            "check\nboringcache/demo\nbuild-cache-step-one,build-cache-step-two,build-cache-scope\n--json"
        ));
    }

    #[test]
    #[ignore]
    fn inspect_step_slice_install_build_packages_blob() {
        let backend = BoringCacheBackend::open(
            Some("boringcache/rails".to_string()),
            Some(PathBuf::from("/Users/gaurav/.local/bin/boringcache")),
            BoringCacheTagScope::portable_no_git(),
        )
        .unwrap();

        let key = "step-slice-slice-bench-20260407-161545-install-build-packages-ubuntu-24-x86_64";
        let manifest = backend
            .resolve_ref_tag(key, Some(key))
            .unwrap()
            .with_context(|| format!("missing manifest for {key}"))
            .unwrap();
        let blob = crate::cache::backend::single_blob_from_manifest(
            &manifest,
            crate::cache::backend::TREE_CACHE_ARTIFACT_KIND,
            backend.kind(),
        )
        .unwrap();

        let temp = tempdir().unwrap();
        let archive_path = temp.path().join("slice.tar.zst");
        backend.fetch_blob(&blob.digest, &archive_path).unwrap();

        let decoder = Decoder::new(fs::File::open(&archive_path).unwrap()).unwrap();
        let mut archive = Archive::new(decoder);
        let mut interesting = Vec::new();
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path().unwrap().into_owned();
            let rendered = path.display().to_string();
            if rendered.contains("libsodium")
                || rendered.contains("ld.so")
                || rendered.contains("x86_64-linux-gnu")
                || rendered.contains("dpkg")
            {
                interesting.push(rendered);
            }
        }
        interesting.sort();
        interesting.dedup();

        println!("manifest: {:?}", manifest);
        println!("blob: {:?}", blob);
        println!("interesting entries:");
        for entry in interesting {
            println!("{entry}");
        }
    }
}
