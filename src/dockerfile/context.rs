use std::fs;
use std::fs::File;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::schema::DockerContext;
use crate::util::fs::{copy_path, directory_content_hash};

pub fn load(dockerfile_path: &Path, context_dir: &Path) -> Result<DockerContext> {
    let root = context_dir
        .canonicalize()
        .unwrap_or_else(|_| context_dir.to_path_buf());
    let ignore_path = dockerfile_specific_ignore_path(dockerfile_path)
        .filter(|path| path.exists())
        .or_else(|| {
            let path = root.join(".dockerignore");
            path.exists().then_some(path)
        });

    let ignore_patterns = if let Some(path) = ignore_path {
        read_ignore_patterns(&path)?
    } else {
        Vec::new()
    };

    Ok(DockerContext {
        root,
        ignore_patterns,
    })
}

pub fn matcher(context: &DockerContext) -> Result<Gitignore> {
    let mut builder = GitignoreBuilder::new(&context.root);
    for pattern in &context.ignore_patterns {
        builder
            .add_line(None, pattern)
            .with_context(|| format!("invalid .dockerignore pattern: {pattern}"))?;
    }
    builder
        .build()
        .map_err(|err| anyhow!("failed to compile .dockerignore patterns: {err}"))
}

pub fn normalize_source(source: &str) -> Result<PathBuf> {
    normalize_source_impl(source, false)
}

pub fn expand_sources(context: &DockerContext, source: &str) -> Result<Vec<String>> {
    let relative = normalize_source_impl(source, true)?;
    if !contains_glob(source.trim()) {
        let (resolved, _) = resolve_source(context, source)?;
        return Ok(vec![display_relative_source(&resolved)]);
    }

    let pattern = display_relative_source(&relative);
    let glob = globset::GlobBuilder::new(&pattern)
        .literal_separator(true)
        .build()
        .with_context(|| format!("invalid Docker build-context glob source: {source}"))?
        .compile_matcher();
    let matcher = matcher(context)?;
    let mut matches = Vec::new();

    for entry in WalkDir::new(&context.root).min_depth(1).sort_by_file_name() {
        let entry = entry.with_context(|| {
            format!(
                "failed to walk Docker build context under {}",
                context.root.display()
            )
        })?;
        let relative = entry
            .path()
            .strip_prefix(&context.root)
            .expect("walked entry should stay inside Docker context root");
        if !is_included_with_matcher(&matcher, relative, entry.file_type().is_dir()) {
            continue;
        }
        let candidate = display_relative_source(relative);
        if glob.is_match(&candidate) {
            matches.push(candidate);
        }
    }

    matches.sort();
    matches.dedup();
    if matches.is_empty() {
        bail!(
            "Docker build-context source '{}' matched no files under {}",
            source,
            context.root.display()
        );
    }

    Ok(matches)
}

pub fn hash_source(context: &DockerContext, source: &str) -> Result<String> {
    let (_, absolute) = resolve_source(context, source)?;
    let metadata = fs::symlink_metadata(&absolute).with_context(|| {
        format!(
            "failed to stat Docker build-context source {}",
            absolute.display()
        )
    })?;
    if !metadata.is_dir() {
        return directory_content_hash(&absolute);
    }

    let matcher = matcher(context)?;
    let mut hasher = Sha256::new();
    hasher.update(b"boringbuilder-docker-context-source-hash-v1\0");
    let mut entries = WalkDir::new(&absolute)
        .follow_links(false)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by(|left, right| left.path().cmp(right.path()));

    for entry in entries {
        let path = entry.path();
        if path == absolute {
            continue;
        }
        let context_relative = path.strip_prefix(&context.root).with_context(|| {
            format!(
                "failed to compute Docker build-context relative path for {}",
                path.display()
            )
        })?;
        let metadata = fs::symlink_metadata(path)?;
        if !is_included_with_matcher(&matcher, context_relative, metadata.is_dir()) {
            continue;
        }
        let source_relative = path.strip_prefix(&absolute).with_context(|| {
            format!(
                "failed to compute Docker source-relative path for {}",
                path.display()
            )
        })?;
        hash_entry(&mut hasher, path, source_relative)?;
    }

    Ok(hex::encode(hasher.finalize()))
}

