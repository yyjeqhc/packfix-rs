use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;

use crate::utils::command::{CommandResult, CommandSpec, run_command, run_command_blocking};

#[derive(Debug, Clone)]
pub struct BuildResult {
    pub success: bool,
    pub returncode: i32,
    pub log_path: PathBuf,
    pub stdout: String,
    pub stderr: String,
}

#[allow(dead_code)]
pub fn osc_build(
    workdir: &Path,
    spec_path: &Path,
    repository: &str,
    arch: &str,
    project: Option<&str>,
    osc_bin: &Path,
    attempt_index: usize,
    build_root: Option<&Path>,
) -> Result<BuildResult> {
    let args = build_args(repository, arch, spec_path, project, build_root);
    let log_path = workdir
        .join("logs")
        .join(format!("build_attempt_{attempt_index:03}.log"));
    let result: CommandResult = run_command_blocking(CommandSpec {
        program: osc_bin.to_path_buf(),
        args,
        cwd: Some(workdir.to_path_buf()),
        timeout: Duration::from_secs(3600),
        log_path: log_path.clone(),
    })?;

    Ok(BuildResult {
        success: result.returncode == 0,
        returncode: result.returncode,
        log_path: result.log_path,
        stdout: result.stdout,
        stderr: result.stderr,
    })
}

pub fn build_args(
    repository: &str,
    arch: &str,
    spec_path: &Path,
    project: Option<&str>,
    build_root: Option<&Path>,
) -> Vec<String> {
    let mut args = vec![
        "build".to_string(),
        "--noservice".to_string(),
        "--no-verify".to_string(),
    ];
    // --root goes before positional arguments (repository/arch/spec_path)
    // so it is easier to spot in logs and less likely to be parsed as a
    // positional by osc.
    if let Some(root) = build_root {
        args.push("--root".to_string());
        args.push(root.display().to_string());
    }
    args.push("--release".to_string());
    args.push("0".to_string());
    args.push(repository.to_string());
    args.push(arch.to_string());
    args.push(spec_path.display().to_string());
    if let Some(project) = project {
        args.push("--alternative-project".to_string());
        args.push(project.to_string());
    }
    args
}

pub fn osc_checkout(
    project: &str,
    package: Option<&str>,
    checkout_root: Option<&Path>,
    osc_bin: &Path,
) -> Result<BuildResult> {
    let args = checkout_args(project, package);
    let checkout_root = checkout_root.unwrap_or_else(|| Path::new("."));
    let log_path = checkout_log_path(checkout_root, project, package);
    let result: CommandResult = run_command_blocking(CommandSpec {
        program: osc_bin.to_path_buf(),
        args,
        cwd: Some(checkout_root.to_path_buf()),
        timeout: Duration::from_secs(300),
        log_path: log_path.clone(),
    })?;
    Ok(BuildResult {
        success: result.returncode == 0,
        returncode: result.returncode,
        log_path: result.log_path,
        stdout: result.stdout,
        stderr: result.stderr,
    })
}

pub fn checkout_target_dir(checkout_root: &Path, project: &str, package: Option<&str>) -> PathBuf {
    let project_dir = checkout_root.join(project);
    match package {
        Some(pkg) => project_dir.join(pkg),
        None => project_dir,
    }
}

fn checkout_log_path(checkout_root: &Path, project: &str, package: Option<&str>) -> PathBuf {
    let suffix = package.unwrap_or(project);
    checkout_root
        .join("logs")
        .join(format!("osc_checkout_{suffix}.log"))
}

pub fn osc_update(workdir: &Path, osc_bin: &Path) -> Result<BuildResult> {
    let args = vec!["up".to_string(), "-S".to_string()];
    let log_path = workdir.join("logs").join("osc_update.log");
    let result: CommandResult = run_command_blocking(CommandSpec {
        program: osc_bin.to_path_buf(),
        args,
        cwd: Some(workdir.to_path_buf()),
        timeout: Duration::from_secs(300),
        log_path: log_path.clone(),
    })?;
    Ok(BuildResult {
        success: result.returncode == 0,
        returncode: result.returncode,
        log_path: result.log_path,
        stdout: result.stdout,
        stderr: result.stderr,
    })
}

