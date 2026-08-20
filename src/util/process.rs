use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

#[derive(Debug)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub status: ExitStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamSource {
    Stdout,
    Stderr,
}

pub type StreamObserver = Arc<dyn for<'a> Fn(StreamSource, &'a str) + Send + Sync + 'static>;

pub fn find_command(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for entry in std::env::split_paths(&path) {
        let candidate = entry.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn run_capture(program: &Path, args: &[String]) -> Result<CommandOutput> {
    run_capture_with_env(program, args, &[])
}

pub fn run_capture_with_env(
    program: &Path,
    args: &[String],
    env: &[(&str, &OsStr)],
) -> Result<CommandOutput> {
    let output = run_command_output(program, args, env)
        .with_context(|| format!("failed to run {}", display_command(program, args)))?;

    Ok(CommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        status: output.status,
    })
}

pub fn run_checked(program: &Path, args: &[String]) -> Result<()> {
    let output = run_capture(program, args)?;
    if output.status.success() {
        return Ok(());
    }

    bail!(
        "{} failed with status {}{}{}",
        display_command(program, args),
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

pub fn run_streaming(program: &Path, args: &[String]) -> Result<()> {
    let status = run_command_status(program, args, |command| {
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
    })
    .with_context(|| format!("failed to run {}", display_command(program, args)))?;

    if status.success() {
        return Ok(());
    }

    Err(anyhow!(
        "{} failed with status {}",
        display_command(program, args),
        status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string())
    ))
}

pub fn run_streaming_prefixed(
    program: &Path,
    args: &[String],
    stdout_prefix: &str,
    stderr_prefix: &str,
) -> Result<()> {
    run_streaming_prefixed_passthrough(program, args, stdout_prefix, stderr_prefix, None)
}

pub fn run_streaming_prefixed_passthrough(
    program: &Path,
    args: &[String],
    stdout_prefix: &str,
    stderr_prefix: &str,
    passthrough_prefix: Option<&str>,
) -> Result<()> {
    run_streaming_prefixed_passthrough_with_observer(
        program,
        args,
        stdout_prefix,
        stderr_prefix,
        passthrough_prefix,
        None,
    )
}

pub fn run_streaming_prefixed_passthrough_with_observer(
    program: &Path,
    args: &[String],
    stdout_prefix: &str,
    stderr_prefix: &str,
    passthrough_prefix: Option<&str>,
    observer: Option<StreamObserver>,
) -> Result<()> {
    let mut child = spawn_command(program, args, |command| {
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    })
    .with_context(|| format!("failed to run {}", display_command(program, args)))?;

    let stdout = child.stdout.take().ok_or_else(|| {
        anyhow!(
            "failed to capture stdout for {}",
            display_command(program, args)
        )
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        anyhow!(
            "failed to capture stderr for {}",
            display_command(program, args)
        )
    })?;

    let stdout_prefix = stdout_prefix.to_string();
    let stderr_prefix = stderr_prefix.to_string();
    let stdout_passthrough = passthrough_prefix.map(str::to_owned);
    let stderr_passthrough = passthrough_prefix.map(str::to_owned);
    let stdout_observer = observer.clone();
    let stderr_observer = observer;
    let stdout_thread = thread::spawn(move || {
        stream_with_prefix(
            stdout,
            &stdout_prefix,
            false,
            stdout_passthrough.as_deref(),
            stdout_observer,
            StreamSource::Stdout,
        )
    });
    let stderr_thread = thread::spawn(move || {
        stream_with_prefix(
            stderr,
            &stderr_prefix,
            true,
            stderr_passthrough.as_deref(),
            stderr_observer,
            StreamSource::Stderr,
        )
    });

    let status = child
        .wait()
        .with_context(|| format!("failed to wait for {}", display_command(program, args)))?;

    stdout_thread
        .join()
        .map_err(|_| anyhow!("stdout streaming thread panicked"))??;
    stderr_thread
        .join()
        .map_err(|_| anyhow!("stderr streaming thread panicked"))??;

    if status.success() {
        return Ok(());
    }

    Err(anyhow!(
        "{} failed with status {}",
        display_command(program, args),
        status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string())
    ))
}

/// True if `name` is a valid env-var name that looks secret-bearing.
/// Used to redact values when rendering commands for logs/errors.
fn env_name_is_secret(name: &str) -> bool {
    if name.is_empty()
        || !name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return false;
    }
    let upper = name.to_ascii_uppercase();
    ["TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL", "KEY"]
        .iter()
        .any(|marker| upper.contains(marker))
}

/// Mask the value of a `NAME=VALUE` arg when NAME looks like a secret, so that
/// `env -i NAME=VALUE ...` invocations surfaced in error messages don't leak
/// forwarded tokens (BoringCache tokens, RAILS_MASTER_KEY, etc.) into logs.
fn redact_command_arg(arg: &str) -> String {
    match arg.split_once('=') {
        Some((name, value)) if !value.is_empty() && env_name_is_secret(name) => {
            format!("{name}=***REDACTED***")
        }
        _ => arg.to_string(),
    }
}

pub fn display_command(program: &Path, args: &[String]) -> String {
    let rendered_args = args
        .iter()
        .map(|arg| shell_words::quote(&redact_command_arg(arg)).into_owned())
        .collect::<Vec<_>>()
        .join(" ");

    if rendered_args.is_empty() {
        program.display().to_string()
    } else {
        format!("{} {}", program.display(), rendered_args)
    }
}

