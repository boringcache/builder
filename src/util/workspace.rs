use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::fs::File;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::util::process::{find_command, run_capture};

struct GitWorkspaceLocation {
    git: PathBuf,
    git_root: PathBuf,
    prefix: PathBuf,
}

struct GitWorkspaceDigestMetadata {
    tracked_blob_ids: BTreeMap<PathBuf, String>,
    dirty_paths: BTreeSet<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWorkspaceChangeSummary {
    pub paths: Vec<PathBuf>,
    pub truncated: bool,
    pub total_paths: usize,
}

pub fn prepare_git_workspace_paths(
    base_dir: &Path,
    excluded_paths: &[PathBuf],
    required_paths: &[PathBuf],
) -> Result<Option<Vec<PathBuf>>> {
    let Some(location) = locate_git_workspace(base_dir)? else {
        return Ok(None);
    };
    let output = run_capture(
        &location.git,
        &[
            "-C".to_string(),
            location.git_root.display().to_string(),
            "ls-files".to_string(),
            "--cached".to_string(),
            "--others".to_string(),
            "--exclude-standard".to_string(),
            "-z".to_string(),
        ],
    )?;
    if !output.status.success() {
        return Ok(None);
    }

    let mut relative_paths = BTreeSet::new();
    for raw in output.stdout.split('\0').filter(|value| !value.is_empty()) {
        let repo_relative = Path::new(raw);
        let Some(workspace_relative) =
            repo_relative_to_workspace_relative(repo_relative, &location.prefix)
        else {
            continue;
        };
        if workspace_relative.as_os_str().is_empty()
            || is_excluded_workspace_path(&workspace_relative, excluded_paths)
        {
            continue;
        }
        if !base_dir.join(&workspace_relative).exists() {
            continue;
        }
        insert_required_workspace_path(
            &mut relative_paths,
            base_dir,
            &workspace_relative,
            excluded_paths,
        )?;
    }

    for required in required_paths {
        if required.as_os_str().is_empty()
            || is_excluded_workspace_path(required, excluded_paths)
            || !base_dir.join(required).exists()
        {
            continue;
        }
        insert_required_workspace_path(&mut relative_paths, base_dir, required, excluded_paths)?;
    }

    Ok(Some(relative_paths.into_iter().collect()))
}

fn insert_required_workspace_path(
    relative_paths: &mut BTreeSet<PathBuf>,
    base_dir: &Path,
    required: &Path,
    excluded_paths: &[PathBuf],
) -> Result<()> {
    let absolute = base_dir.join(required);
    let metadata = fs::symlink_metadata(&absolute).with_context(|| {
        format!(
            "failed to stat required workspace path {}",
            absolute.display()
        )
    })?;
    if metadata.is_dir() {
        let mut entries = WalkDir::new(&absolute)
            .follow_links(false)
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()?;
        entries.sort_by(|left, right| left.path().cmp(right.path()));
        for entry in entries {
            let relative = entry
                .path()
                .strip_prefix(base_dir)
                .with_context(|| {
                    format!(
                        "failed to compute required workspace relative path for {}",
                        entry.path().display()
                    )
                })?
                .to_path_buf();
            if relative.as_os_str().is_empty()
                || is_excluded_workspace_path(&relative, excluded_paths)
            {
                continue;
            }
            relative_paths.insert(relative);
        }
        return Ok(());
    }

    relative_paths.insert(required.to_path_buf());
    Ok(())
}

pub fn is_excluded_workspace_path(relative: &Path, excluded_paths: &[PathBuf]) -> bool {
    excluded_paths
        .iter()
        .any(|excluded| relative == excluded || relative.starts_with(excluded))
}

pub fn selected_workspace_entries(relative_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut entries = BTreeSet::new();
    for path in relative_paths {
        let mut current = path.parent();
        while let Some(parent) = current {
            if parent.as_os_str().is_empty() {
                break;
            }
            entries.insert(parent.to_path_buf());
            current = parent.parent();
        }
        entries.insert(path.clone());
    }
    entries.into_iter().collect()
}

pub fn hash_workspace_tree(root: &Path, excluded_paths: &[PathBuf]) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut entries = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by(|left, right| left.path().cmp(right.path()));
    for entry in entries {
        let path = entry.path();
        if path == root {
            continue;
        }
        let relative = path.strip_prefix(root)?;
        if is_excluded_workspace_path(relative, excluded_paths) {
            continue;
        }
        hash_tree_entry(&mut hasher, path, relative)?;
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn compile_workspace_ignore_matcher(root: &Path, patterns: &[String]) -> Result<Gitignore> {
    let mut builder = GitignoreBuilder::new(root);
    for pattern in patterns {
        if pattern.trim().is_empty() {
            bail!("empty workspace ignore pattern is not allowed");
        }
        builder
            .add_line(None, pattern)
            .with_context(|| format!("invalid workspace ignore pattern: {pattern}"))?;
    }
    builder
        .build()
        .map_err(|err| anyhow!("failed to compile workspace ignore patterns: {err}"))
}

pub fn validate_workspace_ignore_patterns(patterns: &[String]) -> Result<()> {
    let _ = compile_workspace_ignore_matcher(Path::new("/"), patterns)?;
    Ok(())
}

pub fn hash_workspace_tree_with_patterns(
    root: &Path,
    exclude_patterns: &[String],
) -> Result<String> {
    let matcher = compile_workspace_ignore_matcher(root, exclude_patterns)?;
    let mut hasher = Sha256::new();
    let mut entries = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by(|left, right| left.path().cmp(right.path()));
    for entry in entries {
        let path = entry.path();
        if path == root {
            continue;
        }
        let relative = path.strip_prefix(root)?;
        let metadata = fs::symlink_metadata(path)?;
        if matcher
            .matched_path_or_any_parents(relative, metadata.is_dir())
            .is_ignore()
        {
            continue;
        }
        hash_tree_entry(&mut hasher, path, relative)?;
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn hash_path_with_patterns(root: &Path, exclude_patterns: &[String]) -> Result<String> {
    if exclude_patterns.is_empty() {
        return crate::util::fs::directory_content_hash(root);
    }

    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return crate::util::fs::directory_content_hash(root);
        }
        Err(err) => return Err(err.into()),
    };

    if !metadata.is_dir() {
        return crate::util::fs::directory_content_hash(root);
    }

    hash_workspace_tree_with_patterns(root, exclude_patterns)
}

pub fn hash_selected_workspace_tree(root: &Path, relative_paths: &[PathBuf]) -> Result<String> {
    let mut hasher = Sha256::new();
    for relative in selected_workspace_entries(relative_paths) {
        hash_tree_entry(&mut hasher, &root.join(&relative), &relative)?;
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn hash_git_aware_workspace_tree(
    root: &Path,
    excluded_paths: &[PathBuf],
    required_paths: &[PathBuf],
) -> Result<Option<String>> {
    let Some(relative_paths) = prepare_git_workspace_paths(root, excluded_paths, required_paths)?
    else {
        return Ok(None);
    };
    Ok(Some(hash_git_selected_workspace_tree(
        root,
        &relative_paths,
    )?))
}

pub fn hash_git_aware_workspace_tree_with_patterns(
    root: &Path,
    exclude_patterns: &[String],
    required_paths: &[PathBuf],
) -> Result<Option<String>> {
    let Some(relative_paths) = prepare_git_workspace_paths(root, &[], required_paths)? else {
        return Ok(None);
    };
    let matcher = compile_workspace_ignore_matcher(root, exclude_patterns)?;
    let filtered = relative_paths
        .into_iter()
        .filter(|relative| {
            let absolute = root.join(relative);
            let is_dir = fs::symlink_metadata(&absolute)
                .map(|metadata| metadata.is_dir())
                .unwrap_or(false);
            !matcher
                .matched_path_or_any_parents(relative, is_dir)
                .is_ignore()
        })
        .collect::<Vec<_>>();
    Ok(Some(hash_git_selected_workspace_tree(root, &filtered)?))
}

pub fn hash_git_selected_workspace_tree(root: &Path, relative_paths: &[PathBuf]) -> Result<String> {
    let Some(metadata) = load_git_workspace_digest_metadata(root)? else {
        return hash_selected_workspace_tree(root, relative_paths);
    };

    let mut hasher = Sha256::new();
    for relative in selected_workspace_entries(relative_paths) {
        if let Some(blob_id) = metadata.tracked_blob_ids.get(&relative)
            && !metadata.dirty_paths.contains(&relative)
        {
            hash_tree_entry_from_git_blob(&mut hasher, &root.join(&relative), &relative, blob_id)?;
            continue;
        }
        hash_tree_entry(&mut hasher, &root.join(&relative), &relative)?;
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn summarize_git_workspace_changes_with_patterns(
    root: &Path,
    exclude_patterns: &[String],
    limit: usize,
) -> Result<Option<GitWorkspaceChangeSummary>> {
    let Some(location) = locate_git_workspace(root)? else {
        return Ok(None);
    };
    let worktree_output = run_capture(
        &location.git,
        &[
            "-C".to_string(),
            location.git_root.display().to_string(),
            "ls-files".to_string(),
            "--modified".to_string(),
            "--deleted".to_string(),
            "--others".to_string(),
            "--exclude-standard".to_string(),
            "-z".to_string(),
        ],
    )?;
    if !worktree_output.status.success() {
        return Ok(None);
    }
    let staged_output = run_capture(
        &location.git,
        &[
            "-C".to_string(),
            location.git_root.display().to_string(),
            "diff".to_string(),
            "--cached".to_string(),
            "--name-only".to_string(),
            "-z".to_string(),
            "--diff-filter=ACDMRTUXB".to_string(),
        ],
    )?;
    if !staged_output.status.success() {
        return Ok(None);
    }

    let matcher = compile_workspace_ignore_matcher(root, exclude_patterns)?;
    let mut paths = BTreeSet::new();
    for raw in worktree_output
        .stdout
        .split('\0')
        .chain(staged_output.stdout.split('\0'))
        .filter(|value| !value.is_empty())
    {
        let repo_relative = Path::new(raw);
        let Some(workspace_relative) =
            repo_relative_to_workspace_relative(repo_relative, &location.prefix)
        else {
            continue;
        };
        if workspace_relative.as_os_str().is_empty() {
            continue;
        }
        let absolute = root.join(&workspace_relative);
        let is_dir = fs::symlink_metadata(&absolute)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false);
        if matcher
            .matched_path_or_any_parents(&workspace_relative, is_dir)
            .is_ignore()
        {
            continue;
        }
        paths.insert(workspace_relative);
    }

    let total = paths.len();
    let paths = paths.into_iter().take(limit).collect::<Vec<_>>();
    Ok(Some(GitWorkspaceChangeSummary {
        paths,
        truncated: total > limit,
        total_paths: total,
    }))
}

fn locate_git_workspace(base_dir: &Path) -> Result<Option<GitWorkspaceLocation>> {
    let Some(git) = find_command("git") else {
        return Ok(None);
    };
    let git_root = match git_repo_root(&git, base_dir)? {
        Some(root) => root,
        None => return Ok(None),
    };
    let canonical_base = fs::canonicalize(base_dir).unwrap_or_else(|_| base_dir.to_path_buf());
    let canonical_git_root = fs::canonicalize(&git_root).unwrap_or_else(|_| git_root.clone());
    let prefix = match canonical_base.strip_prefix(&canonical_git_root) {
        Ok(path) => path.to_path_buf(),
        Err(_) => return Ok(None),
    };
    Ok(Some(GitWorkspaceLocation {
        git,
        git_root,
        prefix,
    }))
}

fn repo_relative_to_workspace_relative(repo_relative: &Path, prefix: &Path) -> Option<PathBuf> {
    if prefix.as_os_str().is_empty() {
        return Some(repo_relative.to_path_buf());
    }
    repo_relative
        .strip_prefix(prefix)
        .ok()
        .map(Path::to_path_buf)
}

fn load_git_workspace_digest_metadata(
    base_dir: &Path,
) -> Result<Option<GitWorkspaceDigestMetadata>> {
    let Some(location) = locate_git_workspace(base_dir)? else {
        return Ok(None);
    };

    let tracked_output = run_capture(
        &location.git,
        &[
            "-C".to_string(),
            location.git_root.display().to_string(),
            "ls-files".to_string(),
            "--cached".to_string(),
            "--stage".to_string(),
            "-z".to_string(),
        ],
    )?;
    if !tracked_output.status.success() {
        return Ok(None);
    }

    let dirty_output = run_capture(
        &location.git,
        &[
            "-C".to_string(),
            location.git_root.display().to_string(),
            "ls-files".to_string(),
            "--modified".to_string(),
            "--deleted".to_string(),
            "--others".to_string(),
            "--exclude-standard".to_string(),
            "-z".to_string(),
        ],
    )?;
    if !dirty_output.status.success() {
        return Ok(None);
    }

    let mut tracked_blob_ids = BTreeMap::new();
    for record in tracked_output
        .stdout
        .split('\0')
        .filter(|value| !value.is_empty())
    {
        let Some((metadata, path)) = record.split_once('\t') else {
            return Ok(None);
        };
        let Some(blob_id) = metadata.split_whitespace().nth(1) else {
            return Ok(None);
        };
        let repo_relative = Path::new(path);
        let Some(workspace_relative) =
            repo_relative_to_workspace_relative(repo_relative, &location.prefix)
        else {
            continue;
        };
        if workspace_relative.as_os_str().is_empty() || !base_dir.join(&workspace_relative).exists()
        {
            continue;
        }
        tracked_blob_ids.insert(workspace_relative, blob_id.to_string());
    }

    let mut dirty_paths = BTreeSet::new();
    for raw in dirty_output
        .stdout
        .split('\0')
        .filter(|value| !value.is_empty())
    {
        let repo_relative = Path::new(raw);
        let Some(workspace_relative) =
            repo_relative_to_workspace_relative(repo_relative, &location.prefix)
        else {
            continue;
        };
        if workspace_relative.as_os_str().is_empty() {
            continue;
        }
        dirty_paths.insert(workspace_relative);
    }

    Ok(Some(GitWorkspaceDigestMetadata {
        tracked_blob_ids,
        dirty_paths,
    }))
}

fn git_repo_root(git: &Path, base_dir: &Path) -> Result<Option<PathBuf>> {
    let output = run_capture(
        git,
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

fn hash_tree_entry(hasher: &mut Sha256, path: &Path, relative: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    hash_tree_entry_header(hasher, relative, &metadata);

    if metadata.file_type().is_symlink() {
        hasher.update(b"symlink\0");
        let target =
            fs::read_link(path).with_context(|| format!("failed to read {}", path.display()))?;
        hasher.update(target.to_string_lossy().as_bytes());
        hasher.update([0]);
        return Ok(());
    }

    if metadata.is_dir() {
        hasher.update(b"dir\0");
        return Ok(());
    }

    if metadata.is_file() {
        hasher.update(b"file\0");
        let mut file = File::open(path)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;
        hasher.update(buffer);
        return Ok(());
    }

    bail!("unsupported path type while hashing {}", path.display())
}

fn hash_tree_entry_from_git_blob(
    hasher: &mut Sha256,
    path: &Path,
    relative: &Path,
    blob_id: &str,
) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    hash_tree_entry_header(hasher, relative, &metadata);
    if metadata.file_type().is_symlink() {
        hasher.update(b"git-symlink-blob\0");
        hasher.update(blob_id.as_bytes());
        hasher.update([0]);
        return Ok(());
    }

    if metadata.is_dir() {
        hasher.update(b"dir\0");
        return Ok(());
    }

    if metadata.is_file() {
        hasher.update(b"git-file-blob\0");
        hasher.update(blob_id.as_bytes());
        hasher.update([0]);
        return Ok(());
    }

    bail!("unsupported path type while hashing {}", path.display())
}

fn hash_tree_entry_header(hasher: &mut Sha256, relative: &Path, metadata: &fs::Metadata) {
    #[cfg(unix)]
    {
        hasher.update(relative.display().to_string().as_bytes());
        hasher.update([0]);
        hasher.update(metadata.permissions().mode().to_le_bytes());
        hasher.update([0]);
    }
    #[cfg(not(unix))]
    {
        hasher.update(relative.display().to_string().as_bytes());
        hasher.update([0]);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    use tempfile::tempdir;

    use super::{
        hash_git_aware_workspace_tree, hash_git_aware_workspace_tree_with_patterns,
        hash_git_selected_workspace_tree, hash_path_with_patterns,
        hash_workspace_tree_with_patterns, prepare_git_workspace_paths,
        summarize_git_workspace_changes_with_patterns,
    };

    fn git_capture(root: &std::path::Path, args: &[&str]) {
        let global_config = root.join(".gitconfig-test");
        if !global_config.exists() {
            fs::write(&global_config, "").unwrap();
        }
        let output = Command::new("git")
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", &global_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn git_aware_tree_hash_ignores_gitignored_paths() {
        let temp = tempdir().unwrap();
        let root = temp.path();
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

        fs::write(root.join(".gitignore"), "tmp/\n").unwrap();
        fs::create_dir_all(root.join("app")).unwrap();
        fs::write(root.join("app/main.rb"), "puts 'hello'\n").unwrap();
        crate::util::process::run_capture(
            &git,
            &[
                "-C".to_string(),
                root.display().to_string(),
                "add".to_string(),
                ".gitignore".to_string(),
                "app/main.rb".to_string(),
            ],
        )
        .unwrap();

        let before = hash_git_aware_workspace_tree(root, &[], &[])
            .unwrap()
            .unwrap();
        fs::create_dir_all(root.join("tmp/bench")).unwrap();
        fs::write(root.join("tmp/bench/output.json"), "{\"ok\":true}\n").unwrap();
        let after = hash_git_aware_workspace_tree(root, &[], &[])
            .unwrap()
            .unwrap();

        assert_eq!(before, after);
    }

    #[test]
    fn prepare_git_workspace_paths_skips_tracked_files_deleted_from_worktree() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        git_capture(root, &["init"]);
        git_capture(root, &["config", "user.email", "test@example.com"]);
        git_capture(root, &["config", "user.name", "Test"]);

        fs::create_dir_all(root.join("app/views")).unwrap();
        fs::write(root.join("app/views/kept.html.erb"), "kept\n").unwrap();
        fs::write(root.join("app/views/deleted.html.erb"), "deleted\n").unwrap();
        git_capture(root, &["add", "."]);
        git_capture(root, &["commit", "-m", "seed"]);

        fs::remove_file(root.join("app/views/deleted.html.erb")).unwrap();

        let paths = prepare_git_workspace_paths(root, &[], &[])
            .unwrap()
            .unwrap();
        assert!(paths.contains(&PathBuf::from("app/views/kept.html.erb")));
        assert!(!paths.contains(&PathBuf::from("app/views/deleted.html.erb")));
    }

    #[test]
    fn git_selected_workspace_hash_is_stable_for_clean_repo() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        git_capture(root, &["init"]);
        git_capture(root, &["config", "user.email", "test@example.com"]);
        git_capture(root, &["config", "user.name", "Test"]);

        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("src/nested/lib.rs"), "pub fn ready() {}\n").unwrap();
        fs::write(root.join("README.md"), "ready\n").unwrap();
        git_capture(root, &["add", "."]);
        git_capture(root, &["commit", "-m", "seed"]);

        let relative_paths = prepare_git_workspace_paths(root, &[], &[])
            .unwrap()
            .unwrap();
        let first = hash_git_selected_workspace_tree(root, &relative_paths).unwrap();
        let second = hash_git_selected_workspace_tree(root, &relative_paths).unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn git_selected_workspace_hash_tracks_dirty_repo_changes() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        git_capture(root, &["init"]);
        git_capture(root, &["config", "user.email", "test@example.com"]);
        git_capture(root, &["config", "user.name", "Test"]);

        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        git_capture(root, &["add", "."]);
        git_capture(root, &["commit", "-m", "seed"]);

        let relative_paths = prepare_git_workspace_paths(root, &[], &[])
            .unwrap()
            .unwrap();
        let clean = hash_git_selected_workspace_tree(root, &relative_paths).unwrap();

        fs::write(
            root.join("src/main.rs"),
            "fn main() { println!(\"dirty\"); }\n",
        )
        .unwrap();
        fs::write(root.join("notes.txt"), "untracked\n").unwrap();

        let relative_paths = prepare_git_workspace_paths(root, &[], &[])
            .unwrap()
            .unwrap();
        let dirty = hash_git_selected_workspace_tree(root, &relative_paths).unwrap();

        assert_ne!(clean, dirty);
    }

    #[test]
    fn hash_path_with_patterns_ignores_excluded_directory_content() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("target/cache.txt"), "old\n").unwrap();

        let before = hash_path_with_patterns(root, &["target".to_string()]).unwrap();
        fs::write(root.join("target/cache.txt"), "new\n").unwrap();
        let after = hash_path_with_patterns(root, &["target".to_string()]).unwrap();

        assert_eq!(before, after);
    }

    #[test]
    fn summarize_git_workspace_changes_respects_excludes_and_limit() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        git_capture(root, &["init"]);
        git_capture(root, &["config", "user.email", "test@example.com"]);
        git_capture(root, &["config", "user.name", "Test"]);

        fs::create_dir_all(root.join("app")).unwrap();
        fs::create_dir_all(root.join("tmp")).unwrap();
        fs::write(root.join("app/main.rb"), "puts 'one'\n").unwrap();
        fs::write(root.join("README.md"), "one\n").unwrap();
        git_capture(root, &["add", "."]);
        git_capture(root, &["commit", "-m", "seed"]);

        fs::write(root.join("app/main.rb"), "puts 'two'\n").unwrap();
        fs::write(root.join("README.md"), "two\n").unwrap();
        fs::write(root.join("tmp/build.log"), "ignored\n").unwrap();

        let summary = summarize_git_workspace_changes_with_patterns(root, &["tmp".to_string()], 1)
            .unwrap()
            .unwrap();
        assert_eq!(summary.paths.len(), 1);
        assert!(summary.truncated);
        assert_eq!(summary.total_paths, 2);
        assert_ne!(summary.paths[0], PathBuf::from("tmp/build.log"));
    }

    #[test]
    fn summarize_git_workspace_changes_reports_clean_repo() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        git_capture(root, &["init"]);
        git_capture(root, &["config", "user.email", "test@example.com"]);
        git_capture(root, &["config", "user.name", "Test"]);

        fs::write(root.join("README.md"), "one\n").unwrap();
        git_capture(root, &["add", "."]);
        git_capture(root, &["commit", "-m", "seed"]);

        let summary = summarize_git_workspace_changes_with_patterns(root, &[], 3)
            .unwrap()
            .unwrap();
        assert!(summary.paths.is_empty());
        assert!(!summary.truncated);
        assert_eq!(summary.total_paths, 0);
    }