pub fn materialize_source(context: &DockerContext, source: &str, destination: &Path) -> Result<()> {
    let (_, absolute) = resolve_source(context, source)?;
    let metadata = fs::symlink_metadata(&absolute).with_context(|| {
        format!(
            "failed to stat Docker build-context source {}",
            absolute.display()
        )
    })?;
    if !metadata.is_dir() {
        return copy_path(&absolute, destination).with_context(|| {
            format!("failed to materialize Docker source {}", absolute.display())
        });
    }

    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    fs::set_permissions(destination, metadata.permissions())
        .with_context(|| format!("failed to set permissions on {}", destination.display()))?;

    let matcher = matcher(context)?;
    let mut entries = WalkDir::new(&absolute)
        .follow_links(false)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by(|left, right| left.path().cmp(right.path()));

    for entry in entries {
        let path = entry.path();
        let source_relative = path.strip_prefix(&absolute).with_context(|| {
            format!(
                "failed to compute Docker source-relative path for {}",
                path.display()
            )
        })?;
        if source_relative.as_os_str().is_empty() {
            continue;
        }
        let context_relative = path.strip_prefix(&context.root).with_context(|| {
            format!(
                "failed to compute Docker build-context relative path for {}",
                path.display()
            )
        })?;
        let metadata = fs::symlink_metadata(path)?;
        if !is_included_with_matcher(&matcher, context_relative, metadata.is_dir()) {
            continue;
        }
        copy_entry(path, &destination.join(source_relative))?;
    }

    Ok(())
}

fn normalize_source_impl(source: &str, allow_glob: bool) -> Result<PathBuf> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        bail!("empty Docker build-context source is not allowed");
    }
    if trimmed.starts_with('/') {
        bail!("absolute Docker build-context sources are not supported: {trimmed}");
    }
    if !allow_glob && contains_glob(trimmed) {
        bail!("Dockerfile glob sources are not supported yet: {trimmed}");
    }

    let mut normalized = PathBuf::new();
    for component in Path::new(trimmed).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                bail!("Docker build-context source escapes the context root: {trimmed}")
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("absolute Docker build-context sources are not supported: {trimmed}")
            }
        }
    }

    Ok(normalized)
}

pub fn is_included(context: &DockerContext, relative: &Path, is_dir: bool) -> Result<bool> {
    let matcher = matcher(context)?;
    Ok(is_included_with_matcher(&matcher, relative, is_dir))
}

pub fn resolve_source(context: &DockerContext, source: &str) -> Result<(PathBuf, PathBuf)> {
    let relative = normalize_source(source)?;
    let absolute = if relative.as_os_str().is_empty() {
        context.root.clone()
    } else {
        context.root.join(&relative)
    };
    let metadata = fs::symlink_metadata(&absolute).with_context(|| {
        format!(
            "Docker build-context source '{}' does not exist under {}",
            source,
            context.root.display()
        )
    })?;

    if !relative.as_os_str().is_empty() && !is_included(context, &relative, metadata.is_dir())? {
        bail!(
            "Docker build-context source '{}' is excluded by .dockerignore",
            source
        );
    }

    Ok((relative, absolute))
}

fn dockerfile_specific_ignore_path(dockerfile_path: &Path) -> Option<PathBuf> {
    let file_name = dockerfile_path.file_name()?.to_str()?;
    Some(dockerfile_path.with_file_name(format!("{file_name}.dockerignore")))
}

fn is_included_with_matcher(matcher: &Gitignore, relative: &Path, is_dir: bool) -> bool {
    !matcher
        .matched_path_or_any_parents(relative, is_dir)
        .is_ignore()
}

fn display_relative_source(path: &Path) -> String {
    let rendered = path.to_string_lossy().replace('\\', "/");
    if rendered.is_empty() {
        ".".to_string()
    } else {
        rendered
    }
}

fn read_ignore_patterns(path: &Path) -> Result<Vec<String>> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect())
}

fn contains_glob(source: &str) -> bool {
    source.contains('*') || source.contains('?') || source.contains('[')
}

fn copy_entry(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed to stat {}", source.display()))?;
    if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            symlink(fs::read_link(source)?, destination)?;
        }
        #[cfg(not(unix))]
        {
            anyhow::bail!("symlink-preserving copy is only supported on unix");
        }
        return Ok(());
    }

    if metadata.is_dir() {
        fs::create_dir_all(destination)?;
        fs::set_permissions(destination, metadata.permissions())?;
        return Ok(());
    }

    if !metadata.is_file() {
        return Ok(());
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination)?;
    fs::set_permissions(destination, metadata.permissions())?;
    Ok(())
}

