use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, BufReader as TokioBufReader};
use tracing::{info, warn};

const ALLOWED: &[&str] = &["takopack", "osc", "git"];

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub timeout: Duration,
    pub log_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub returncode: i32,
    pub log_path: PathBuf,
}

#[derive(Debug)]
struct StreamLine {
    is_stderr: bool,
    line: String,
}

// ---------------------------------------------------------------------------
// Async version (preferred for async contexts)
// ---------------------------------------------------------------------------

pub async fn run_command(spec: CommandSpec) -> Result<CommandResult> {
    ensure_allowed_command(&spec.program)?;

    if let Some(parent) = spec.log_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let cmdline = if spec.args.is_empty() {
        spec.program.display().to_string()
    } else {
        format!("{} {}", spec.program.display(), spec.args.join(" "))
    };
    let cwd = spec
        .cwd
        .as_deref()
        .unwrap_or_else(|| Path::new("."))
        .display()
        .to_string();

    info!(
        command = %cmdline,
        cwd = %cwd,
        timeout_s = spec.timeout.as_secs(),
        log_path = %spec.log_path.display(),
        "command started (full cmdline in log file)"
    );

    let started = Instant::now();
    let started_at = SystemTime::now();

    async_write_log_header(&spec, started_at).await?;

    let mut child = tokio::process::Command::new(&spec.program)
        .args(&spec.args)
        .current_dir(spec.cwd.as_deref().unwrap_or_else(|| Path::new(".")))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Create a new process group with pgid == child pid.
        // Sub-processes forked by the child inherit this pgid, so killpg
        // can terminate the entire tree.
        .process_group(0)
        .spawn()
        .with_context(|| format!("failed to start {:?}", spec.program))?;

    // After process_group(0), child.id() == pgid of the new group.
    let child_pid = child.id().context("child has no pid")? as i32;

    let child_stdout = child
        .stdout
        .take()
        .context("failed to capture child stdout")?;
    let child_stderr = child
        .stderr
        .take()
        .context("failed to capture child stderr")?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamLine>(1024);

    let mut last_output_at = Instant::now();
    let mut last_output_line: Option<String> = None;
    let mut aggregated_stdout = String::new();
    let mut aggregated_stderr = String::new();

    // Spawn reader tasks for stdout and stderr
    let tx_out = tx.clone();
    let stdout_task = tokio::spawn(async move {
        let mut lines = TokioBufReader::new(child_stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = tx_out
                .send(StreamLine {
                    is_stderr: false,
                    line,
                })
                .await;
        }
    });

    let tx_err = tx;
    let stderr_task = tokio::spawn(async move {
        let mut lines = TokioBufReader::new(child_stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = tx_err
                .send(StreamLine {
                    is_stderr: true,
                    line,
                })
                .await;
        }
    });

    // Absolute deadline — must be outside the loop so it is NOT reset on each
    // iteration.  `tokio::pin!` makes it usable as a `&mut` inside `select!`.
    let deadline = tokio::time::sleep(spec.timeout);
    tokio::pin!(deadline);

    let mut timed_out = false;
    let mut exit_status: Option<std::process::ExitStatus> = None;
    let mut last_progress_log = Instant::now();

    // ── Main collection loop ────────────────────────────────────────────────
    // Collects stdout/stderr lines, waits for child exit, fires timeout.
    loop {
        tokio::select! {
            item = rx.recv() => {
                match item {
                    Some(sl) => {
                        if sl.is_stderr {
                            aggregated_stderr.push_str(&sl.line);
                            aggregated_stderr.push('\n');
                        } else {
                            aggregated_stdout.push_str(&sl.line);
                            aggregated_stdout.push('\n');
                        }
                        last_output_at = Instant::now();
                        last_output_line = Some(sl.line);
                    }
                    None => break, // all readers done
                }
            }
            status = child.wait(), if exit_status.is_none() => {
                if let Err(e) = status {
                    return Err(e.into());
                }
                exit_status = status.ok();
                // Keep looping — readers may still have buffered lines.
            }
            _ = &mut deadline => {
                timed_out = true;
                warn!(
                    command = %cmdline,
                    pid = child_pid,
                    elapsed_s = started.elapsed().as_secs(),
                    "command timed out; sending kill to process group"
                );
                // Kill the entire process group (child + all descendants).
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(child_pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
                // Reap the child so it doesn't become a zombie.
                let _ = child.wait().await;
                break;
            }
            _ = tokio::time::sleep(Duration::from_secs(30)),
                if last_progress_log.elapsed() >= Duration::from_secs(30) =>
            {
                let idle_s = last_output_at.elapsed().as_secs();
                let last_line = last_output_line.as_deref().unwrap_or("<no output>");
                let truncated = if last_line.len() > 200 {
                    format!("{}...", &last_line[..200])
                } else {
                    last_line.to_string()
                };
                info!(
                    command = %cmdline,
                    elapsed_s = started.elapsed().as_secs(),
                    idle_output_s = idle_s,
                    last_output = %truncated,
                    log_path = %spec.log_path.display(),
                    "command still running (full output in log file)"
                );
                last_progress_log = Instant::now();
            }
        }
    }

    // ── Post-loop drain ─────────────────────────────────────────────────────
    // After timeout or normal exit, keep draining the channel so reader tasks
    // are never blocked on a full mpsc buffer.  This preserves as much output
    // as possible and guarantees the tasks can exit cleanly.
    if timed_out {
        // Readers are still alive; drain until they finish (pipe EOF after kill).
        loop {
            let tasks_finished = stdout_task.is_finished() && stderr_task.is_finished();
            if tasks_finished {
                // One last drain for anything buffered
                while let Ok(sl) = rx.try_recv() {
                    if sl.is_stderr {
                        aggregated_stderr.push_str(&sl.line);
                        aggregated_stderr.push('\n');
                    } else {
                        aggregated_stdout.push_str(&sl.line);
                        aggregated_stdout.push('\n');
                    }
                }
                break;
            }
            match rx.recv().await {
                Some(sl) => {
                    if sl.is_stderr {
                        aggregated_stderr.push_str(&sl.line);
                        aggregated_stderr.push('\n');
                    } else {
                        aggregated_stdout.push_str(&sl.line);
                        aggregated_stdout.push('\n');
                    }
                }
                None => break,
            }
        }
    }

    // Ensure we have the exit status (may have been missed if channel closed first)
    if exit_status.is_none()
        && let Ok(s) = child.wait().await
    {
        exit_status = Some(s);
    }

    // Wait for reader tasks to finish
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    let returncode = if timed_out {
        if !aggregated_stderr.is_empty() && !aggregated_stderr.ends_with('\n') {
            aggregated_stderr.push('\n');
        }
        aggregated_stderr.push_str(&format!("TIMEOUT after {}s", spec.timeout.as_secs()));
        warn!(
            command = %cmdline,
            log_path = %spec.log_path.display(),
            "command timed out"
        );
        -1
    } else {
        exit_status.and_then(|s| s.code()).unwrap_or(-1)
    };

    let finished_at = SystemTime::now();
    async_append_log_result(
        &spec,
        &aggregated_stdout,
        &aggregated_stderr,
        returncode,
        finished_at,
        started.elapsed(),
    )
    .await?;
    info!(
        command = %cmdline,
        returncode,
        elapsed_s = started.elapsed().as_secs(),
        log_path = %spec.log_path.display(),
        "command finished (full output in log file)"
    );
    Ok(CommandResult {
        stdout: aggregated_stdout,
        stderr: aggregated_stderr,
        returncode,
        log_path: spec.log_path,
    })
}

// ---------------------------------------------------------------------------
// Blocking version (for non-async callers only)
// ---------------------------------------------------------------------------

fn spawn_reader<R: std::io::Read + Send + 'static>(
    reader: R,
    is_stderr: bool,
    tx: mpsc::Sender<StreamLine>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut br = BufReader::new(reader);
        let mut buf = String::new();
        loop {
            buf.clear();
            let read = match br.read_line(&mut buf) {
                Ok(n) => n,
                Err(_) => break,
            };
            if read == 0 {
                break;
            }
            while buf.ends_with('\n') || buf.ends_with('\r') {
                buf.pop();
            }
            let _ = tx.send(StreamLine {
                is_stderr,
                line: buf.clone(),
            });
        }
    })
}

