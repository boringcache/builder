use std::fs;
use std::path::Path;

use anyhow::{Result, bail};
use tempfile::TempDir;

use crate::schema::{Operation, RemoteAddOp};

const HELPER_DIR_GUEST: &str = "/boringbuilder-helper";
const HELPER_SCRIPT_NAME: &str = "guest-helper.sh";
const SNAPSHOT_DIR_GUEST: &str = "/boringbuilder-stage-snapshot";

pub struct GuestHelperHost {
    dir: TempDir,
    next_batch: usize,
}

impl GuestHelperHost {
    pub fn create() -> Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("boringbuilder-macos-helper-")
            .tempdir()?;
        let helper_path = dir.path().join(HELPER_SCRIPT_NAME);
        fs::write(&helper_path, helper_script())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&helper_path)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&helper_path, perms)?;
        }
        Ok(Self { dir, next_batch: 0 })
    }

    pub fn mount_source(&self) -> &Path {
        self.dir.path()
    }

    pub fn guest_helper_path(&self) -> String {
        format!("{HELPER_DIR_GUEST}/{HELPER_SCRIPT_NAME}")
    }

    pub fn write_batch(&mut self, operations: &[Operation]) -> Result<String> {
        self.next_batch += 1;
        let file_name = format!("batch-{:04}.sh", self.next_batch);
        let batch_path = self.dir.path().join(&file_name);
        fs::write(&batch_path, build_batch_script(operations)?)?;
        Ok(format!("{HELPER_DIR_GUEST}/{file_name}"))
    }
}

pub fn helper_mount_guest_dir() -> &'static str {
    HELPER_DIR_GUEST
}

pub fn helper_snapshot_guest_dir() -> &'static str {
    SNAPSHOT_DIR_GUEST
}

pub fn is_helper_operation(operation: &Operation) -> bool {
    matches!(
        operation,
        Operation::CopyFromContext(_) | Operation::CopyFromStage(_) | Operation::AddRemote(_)
    )
}

pub fn helper_batch_label(operations: &[Operation], start_index: usize) -> String {
    if operations.len() == 1 {
        return operations[0]
            .name()
            .map(str::to_string)
            .unwrap_or_else(|| format!("step-{}", start_index + 1));
    }

    if let Some(first) = operations[0].name() {
        format!("{first} + {} more filesystem ops", operations.len() - 1)
    } else {
        format!("filesystem-batch-{}", start_index + 1)
    }
}

fn build_batch_script(operations: &[Operation]) -> Result<String> {
    let mut lines = vec!["set -eu".to_string()];
    for operation in operations {
        match operation {
            Operation::CopyFromContext(op) => {
                let mut line = format!(
                    "bb_copy_context {} {} {} {} {}",
                    if op.extract_archives { "1" } else { "0" },
                    if op.preserve_parents { "1" } else { "0" },
                    shell_words::quote(&op.dest),
                    shell_words::quote(op.chown.as_deref().unwrap_or("0:0")),
                    shell_words::quote(op.chmod.as_deref().unwrap_or("-"))
                );
                for exclude in &op.exclude {
                    line.push(' ');
                    line.push_str("--exclude ");
                    line.push_str(&shell_words::quote(exclude));
                }
                line.push_str(" --");
                for source in &op.sources {
                    line.push(' ');
                    line.push_str(&shell_words::quote(source));
                }
                lines.push(line);
            }
            Operation::CopyFromStage(op) => {
                let mut line = format!(
                    "bb_copy_stage {} {} {} {} {} {}",
                    if op.follow_symlinks { "1" } else { "0" },
                    if op.preserve_parents { "1" } else { "0" },
                    shell_words::quote(&op.stage),
                    shell_words::quote(&op.dest),
                    shell_words::quote(op.chown.as_deref().unwrap_or("0:0")),
                    shell_words::quote(op.chmod.as_deref().unwrap_or("-"))
                );
                for exclude in &op.exclude {
                    line.push(' ');
                    line.push_str("--exclude ");
                    line.push_str(&shell_words::quote(exclude));
                }
                line.push_str(" --");
                for source in &op.sources {
                    line.push(' ');
                    line.push_str(&shell_words::quote(source));
                }
                lines.push(line);
            }
            Operation::AddRemote(RemoteAddOp {
                url,
                dest,
                checksum,
                ..
            }) => lines.push(format!(
                "bb_add_remote {} {} {}",
                shell_words::quote(url),
                shell_words::quote(dest),
                shell_words::quote(checksum.as_deref().unwrap_or("-"))
            )),
            Operation::Exec(_) => bail!("guest helper batch only supports filesystem operations"),
        }
    }
    Ok(lines.join("\n") + "\n")
}