fn hash_entry(hasher: &mut Sha256, path: &Path, relative: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    hasher.update(relative.to_string_lossy().as_bytes());
    hasher.update([0]);
    #[cfg(unix)]
    hasher.update((metadata.permissions().mode() & 0o7777).to_le_bytes());

    if metadata.file_type().is_symlink() {
        hasher.update(b"symlink\0");
        hasher.update(fs::read_link(path)?.to_string_lossy().as_bytes());
        return Ok(());
    }
    if metadata.is_dir() {
        hasher.update(b"dir\0");
        return Ok(());
    }
    if !metadata.is_file() {
        hasher.update(b"special\0");
        return Ok(());
    }

    hasher.update(b"file\0");
    hash_regular_file(path, hasher)
}

fn hash_regular_file(path: &Path, hasher: &mut Sha256) -> Result<()> {
    let metadata = fs::metadata(path)?;
    hasher.update(metadata.len().to_le_bytes());

    let mut file = File::open(path)?;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::{expand_sources, is_included, load, normalize_source, resolve_source};

    #[test]
    fn prefers_dockerfile_specific_ignore() {
        let temp = tempdir().unwrap();
        let dockerfile = temp.path().join("Dockerfile.custom");
        fs::write(&dockerfile, "FROM alpine\n").unwrap();
        fs::write(temp.path().join(".dockerignore"), "ignored.txt\n").unwrap();
        fs::write(
            temp.path().join("Dockerfile.custom.dockerignore"),
            "special.txt\n",
        )
        .unwrap();

        let context = load(&dockerfile, temp.path()).unwrap();
        assert_eq!(context.ignore_patterns, vec!["special.txt".to_string()]);
    }

    #[test]
    fn rejects_parent_directory_escape() {
        let err = normalize_source("../secrets.txt").unwrap_err();
        assert!(err.to_string().contains("escapes the context root"));
    }

    #[test]
    fn matches_dockerignore_rules() {
        let temp = tempdir().unwrap();
        let dockerfile = temp.path().join("Dockerfile");
        fs::write(&dockerfile, "FROM alpine\n").unwrap();
        fs::write(temp.path().join(".dockerignore"), "dist/\n").unwrap();

        let context = load(&dockerfile, temp.path()).unwrap();
        assert!(is_included(&context, Path::new("src/main.rs"), false).unwrap());
        assert!(!is_included(&context, Path::new("dist/app"), false).unwrap());
    }

    #[test]
    fn reject_ignored_source() {
        let temp = tempdir().unwrap();
        let dockerfile = temp.path().join("Dockerfile");
        fs::write(&dockerfile, "FROM alpine\n").unwrap();
        fs::write(temp.path().join(".dockerignore"), "secret.txt\n").unwrap();
        fs::write(temp.path().join("secret.txt"), "nope").unwrap();

        let context = load(&dockerfile, temp.path()).unwrap();
        let err = resolve_source(&context, "secret.txt").unwrap_err();
        assert!(err.to_string().contains("excluded by .dockerignore"));
    }

    #[test]
    fn expands_glob_sources_and_respects_dockerignore() {
        let temp = tempdir().unwrap();
        let dockerfile = temp.path().join("Dockerfile");
        fs::write(&dockerfile, "FROM alpine\n").unwrap();
        fs::create_dir_all(temp.path().join("src/nested")).unwrap();
        fs::write(temp.path().join("src/app.rb"), "puts 'app'\n").unwrap();
        fs::write(temp.path().join("src/nested/util.rb"), "puts 'util'\n").unwrap();
        fs::write(temp.path().join("src/ignored.rb"), "puts 'ignored'\n").unwrap();
        fs::write(temp.path().join(".dockerignore"), "src/ignored.rb\n").unwrap();

        let context = load(&dockerfile, temp.path()).unwrap();

        let recursive = expand_sources(&context, "src/**/*.rb").unwrap();
        assert_eq!(
            recursive,
            vec!["src/app.rb".to_string(), "src/nested/util.rb".to_string()]
        );

        let flat = expand_sources(&context, "src/*.rb").unwrap();
        assert_eq!(flat, vec!["src/app.rb".to_string()]);
    }

    #[test]
    fn rejects_glob_source_with_no_matches() {
        let temp = tempdir().unwrap();
        let dockerfile = temp.path().join("Dockerfile");
        fs::write(&dockerfile, "FROM alpine\n").unwrap();
        let context = load(&dockerfile, temp.path()).unwrap();

        let err = expand_sources(&context, "src/**/*.rb").unwrap_err();
        assert!(err.to_string().contains("matched no files"));
    }
}