    #[test]
    fn summarize_git_workspace_changes_includes_staged_paths() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        git_capture(root, &["init"]);
        git_capture(root, &["config", "user.email", "test@example.com"]);
        git_capture(root, &["config", "user.name", "Test"]);

        fs::write(root.join("README.md"), "one\n").unwrap();
        git_capture(root, &["add", "."]);
        git_capture(root, &["commit", "-m", "seed"]);

        fs::write(root.join("README.md"), "two\n").unwrap();
        git_capture(root, &["add", "README.md"]);

        let summary = summarize_git_workspace_changes_with_patterns(root, &[], 3)
            .unwrap()
            .unwrap();
        assert_eq!(summary.paths, vec![PathBuf::from("README.md")]);
        assert_eq!(summary.total_paths, 1);
    }

    #[test]
    fn workspace_tree_hash_with_patterns_skips_excluded_paths() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("app")).unwrap();
        fs::create_dir_all(root.join("log")).unwrap();
        fs::write(root.join("app/main.rb"), "puts 'one'\n").unwrap();
        fs::write(root.join("log/production.log"), "one\n").unwrap();

        let before = hash_workspace_tree_with_patterns(root, &["log".to_string()]).unwrap();
        fs::write(root.join("log/production.log"), "two\n").unwrap();
        let after = hash_workspace_tree_with_patterns(root, &["log".to_string()]).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn prepare_git_workspace_paths_expands_required_directories() {
        let temp = tempdir().unwrap();
        let root = temp.path();
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

        fs::write(root.join("boringbuilder.yml"), "image: alpine:3.20\n").unwrap();
        crate::util::process::run_capture(
            &git,
            &[
                "-C".to_string(),
                root.display().to_string(),
                "add".to_string(),
                "boringbuilder.yml".to_string(),
            ],
        )
        .unwrap();
        fs::create_dir_all(root.join("boringbuilder-ci-handoff/workspace/dist")).unwrap();
        fs::write(
            root.join("boringbuilder-ci-handoff/workspace/dist/value.txt"),
            "handoff\n",
        )
        .unwrap();

        let paths =
            prepare_git_workspace_paths(root, &[], &[PathBuf::from("boringbuilder-ci-handoff")])
                .unwrap()
                .unwrap();

        assert!(paths.contains(&PathBuf::from("boringbuilder.yml")));
        assert!(paths.contains(&PathBuf::from("boringbuilder-ci-handoff")));
        assert!(paths.contains(&PathBuf::from("boringbuilder-ci-handoff/workspace")));
        assert!(paths.contains(&PathBuf::from(
            "boringbuilder-ci-handoff/workspace/dist/value.txt"
        )));
    }

    #[test]
    fn prepare_git_workspace_paths_expands_submodule_contents() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("workspace");
        let submodule = temp.path().join("upstream");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&submodule).unwrap();

        git_capture(&submodule, &["init"]);
        git_capture(&submodule, &["config", "user.email", "test@example.com"]);
        git_capture(&submodule, &["config", "user.name", "Test"]);
        fs::create_dir_all(submodule.join("tools/api_reference")).unwrap();
        fs::write(
            submodule.join("tools/bazel.rc"),
            "common --disk_cache=/tmp\n",
        )
        .unwrap();
        fs::write(submodule.join("tools/README.md"), "docs\n").unwrap();
        fs::write(submodule.join("tools/api_reference/index.md"), "index\n").unwrap();
        git_capture(&submodule, &["add", "."]);
        git_capture(&submodule, &["commit", "-m", "seed"]);

        git_capture(&root, &["init"]);
        git_capture(&root, &["config", "user.email", "test@example.com"]);
        git_capture(&root, &["config", "user.name", "Test"]);

        let global_config = root.join(".gitconfig-test");
        if !global_config.exists() {
            fs::write(&global_config, "").unwrap();
        }
        let output = Command::new("git")
            .current_dir(&root)
            .env("GIT_CONFIG_GLOBAL", &global_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args([
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                submodule.to_str().unwrap(),
                "upstream",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git submodule add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let paths = prepare_git_workspace_paths(&root, &[], &[])
            .unwrap()
            .unwrap();
        assert!(paths.contains(&PathBuf::from("upstream")));
        assert!(paths.contains(&PathBuf::from("upstream/tools")));
        assert!(paths.contains(&PathBuf::from("upstream/tools/bazel.rc")));
        assert!(paths.contains(&PathBuf::from("upstream/tools/README.md")));
        assert!(paths.contains(&PathBuf::from("upstream/tools/api_reference/index.md")));
    }

    #[test]
    fn git_aware_tree_hash_with_patterns_skips_excluded_paths() {
        let temp = tempdir().unwrap();
        let root = temp.path();
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

        fs::create_dir_all(root.join("app")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("app/main.rb"), "puts 'one'\n").unwrap();
        fs::write(root.join("docs/readme.md"), "one\n").unwrap();
        crate::util::process::run_capture(
            &git,
            &[
                "-C".to_string(),
                root.display().to_string(),
                "add".to_string(),
                "app/main.rb".to_string(),
                "docs/readme.md".to_string(),
            ],
        )
        .unwrap();

        let before = hash_git_aware_workspace_tree_with_patterns(root, &["docs".to_string()], &[])
            .unwrap()
            .unwrap();
        fs::write(root.join("docs/readme.md"), "two\n").unwrap();
        let after = hash_git_aware_workspace_tree_with_patterns(root, &["docs".to_string()], &[])
            .unwrap()
            .unwrap();

        assert_eq!(before, after);
    }
}
