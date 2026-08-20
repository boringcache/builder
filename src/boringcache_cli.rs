use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};

use crate::util::process::{CommandOutput, display_command, find_command};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoringCachePlatformScope {
    PlatformScoped,
    PortableAcrossPlatforms,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoringCacheGitScope {
    GitScoped,
    IgnoreGit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoringCacheTagScope {
    platform: BoringCachePlatformScope,
    git: BoringCacheGitScope,
}

impl BoringCacheTagScope {
    pub const fn new(platform: BoringCachePlatformScope, git: BoringCacheGitScope) -> Self {
        Self { platform, git }
    }

    pub const fn platform_scoped_no_git() -> Self {
        Self::new(
            BoringCachePlatformScope::PlatformScoped,
            BoringCacheGitScope::IgnoreGit,
        )
    }

    pub const fn portable_no_git() -> Self {
        Self::new(
            BoringCachePlatformScope::PortableAcrossPlatforms,
            BoringCacheGitScope::IgnoreGit,
        )
    }

    pub const fn from_cli_flags(no_platform: bool, no_git: bool) -> Self {
        Self::new(
            if no_platform {
                BoringCachePlatformScope::PortableAcrossPlatforms
            } else {
                BoringCachePlatformScope::PlatformScoped
            },
            if no_git {
                BoringCacheGitScope::IgnoreGit
            } else {
                BoringCacheGitScope::GitScoped
            },
        )
    }

    pub fn append_cli_args(self, args: &mut Vec<String>) {
        if matches!(
            self.platform,
            BoringCachePlatformScope::PortableAcrossPlatforms
        ) {
            args.push("--no-platform".to_string());
        }
        if matches!(self.git, BoringCacheGitScope::IgnoreGit) {
            args.push("--no-git".to_string());
        }
    }

    pub fn shell_fragment(self) -> String {
        let mut args = Vec::new();
        self.append_cli_args(&mut args);
        if args.is_empty() {
            String::new()
        } else {
            format!(" {}", args.join(" "))
        }
    }
}

#[derive(Debug, Clone)]
pub struct BoringCacheCli {
    workspace: String,
    binary: PathBuf,
}

impl BoringCacheCli {
    pub fn resolve(workspace: Option<String>, binary: Option<PathBuf>) -> Result<Self> {
        Ok(Self {
            workspace: resolve_workspace(workspace)?,
            binary: resolve_binary(binary)?,
        })
    }

    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    pub fn detail(&self) -> String {
        format!("{} ({})", self.workspace, self.binary.display())
    }

    pub fn run_capture(&self, args: &[String]) -> Result<CommandOutput> {
        let mut command = Command::new(self.binary());
        command.args(args);
        if let Some(token) = fallback_boringcache_api_token_from_lookup(|name| {
            std::env::var_os(name).filter(|value| !value.is_empty())
        }) {
            command.env("BORINGCACHE_API_TOKEN", token);
        }
        let output = command
            .output()
            .with_context(|| format!("failed to run {}", display_command(self.binary(), args)))?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            status: output.status,
        })
    }

    pub fn supports_cache_registry(&self) -> Result<bool> {
        let output = self.run_capture(&["cache-registry".to_string(), "--help".to_string()])?;
        Ok(output.status.success())
    }
}

fn resolve_workspace(override_workspace: Option<String>) -> Result<String> {
    override_workspace
        .or_else(|| std::env::var("BORINGBUILDER_CACHE_WORKSPACE").ok())
        .or_else(|| std::env::var("BORINGCACHE_DEFAULT_WORKSPACE").ok())
        .map(|workspace| workspace.trim().to_string())
        .filter(|workspace| !workspace.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "BoringCache integration requires a workspace; pass --cache-workspace or set BORINGBUILDER_CACHE_WORKSPACE/BORINGCACHE_DEFAULT_WORKSPACE"
            )
        })
}

fn resolve_binary(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path);
    }
    if let Some(path) = std::env::var_os("BORINGBUILDER_CACHE_BIN").map(PathBuf::from) {
        return Ok(path);
    }
    find_command("boringcache").ok_or_else(|| {
        anyhow!("BoringCache integration requires the `boringcache` CLI in PATH or --cache-bin")
    })
}

