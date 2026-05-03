use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use tracing::warn;

pub fn operation_log_path(workdir: &Path) -> PathBuf {
    workdir.join("logs").join("packfix_operations.log")
}

pub fn append_operation(workdir: &Path, title: &str, lines: &[String]) -> Result<PathBuf> {
    let path = operation_log_path(workdir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "== {} | {} ==", timestamp(), title)?;
    for line in lines {
        writeln!(file, "{line}")?;
    }
    writeln!(file)?;
    Ok(path)
}

/// Like [`append_operation`] but logs a warning instead of returning an
/// error.  The ops log is non-critical — a full disk or permission problem
/// should not abort the build pipeline.
pub fn log_operation(workdir: &Path, title: &str, lines: &[String]) {
    if let Err(e) = append_operation(workdir, title, lines) {
        warn!(
            workdir = %workdir.display(),
            title,
            error = %e,
            "failed to write ops log entry"
        );
    }
}

fn timestamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("{}.{:03}Z", duration.as_secs(), duration.subsec_millis()),
        Err(_) => "before-unix-epoch".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn append_operation_creates_log_and_appends_entry() {
        let dir = tempdir().expect("tempdir");
        let path = append_operation(
            dir.path(),
            "build-attempt",
            &["ATTEMPT: 1".into(), "STATUS: failed".into()],
        )
        .expect("append");

        let text = std::fs::read_to_string(path).expect("read log");
        assert!(text.contains("build-attempt"));
        assert!(text.contains("ATTEMPT: 1"));
        assert!(text.contains("STATUS: failed"));
    }
}