fn helper_script() -> &'static str {
    r#"#!/bin/sh
set -eu

BB_CONTEXT_ROOT="${BB_CONTEXT_ROOT:-/boringbuilder-context}"
BB_STAGE_ROOT="${BB_STAGE_ROOT:-/boringbuilder-stages}"
BB_SNAPSHOT_ROOT="${BB_SNAPSHOT_ROOT:-/boringbuilder-stage-snapshot}"

bb_dest_has_trailing_slash() {
  case "$1" in
    */) return 0 ;;
    *) return 1 ;;
  esac
}

bb_source_contents_only() {
  case "$1" in
    "."|"./"|*/) return 0 ;;
    *) return 1 ;;
  esac
}

bb_trim_trailing_slashes() {
  value="$1"
  while [ -n "$value" ] && [ "$value" != "${value%/}" ]; do
    value="${value%/}"
  done
  printf '%s' "$value"
}

bb_strip_parents_marker() {
  value="$1"
  case "$value" in
    /./*)
      printf '/%s' "${value#/./}"
      ;;
    ./*)
      printf '%s' "${value#./}"
      ;;
    */./*)
      prefix="${value%%/./*}"
      suffix="${value#"$prefix"/./}"
      if [ -n "$prefix" ]; then
        printf '%s/%s' "$prefix" "$suffix"
      else
        printf '%s' "$suffix"
      fi
      ;;
    *)
      printf '%s' "$value"
      ;;
  esac
}

bb_preserve_source_ref() {
  source="$1"
  case "$source" in
    /./*)
      trimmed="${source#/./}"
      ;;
    ./*)
      trimmed="${source#./}"
      ;;
    */./*)
      prefix="${source%%/./*}"
      trimmed="${source#"$prefix"/./}"
      ;;
    /*)
      trimmed="${source#/}"
      ;;
    *)
      trimmed="$source"
      ;;
  esac
  bb_trim_trailing_slashes "$trimmed"
}

bb_resolve_context_source() {
  source="$1"
  source="$(bb_strip_parents_marker "$source")"
  trimmed="${source#./}"
  trimmed="$(bb_trim_trailing_slashes "$trimmed")"
  if [ -z "$trimmed" ] || [ "$trimmed" = "." ]; then
    printf '%s' "$BB_CONTEXT_ROOT"
  else
    printf '%s/%s' "$BB_CONTEXT_ROOT" "$trimmed"
  fi
}

bb_resolve_stage_source() {
  stage="$1"
  source="$2"
  source="$(bb_strip_parents_marker "$source")"
  trimmed="${source#/}"
  trimmed="$(bb_trim_trailing_slashes "$trimmed")"
  if [ -z "$trimmed" ]; then
    printf '%s/%s' "$BB_STAGE_ROOT" "$stage"
  else
    printf '%s/%s/%s' "$BB_STAGE_ROOT" "$stage" "$trimmed"
  fi
}

bb_cp() {
  follow="$1"
  shift
  if [ "$follow" = "1" ]; then
    cp -aL "$@"
  else
    cp -a "$@"
  fi
}

bb_cleanup_excludes() {
  if [ -n "${1:-}" ] && [ -f "$1" ]; then
    rm -f "$1"
  fi
}

bb_trim_leading_dots_and_slashes() {
  value="$1"
  while [ "${value#./}" != "$value" ]; do
    value="${value#./}"
  done
  while [ "${value#/}" != "$value" ]; do
    value="${value#/}"
  done
  printf '%s' "$value"
}

bb_normalize_exclude_for_source() {
  source="$1"
  pattern="$2"
  prefix="$(bb_trim_leading_dots_and_slashes "$source")"
  prefix="$(bb_trim_trailing_slashes "$prefix")"
  if [ -n "$prefix" ]; then
    case "$pattern" in
      "$prefix"/*)
        printf '%s' "${pattern#"$prefix"/}"
        return
        ;;
    esac
  fi
  printf '%s' "$pattern"
}

bb_copy_tree_with_excludes() {
  follow="$1"
  src="$2"
  dest="$3"
  source_ref="$4"
  excludes_file="$5"

  mkdir -p "$dest"
  normalized_excludes="$(mktemp /tmp/boringbuilder-excludes-normalized.XXXXXX)"
  while IFS= read -r pattern; do
    normalized="$(bb_normalize_exclude_for_source "$source_ref" "$pattern")"
    if [ -n "$normalized" ]; then
      printf '%s\n' "$normalized" >> "$normalized_excludes"
    fi
  done < "$excludes_file"

  if [ "$follow" = "1" ]; then
    tar_follow="-h"
  else
    tar_follow=""
  fi

  if [ -s "$normalized_excludes" ]; then
    tar $tar_follow -C "$src" --exclude-from "$normalized_excludes" -cf - . | tar -C "$dest" -xf -
  else
    tar $tar_follow -C "$src" -cf - . | tar -C "$dest" -xf -
  fi
  rm -f "$normalized_excludes"
}

bb_copy_entry() {
  follow="$1"
  src="$2"
  dest="$3"
  contents_only="$4"

  if [ -d "$src" ] && [ "$contents_only" = "1" ]; then
    mkdir -p "$dest"
    bb_cp "$follow" "$src"/. "$dest"
    return
  fi

  if [ -d "$dest" ] || bb_dest_has_trailing_slash "$dest"; then
    mkdir -p "$dest"
    bb_cp "$follow" "$src" "$dest"
    return
  fi

  parent="$(dirname "$dest")"
  mkdir -p "$parent"
  bb_cp "$follow" "$src" "$dest"
}

bb_looks_archive() {
  case "$1" in
    *.tar|*.tar.gz|*.tgz|*.tar.xz|*.txz|*.tar.bz2|*.tbz2) return 0 ;;
    *) return 1 ;;
  esac
}

bb_extract_archive() {
  src="$1"
  dest="$2"
  mkdir -p "$dest"
  tar -xf "$src" -C "$dest"
}

bb_apply_copy_meta() {
  target="$1"
  owner="$2"
  mode="$3"

  chown -hR "$owner" "$target"
  if [ "$mode" != "-" ]; then
    chmod -R "$mode" "$target"
  fi
}

bb_copy_context() {
  extract="$1"
  parents="$2"
  dest="$3"
  owner="$4"
  mode="$5"
  shift 5
  excludes_file="$(mktemp /tmp/boringbuilder-excludes.XXXXXX)"
  while [ "$#" -gt 0 ]; do
    if [ "$1" = "--exclude" ]; then
      printf '%s\n' "$2" >> "$excludes_file"
      shift 2
      continue
    fi
    if [ "$1" = "--" ]; then
      shift
      break
    fi
    echo "unexpected helper flag: $1" >&2
    exit 1
  done

  if [ "$#" -gt 1 ]; then
    mkdir -p "$dest"
  fi

  for source in "$@"; do
    src="$(bb_resolve_context_source "$source")"
    target="$dest"
    if [ "$parents" = "1" ]; then
      preserved="$(bb_preserve_source_ref "$source")"
      if [ -n "$preserved" ] && [ "$preserved" != "." ]; then
        target="$dest/$preserved"
      fi
    fi
    if [ "$extract" = "1" ] && bb_looks_archive "$source"; then
      bb_extract_archive "$src" "$target"
      bb_apply_copy_meta "$target" "$owner" "$mode"
      continue
    fi

    contents_only=0
    if bb_source_contents_only "$source"; then
      contents_only=1
    fi
    if [ -s "$excludes_file" ] && [ -d "$src" ]; then
      bb_copy_tree_with_excludes "0" "$src" "$target" "$source" "$excludes_file"
    else
      bb_copy_entry "0" "$src" "$target" "$contents_only"
    fi
    bb_apply_copy_meta "$target" "$owner" "$mode"
  done
  bb_cleanup_excludes "$excludes_file"
}

bb_copy_stage() {
  follow="$1"
  parents="$2"
  stage="$3"
  dest="$4"
  owner="$5"
  mode="$6"
  shift 6
  excludes_file="$(mktemp /tmp/boringbuilder-excludes.XXXXXX)"
  while [ "$#" -gt 0 ]; do
    if [ "$1" = "--exclude" ]; then
      printf '%s\n' "$2" >> "$excludes_file"
      shift 2
      continue
    fi
    if [ "$1" = "--" ]; then
      shift
      break
    fi
    echo "unexpected helper flag: $1" >&2
    exit 1
  done

  if [ "$#" -gt 1 ]; then
    mkdir -p "$dest"
  fi

  for source in "$@"; do
    src="$(bb_resolve_stage_source "$stage" "$source")"
    target="$dest"
    if [ "$parents" = "1" ]; then
      preserved="$(bb_preserve_source_ref "$source")"
      if [ -n "$preserved" ] && [ "$preserved" != "." ]; then
        target="$dest/$preserved"
      fi
    fi
    if [ -s "$excludes_file" ] && [ -d "$src" ]; then
      bb_copy_tree_with_excludes "$follow" "$src" "$target" "$source" "$excludes_file"
    else
      bb_copy_entry "$follow" "$src" "$target" "0"
    fi
    bb_apply_copy_meta "$target" "$owner" "$mode"
  done
  bb_cleanup_excludes "$excludes_file"
}

bb_remote_basename() {
  url="$1"
  name="${url##*/}"
  name="${name%%\?*}"
  name="${name%%\#*}"
  if [ -z "$name" ]; then
    name="download"
  fi
  printf '%s' "$name"
}

