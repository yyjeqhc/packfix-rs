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

pub fn run_command(spec: CommandSpec) -> Result<CommandResult> {
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

fn format_system_time(time: SystemTime) -> String {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("{}.{:03}Z", duration.as_secs(), duration.subsec_millis()),
        Err(_) => "before-unix-epoch".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn static_allowed_list_respects_rules() {
        assert!(ensure_allowed_command(Path::new("takopack")).is_ok());
        assert!(ensure_allowed_command(Path::new("osc")).is_ok());
        assert!(ensure_allowed_command(Path::new("git")).is_ok());
        assert!(ensure_allowed_command(Path::new("baz")).is_err());
    }

    #[test]
    fn run_command_log_contains_header_and_returncode() {
        let dir = tempdir().expect("tempdir");
        let osc = dir.path().join("osc");
        std::fs::write(&osc, "#!/bin/sh\nexit 7\n").expect("write script");
        std::fs::set_permissions(&osc, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");

        let log_path = dir.path().join("logs").join("osc.log");
        let result = run_command(CommandSpec {
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
    fn run_command_collects_stdout_and_stderr() {
        let dir = tempdir().expect("tempdir");
        let osc = dir.path().join("osc");
        std::fs::write(&osc, "#!/bin/sh\necho hello\necho boom >&2\n").expect("write script");
        std::fs::set_permissions(&osc, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");

        let result = run_command(CommandSpec {
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
}