fn drain_stream_lines(
    rx: &mpsc::Receiver<StreamLine>,
    stdout: &mut String,
    stderr: &mut String,
    last_output_at: &mut Instant,
    last_output_line: &mut Option<String>,
) {
    while let Ok(item) = rx.try_recv() {
        if item.is_stderr {
            stderr.push_str(&item.line);
            stderr.push('\n');
        } else {
            stdout.push_str(&item.line);
            stdout.push('\n');
        }
        *last_output_at = Instant::now();
        *last_output_line = Some(item.line);
    }
}

pub fn run_command_blocking(spec: CommandSpec) -> Result<CommandResult> {
    ensure_allowed_command(&spec.program)?;

    if let Some(parent) = spec.log_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let cmdline = if spec.args.is_empty() {
        spec.program.display().to_string()
    } else {
        format!("{} {}", spec.program.display(), spec.args.join(" "))
    };
    let cwd = spec
        .cwd
        .as_deref()
        .unwrap_or_else(|| Path::new("."))
        .display()
        .to_string();

    info!(
        command = %cmdline,
        cwd = %cwd,
        timeout_s = spec.timeout.as_secs(),
        log_path = %spec.log_path.display(),
        "command started (full cmdline in log file)"
    );

    let started = Instant::now();
    let started_at = SystemTime::now();
    let mut last_progress_log = Instant::now();
    let mut last_output_at = started;
    let mut last_output_line: Option<String> = None;
    let mut aggregated_stdout = String::new();
    let mut aggregated_stderr = String::new();
    let mut child = Command::new(&spec.program)
        .args(&spec.args)
        .current_dir(spec.cwd.as_deref().unwrap_or_else(|| Path::new(".")))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start {:?}", spec.program))?;

    write_log_header(&spec, started_at)?;

    let child_stdout = child
        .stdout
        .take()
        .context("failed to capture child stdout")?;
    let child_stderr = child
        .stderr
        .take()
        .context("failed to capture child stderr")?;
    let (tx, rx) = mpsc::channel::<StreamLine>();
    let stdout_handle = spawn_reader(child_stdout, false, tx.clone());
    let stderr_handle = spawn_reader(child_stderr, true, tx);

    let mut timed_out = false;
    let final_status: std::process::ExitStatus;

    loop {
        drain_stream_lines(
            &rx,
            &mut aggregated_stdout,
            &mut aggregated_stderr,
            &mut last_output_at,
            &mut last_output_line,
        );

        if started.elapsed() > spec.timeout {
            warn!(
                command = %cmdline,
                elapsed_s = started.elapsed().as_secs(),
                "command timed out; sending kill"
            );
            let _ = child.kill();
            final_status = child.wait()?;
            timed_out = true;
            break;
        }
        if let Some(status) = child.try_wait()? {
            final_status = status;
            break;
        }
        if last_progress_log.elapsed() >= Duration::from_secs(30) {
            let idle_s = last_output_at.elapsed().as_secs();
            let last_line = last_output_line.as_deref().unwrap_or("<no output>");
            let truncated = if last_line.len() > 200 {
                format!("{}...", &last_line[..200])
            } else {
                last_line.to_string()
            };
            info!(
                command = %cmdline,
                elapsed_s = started.elapsed().as_secs(),
                idle_output_s = idle_s,
                last_output = %truncated,
                log_path = %spec.log_path.display(),
                "command still running (full output in log file)"
            );
            last_progress_log = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    while !(stdout_handle.is_finished() && stderr_handle.is_finished()) {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(item) => {
                if item.is_stderr {
                    aggregated_stderr.push_str(&item.line);
                    aggregated_stderr.push('\n');
                } else {
                    aggregated_stdout.push_str(&item.line);
                    aggregated_stdout.push('\n');
                }
                last_output_at = Instant::now();
                last_output_line = Some(item.line);
            }
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {}
        }
    }
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    drain_stream_lines(
        &rx,
        &mut aggregated_stdout,
        &mut aggregated_stderr,
        &mut last_output_at,
        &mut last_output_line,
    );

    let returncode = if timed_out {
        -1
    } else {
        final_status.code().unwrap_or(-1)
    };
    if timed_out {
        if !aggregated_stderr.is_empty() && !aggregated_stderr.ends_with('\n') {
            aggregated_stderr.push('\n');
        }
        aggregated_stderr.push_str(&format!("TIMEOUT after {}s", spec.timeout.as_secs()));
        warn!(
            command = %cmdline,
            log_path = %spec.log_path.display(),
            "command timed out"
        );
    }

    let finished_at = SystemTime::now();
    append_log_result(
        &spec,
        &aggregated_stdout,
        &aggregated_stderr,
        returncode,
        finished_at,
        started.elapsed(),
    )?;
    info!(
        command = %cmdline,
        returncode,
        elapsed_s = started.elapsed().as_secs(),
        log_path = %spec.log_path.display(),
        "command finished (full output in log file)"
    );
    Ok(CommandResult {
        stdout: aggregated_stdout,
        stderr: aggregated_stderr,
        returncode,
        log_path: spec.log_path,
    })
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn ensure_allowed_command(program: &Path) -> Result<()> {
    let name = program
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if ALLOWED.contains(&name) {
        return Ok(());
    }
    bail!(
        "uncontrolled command '{}' (allowed: {:?})",
        program.display(),
        ALLOWED
    )
}

fn write_log_header(spec: &CommandSpec, started_at: SystemTime) -> Result<()> {
    let mut file = fs::File::create(&spec.log_path)?;
    writeln!(
        file,
        "COMMAND: {} {}\nCWD: {}\nSTARTED_AT: {}\nTIMEOUT: {}s\n",
        spec.program.display(),
        spec.args.join(" "),
        spec.cwd
            .as_deref()
            .unwrap_or_else(|| Path::new("."))
            .display(),
        format_system_time(started_at),
        spec.timeout.as_secs()
    )?;
    Ok(())
}

async fn async_write_log_header(spec: &CommandSpec, started_at: SystemTime) -> Result<()> {
    let content = format!(
        "COMMAND: {} {}\nCWD: {}\nSTARTED_AT: {}\nTIMEOUT: {}s\n",
        spec.program.display(),
        spec.args.join(" "),
        spec.cwd
            .as_deref()
            .unwrap_or_else(|| Path::new("."))
            .display(),
        format_system_time(started_at),
        spec.timeout.as_secs()
    );
    tokio::fs::write(&spec.log_path, content).await?;
    Ok(())
}

fn append_log_result(
    spec: &CommandSpec,
    stdout: &str,
    stderr: &str,
    returncode: i32,
    finished_at: SystemTime,
    elapsed: Duration,
) -> Result<()> {
    let mut file = fs::OpenOptions::new().append(true).open(&spec.log_path)?;
    writeln!(
        file,
        "STDOUT:\n{}\n\nSTDERR:\n{}\n\nRETURNCODE: {}\nFINISHED_AT: {}\nELAPSED: {:.3}s",
        stdout,
        stderr,
        returncode,
        format_system_time(finished_at),
        elapsed.as_secs_f64()
    )?;
    Ok(())
}

async fn async_append_log_result(
    spec: &CommandSpec,
    stdout: &str,
    stderr: &str,
    returncode: i32,
    finished_at: SystemTime,
    elapsed: Duration,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let content = format!(
        "STDOUT:\n{}\n\nSTDERR:\n{}\n\nRETURNCODE: {}\nFINISHED_AT: {}\nELAPSED: {:.3}s",
        stdout,
        stderr,
        returncode,
        format_system_time(finished_at),
        elapsed.as_secs_f64()
    );
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&spec.log_path)
        .await?;
    file.write_all(content.as_bytes()).await?;
    file.flush().await?;
    Ok(())
}

fn format_system_time(time: SystemTime) -> String {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("{}.{:03}Z", duration.as_secs(), duration.subsec_millis()),
        Err(_) => "before-unix-epoch".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    // -- Blocking version tests (existing) --

    #[test]
    fn static_allowed_list_respects_rules() {
        assert!(ensure_allowed_command(Path::new("takopack")).is_ok());
        assert!(ensure_allowed_command(Path::new("osc")).is_ok());
        assert!(ensure_allowed_command(Path::new("git")).is_ok());
        assert!(ensure_allowed_command(Path::new("baz")).is_err());
    }

    #[test]
    fn run_command_blocking_log_contains_header_and_returncode() {
        let dir = tempdir().expect("tempdir");
        let osc = dir.path().join("osc");
        std::fs::write(&osc, "#!/bin/sh\nexit 7\n").expect("write script");
        std::fs::set_permissions(&osc, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");

        let log_path = dir.path().join("logs").join("osc.log");
        let result = run_command_blocking(CommandSpec {
            program: osc,
            args: vec!["checkout".into()],
            cwd: Some(dir.path().to_path_buf()),
            timeout: Duration::from_secs(5),
            log_path: log_path.clone(),
        })
        .expect("run command");

        assert_eq!(result.returncode, 7);
        let log = std::fs::read_to_string(&log_path).expect("read log");
        assert!(log.contains("COMMAND:"));
        assert!(log.contains("CWD:"));
        assert!(log.contains("STARTED_AT:"));
        assert!(log.contains("TIMEOUT: 5s"));
        assert!(log.contains("RETURNCODE: 7"));
        assert!(log.contains("FINISHED_AT:"));
        assert!(log.contains("ELAPSED:"));
    }

    #[test]
    fn run_command_blocking_collects_stdout_and_stderr() {
        let dir = tempdir().expect("tempdir");
        let osc = dir.path().join("osc");
        std::fs::write(&osc, "#!/bin/sh\necho hello\necho boom >&2\n").expect("write script");
        std::fs::set_permissions(&osc, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");

        let result = run_command_blocking(CommandSpec {
            program: osc,
            args: vec!["build".into()],
            cwd: Some(dir.path().to_path_buf()),
            timeout: Duration::from_secs(5),
            log_path: dir.path().join("logs").join("osc-build.log"),
        })
        .expect("run command");

        assert_eq!(result.stdout, "hello\n");
        assert_eq!(result.stderr, "boom\n");
    }

    // -- Async version tests --

    #[tokio::test]
    async fn async_run_command_denies_unlisted_program() {
        let dir = tempdir().expect("tempdir");
        let result = run_command(CommandSpec {
            program: "rm".into(),
            args: vec!["-rf".into(), "/".into()],
            cwd: Some(dir.path().to_path_buf()),
            timeout: Duration::from_secs(5),
            log_path: dir.path().join("logs").join("rm.log"),
        })
        .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("uncontrolled command")
        );
    }

    #[tokio::test]
    async fn async_run_command_collects_stdout_and_stderr() {
        let dir = tempdir().expect("tempdir");
        let script = dir.path().join("git");
        std::fs::write(&script, "#!/bin/sh\necho out1\necho err1 >&2\necho out2\n")
            .expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let result = run_command(CommandSpec {
            program: script,
            args: vec!["log".into()],
            cwd: Some(dir.path().to_path_buf()),
            timeout: Duration::from_secs(5),
            log_path: dir.path().join("logs").join("async.log"),
        })
        .await
        .expect("run command");

        assert_eq!(result.stdout, "out1\nout2\n");
        assert_eq!(result.stderr, "err1\n");
    }

    #[tokio::test]
    async fn async_run_command_preserves_nonzero_exit_code() {
        let dir = tempdir().expect("tempdir");
        let script = dir.path().join("osc");
        std::fs::write(&script, "#!/bin/sh\nexit 42\n").expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let result = run_command(CommandSpec {
            program: script,
            args: vec!["build".into()],
            cwd: Some(dir.path().to_path_buf()),
            timeout: Duration::from_secs(5),
            log_path: dir.path().join("logs").join("exit.log"),
        })
        .await
        .expect("run command");

        assert_eq!(result.returncode, 42);
    }

    #[tokio::test]
    async fn async_run_command_kills_on_timeout() {
        let dir = tempdir().expect("tempdir");
        let script = dir.path().join("git");
        // Script that sleeps forever
        std::fs::write(&script, "#!/bin/sh\nwhile true; do sleep 1; done\n").expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let result = run_command(CommandSpec {
            program: script,
            args: vec!["status".into()],
            cwd: Some(dir.path().to_path_buf()),
            timeout: Duration::from_millis(500),
            log_path: dir.path().join("logs").join("timeout.log"),
        })
        .await
        .expect("run command");

        assert_eq!(result.returncode, -1);
        assert!(result.stderr.contains("TIMEOUT after 0s"));
    }

    #[tokio::test]
    async fn async_run_command_large_output_does_not_deadlock() {
        let dir = tempdir().expect("tempdir");
        let script = dir.path().join("git");
        // Generate 1000 lines on both stdout and stderr concurrently
        std::fs::write(
            &script,
            "#!/bin/sh\nfor i in $(seq 1 1000); do echo \"stdout line $i\"; echo \"stderr line $i\" >&2; done\n",
        )
        .expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let result = run_command(CommandSpec {
            program: script,
            args: vec!["log".into()],
            cwd: Some(dir.path().to_path_buf()),
            timeout: Duration::from_secs(30),
            log_path: dir.path().join("logs").join("big.log"),
        })
        .await
        .expect("run command");

        // Both streams should be fully collected without deadlock
        let stdout_lines = result.stdout.lines().count();
        let stderr_lines = result.stderr.lines().count();
        assert_eq!(stdout_lines, 1000, "expected 1000 stdout lines");
        assert_eq!(stderr_lines, 1000, "expected 1000 stderr lines");
    }

    #[tokio::test]
    async fn async_run_command_log_contains_all_fields() {
        let dir = tempdir().expect("tempdir");
        let script = dir.path().join("osc");
        std::fs::write(&script, "#!/bin/sh\nexit 3\n").expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let log_path = dir.path().join("logs").join("fields.log");
        let result = run_command(CommandSpec {
            program: script,
            args: vec!["api".into()],
            cwd: Some(dir.path().to_path_buf()),
            timeout: Duration::from_secs(5),
            log_path: log_path.clone(),
        })
        .await
        .expect("run command");

        assert_eq!(result.returncode, 3);
        let log = tokio::fs::read_to_string(&log_path)
            .await
            .expect("read log");
        assert!(log.contains("COMMAND:"));
        assert!(log.contains("CWD:"));
        assert!(log.contains("STARTED_AT:"));
        assert!(log.contains("TIMEOUT: 5s"));
        assert!(log.contains("RETURNCODE: 3"));
        assert!(log.contains("FINISHED_AT:"));
        assert!(log.contains("ELAPSED:"));
    }

    /// Timeout fires even when the child continuously writes output.
    /// Regression test for the "deadline resets every iteration" bug.
    #[tokio::test]
    async fn async_run_command_timeout_kills_despite_continuous_output() {
        let dir = tempdir().expect("tempdir");
        let script = dir.path().join("git");
        // Outputs a tick every 10ms, runs forever
        std::fs::write(
            &script,
            "#!/bin/sh\nwhile true; do echo tick; sleep 0.01; done\n",
        )
        .expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let result = run_command(CommandSpec {
            program: script,
            args: vec!["log".into()],
            cwd: Some(dir.path().to_path_buf()),
            timeout: Duration::from_millis(500),
            log_path: dir.path().join("logs").join("tick.log"),
        })
        .await
        .expect("run command");

        assert_eq!(result.returncode, -1);
        assert!(
            result.stderr.contains("TIMEOUT"),
            "expected TIMEOUT message in stderr"
        );
        // Some ticks should have been collected before the kill
        assert!(
            result.stdout.contains("tick"),
            "expected at least one tick before timeout"
        );
    }

    /// Timeout with massive concurrent output must not hang.
    /// Regression test for the "reader blocked on full channel" bug.
    #[tokio::test]
    async fn async_run_command_timeout_with_heavy_output_does_not_hang() {
        let dir = tempdir().expect("tempdir");
        let script = dir.path().join("git");
        // Writes 100k lines then sleeps — we should timeout during the write
        std::fs::write(
            &script,
            "#!/bin/sh\nfor i in $(seq 1 100000); do echo \"line $i\"; echo \"err $i\" >&2; done; sleep 999\n",
        )
        .expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        // 2s timeout; the script takes much longer
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_command(CommandSpec {
                program: script,
                args: vec!["log".into()],
                cwd: Some(dir.path().to_path_buf()),
                timeout: Duration::from_secs(2),
                log_path: dir.path().join("logs").join("heavy.log"),
            }),
        )
        .await
        .expect("run_command itself must not hang beyond the 10s outer timeout")
        .expect("run command");

        assert_eq!(result.returncode, -1);
        assert!(result.stderr.contains("TIMEOUT"));
    }

    /// Background child processes must be killed via process group on timeout.
    /// The script spawns two background loops that write forever, then the
    /// foreground waits.  Without killpg the background processes would hold
    /// the pipe open and run_command would hang.
    #[tokio::test]
    async fn async_run_command_timeout_kills_background_children() {
        let dir = tempdir().expect("tempdir");
        let script = dir.path().join("git");
        // Two background writers + foreground wait
        std::fs::write(
            &script,
            r#"#!/bin/sh
while true; do echo bg1_stdout; sleep 0.01; done &
while true; do echo bg2_stderr >&2; sleep 0.01; done &
sleep 999
"#,
        )
        .expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_command(CommandSpec {
                program: script,
                args: vec!["log".into()],
                cwd: Some(dir.path().to_path_buf()),
                timeout: Duration::from_secs(1),
                log_path: dir.path().join("logs").join("bg.log"),
            }),
        )
        .await
        .expect("run_command must not hang (10s outer guard)")
        .expect("run command");

        assert_eq!(result.returncode, -1);
        assert!(result.stderr.contains("TIMEOUT"));
        // Background writers should have produced some output before being killed
        assert!(
            result.stdout.contains("bg1_stdout") || result.stderr.contains("bg2_stderr"),
            "expected output from background children"
        );
    }
}