bb_download_remote() {
  url="$1"
  dest="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$dest"
    return
  fi
  if command -v wget >/dev/null 2>&1; then
    wget -qO "$dest" "$url"
    return
  fi
  if command -v busybox >/dev/null 2>&1; then
    busybox wget -qO "$dest" "$url"
    return
  fi
  echo "remote ADD requires curl, wget, or busybox wget in the container" >&2
  exit 1
}

bb_add_remote() {
  url="$1"
  dest="$2"
  checksum="${3:-"-"}"
  tmp="$(mktemp /tmp/boringbuilder-remote.XXXXXX)"
  bb_download_remote "$url" "$tmp"

  if [ "$checksum" != "-" ]; then
    case "$checksum" in
      sha256:*)
        expected="${checksum#sha256:}"
        if command -v sha256sum >/dev/null 2>&1; then
          actual="$(sha256sum "$tmp" | awk '{print $1}')"
        elif command -v shasum >/dev/null 2>&1; then
          actual="$(shasum -a 256 "$tmp" | awk '{print $1}')"
        elif command -v busybox >/dev/null 2>&1; then
          actual="$(busybox sha256sum "$tmp" | awk '{print $1}')"
        else
          echo "ADD --checksum requires sha256sum, shasum, or busybox sha256sum in the container" >&2
          exit 1
        fi
        if [ "$actual" != "$expected" ]; then
          echo "ADD --checksum mismatch for $url: expected $checksum got sha256:$actual" >&2
          exit 1
        fi
        ;;
      *)
        echo "unsupported ADD --checksum algorithm: $checksum" >&2
        exit 1
        ;;
    esac
  fi

  if [ -d "$dest" ] || bb_dest_has_trailing_slash "$dest"; then
    mkdir -p "$dest"
    target="$dest/$(bb_remote_basename "$url")"
  else
    parent="$(dirname "$dest")"
    mkdir -p "$parent"
    target="$dest"
  fi

  mv "$tmp" "$target"
}