fn run_command_output(
    program: &Path,
    args: &[String],
    env: &[(&str, &OsStr)],
) -> std::io::Result<std::process::Output> {
    with_exec_busy_retry(|| {
        Command::new(program)
            .args(args)
            .envs(env.iter().copied())
            .output()
    })
}

fn run_command_status<F>(
    program: &Path,
    args: &[String],
    configure: F,
) -> std::io::Result<ExitStatus>
where
    F: Fn(&mut Command),
{
    with_exec_busy_retry(|| {
        let mut command = Command::new(program);
        command.args(args);
        configure(&mut command);
        command.status()
    })
}

fn spawn_command<F>(
    program: &Path,
    args: &[String],
    configure: F,
) -> std::io::Result<std::process::Child>
where
    F: Fn(&mut Command),
{
    with_exec_busy_retry(|| {
        let mut command = Command::new(program);
        command.args(args);
        configure(&mut command);
        command.spawn()
    })
}

fn with_exec_busy_retry<T, F>(mut action: F) -> std::io::Result<T>
where
    F: FnMut() -> std::io::Result<T>,
{
    let mut attempt = 0u32;
    loop {
        match action() {
            Ok(value) => return Ok(value),
            Err(err) if should_retry_exec_busy(&err) && attempt < exec_busy_retry_attempts() => {
                thread::sleep(exec_busy_retry_delay(attempt));
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

fn should_retry_exec_busy(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(26))
}

fn exec_busy_retry_attempts() -> u32 {
    6
}

fn exec_busy_retry_delay(attempt: u32) -> Duration {
    Duration::from_millis(25 * (attempt as u64 + 1))
}

fn render_stream_line(line: &str, prefix: &str, passthrough_prefix: Option<&str>) -> String {
    if let Some(passthrough_prefix) = passthrough_prefix
        && line.starts_with(passthrough_prefix)
    {
        return line.to_string();
    }
    format!("{prefix} {line}")
}

fn stream_with_prefix<R: std::io::Read>(
    reader: R,
    prefix: &str,
    stderr: bool,
    passthrough_prefix: Option<&str>,
    observer: Option<StreamObserver>,
    stream: StreamSource,
) -> Result<()> {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();

    loop {
        line.clear();
        let bytes = reader.read_until(b'\n', &mut line)?;
        if bytes == 0 {
            break;
        }

        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        let observed = String::from_utf8_lossy(&line).into_owned();
        if let Some(observer) = observer.as_ref() {
            observer(stream, &observed);
        }
        let rendered = render_stream_line(&observed, prefix, passthrough_prefix);
        if stderr {
            let mut handle = std::io::stderr().lock();
            writeln!(handle, "{rendered}")?;
            handle.flush()?;
        } else {
            let mut handle = std::io::stdout().lock();
            writeln!(handle, "{rendered}")?;
            handle.flush()?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        StreamSource, display_command, render_stream_line, should_retry_exec_busy,
        with_exec_busy_retry,
    };

    #[test]
    fn display_command_quotes_args() {
        let rendered = display_command(
            Path::new("/bin/echo"),
            &["hello world".to_string(), "plain".to_string()],
        );
        assert_eq!(rendered, "/bin/echo 'hello world' plain");
    }

    #[test]
    fn display_command_redacts_secret_env_values() {
        let rendered = display_command(
            Path::new("/usr/bin/env"),
            &[
                "-i".to_string(),
                "BORINGCACHE_SAVE_TOKEN=bc_wrk_abc:def".to_string(),
                "RAILS_MASTER_KEY=deadbeefdeadbeef".to_string(),
                "AWS_SECRET_ACCESS_KEY=topsecret".to_string(),
                "RAILS_ENV=test".to_string(),
            ],
        );
        // Secret-named values are masked; their plaintext never appears.
        assert!(rendered.contains("BORINGCACHE_SAVE_TOKEN=***REDACTED***"));
        assert!(rendered.contains("RAILS_MASTER_KEY=***REDACTED***"));
        assert!(rendered.contains("AWS_SECRET_ACCESS_KEY=***REDACTED***"));
        assert!(!rendered.contains("bc_wrk_abc"));
        assert!(!rendered.contains("deadbeefdeadbeef"));
        assert!(!rendered.contains("topsecret"));
        // Non-secret env is preserved verbatim.
        assert!(rendered.contains("RAILS_ENV=test"));
    }

    #[test]
    fn render_stream_line_can_passthrough_event_payloads() {
        let line = "\u{1e}bbui\u{1f}step\u{1f}1\u{1f}2\u{1f}build\u{1f}0";
        assert_eq!(
            render_stream_line(line, " build |", Some("\u{1e}bbui")),
            "\u{1e}bbui\u{1f}step\u{1f}1\u{1f}2\u{1f}build\u{1f}0"
        );
        assert_eq!(
            render_stream_line("plain line", " build |", Some("\u{1e}bbui")),
            " build | plain line"
        );
    }

    #[test]
    fn stream_source_is_copy_and_equatable() {
        assert_eq!(StreamSource::Stdout, StreamSource::Stdout);
        assert_ne!(StreamSource::Stdout, StreamSource::Stderr);
    }

    #[test]
    fn retries_executable_file_busy_errors() {
        let mut attempts = 0;
        let result = with_exec_busy_retry(|| {
            attempts += 1;
            if attempts < 3 {
                Err(std::io::Error::from_raw_os_error(26))
            } else {
                Ok("started")
            }
        })
        .unwrap();

        assert_eq!(result, "started");
        assert_eq!(attempts, 3);
        assert!(should_retry_exec_busy(&std::io::Error::from_raw_os_error(
            26
        )));
    }
}
