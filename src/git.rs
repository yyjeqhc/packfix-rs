use std::{path::Path, time::Duration};

use anyhow::{Context, Result};

use crate::utils::command::{CommandSpec, run_command};

pub async fn checkout(repo_dir: &Path, branch: &str) -> Result<()> {
    run_git(repo_dir, &["checkout", branch], "checkout").await
}

pub async fn create_branch(repo_dir: &Path, branch: &str) -> Result<()> {
    run_git(repo_dir, &["checkout", "-b", branch], "create_branch").await
}

pub async fn add(repo_dir: &Path, paths: &[&str]) -> Result<()> {
    let mut args = vec!["add"];
    args.extend_from_slice(paths);
    run_git(repo_dir, &args, "add").await
}

pub async fn commit(repo_dir: &Path, message: &str) -> Result<()> {
    run_git(
        repo_dir,
        &["commit", "--no-verify", "-m", message],
        "commit",
    )
    .await
}

pub async fn push(repo_dir: &Path, remote: &str, branch: &str) -> Result<()> {
    run_git(repo_dir, &["push", remote, branch], "push").await
}

pub async fn branch_exists(repo_dir: &Path, branch: &str) -> Result<bool> {
    let result = run_git_capture(
        repo_dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
        "branch_exists",
    )
    .await?;
    Ok(result.returncode == 0)
}

pub async fn checkout_or_create_branch(repo_dir: &Path, branch: &str) -> Result<()> {
    if branch_exists(repo_dir, branch).await? {
        checkout(repo_dir, branch).await
    } else {
        create_branch(repo_dir, branch).await
    }
}

pub async fn has_staged_changes(repo_dir: &Path) -> Result<bool> {
    let result = run_git_capture(
        repo_dir,
        &["diff", "--cached", "--quiet", "--exit-code"],
        "has_staged_changes",
    )
    .await?;
    match result.returncode {
        0 => Ok(false),
        1 => Ok(true),
        code => anyhow::bail!(
            "git diff --cached failed in {} with rc={code}: {}",
            repo_dir.display(),
            result.stderr
        ),
    }
}

pub async fn commit_if_staged(repo_dir: &Path, message: &str) -> Result<bool> {
    if !has_staged_changes(repo_dir).await? {
        return Ok(false);
    }
    commit(repo_dir, message).await?;
    Ok(true)
}

async fn run_git(repo_dir: &Path, args: &[&str], operation: &str) -> Result<()> {
    let result = run_git_capture(repo_dir, args, operation).await?;
    if result.returncode != 0 {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            if result.stderr.trim().is_empty() {
                result.stdout.trim()
            } else {
                result.stderr.trim()
            }
        );
    }
    Ok(())
}

async fn run_git_capture(
    repo_dir: &Path,
    args: &[&str],
    operation: &str,
) -> Result<crate::utils::command::CommandResult> {
    run_command(CommandSpec {
        program: "git".into(),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        cwd: Some(repo_dir.to_path_buf()),
        timeout: Duration::from_secs(300),
        log_path: repo_dir.join("logs").join(format!("git_{operation}.log")),
    })
    .await
    .with_context(|| format!("git {} failed in {}", args.join(" "), repo_dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .expect("git init");
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir.path())
            .output()
            .expect("git config email");
        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(dir.path())
            .output()
            .expect("git config name");
        std::fs::write(dir.path().join("README.md"), "hello\n").expect("write readme");
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(dir.path())
            .output()
            .expect("git add");
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .expect("git commit");
        dir
    }

    #[tokio::test]
    async fn checkout_or_create_branch_existing_branch() {
        let dir = init_repo();
        checkout_or_create_branch(dir.path(), "feature")
            .await
            .expect("create branch");
        // Try master first, then main (depends on git version)
        if checkout(dir.path(), "master").await.is_err() {
            checkout(dir.path(), "main").await.expect("checkout main");
        }
        checkout_or_create_branch(dir.path(), "feature")
            .await
            .expect("reuse branch");
        let output = Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(dir.path())
            .output()
            .expect("branch show-current");
        let current = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert_eq!(current, "feature");
    }

    #[tokio::test]
    async fn commit_if_staged_no_changes_returns_false() {
        let dir = init_repo();
        let committed = commit_if_staged(dir.path(), "no-op")
            .await
            .expect("commit_if_staged");
        assert!(!committed);
    }

    #[tokio::test]
    async fn commit_if_staged_with_changes_returns_true() {
        let dir = init_repo();
        std::fs::write(dir.path().join("demo.txt"), "x\n").expect("write demo");
        add(dir.path(), &["demo.txt"]).await.expect("git add");
        let committed = commit_if_staged(dir.path(), "add demo")
            .await
            .expect("commit_if_staged");
        assert!(committed);
    }
}