pub fn sanitize_checkout_dir(dir: &Path) -> Result<()> {
    use anyhow::Context;
    if !dir.exists() {
        return Ok(());
    }
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("read_dir {} failed", dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name().into_string().unwrap_or_default();
        let path = entry.path();
        if name == "_service" {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if name.contains(':') {
            // strip everything up to last ':'
            if let Some(pos) = name.rfind(':') {
                let new_name = &name[pos + 1..];
                let new_path = dir.join(new_name);
                // avoid overwriting existing file
                if !new_path.exists() {
                    std::fs::rename(&path, &new_path).with_context(|| {
                        format!(
                            "failed to rename {} -> {}",
                            path.display(),
                            new_path.display()
                        )
                    })?;
                } else {
                    // remove the prefixed copy if destination exists
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    Ok(())
}

pub fn osc_api_status(
    project: &str,
    package: &str,
    repository: &str,
    arch: &str,
    osc_bin: &Path,
) -> Result<BuildResult> {
    let url = format!("/build/{project}/{repository}/{arch}/{package}/_status");
    let args = vec!["api".to_string(), url];
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("_{:016}", d.as_nanos()))
        .unwrap_or_default();
    let log_path = Path::new(".")
        .join("logs")
        .join(format!("osc_status_{package}{ts}.log"));
    let result: CommandResult = run_command_blocking(CommandSpec {
        program: osc_bin.to_path_buf(),
        args,
        cwd: Some(Path::new(".").to_path_buf()),
        timeout: Duration::from_secs(60),
        log_path: log_path.clone(),
    })?;
    Ok(BuildResult {
        success: result.returncode == 0,
        returncode: result.returncode,
        log_path: result.log_path,
        stdout: result.stdout,
        stderr: result.stderr,
    })
}

fn checkout_args(project: &str, package: Option<&str>) -> Vec<String> {
    let target = match package {
        Some(pkg) => format!("{project}/{pkg}"),
        None => project.to_string(),
    };
    vec!["checkout".to_string(), target]
}

// ---------------------------------------------------------------------------
// Async versions (for use in async contexts)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn osc_build_async(
    workdir: &Path,
    spec_path: &Path,
    repository: &str,
    arch: &str,
    project: Option<&str>,
    osc_bin: &Path,
    attempt_index: usize,
    build_root: Option<&Path>,
) -> Result<BuildResult> {
    let args = build_args(repository, arch, spec_path, project, build_root);
    let log_path = workdir
        .join("logs")
        .join(format!("build_attempt_{attempt_index:03}.log"));
    let result: CommandResult = run_command(CommandSpec {
        program: osc_bin.to_path_buf(),
        args,
        cwd: Some(workdir.to_path_buf()),
        timeout: Duration::from_secs(3600),
        log_path: log_path.clone(),
    })
    .await?;

    Ok(BuildResult {
        success: result.returncode == 0,
        returncode: result.returncode,
        log_path: result.log_path,
        stdout: result.stdout,
        stderr: result.stderr,
    })
}

pub async fn osc_checkout_async(
    project: &str,
    package: Option<&str>,
    checkout_root: Option<&Path>,
    osc_bin: &Path,
) -> Result<BuildResult> {
    let args = checkout_args(project, package);
    let checkout_root = checkout_root.unwrap_or_else(|| Path::new("."));
    let log_path = checkout_log_path(checkout_root, project, package);
    let result: CommandResult = run_command(CommandSpec {
        program: osc_bin.to_path_buf(),
        args,
        cwd: Some(checkout_root.to_path_buf()),
        timeout: Duration::from_secs(300),
        log_path: log_path.clone(),
    })
    .await?;
    Ok(BuildResult {
        success: result.returncode == 0,
        returncode: result.returncode,
        log_path: result.log_path,
        stdout: result.stdout,
        stderr: result.stderr,
    })
}

pub async fn osc_update_async(workdir: &Path, osc_bin: &Path) -> Result<BuildResult> {
    let args = vec!["up".to_string(), "-S".to_string()];
    let log_path = workdir.join("logs").join("osc_update.log");
    let result: CommandResult = run_command(CommandSpec {
        program: osc_bin.to_path_buf(),
        args,
        cwd: Some(workdir.to_path_buf()),
        timeout: Duration::from_secs(300),
        log_path: log_path.clone(),
    })
    .await?;
    Ok(BuildResult {
        success: result.returncode == 0,
        returncode: result.returncode,
        log_path: result.log_path,
        stdout: result.stdout,
        stderr: result.stderr,
    })
}

pub async fn osc_api_status_async(
    project: &str,
    package: &str,
    repository: &str,
    arch: &str,
    osc_bin: &Path,
) -> Result<BuildResult> {
    let url = format!("/build/{project}/{repository}/{arch}/{package}/_status");
    let args = vec!["api".to_string(), url];
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("_{:016}", d.as_nanos()))
        .unwrap_or_default();
    let log_path = Path::new(".")
        .join("logs")
        .join(format!("osc_status_{package}{ts}.log"));
    let result: CommandResult = run_command(CommandSpec {
        program: osc_bin.to_path_buf(),
        args,
        cwd: Some(Path::new(".").to_path_buf()),
        timeout: Duration::from_secs(60),
        log_path: log_path.clone(),
    })
    .await?;
    Ok(BuildResult {
        success: result.returncode == 0,
        returncode: result.returncode,
        log_path: result.log_path,
        stdout: result.stdout,
        stderr: result.stderr,
    })
}

#[cfg(test)]
fn update_args() -> Vec<String> {
    vec!["up".to_string(), "-S".to_string()]
}

#[cfg(test)]
fn api_status_args(project: &str, package: &str, repository: &str, arch: &str) -> Vec<String> {
    vec![
        "api".to_string(),
        format!("/build/{project}/{repository}/{arch}/{package}/_status"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc_build_flags_include_no_hooks() {
        let args = build_args("x64", "x86_64", Path::new("foo.spec"), None, None);
        assert!(
            args.contains(&"--noservice".to_string()),
            "missing --noservice"
        );
        assert!(
            args.contains(&"--no-verify".to_string()),
            "missing --no-verify"
        );
    }

    #[test]
    fn osc_build_release_is_zero() {
        let args = build_args("x64", "x86_64", Path::new("foo.spec"), None, None);
        let idx = args
            .iter()
            .position(|a| a == "--release")
            .expect("missing --release");
        assert_eq!(args.get(idx + 1).map(String::as_str), Some("0"));
    }

    #[test]
    fn osc_build_args_contain_spec() {
        let args = build_args("x64", "x86_64", Path::new("foo.spec"), None, None);
        assert!(args.contains(&"foo.spec".to_string()));
    }

    #[test]
    fn osc_build_args_with_root() {
        let args = build_args(
            "x64",
            "x86_64",
            Path::new("foo.spec"),
            None,
            Some(Path::new("/var/tmp/build-root/packfix-1-x64-x86_64")),
        );
        let root_idx = args
            .iter()
            .position(|a| a == "--root")
            .expect("missing --root");
        assert_eq!(
            args.get(root_idx + 1).map(String::as_str),
            Some("/var/tmp/build-root/packfix-1-x64-x86_64")
        );
    }

    #[test]
    fn osc_build_args_with_root_and_project() {
        let args = build_args(
            "x64",
            "x86_64",
            Path::new("foo.spec"),
            Some("home:test"),
            Some(Path::new("/var/tmp/build-root/packfix-2-x64-x86_64")),
        );
        assert!(args.contains(&"--alternative-project".to_string()));
        assert!(args.contains(&"home:test".to_string()));
        let root_idx = args
            .iter()
            .position(|a| a == "--root")
            .expect("missing --root");
        assert_eq!(
            args.get(root_idx + 1).map(String::as_str),
            Some("/var/tmp/build-root/packfix-2-x64-x86_64")
        );
    }

    #[test]
    fn osc_build_args_without_root_does_not_contain_root_flag() {
        let args = build_args("x64", "x86_64", Path::new("foo.spec"), None, None);
        assert!(!args.contains(&"--root".to_string()));
    }

    #[test]
    fn root_flag_comes_before_positional_args() {
        let args = build_args(
            "x64",
            "x86_64",
            Path::new("foo.spec"),
            None,
            Some(Path::new("/tmp/r")),
        );
        let root_idx = args
            .iter()
            .position(|a| a == "--root")
            .expect("missing --root");
        let repo_idx = args.iter().position(|a| a == "x64").expect("missing repo");
        assert!(
            root_idx < repo_idx,
            "--root (at {root_idx}) must appear before repository positional (at {repo_idx})"
        );
    }

    #[test]
    fn root_flag_absent_when_not_in_pool() {
        // Double-check: the "build" subcommand and args always present.
        let args = build_args("x64", "x86_64", Path::new("x.spec"), None, None);
        assert_eq!(args[0], "build");
        assert!(args.contains(&"--noservice".to_string()));
        assert!(!args.contains(&"--root".to_string()));
    }

    #[test]
    fn root_flag_present_when_pool_configured() {
        let args = build_args(
            "x64",
            "x86_64",
            Path::new("x.spec"),
            None,
            Some(Path::new("/var/tmp/build-root/packfix-1-x64-x86_64")),
        );
        assert!(args.contains(&"--root".to_string()));
        assert!(
            args.contains(&"/var/tmp/build-root/packfix-1-x64-x86_64".to_string()),
            "root path must be in args"
        );
    }

    #[test]
    fn osc_checkout_args_with_package() {
        let args = checkout_args("home:test:proj", Some("python-foo"));
        assert_eq!(args[0], "checkout");
        assert_eq!(args[1], "home:test:proj/python-foo");
        assert_eq!(args.len(), 2);
    }

    #[test]
    fn osc_checkout_args_project_only() {
        let args = checkout_args("home:test:proj", None);
        assert_eq!(args[1], "home:test:proj");
    }

    #[test]
    fn osc_checkout_log_path_includes_package() {
        let root = Path::new("/tmp/checkouts");
        let path = checkout_log_path(root, "home:test:proj", Some("python-fonttools"));
        assert_eq!(
            path,
            root.join("logs").join("osc_checkout_python-fonttools.log")
        );
    }

    #[test]
    fn osc_checkout_args_do_not_confuse_cwd_and_target() {
        let args = checkout_args("home:test:proj", Some("python-fonttools"));
        assert_eq!(
            args,
            vec![
                "checkout".to_string(),
                "home:test:proj/python-fonttools".to_string()
            ]
        );
    }

    #[test]
    fn checkout_root_semantics_documented_by_test() {
        let root = Path::new("/tmp/checkouts");
        assert_eq!(
            checkout_target_dir(root, "home:test:proj", Some("python-fonttools")),
            root.join("home:test:proj").join("python-fonttools")
        );
        assert_eq!(
            checkout_target_dir(root, "home:test:proj", None),
            root.join("home:test:proj")
        );
    }

    #[test]
    fn osc_update_args() {
        let args = update_args();
        assert_eq!(args, vec!["up", "-S"]);
    }

    #[test]
    fn osc_api_status_args() {
        let args = api_status_args("home:test:proj", "python-foo", "x64", "x86_64");
        assert_eq!(args[0], "api");
        assert_eq!(
            args[1],
            "/build/home:test:proj/x64/x86_64/python-foo/_status"
        );
    }
}
