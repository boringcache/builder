use anyhow::{Context, Result, anyhow, bail, ensure};
use std::path::Path;

use crate::schema::CacheMode;

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogicalLine {
    line_number: usize,
    text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Instruction {
    From {
        image: String,
        alias: Option<String>,
        platform: Option<String>,
    },
    Run {
        command: String,
        exec: Option<Vec<String>>,
        mounts: Vec<RunMount>,
    },
    Copy {
        from: Option<String>,
        sources: Vec<String>,
        dest: String,
        exclude: Vec<String>,
        parents: bool,
        chown: Option<String>,
        chmod: Option<String>,
    },
    Add {
        sources: Vec<String>,
        dest: String,
        checksum: Option<String>,
    },
    Env {
        entries: Vec<(String, String)>,
    },
    Arg {
        key: String,
        default: Option<String>,
    },
    Workdir {
        path: String,
    },
    Expose {
        port: String,
    },
    Label {
        key: String,
        value: String,
    },
    User {
        user: String,
    },
    Entrypoint {
        exec: Vec<String>,
    },
    Cmd {
        exec: Vec<String>,
    },
    Healthcheck {
        test: Option<Vec<String>>,
        interval_nanos: Option<u64>,
        timeout_nanos: Option<u64>,
        start_period_nanos: Option<u64>,
        start_interval_nanos: Option<u64>,
        retries: Option<u32>,
    },
    Volume {
        paths: Vec<String>,
    },
    StopSignal {
        signal: String,
    },
    Shell {
        exec: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunMount {
    Cache {
        target: String,
        id: Option<String>,
        readonly: bool,
        sharing: CacheMode,
    },
    Bind {
        target: String,
        source: Option<String>,
        from: Option<String>,
        readonly: bool,
    },
    Tmpfs {
        target: String,
        size: Option<String>,
    },
    Secret {
        target: String,
        id: String,
        env: Option<String>,
        required: bool,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
    },
    Ssh {
        target: String,
        id: String,
        required: bool,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
    },
}

pub fn parse(content: &str) -> Result<Vec<Instruction>> {
    let logical_lines = join_continuations(content);
    let mut instructions = Vec::new();

    for line in &logical_lines {
        let trimmed = line.text.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let (keyword, rest) = split_instruction(trimmed);
        let instruction = parse_instruction(&keyword.to_ascii_uppercase(), rest)
            .map_err(|err| anyhow!("Dockerfile line {}: {}: {err}", line.line_number, trimmed))?;
        if let Some(inst) = instruction {
            instructions.push(inst);
        }
    }

    Ok(instructions)
}

fn join_continuations(content: &str) -> Vec<LogicalLine> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_line_number = 1usize;
    let mut in_heredoc = false;
    let mut heredoc_marker = String::new();

    for (index, raw_line) in content.lines().enumerate() {
        let line_number = index + 1;
        if in_heredoc {
            current.push('\n');
            current.push_str(raw_line);
            if raw_line.trim() == heredoc_marker {
                in_heredoc = false;
                lines.push(LogicalLine {
                    line_number: current_line_number,
                    text: std::mem::take(&mut current),
                });
            }
            continue;
        }

        if raw_line.contains("<<")
            && !raw_line.trim_start().starts_with('#')
            && let Some(marker) = extract_heredoc_marker(raw_line)
        {
            heredoc_marker = marker;
            in_heredoc = true;
            if current.is_empty() {
                current_line_number = line_number;
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(raw_line);
            continue;
        }

        if let Some(without_backslash) = raw_line.strip_suffix('\\') {
            if current.is_empty() {
                current_line_number = line_number;
                current.push_str(without_backslash);
            } else {
                current.push(' ');
                current.push_str(without_backslash.trim_start());
            }
        } else if current.is_empty() {
            lines.push(LogicalLine {
                line_number,
                text: raw_line.to_string(),
            });
        } else {
            current.push(' ');
            current.push_str(raw_line.trim_start());
            lines.push(LogicalLine {
                line_number: current_line_number,
                text: std::mem::take(&mut current),
            });
        }
    }

    if !current.is_empty() {
        lines.push(LogicalLine {
            line_number: current_line_number,
            text: current,
        });
    }

    lines
}

fn extract_heredoc_marker(line: &str) -> Option<String> {
    let pos = line.find("<<")?;
    let after = &line[pos + 2..];
    let marker = after.trim().trim_matches(|c| c == '\'' || c == '"');
    if marker.is_empty() || marker.contains(' ') {
        return None;
    }
    Some(marker.to_string())
}

fn split_instruction(line: &str) -> (String, &str) {
    let line = line.trim();
    if let Some(space_idx) = line.find(|c: char| c.is_whitespace()) {
        let keyword = line[..space_idx].to_string();
        let rest = line[space_idx..].trim_start();
        (keyword, rest)
    } else {
        (line.to_string(), "")
    }
}

fn parse_instruction(keyword: &str, args: &str) -> Result<Option<Instruction>> {
    match keyword {
        "FROM" => Ok(Some(parse_from(args)?)),
        "RUN" => Ok(Some(parse_run(args)?)),
        "COPY" => Ok(Some(parse_copy(args)?)),
        "ADD" => Ok(Some(parse_add(args)?)),
        "ENV" => Ok(Some(parse_env(args)?)),
        "ARG" => Ok(Some(parse_arg(args)?)),
        "WORKDIR" => Ok(Some(Instruction::Workdir {
            path: args.to_string(),
        })),
        "EXPOSE" => Ok(Some(Instruction::Expose {
            port: args.to_string(),
        })),
        "LABEL" => parse_label(args),
        "USER" => Ok(Some(Instruction::User {
            user: args.to_string(),
        })),
        "ENTRYPOINT" => Ok(Some(parse_entrypoint(args)?)),
        "CMD" => Ok(Some(parse_cmd(args)?)),
        "HEALTHCHECK" => Ok(Some(parse_healthcheck(args)?)),
        "VOLUME" => Ok(Some(parse_volume(args)?)),
        "STOPSIGNAL" => Ok(Some(Instruction::StopSignal {
            signal: args.to_string(),
        })),
        "SHELL" => Ok(Some(parse_shell(args)?)),
        "ONBUILD" | "MAINTAINER" => {
            bail!("{keyword} is not supported yet")
        }
        _ => bail!("unknown Dockerfile instruction: {keyword}"),
    }
}

fn parse_from(args: &str) -> Result<Instruction> {
    let args = args.trim();
    if args.is_empty() {
        bail!("FROM requires an image argument");
    }

    let (flags, rest) = extract_flags(args);
    let mut platform = None;
    let unsupported: Vec<&str> = flags
        .iter()
        .map(String::as_str)
        .filter(|flag| {
            if let Some(value) = flag.strip_prefix("--platform=") {
                platform = Some(value.trim_matches('"').to_string());
                false
            } else {
                true
            }
        })
        .collect();
    if !unsupported.is_empty() {
        bail!("unsupported FROM flags: {}", unsupported.join(", "));
    }

    let parts: Vec<&str> = rest.split_whitespace().collect();
    if parts.is_empty() {
        bail!("FROM requires an image argument");
    }
    let image = parts[0].to_string();
    let alias = if parts.len() == 3 && parts[1].eq_ignore_ascii_case("as") {
        Some(parts[2].to_string())
    } else {
        ensure_no_extra_from_tokens(&parts)?;
        None
    };

    Ok(Instruction::From {
        image,
        alias,
        platform,
    })
}

fn parse_run(args: &str) -> Result<Instruction> {
    let args = args.trim();
    if args.is_empty() {
        bail!("RUN requires a command");
    }

    let (mounts, command_args) = if args.starts_with("--mount=") {
        let (flags, rest) = extract_flags(args);
        ensure!(!rest.is_empty(), "RUN requires a command");
        (parse_run_flags(&flags)?, rest)
    } else {
        (Vec::new(), args)
    };

    let command = if command_args.starts_with('[') {
        let exec = parse_json_array(command_args)?;
        return Ok(Instruction::Run {
            command: exec
                .iter()
                .map(|item| shell_words::quote(item).into_owned())
                .collect::<Vec<_>>()
                .join(" "),
            exec: Some(exec),
            mounts,
        });
    } else {
        command_args.to_string()
    };

    Ok(Instruction::Run {
        command,
        exec: None,
        mounts,
    })
}

fn parse_run_flags(flags: &[String]) -> Result<Vec<RunMount>> {
    let mut mounts = Vec::new();
    let unsupported = flags
        .iter()
        .filter(|flag| !flag.starts_with("--mount="))
        .cloned()
        .collect::<Vec<_>>();
    if !unsupported.is_empty() {
        bail!("unsupported RUN flags: {}", unsupported.join(", "));
    }
    for flag in flags {
        let mount = flag
            .strip_prefix("--mount=")
            .expect("RUN flag should have been validated as mount");
        mounts.push(parse_run_mount(mount)?);
    }
    Ok(mounts)
}

fn parse_run_mount(raw: &str) -> Result<RunMount> {
    let options = split_mount_options(raw)?;
    let mut mount_type = "bind".to_string();
    let mut target = None;
    let mut source = None;
    let mut from = None;
    let mut id = None;
    let mut env = None;
    let mut required = false;
    let mut readonly = None;
    let mut sharing = CacheMode::Shared;
    let mut size = None;
    let mut mode = None;
    let mut uid = None;
    let mut gid = None;

    for option in options {
        let trimmed = option.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            match trimmed {
                "ro" | "readonly" => {
                    readonly = Some(true);
                    continue;
                }
                "rw" | "readwrite" => {
                    readonly = Some(false);
                    continue;
                }
                _ => bail!("unsupported RUN --mount option: {trimmed}"),
            }
        };
        let value = value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        match key {
            "type" => mount_type = value.to_ascii_lowercase(),
            "target" | "dst" | "destination" => target = Some(value),
            "source" | "src" => source = Some(value),
            "from" => from = Some(value),
            "id" => id = Some(value),
            "env" => env = Some(value),
            "required" => required = parse_mount_bool("required", &value)?,
            "sharing" => {
                sharing = match value.as_str() {
                    "shared" => CacheMode::Shared,
                    "locked" => CacheMode::Locked,
                    "private" => bail!("RUN --mount sharing=private is not supported yet"),
                    _ => bail!("unsupported RUN --mount sharing mode: {value}"),
                };
            }
            "mode" => mode = Some(parse_mount_mode(&value)?),
            "uid" => uid = Some(parse_mount_u32("uid", &value)?),
            "gid" => gid = Some(parse_mount_u32("gid", &value)?),
            "size" => size = Some(value),
            _ => bail!("unsupported RUN --mount option: {key}"),
        }
    }

    match mount_type.as_str() {
        "bind" => {
            let target = target.ok_or_else(|| anyhow!("RUN --mount requires target="))?;
            ensure!(id.is_none(), "RUN --mount=type=bind does not support id=");
            ensure!(
                env.is_none()
                    && !required
                    && size.is_none()
                    && mode.is_none()
                    && uid.is_none()
                    && gid.is_none(),
                "RUN --mount=type=bind only supports target=, source=, from=, and ro/rw today"
            );
            ensure!(
                sharing == CacheMode::Shared,
                "RUN --mount=type=bind does not support sharing="
            );
            Ok(RunMount::Bind {
                target,
                source,
                from,
                readonly: readonly.unwrap_or(true),
            })
        }
        "cache" => {
            let target = target.ok_or_else(|| anyhow!("RUN --mount requires target="))?;
            ensure!(
                source.is_none()
                    && from.is_none()
                    && env.is_none()
                    && !required
                    && size.is_none()
                    && mode.is_none()
                    && uid.is_none()
                    && gid.is_none(),
                "RUN --mount=type=cache only supports target=, id=, sharing=, and ro/rw today"
            );
            Ok(RunMount::Cache {
                target,
                id,
                readonly: readonly.unwrap_or(false),
                sharing,
            })
        }
        "tmpfs" => {
            let target = target.ok_or_else(|| anyhow!("RUN --mount requires target="))?;
            ensure!(
                source.is_none()
                    && from.is_none()
                    && id.is_none()
                    && env.is_none()
                    && !required
                    && mode.is_none()
                    && uid.is_none()
                    && gid.is_none(),
                "RUN --mount=type=tmpfs only supports target= and optional size="
            );
            ensure!(
                sharing == CacheMode::Shared,
                "RUN --mount=type=tmpfs does not support sharing="
            );
            ensure!(
                readonly != Some(true),
                "RUN --mount=type=tmpfs does not support readonly"
            );
            Ok(RunMount::Tmpfs { target, size })
        }
        "secret" => {
            ensure!(
                source.is_none()
                    && from.is_none()
                    && size.is_none()
                    && sharing == CacheMode::Shared,
                "RUN --mount=type=secret only supports id=, target=, env=, required=, mode=, uid=, gid=, and ro today"
            );
            ensure!(
                readonly != Some(false),
                "RUN --mount=type=secret does not support readwrite"
            );
            let id = id
                .or_else(|| env.clone())
                .or_else(|| target.as_deref().and_then(default_secret_id))
                .ok_or_else(|| anyhow!("RUN --mount=type=secret requires id=, env=, or target="))?;
            let target = target.unwrap_or_else(|| default_secret_target(&id));
            Ok(RunMount::Secret {
                target,
                id,
                env,
                required,
                mode,
                uid,
                gid,
            })
        }
        "ssh" => {
            ensure!(
                source.is_none()
                    && from.is_none()
                    && env.is_none()
                    && size.is_none()
                    && sharing == CacheMode::Shared,
                "RUN --mount=type=ssh only supports id=, target=, required=, mode=, uid=, gid=, and ro today"
            );
            ensure!(
                readonly != Some(false),
                "RUN --mount=type=ssh does not support readwrite"
            );
            let id = id.unwrap_or_else(|| "default".to_string());
            let target = target.unwrap_or_else(|| default_ssh_target(&id));
            Ok(RunMount::Ssh {
                target,
                id,
                required,
                mode,
                uid,
                gid,
            })
        }
        _ => bail!("unsupported RUN --mount type: {mount_type}"),
    }
}

fn parse_mount_bool(key: &str, value: &str) -> Result<bool> {
    match value {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => bail!("RUN --mount option {key} expects true/false or 1/0, got {value}"),
    }
}

fn parse_mount_u32(key: &str, value: &str) -> Result<u32> {
    value
        .parse::<u32>()
        .with_context(|| format!("RUN --mount option {key} must be an integer, got {value}"))
}

fn parse_mount_mode(value: &str) -> Result<u32> {
    let normalized = value.strip_prefix("0o").unwrap_or(value);
    ensure!(
        !normalized.is_empty() && normalized.chars().all(|ch| matches!(ch, '0'..='7')),
        "RUN --mount option mode must be an octal permission like 0400 or 0600"
    );
    u32::from_str_radix(normalized, 8)
        .with_context(|| format!("RUN --mount option mode must be valid octal, got {value}"))
}

fn default_secret_id(target: &str) -> Option<String> {
    Path::new(target)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn default_secret_target(id: &str) -> String {
    format!("/run/secrets/{id}")
}

fn default_ssh_target(id: &str) -> String {
    format!("/run/buildkit/ssh_agent.{id}")
}

fn split_mount_options(raw: &str) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let mut quote_char = '\0';

    for ch in raw.chars() {
        if in_quote {
            if ch == quote_char {
                in_quote = false;
            } else {
                current.push(ch);
            }
            continue;
        }

        match ch {
            '"' | '\'' => {
                in_quote = true;
                quote_char = ch;
            }
            ',' => {
                let trimmed = current.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    ensure!(!in_quote, "unterminated quote in RUN --mount");
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        parts.push(trimmed.to_string());
    }
    Ok(parts)
}

fn parse_copy(args: &str) -> Result<Instruction> {
    let (flags, rest) = extract_flags(args);
    let mut from = None;
    let mut chown = None;
    let mut chmod = None;
    let mut exclude = Vec::new();
    let mut parents = false;
    let unsupported: Vec<&str> = flags
        .iter()
        .map(String::as_str)
        .filter(|flag| {
            if let Some(value) = flag.strip_prefix("--from=") {
                from = Some(value.trim_matches('"').to_string());
                false
            } else if let Some(value) = flag.strip_prefix("--exclude=") {
                exclude.push(value.trim_matches('"').to_string());
                false
            } else if let Some(value) = flag.strip_prefix("--chown=") {
                chown = Some(value.trim_matches('"').to_string());
                false
            } else if let Some(value) = flag.strip_prefix("--chmod=") {
                chmod = Some(value.trim_matches('"').to_string());
                false
            } else if *flag == "--parents" {
                parents = true;
                false
            } else if let Some(value) = flag.strip_prefix("--parents=") {
                parents = match value.trim_matches('"') {
                    "true" | "1" => true,
                    "false" | "0" => false,
                    _ => return true,
                };
                false
            } else {
                true
            }
        })
        .collect();
    if !unsupported.is_empty() {
        bail!("unsupported COPY flags: {}", unsupported.join(", "));
    }

    let parts = shell_split(rest);
    if parts.len() < 2 {
        bail!("COPY requires at least a source and destination");
    }

    let dest = parts.last().unwrap().clone();
    let sources = parts[..parts.len() - 1].to_vec();

    Ok(Instruction::Copy {
        from,
        sources,
        dest,
        exclude,
        parents,
        chown,
        chmod,
    })
}

fn parse_add(args: &str) -> Result<Instruction> {
    let (flags, rest) = extract_flags(args);
    let mut checksum = None;
    let unsupported: Vec<&str> = flags
        .iter()
        .map(String::as_str)
        .filter(|flag| {
            if let Some(value) = flag.strip_prefix("--checksum=") {
                if checksum.is_some() {
                    return true;
                }
                checksum = Some(value.trim_matches('"').to_string());
                false
            } else {
                true
            }
        })
        .collect();
    if !unsupported.is_empty() {
        bail!("unsupported ADD flags: {}", unsupported.join(", "));
    }
    let parts = shell_split(rest);
    if parts.len() < 2 {
        bail!("ADD requires at least a source and destination");
    }

    let dest = parts.last().unwrap().clone();
    let sources = parts[..parts.len() - 1].to_vec();

    Ok(Instruction::Add {
        sources,
        dest,
        checksum,
    })
}

fn ensure_no_extra_from_tokens(parts: &[&str]) -> Result<()> {
    if parts.len() <= 1 {
        return Ok(());
    }
    if parts.len() == 3 && parts[1].eq_ignore_ascii_case("as") {
        return Ok(());
    }
    bail!("invalid FROM syntax: {}", parts.join(" "))
}

fn parse_env(args: &str) -> Result<Instruction> {
    let args = args.trim();
    let parts = shell_split(args);
    if !parts.is_empty() && parts.iter().all(|part| part.contains('=')) {
        let mut entries = Vec::with_capacity(parts.len());
        for part in parts {
            let Some((key, value)) = part.split_once('=') else {
                bail!("invalid ENV entry: {part}");
            };
            entries.push((key.trim().to_string(), value.to_string()));
        }
        return Ok(Instruction::Env { entries });
    }

    let mut parts = args.splitn(2, char::is_whitespace);
    let key = parts.next().unwrap_or("").trim();
    if key.is_empty() {
        bail!("ENV requires a key");
    }
    let value = parts.next().unwrap_or("").trim().to_string();
    Ok(Instruction::Env {
        entries: vec![(key.to_string(), value)],
    })
}

fn parse_arg(args: &str) -> Result<Instruction> {
    let args = args.trim();
    if let Some(eq_pos) = args.find('=') {
        let key = args[..eq_pos].trim().to_string();
        let raw_value = args[eq_pos + 1..].trim();
        let value = raw_value.trim_matches('"').to_string();
        Ok(Instruction::Arg {
            key,
            default: Some(value),
        })
    } else {
        Ok(Instruction::Arg {
            key: args.to_string(),
            default: None,
        })
    }
}

fn parse_label(args: &str) -> Result<Option<Instruction>> {
    let args = args.trim();
    if let Some(eq_pos) = args.find('=') {
        let key = args[..eq_pos].trim().trim_matches('"').to_string();
        let value = args[eq_pos + 1..].trim().trim_matches('"').to_string();
        Ok(Some(Instruction::Label { key, value }))
    } else {
        bail!("unsupported LABEL syntax: expected key=value");
    }
}

fn parse_entrypoint(args: &str) -> Result<Instruction> {
    let args = args.trim();
    let exec = if args.starts_with('[') {
        parse_json_array(args)?
    } else {
        vec!["/bin/sh".to_string(), "-c".to_string(), args.to_string()]
    };
    Ok(Instruction::Entrypoint { exec })
}

fn parse_cmd(args: &str) -> Result<Instruction> {
    let args = args.trim();
    let exec = if args.starts_with('[') {
        parse_json_array(args)?
    } else {
        vec!["/bin/sh".to_string(), "-c".to_string(), args.to_string()]
    };
    Ok(Instruction::Cmd { exec })
}

fn parse_healthcheck(args: &str) -> Result<Instruction> {
    let (flags, rest) = extract_flags(args.trim());
    let mut interval_nanos = None;
    let mut timeout_nanos = None;
    let mut start_period_nanos = None;
    let mut start_interval_nanos = None;
    let mut retries = None;
    let mut unsupported = Vec::new();
    for flag in &flags {
        if let Some(value) = flag.strip_prefix("--interval=") {
            interval_nanos = Some(parse_duration_nanos(value.trim_matches('"'))?);
        } else if let Some(value) = flag.strip_prefix("--timeout=") {
            timeout_nanos = Some(parse_duration_nanos(value.trim_matches('"'))?);
        } else if let Some(value) = flag.strip_prefix("--start-period=") {
            start_period_nanos = Some(parse_duration_nanos(value.trim_matches('"'))?);
        } else if let Some(value) = flag.strip_prefix("--start-interval=") {
            start_interval_nanos = Some(parse_duration_nanos(value.trim_matches('"'))?);
        } else if let Some(value) = flag.strip_prefix("--retries=") {
            retries = Some(
                value
                    .trim_matches('"')
                    .parse::<u32>()
                    .with_context(|| format!("invalid HEALTHCHECK retries value: {value}"))?,
            );
        } else {
            unsupported.push(flag.as_str());
        }
    }
    if !unsupported.is_empty() {
        bail!("unsupported HEALTHCHECK flags: {}", unsupported.join(", "));
    }

    let rest = rest.trim();
    if rest.eq_ignore_ascii_case("NONE") {
        return Ok(Instruction::Healthcheck {
            test: None,
            interval_nanos,
            timeout_nanos,
            start_period_nanos,
            start_interval_nanos,
            retries,
        });
    }

    let command = rest
        .strip_prefix("CMD")
        .or_else(|| rest.strip_prefix("cmd"))
        .map(str::trim_start)
        .ok_or_else(|| anyhow!("HEALTHCHECK requires CMD or NONE"))?;
    ensure!(!command.is_empty(), "HEALTHCHECK CMD requires a command");
    let test = if command.starts_with('[') {
        let mut exec = vec!["CMD".to_string()];
        exec.extend(parse_json_array(command)?);
        exec
    } else {
        vec!["CMD-SHELL".to_string(), command.to_string()]
    };

    Ok(Instruction::Healthcheck {
        test: Some(test),
        interval_nanos,
        timeout_nanos,
        start_period_nanos,
        start_interval_nanos,
        retries,
    })
}

fn parse_volume(args: &str) -> Result<Instruction> {
    let args = args.trim();
    let paths = if args.starts_with('[') {
        parse_json_array(args)?
    } else {
        args.split_whitespace().map(String::from).collect()
    };
    Ok(Instruction::Volume { paths })
}

fn parse_shell(args: &str) -> Result<Instruction> {
    let args = args.trim();
    let exec = parse_json_array(args)?;
    Ok(Instruction::Shell { exec })
}

fn parse_json_array(input: &str) -> Result<Vec<String>> {
    let input = input.trim();
    if !input.starts_with('[') || !input.ends_with(']') {
        bail!("expected JSON array, got: {input}");
    }
    let inner = &input[1..input.len() - 1];
    let mut items = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut escaped = false;

    for ch in inner.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            ',' if !in_string => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    items.push(trimmed);
                }
                current.clear();
            }
            _ if in_string => current.push(ch),
            _ => {}
        }
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        items.push(trimmed);
    }
    Ok(items)
}