bb_snapshot_path() {
  src="$1"
  trimmed="${src#/}"
  if [ -z "$trimmed" ]; then
    echo "refusing to snapshot container root '/'" >&2
    exit 1
  fi

  dest="$BB_SNAPSHOT_ROOT/$trimmed"
  parent="$(dirname "$dest")"
  mkdir -p "$parent"
  rm -rf "$dest"
  if [ -d "$src" ] && [ ! -L "$src" ]; then
    mkdir -p "$dest"
    (
      cd "$src"
      find . -mindepth 1 -type d | while IFS= read -r rel; do
        mkdir -p "$dest/${rel#./}"
      done
      find . -mindepth 1 ! -type d | while IFS= read -r rel; do
        target="$dest/${rel#./}"
        mkdir -p "$(dirname "$target")"
        cp -P "$rel" "$target"
      done
    )
    return
  fi

  cp -P "$src" "$dest"
}

cmd="${1:-}"
case "$cmd" in
  batch)
    shift
    . "$1"
    ;;
  copy-context)
    shift
    bb_copy_context "$@"
    ;;
  copy-stage)
    shift
    bb_copy_stage "$@"
    ;;
  add-remote)
    shift
    bb_add_remote "$@"
    ;;
  snapshot)
    shift
    for path in "$@"; do
      bb_snapshot_path "$path"
    done
    ;;
  *)
    echo "unknown guest helper command: $cmd" >&2
    exit 1
    ;;