fn fallback_boringcache_api_token_from_lookup<F>(mut lookup: F) -> Option<OsString>
where
    F: FnMut(&str) -> Option<OsString>,
{
    if lookup("BORINGCACHE_API_TOKEN").is_some() || lookup("BORINGCACHE_TOKEN_FILE").is_some() {
        return None;
    }
    lookup("BORINGCACHE_ADMIN_TOKEN")
        .or_else(|| lookup("BORINGCACHE_SAVE_TOKEN"))
        .or_else(|| lookup("BORINGCACHE_RESTORE_TOKEN"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::fs::File;
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use tempfile::tempdir;

    use super::{
        BoringCacheCli, BoringCacheGitScope, BoringCachePlatformScope, BoringCacheTagScope,
    };

    fn write_executable_script(path: &Path, contents: &str) {
        let temp_path = path.with_extension("tmp");
        let mut file = File::create(&temp_path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file.sync_all().unwrap();
        drop(file);
        fs::rename(&temp_path, path).unwrap();
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).unwrap();
        }
    }

    #[test]
    fn resolves_workspace_and_binary_from_explicit_values() {
        let cli = BoringCacheCli::resolve(
            Some("boringcache/demo".to_string()),
            Some("/tmp/boringcache".into()),
        )
        .unwrap();

        assert_eq!(cli.workspace(), "boringcache/demo");
        assert_eq!(cli.binary().to_string_lossy(), "/tmp/boringcache");
    }

    #[test]
    fn tag_scope_renders_expected_cli_args() {
        let mut args = Vec::new();
        BoringCacheTagScope::new(
            BoringCachePlatformScope::PortableAcrossPlatforms,
            BoringCacheGitScope::IgnoreGit,
        )
        .append_cli_args(&mut args);
        assert_eq!(args, vec!["--no-platform", "--no-git"]);

        let mut args = Vec::new();
        BoringCacheTagScope::platform_scoped_no_git().append_cli_args(&mut args);
        assert_eq!(args, vec!["--no-git"]);
    }

    #[test]
    fn detects_cache_registry_support() {
        let temp = tempdir().unwrap();
        let binary = temp.path().join("boringcache");
        write_executable_script(
            &binary,
            r#"#!/bin/sh
set -eu
if [ "$1" = "cache-registry" ] && [ "$2" = "--help" ]; then
  exit 0
fi
exit 1
"#,
        );

        let cli =
            BoringCacheCli::resolve(Some("boringcache/demo".to_string()), Some(binary)).unwrap();
        assert!(cli.supports_cache_registry().unwrap());
    }

    #[test]
    fn detects_missing_cache_registry_support() {
        let temp = tempdir().unwrap();
        let binary = temp.path().join("boringcache");
        write_executable_script(
            &binary,
            r#"#!/bin/sh
set -eu
exit 1
"#,
        );

        let cli =
            BoringCacheCli::resolve(Some("boringcache/demo".to_string()), Some(binary)).unwrap();
        assert!(!cli.supports_cache_registry().unwrap());
    }

    #[test]
    fn fallback_prefers_admin_then_save_then_restore() {
        let admin = super::fallback_boringcache_api_token_from_lookup(|name| match name {
            "BORINGCACHE_ADMIN_TOKEN" => Some("admin".into()),
            "BORINGCACHE_SAVE_TOKEN" => Some("save".into()),
            "BORINGCACHE_RESTORE_TOKEN" => Some("restore".into()),
            _ => None,
        });
        assert_eq!(admin, Some("admin".into()));

        let save = super::fallback_boringcache_api_token_from_lookup(|name| match name {
            "BORINGCACHE_SAVE_TOKEN" => Some("save".into()),
            "BORINGCACHE_RESTORE_TOKEN" => Some("restore".into()),
            _ => None,
        });
        assert_eq!(save, Some("save".into()));

        let restore = super::fallback_boringcache_api_token_from_lookup(|name| match name {
            "BORINGCACHE_RESTORE_TOKEN" => Some("restore".into()),
            _ => None,
        });
        assert_eq!(restore, Some("restore".into()));
    }

    #[test]
    fn fallback_skips_when_api_token_or_token_file_is_configured() {
        let api = super::fallback_boringcache_api_token_from_lookup(|name| match name {
            "BORINGCACHE_API_TOKEN" => Some("api".into()),
            "BORINGCACHE_ADMIN_TOKEN" => Some("admin".into()),
            _ => None,
        });
        assert_eq!(api, None);

        let token_file = super::fallback_boringcache_api_token_from_lookup(|name| match name {
            "BORINGCACHE_TOKEN_FILE" => Some("/tmp/token".into()),
            "BORINGCACHE_ADMIN_TOKEN" => Some("admin".into()),
            _ => None,
        });
        assert_eq!(token_file, None);
    }
}