fn parse_duration_nanos(raw: &str) -> Result<u64> {
    let mut rest = raw.trim();
    ensure!(!rest.is_empty(), "duration value must not be empty");
    let mut total = 0u128;

    while !rest.is_empty() {
        let number_end = rest
            .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
            .ok_or_else(|| anyhow!("invalid duration segment: {rest}"))?;
        ensure!(number_end > 0, "invalid duration segment: {rest}");
        let value = &rest[..number_end];
        let unit = ["ns", "us", "µs", "ms", "s", "m", "h"]
            .into_iter()
            .find(|unit| rest[number_end..].starts_with(unit))
            .ok_or_else(|| anyhow!("invalid duration unit in {raw}"))?;
        total = total
            .checked_add(parse_duration_component_nanos(value, unit_nanos(unit))?)
            .ok_or_else(|| anyhow!("duration is too large: {raw}"))?;
        rest = &rest[number_end + unit.len()..];
    }

    u64::try_from(total).map_err(|_| anyhow!("duration is too large: {raw}"))
}

fn parse_duration_component_nanos(value: &str, unit_nanos: u128) -> Result<u128> {
    let (whole, fractional) = match value.split_once('.') {
        Some(parts) => parts,
        None => (value, ""),
    };
    ensure!(
        !whole.is_empty() || !fractional.is_empty(),
        "invalid duration value: {value}"
    );
    let whole_nanos = if whole.is_empty() {
        0
    } else {
        whole
            .parse::<u128>()
            .with_context(|| format!("invalid duration value: {value}"))?
            .checked_mul(unit_nanos)
            .ok_or_else(|| anyhow!("duration component is too large: {value}"))?
    };
    if fractional.is_empty() {
        return Ok(whole_nanos);
    }
    let fractional_value = fractional
        .parse::<u128>()
        .with_context(|| format!("invalid duration value: {value}"))?;
    let scale = 10u128
        .checked_pow(fractional.len() as u32)
        .ok_or_else(|| anyhow!("duration precision is too large: {value}"))?;
    Ok(whole_nanos + (fractional_value * unit_nanos) / scale)
}