esac
"#
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    use tempfile::tempdir;

    use crate::schema::{ContextCopyOp, Operation, RemoteAddOp, StageCopyOp};

    use super::{build_batch_script, helper_batch_label, helper_script, is_helper_operation};

    fn write_helper_script(temp: &tempfile::TempDir) -> PathBuf {
        let helper = temp.path().join("guest-helper.sh");
        fs::write(&helper, helper_script()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&helper).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&helper, perms).unwrap();
        }
        helper
    }

    #[test]
    fn marks_filesystem_operations_for_helper() {
        assert!(is_helper_operation(&Operation::CopyFromContext(
            ContextCopyOp {
                name: Some("copy".to_string()),
                sources: vec![".".to_string()],
                dest: "/workspace".to_string(),
                exclude: Vec::new(),
                extract_archives: false,
                preserve_parents: false,
                chown: None,
                chmod: None,
            }
        )));
        assert!(is_helper_operation(&Operation::CopyFromStage(
            StageCopyOp {
                name: Some("stage".to_string()),
                stage: "builder".to_string(),
                sources: vec!["/out".to_string()],
                dest: "/workspace".to_string(),
                exclude: Vec::new(),
                preserve_parents: false,
                follow_symlinks: true,
                chown: None,
                chmod: None,
            }
        )));
        assert!(is_helper_operation(&Operation::AddRemote(RemoteAddOp {
            name: Some("remote".to_string()),
            url: "https://example.test/file.txt".to_string(),
            dest: "/workspace/file.txt".to_string(),
            checksum: None,
        })));
    }

    #[test]
    fn builds_batch_script_for_guest_helper() {
        let script = build_batch_script(&[
            Operation::CopyFromContext(ContextCopyOp {
                name: Some("copy".to_string()),
                sources: vec![".".to_string(), "assets/".to_string()],
                dest: "/workspace".to_string(),
                exclude: vec!["*.map".to_string()],
                extract_archives: true,
                preserve_parents: false,
                chown: Some("1000:1001".to_string()),
                chmod: Some("755".to_string()),
            }),
            Operation::CopyFromStage(StageCopyOp {
                name: Some("stage".to_string()),
                stage: "builder".to_string(),
                sources: vec!["/usr/bin/tool".to_string()],
                dest: "/usr/bin/tool".to_string(),
                exclude: vec!["*.tmp".to_string()],
                preserve_parents: false,
                follow_symlinks: false,
                chown: None,
                chmod: None,
            }),
            Operation::AddRemote(RemoteAddOp {
                name: Some("remote".to_string()),
                url: "https://example.test/archive.tgz".to_string(),
                dest: "/tmp/".to_string(),
                checksum: Some("sha256:deadbeef".to_string()),
            }),
        ])
        .unwrap();

        assert!(script.contains("bb_copy_context 1 0 /workspace 1000:1001 755"));
        assert!(script.contains("--exclude '*.map'"));
        assert!(script.contains("assets/"));
        assert!(script.contains("bb_copy_stage 0 0 builder /usr/bin/tool 0:0 -"));
        assert!(script.contains("--exclude '*.tmp'"));
        assert!(script.contains("bb_add_remote"));
        assert!(script.contains("https://example.test/archive.tgz"));
        assert!(script.contains("sha256:deadbeef"));
    }

    #[test]
    fn labels_multi_op_batches() {
        let label = helper_batch_label(
            &[
                Operation::CopyFromContext(ContextCopyOp {
                    name: Some("copy".to_string()),
                    sources: vec![".".to_string()],
                    dest: "/workspace".to_string(),
                    exclude: Vec::new(),
                    extract_archives: false,
                    preserve_parents: false,
                    chown: None,
                    chmod: None,
                }),
                Operation::AddRemote(RemoteAddOp {
                    name: Some("remote".to_string()),
                    url: "https://example.test/archive.tgz".to_string(),
                    dest: "/tmp/".to_string(),
                    checksum: None,
                }),
            ],
            0,
        );
        assert_eq!(label, "copy + 1 more filesystem ops");
    }

    #[test]
    fn snapshot_command_honors_snapshot_root_override_for_files_and_directories() {
        let temp = tempdir().unwrap();
        let helper = write_helper_script(&temp);

        let source_dir = temp.path().join("source-dir");
        let nested_dir = source_dir.join("nested");
        fs::create_dir_all(&nested_dir).unwrap();
        fs::write(nested_dir.join("asset.txt"), "hello from dir").unwrap();

        let source_file = temp.path().join("standalone.txt");
        fs::write(&source_file, "hello from file").unwrap();

        let snapshot_root = temp.path().join("snapshot-root");
        fs::create_dir_all(&snapshot_root).unwrap();

        let status = Command::new("/bin/sh")
            .arg(&helper)
            .arg("snapshot")
            .arg(&source_dir)
            .arg(&source_file)
            .env("BB_SNAPSHOT_ROOT", &snapshot_root)
            .status()
            .unwrap();
        assert!(status.success());

        let dir_dest = snapshot_root.join(source_dir.strip_prefix("/").unwrap());
        let file_dest = snapshot_root.join(source_file.strip_prefix("/").unwrap());
        assert_eq!(
            fs::read_to_string(dir_dest.join("nested/asset.txt")).unwrap(),
            "hello from dir"
        );
        assert_eq!(fs::read_to_string(file_dest).unwrap(), "hello from file");
    }
}