fn unit_nanos(unit: &str) -> u128 {
    match unit {
        "ns" => 1,
        "us" | "µs" => 1_000,
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        "m" => 60 * 1_000_000_000,
        "h" => 60 * 60 * 1_000_000_000,
        _ => unreachable!("duration unit should be validated before conversion"),
    }
}

fn extract_flags(args: &str) -> (Vec<String>, &str) {
    let mut flags = Vec::new();
    let mut chars = args.char_indices().peekable();

    loop {
        while chars.peek().is_some_and(|(_, c)| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_some_and(|(_, c)| *c == '-') {
            let (start, _) = *chars.peek().unwrap();
            let mut end = start;
            for (i, c) in chars.by_ref() {
                end = i + c.len_utf8();
                if c.is_whitespace() {
                    break;
                }
            }
            let flag = args[start..end].trim().to_string();
            if flag.starts_with("--") {
                flags.push(flag);
                continue;
            }
        }
        let rest_start = chars.peek().map(|(i, _)| *i).unwrap_or(args.len());
        return (flags, args[rest_start..].trim());
    }
}

fn shell_split(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let mut quote_char = '"';

    for ch in input.chars() {
        if in_quote {
            if ch == quote_char {
                in_quote = false;
            } else {
                current.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            in_quote = true;
            quote_char = ch;
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_dockerfile() {
        let content = r#"
FROM alpine:3.19
RUN echo hello
COPY . /app
WORKDIR /app
EXPOSE 8080
CMD ["./app"]
"#;
        let instructions = parse(content).unwrap();
        assert_eq!(instructions.len(), 6);
        assert!(
            matches!(&instructions[0], Instruction::From { image, platform: None, .. } if image == "alpine:3.19")
        );
        assert!(
            matches!(&instructions[1], Instruction::Run { command, exec: None, mounts } if command == "echo hello" && mounts.is_empty())
        );
        assert!(matches!(&instructions[4], Instruction::Expose { port } if port == "8080"));
    }

    #[test]
    fn handles_line_continuations() {
        let content = "FROM alpine\nRUN echo hello && \\\n    echo world\n";
        let instructions = parse(content).unwrap();
        assert_eq!(instructions.len(), 2);
        if let Instruction::Run {
            command,
            exec: None,
            mounts,
        } = &instructions[1]
        {
            assert!(command.contains("echo hello"));
            assert!(command.contains("echo world"));
            assert!(mounts.is_empty());
        } else {
            panic!("expected RUN");
        }
    }

    #[test]
    fn handles_comments_and_blank_lines() {
        let content = "# comment\n\nFROM alpine\n# another comment\nRUN echo hi\n";
        let instructions = parse(content).unwrap();
        assert_eq!(instructions.len(), 2);
    }

    #[test]
    fn line_number_tracks_continuation_start() {
        let content = "FROM alpine\nCOPY --bad \\\n  foo /app\n";
        let error = parse(content).unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("Dockerfile line 2"));
        assert!(rendered.contains("unsupported COPY flags: --bad"));
    }

    #[test]
    fn parses_multi_stage() {
        let content = r#"
FROM golang:1.22 AS builder
RUN go build -o /app
FROM alpine:3.19
COPY --from=builder /app /app
CMD ["/app"]
"#;
        let instructions = parse(content).unwrap();
        assert_eq!(instructions.len(), 5);
        assert!(
            matches!(&instructions[0], Instruction::From { alias: Some(a), platform: None, .. } if a == "builder")
        );
        assert!(
            matches!(&instructions[3], Instruction::Copy { from: Some(f), .. } if f == "builder")
        );
    }

    #[test]
    fn parses_from_platform_flag() {
        let content = "FROM --platform=linux/arm64 alpine:3.19 AS base\n";
        let instructions = parse(content).unwrap();
        assert!(matches!(
            &instructions[0],
            Instruction::From {
                image,
                alias: Some(alias),
                platform: Some(platform),
            } if image == "alpine:3.19" && alias == "base" && platform == "linux/arm64"
        ));
    }

    #[test]
    fn parses_json_exec_forms() {
        let content = r#"
FROM alpine
RUN ["echo", "$HOME", "hello world"]
ENTRYPOINT ["python3", "-m", "http.server"]
CMD ["8080"]
"#;
        let instructions = parse(content).unwrap();
        if let Instruction::Run {
            command,
            exec: Some(exec),
            mounts,
        } = &instructions[1]
        {
            assert_eq!(command, "echo '$HOME' 'hello world'");
            assert_eq!(exec, &["echo", "$HOME", "hello world"]);
            assert!(mounts.is_empty());
        } else {
            panic!("expected RUN");
        }
        if let Instruction::Entrypoint { exec } = &instructions[2] {
            assert_eq!(exec, &["python3", "-m", "http.server"]);
        } else {
            panic!("expected ENTRYPOINT");
        }
        if let Instruction::Cmd { exec } = &instructions[3] {
            assert_eq!(exec, &["8080"]);
        } else {
            panic!("expected CMD");
        }
    }

    #[test]
    fn parses_env_both_forms() {
        let content = "FROM alpine\nENV FOO=bar\nENV BAZ world\n";
        let instructions = parse(content).unwrap();
        assert!(
            matches!(&instructions[1], Instruction::Env { entries } if entries == &vec![("FOO".to_string(), "bar".to_string())])
        );
        assert!(
            matches!(&instructions[2], Instruction::Env { entries } if entries == &vec![("BAZ".to_string(), "world".to_string())])
        );
    }

    #[test]
    fn parses_env_multiple_key_value_pairs() {
        let content = "FROM alpine\nENV FOO=bar BAZ=\"hello world\" EMPTY=\n";
        let instructions = parse(content).unwrap();
        assert!(matches!(
            &instructions[1],
            Instruction::Env { entries }
                if entries
                    == &vec![
                        ("FOO".to_string(), "bar".to_string()),
                        ("BAZ".to_string(), "hello world".to_string()),
                        ("EMPTY".to_string(), "".to_string()),
                    ]
        ));
    }

    #[test]
    fn parses_arg_with_and_without_default() {
        let content = "FROM alpine\nARG VERSION\nARG GOVERSION=1.22\n";
        let instructions = parse(content).unwrap();
        assert!(
            matches!(&instructions[1], Instruction::Arg { key, default: None } if key == "VERSION")
        );
        assert!(
            matches!(&instructions[2], Instruction::Arg { key, default: Some(v) } if key == "GOVERSION" && v == "1.22")
        );
    }

    #[test]
    fn parses_copy_with_multiple_sources() {
        let content = "FROM alpine\nCOPY package.json yarn.lock /app/\n";
        let instructions = parse(content).unwrap();
        if let Instruction::Copy {
            from,
            exclude,
            parents,
            chown,
            chmod,
            sources,
            dest,
        } = &instructions[1]
        {
            assert!(from.is_none());
            assert!(exclude.is_empty());
            assert!(!parents);
            assert!(chown.is_none());
            assert!(chmod.is_none());
            assert_eq!(sources, &["package.json", "yarn.lock"]);
            assert_eq!(dest, "/app/");
        } else {
            panic!("expected COPY");
        }
    }

    #[test]
    fn parses_copy_chown_and_chmod_flags() {
        let content = "FROM alpine\nCOPY --chown=1000:1001 --chmod=755 app /usr/local/bin/app\n";
        let instructions = parse(content).unwrap();
        assert!(matches!(
            &instructions[1],
            Instruction::Copy {
                from: None,
                exclude,
                parents,
                chown: Some(chown),
                chmod: Some(chmod),
                sources,
                dest,
            } if chown == "1000:1001"
                && exclude.is_empty()
                && !parents
                && chmod == "755"
                && sources == &["app".to_string()]
                && dest == "/usr/local/bin/app"
        ));
    }

    #[test]
    fn parses_copy_exclude_flags() {
        let content = "FROM alpine\nCOPY --exclude=*.map --exclude=app/**/*.tmp app /workspace/\n";
        let instructions = parse(content).unwrap();
        assert!(matches!(
            &instructions[1],
            Instruction::Copy {
                from: None,
                sources,
                dest,
                exclude,
                parents,
                ..
            } if sources == &["app".to_string()]
                && dest == "/workspace/"
                && !parents
                && exclude == &["*.map".to_string(), "app/**/*.tmp".to_string()]
        ));
    }

    #[test]
    fn parses_copy_parents_flag() {
        let content = "FROM alpine\nCOPY --parents src/./lib/app.rb /workspace/\n";
        let instructions = parse(content).unwrap();
        assert!(matches!(
            &instructions[1],
            Instruction::Copy {
                from: None,
                sources,
                dest,
                parents,
                ..
            } if sources == &["src/./lib/app.rb".to_string()]
                && dest == "/workspace/"
                && *parents
        ));
    }

    #[test]
    fn parses_healthcheck_shell_and_none_forms() {
        let shell = parse(
            "FROM alpine\nHEALTHCHECK --interval=30s --timeout=1.5s --retries=3 CMD curl -f http://localhost || exit 1\n",
        )
        .unwrap();
        assert!(matches!(
            &shell[1],
            Instruction::Healthcheck {
                test: Some(test),
                interval_nanos: Some(interval),
                timeout_nanos: Some(timeout),
                start_period_nanos: None,
                start_interval_nanos: None,
                retries: Some(retries),
            } if test == &vec![
                    "CMD-SHELL".to_string(),
                    "curl -f http://localhost || exit 1".to_string()
                ]
                && *interval == 30_000_000_000
                && *timeout == 1_500_000_000
                && *retries == 3
        ));

        let none = parse("FROM alpine\nHEALTHCHECK NONE\n").unwrap();
        assert!(matches!(
            &none[1],
            Instruction::Healthcheck {
                test: None,
                interval_nanos: None,
                timeout_nanos: None,
                start_period_nanos: None,
                start_interval_nanos: None,
                retries: None,
            }
        ));
    }

    #[test]
    fn parses_add_checksum_flag() {
        let content =
            "FROM alpine\nADD --checksum=sha256:deadbeef https://example.com/app.tgz /tmp/\n";
        let instructions = parse(content).unwrap();
        assert!(matches!(
            &instructions[1],
            Instruction::Add {
                sources,
                dest,
                checksum: Some(checksum),
            } if sources == &["https://example.com/app.tgz".to_string()]
                && dest == "/tmp/"
                && checksum == "sha256:deadbeef"
        ));
    }

    #[test]
    fn parses_label_and_volume() {
        let content = "FROM alpine\nLABEL version=\"1.0\"\nVOLUME [\"/data\", \"/logs\"]\n";
        let instructions = parse(content).unwrap();
        assert!(
            matches!(&instructions[1], Instruction::Label { key, value } if key == "version" && value == "1.0")
        );
        if let Instruction::Volume { paths } = &instructions[2] {
            assert_eq!(paths, &["/data", "/logs"]);
        } else {
            panic!("expected VOLUME");
        }
    }

    #[test]
    fn rejects_unsupported_maintainer() {
        let maintainer = parse("FROM alpine\nMAINTAINER test\n").unwrap_err();
        let maintainer_rendered = format!("{maintainer:#}");
        assert!(maintainer_rendered.contains("Dockerfile line 2"));
        assert!(maintainer_rendered.contains("MAINTAINER is not supported yet"));
    }

    #[test]
    fn rejects_invalid_label_syntax_with_line_number() {
        let error = parse("FROM alpine\nLABEL maintainer\n").unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("Dockerfile line 2"));
        assert!(rendered.contains("unsupported LABEL syntax"));
    }

    #[test]
    fn parses_run_mount_cache_and_bind_flags() {
        let content = r#"
FROM alpine
RUN --mount=type=cache,id=apt-cache,target=/var/cache/apt,sharing=locked \
    --mount=type=bind,from=builder,source=/out,target=./vendor,rw \
    apt-get update
"#;
        let instructions = parse(content).unwrap();
        assert!(matches!(
            &instructions[1],
            Instruction::Run { command, exec: None, mounts }
                if command == "apt-get update"
                    && mounts == &vec![
                        RunMount::Cache {
                            target: "/var/cache/apt".to_string(),
                            id: Some("apt-cache".to_string()),
                            readonly: false,
                            sharing: CacheMode::Locked,
                        },
                        RunMount::Bind {
                            target: "./vendor".to_string(),
                            source: Some("/out".to_string()),
                            from: Some("builder".to_string()),
                            readonly: false,
                        },
                    ]
        ));
    }

    #[test]
    fn parses_run_mount_tmpfs_secret_and_ssh_flags() {
        let content = r#"
FROM alpine
RUN --mount=type=tmpfs,target=/tmp/build,size=64m \
    --mount=type=secret,id=npmrc,target=/root/.npmrc,env=NPMRC,required=true,mode=0400,uid=0,gid=0 \
    --mount=type=ssh,id=git,target=/run/ssh-agent,required=true \
    echo hi
"#;
        let instructions = parse(content).unwrap();
        assert!(matches!(
            &instructions[1],
            Instruction::Run { command, exec: None, mounts }
                if command == "echo hi"
                    && mounts == &vec![
                        RunMount::Tmpfs {
                            target: "/tmp/build".to_string(),
                            size: Some("64m".to_string()),
                        },
                        RunMount::Secret {
                            target: "/root/.npmrc".to_string(),
                            id: "npmrc".to_string(),
                            env: Some("NPMRC".to_string()),
                            required: true,
                            mode: Some(0o400),
                            uid: Some(0),
                            gid: Some(0),
                        },
                        RunMount::Ssh {
                            target: "/run/ssh-agent".to_string(),
                            id: "git".to_string(),
                            required: true,
                            mode: None,
                            uid: None,
                            gid: None,
                        },
                    ]
        ));
    }
}
