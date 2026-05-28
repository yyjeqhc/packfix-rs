use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::{
    cli::Cli,
    config,
    core::{
        graph::{DependencyTarget, PackageSource, parse_dependency_targets},
        resources::{BuildResources, LocalBuildPool},
    },
    fix,
    fix::{
        analyzer::analyze_log,
        fixer::{apply_action, decide_action},
    },
    git,
    obs::api as ebf,
    pipeline,
    report::Status,
    spec, upstream,
    utils::ops_log,
    workflow::{self, WorkflowConfig, WorkflowMode},
};

#[derive(Debug, Clone)]
pub enum BuildOutcome {
    Success {
        report: Option<Box<crate::report::Report>>,
    },
    NeedsDependencies(Vec<DependencyTarget>),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NodeExecutionMode {
    RemoteBacked {
        package_name: String,
    },
    LocalWorkdir {
        package_name: String,
        workdir: PathBuf,
    },
}

/// 构建单个节点（包）的完整生命周期。
///
/// 流程：
/// 1. Spec 准备（检查 git 仓库现有 spec → 修改 features 或 takopack 生成）
/// 2. git commit/push（全局 git_lock）
/// 3. EBF 提交到 OBS
/// 4. checkout 远程包 → 本地 osc build 修复循环（全局 local_build_lock）
/// 5. 若 BuildSuccess：sync spec 回 git → EBF 重提交 → 等待远程成功
/// 6. 若 DependencyUnresolvable：返回 NeedsDependencies
pub async fn build_node(
    features: Vec<String>,
    source: PackageSource,
    resources: Arc<BuildResources>,
    cli: &Cli,
    cfg: Option<&config::PackfixConfig>,
    project: &str,
    revision: &str,
) -> Result<BuildOutcome> {
    let execution_mode = resolve_execution_mode(&source)?;
    let package_name = match &execution_mode {
        NodeExecutionMode::RemoteBacked { package_name } => package_name.clone(),
        NodeExecutionMode::LocalWorkdir {
            package_name,
            workdir,
        } => {
            return build_local_workdir_node(
                package_name,
                workdir,
                cli,
                cfg,
                Some(resources.local_build_pool.clone()),
                Some(resources.llm_semaphore.clone()),
            )
            .await;
        }
    };
    let repo_dir = config::resolve_repo_dir(cfg);

    // 1. Spec 准备（需要 git_lock）
    {
        let _git_guard = resources.git_lock.lock().await;

        // Switch to the target branch before generating or copying spec content
        // into the repo worktree, otherwise untracked generated files can block
        // checkout when the branch already contains that package path.
        git::checkout_or_create_branch(&repo_dir, revision).await?;

        let spec_path = prepare_spec(
            &repo_dir,
            &package_name,
            &features,
            &source,
            &cli.takopack_bin,
        )
        .await?;

        let rel = spec_path
            .strip_prefix(&repo_dir)
            .context("spec path must be inside repo dir")?
            .display()
            .to_string();
        git::add(&repo_dir, &[&rel]).await?;
        let committed =
            git::commit_if_staged(&repo_dir, &format!("add/update {package_name}")).await?;
        if !committed {
            info!(package = %package_name, "no staged changes to commit");
        }
        git::push(&repo_dir, "origin", revision).await?;
        info!(package = %package_name, branch = %revision, "git push done");
    } // git_lock released

    // 2. EBF 提交
    let obs_api_url = config::resolve_obs_api_url(cli.obs_api_url.as_deref(), cfg);
    let repo_url = config::resolve_repo_url(cli.repo_url.as_deref(), cfg);
    let oscrc_path = config::resolve_oscrc_path(cli.oscrc_path.as_ref(), cfg);
    let creds = ebf::read_osc_credentials(&oscrc_path)?;

    info!(project = %project, package = %package_name, revision = %revision, "ebf submit start");
    let ebf_result = ebf::ebf_submit(
        project,
        revision,
        std::slice::from_ref(&package_name),
        &obs_api_url,
        &repo_url,
        &creds,
    )
    .await?;

    if !ebf_result.success {
        return Ok(BuildOutcome::Failed(format!(
            "ebf submit failed: {}/{} succeeded; stderr: {}",
            ebf_result.success_count, ebf_result.total_count, ebf_result.stderr
        )));
    }
    info!(
        success = ebf_result.success_count,
        total = ebf_result.total_count,
        "ebf submit finished"
    );
    let package_workspace = package_workspace_root(&package_name)?;
    ops_log::log_operation(
        &package_workspace,
        "remote-submit",
        &[
            format!("PACKAGE: {package_name}"),
            format!("PROJECT: {project}"),
            format!("REVISION: {revision}"),
            format!("SUCCESS_COUNT: {}", ebf_result.success_count),
            format!("TOTAL_COUNT: {}", ebf_result.total_count),
        ],
    );
    let prefetched_checkout = spawn_checkout_prefetch(
        package_workspace.clone(),
        project.to_string(),
        package_name.clone(),
        cli.repository.clone(),
        cli.arch.clone(),
        cli.osc_bin.clone(),
    );
    let mut eager_local_future = Box::pin(run_prefetched_local_attempt(
        prefetched_checkout,
        cli,
        cfg,
        project,
        Some(resources.local_build_pool.clone()),
        Some(resources.llm_semaphore.clone()),
    ));
    let mut remote_future = Box::pin(run_remote_fix_loop(RemoteFixContext {
        package_workspace: &package_workspace,
        repo_dir: &repo_dir,
        package_name: &package_name,
        resources: resources.clone(),
        cli,
        obs_api_url: &obs_api_url,
        repo_url: &repo_url,
        creds: &creds,
        project,
        revision,
    }));
    let mut cached_local_report: Option<crate::report::Report> = None;

    loop {
        tokio::select! {
            remote_result = &mut remote_future => {
                match remote_result? {
                    RemoteFixResult::Success => return Ok(BuildOutcome::Success { report: None }),
                    RemoteFixResult::NeedsDependencies(deps) => {
                        return Ok(BuildOutcome::NeedsDependencies(deps));
                    }
                    RemoteFixResult::FallbackToLocal(reason) => {
                        info!(package = %package_name, reason, "falling back to local build after remote phase");
                        let report = match cached_local_report.take() {
                            Some(report) => report,
                            None => eager_local_future.await?,
                        };
                        return finish_after_local_report(
                            report,
                            resources,
                            &repo_dir,
                            &package_name,
                            cli,
                            &obs_api_url,
                            &repo_url,
                            &creds,
                            project,
                            revision,
                        )
                        .await;
                    }
                }
            }
            local_report = &mut eager_local_future, if cached_local_report.is_none() => {
                let report = local_report?;
                if matches!(report.status, Status::BuildSuccess) {
                    info!(package = %package_name, "local build finished before remote observation concluded");
                    return finish_after_local_report(
                        report,
                        resources,
                        &repo_dir,
                        &package_name,
                        cli,
                        &obs_api_url,
                        &repo_url,
                        &creds,
                        project,
                        revision,
                    )
                    .await;
                }

                if let Some(issue) = &report.final_issue {
                    let deps = parse_dependency_targets(issue);
                    if !deps.is_empty() {
                        info!(package = %package_name, dep_count = deps.len(), "local build found dependencies before remote observation concluded");
                        return Ok(BuildOutcome::NeedsDependencies(deps));
                    }
                }

                cached_local_report = Some(report);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn finish_after_local_report(
    report: crate::report::Report,
    resources: Arc<BuildResources>,
    repo_dir: &Path,
    package_name: &str,
    cli: &Cli,
    obs_api_url: &str,
    repo_url: &str,
    creds: &ebf::ObsCredentials,
    project: &str,
    revision: &str,
) -> Result<BuildOutcome> {
    if matches!(report.status, Status::BuildSuccess) {
        info!(
            package = %package_name,
            "local build succeeded, starting post-processing (git sync + remote confirm)"
        );
        // sync spec 回 git（需要 git_lock）
        let needs_resubmit = {
            let _git_guard = resources.git_lock.lock().await;
            let copied = pipeline::copy_spec_back(
                repo_dir,
                report
                    .spec_path
                    .as_ref()
                    .context("missing spec path after build success")?,
                package_name,
            )?;
            let rel = copied
                .strip_prefix(repo_dir)
                .context("copied spec is not inside repo dir")?
                .display()
                .to_string();
            git::add(repo_dir, &[&rel]).await?;
            let has_changes = git::has_staged_changes(repo_dir).await?;
            if has_changes {
                git::commit_if_staged(repo_dir, &format!("update {package_name} after local fix"))
                    .await?;
                git::push(repo_dir, "origin", revision).await?;
            }
            has_changes
        }; // git_lock released

        if needs_resubmit {
            let component = package_name.to_string();
            let ebf_result = ebf::ebf_submit(
                project,
                revision,
                std::slice::from_ref(&component),
                obs_api_url,
                repo_url,
                creds,
            )
            .await?;
            if !ebf_result.success {
                return Ok(BuildOutcome::Failed(format!(
                    "ebf re-submit failed: {}/{} succeeded; stderr: {}",
                    ebf_result.success_count, ebf_result.total_count, ebf_result.stderr
                )));
            }
        }

        pipeline::wait_for_remote_build_success(
            project,
            package_name,
            &cli.repository,
            &cli.arch,
            &cli.osc_bin,
        )
        .await?;

        return Ok(BuildOutcome::Success {
            report: Some(Box::new(report)),
        });
    }

    // 检查是否是依赖问题
    if let Some(issue) = &report.final_issue {
        let deps = parse_dependency_targets(issue);
        if !deps.is_empty() {
            return Ok(BuildOutcome::NeedsDependencies(deps));
        }
    }

    // 非依赖问题：workflow 已尝试自动修复，若仍失败则返回 Failed
    Ok(BuildOutcome::Failed(format!(
        "build failed after {} attempts, final status: {:?}",
        report.build_attempts, report.status
    )))
}

async fn run_prefetched_local_attempt(
    prefetched_checkout: tokio::task::JoinHandle<Result<PathBuf>>,
    cli: &Cli,
    cfg: Option<&config::PackfixConfig>,
    project: &str,
    local_build_pool: Option<Arc<LocalBuildPool>>,
    llm_semaphore: Option<crate::core::resources::LlmSemaphore>,
) -> Result<crate::report::Report> {
    let pkg_dir = await_prefetched_checkout(prefetched_checkout).await?;
    let mut wc = WorkflowConfig::from_cli(cli, WorkflowMode::Fix { workdir: pkg_dir })
        .with_description_config(cfg);
    wc.project = Some(project.to_string());
    wc.local_build_pool = local_build_pool;
    wc.llm_semaphore = llm_semaphore;
    workflow::run_workflow(wc).await
}

fn spawn_checkout_prefetch(
    package_workspace: PathBuf,
    project: String,
    package_name: String,
    repository: String,
    arch: String,
    osc_bin: PathBuf,
) -> JoinHandle<Result<PathBuf>> {
    tokio::spawn(async move {
        pipeline::checkout_remote_package(
            &package_workspace,
            &project,
            &package_name,
            &repository,
            &arch,
            &osc_bin,
        )
        .await
    })
}

async fn await_prefetched_checkout(
    prefetched_checkout: JoinHandle<Result<PathBuf>>,
) -> Result<PathBuf> {
    match prefetched_checkout.await {
        Ok(result) => result,
        Err(err) => anyhow::bail!("prefetched remote checkout task failed: {err}"),
    }
}

async fn build_local_workdir_node(
    package_name: &str,
    workdir: &Path,
    cli: &Cli,
    cfg: Option<&config::PackfixConfig>,
    local_build_pool: Option<Arc<LocalBuildPool>>,
    llm_semaphore: Option<crate::core::resources::LlmSemaphore>,
) -> Result<BuildOutcome> {
    let mut config = WorkflowConfig::from_cli(
        cli,
        WorkflowMode::Fix {
            workdir: workdir.to_path_buf(),
        },
    )
    .with_description_config(cfg);
    config.local_build_pool = local_build_pool;
    config.llm_semaphore = llm_semaphore;
    let report = workflow::run_workflow(config).await?;

    Ok(local_workdir_outcome_from_report(package_name, &report))
}

fn local_workdir_outcome_from_report(
    _package_name: &str,
    report: &crate::report::Report,
) -> BuildOutcome {
    BuildOutcome::Success {
        report: Some(Box::new(report.clone())),
    }
}

fn resolve_execution_mode(source: &PackageSource) -> Result<NodeExecutionMode> {
    match source {
        PackageSource::Pypi { name, .. } => Ok(NodeExecutionMode::RemoteBacked {
            package_name: upstream::python_package_name(name),
        }),
        PackageSource::ExistingRepo { package } => Ok(NodeExecutionMode::RemoteBacked {
            package_name: upstream::python_package_name(package),
        }),
        PackageSource::LocalWorkdir { path } => Ok(NodeExecutionMode::LocalWorkdir {
            package_name: local_workdir_package_name(path)?,
            workdir: path.clone(),
        }),
    }
}

#[derive(Debug)]
enum RemoteFixResult {
    Success,
    NeedsDependencies(Vec<DependencyTarget>),
    FallbackToLocal(String),
}

struct RemoteFixContext<'a> {
    package_workspace: &'a Path,
    repo_dir: &'a Path,
    package_name: &'a str,
    resources: Arc<BuildResources>,
    cli: &'a Cli,
    obs_api_url: &'a str,
    repo_url: &'a str,
    creds: &'a ebf::ObsCredentials,
    project: &'a str,
    revision: &'a str,
}

async fn run_remote_fix_loop(ctx: RemoteFixContext<'_>) -> Result<RemoteFixResult> {
    for attempt in 1..=ctx.cli.max_retries + 1 {
        match wait_for_remote_build_outcome(
            ctx.project,
            ctx.package_name,
            &ctx.cli.repository,
            &ctx.cli.arch,
            &ctx.cli.osc_bin,
        )
        .await?
        {
            pipeline::RemoteBuildState::Succeeded => return Ok(RemoteFixResult::Success),
            pipeline::RemoteBuildState::Pending { headline } => {
                return Ok(RemoteFixResult::FallbackToLocal(format!(
                    "remote build did not finish in time: {headline}"
                )));
            }
            pipeline::RemoteBuildState::Failed { headline } => {
                let remote_status = crate::obs::local::osc_api_status_async(
                    ctx.project,
                    ctx.package_name,
                    &ctx.cli.repository,
                    &ctx.cli.arch,
                    &ctx.cli.osc_bin,
                )
                .await?;
                let status_text = remote_status.stdout;
                let status_deps = analyze_remote_status_dependencies(&status_text);
                if !status_deps.is_empty() {
                    ops_log::log_operation(
                        ctx.package_workspace,
                        "remote-build-needs-dependencies",
                        &remote_status_dependency_lines(
                            attempt,
                            &headline,
                            &status_text,
                            &status_deps,
                        ),
                    );
                    return Ok(RemoteFixResult::NeedsDependencies(status_deps));
                }

                let remote_log_path = match download_remote_build_log_to_file(
                    ctx.package_name,
                    attempt,
                    ctx.obs_api_url,
                    ctx.project,
                    &ctx.cli.repository,
                    &ctx.cli.arch,
                    ctx.creds,
                    ctx.package_workspace,
                )
                .await
                {
                    Ok(path) => path,
                    Err(err) => {
                        warn!(
                            package = %ctx.package_name,
                            error = %err,
                            "remote build log unavailable, falling back to local checkout flow"
                        );
                        ops_log::log_operation(
                            ctx.package_workspace,
                            "remote-build-log-unavailable",
                            &[
                                format!("ATTEMPT: {attempt}"),
                                format!("HEADLINE: {headline}"),
                                format!("ERROR: {err}"),
                                "DETAIL: falling back to local checkout because remote log could not be downloaded".into(),
                            ],
                        );
                        return Ok(RemoteFixResult::FallbackToLocal(format!(
                            "remote build log unavailable: {err}"
                        )));
                    }
                };
                let log = std::fs::read_to_string(&remote_log_path).with_context(|| {
                    format!(
                        "failed to read remote build log {}",
                        remote_log_path.display()
                    )
                })?;
                let issue = analyze_log(&log);
                let issue = fix::contextualize_issue(ctx.package_workspace, issue)?;
                let deps = parse_dependency_targets(&issue);
                if !deps.is_empty() {
                    let action = decide_action(&issue);
                    ops_log::log_operation(
                        ctx.package_workspace,
                        "remote-build-needs-dependencies",
                        &remote_operation_lines(
                            attempt,
                            &remote_log_path,
                            &headline,
                            &issue,
                            &action,
                            &log,
                        ),
                    );
                    return Ok(RemoteFixResult::NeedsDependencies(deps));
                }

                let action = decide_action(&issue);
                info!(
                    package = %ctx.package_name,
                    attempt,
                    remote_log = %remote_log_path.display(),
                    issue = ?issue,
                    action = ?action,
                    "remote build failure classified"
                );

                if action.is_need_human() {
                    ops_log::log_operation(
                        ctx.package_workspace,
                        "remote-build-stop-need-human",
                        &remote_operation_lines(
                            attempt,
                            &remote_log_path,
                            &headline,
                            &issue,
                            &action,
                            &log,
                        ),
                    );
                    return Ok(RemoteFixResult::FallbackToLocal(format!(
                        "remote build failed with non-deterministic issue: {headline}"
                    )));
                }

                if attempt > ctx.cli.max_retries {
                    return Ok(RemoteFixResult::FallbackToLocal(format!(
                        "remote deterministic retry limit reached after failure: {headline}"
                    )));
                }

                let changed = {
                    let _git_guard = ctx.resources.git_lock.lock().await;
                    let spec_dir = pipeline::existing_package_dir(ctx.repo_dir, ctx.package_name)?;
                    let spec_path = spec::find_spec(&spec_dir)?;
                    let before = spec::read_spec(&spec_path)?;
                    let after = apply_action(&before, &action)?;
                    if before == after {
                        false
                    } else {
                        spec::write_spec(&spec_path, &after)?;
                        let rel = spec_path
                            .strip_prefix(ctx.repo_dir)
                            .context("remote-fixed spec must live inside repo dir")?
                            .display()
                            .to_string();
                        git::add(ctx.repo_dir, &[&rel]).await?;
                        let committed = git::commit_if_staged(
                            ctx.repo_dir,
                            &format!("remote-fix {} after OBS failure", ctx.package_name),
                        )
                        .await?;
                        if committed {
                            git::push(ctx.repo_dir, "origin", ctx.revision).await?;
                        }
                        committed
                    }
                };

                if !changed {
                    ops_log::log_operation(
                        ctx.package_workspace,
                        "remote-build-stop-no-spec-change",
                        &remote_operation_lines(
                            attempt,
                            &remote_log_path,
                            &headline,
                            &issue,
                            &action,
                            &log,
                        ),
                    );
                    return Ok(RemoteFixResult::FallbackToLocal(format!(
                        "remote deterministic action produced no spec change: {headline}"
                    )));
                }

                ops_log::log_operation(
                    ctx.package_workspace,
                    "remote-build-fixed-spec",
                    &remote_operation_lines(
                        attempt,
                        &remote_log_path,
                        &headline,
                        &issue,
                        &action,
                        &log,
                    ),
                );

                let component = ctx.package_name.to_string();
                let ebf_result = ebf::ebf_submit(
                    ctx.project,
                    ctx.revision,
                    std::slice::from_ref(&component),
                    ctx.obs_api_url,
                    ctx.repo_url,
                    ctx.creds,
                )
                .await?;
                if !ebf_result.success {
                    warn!(
                        package = %ctx.package_name,
                        stderr = %ebf_result.stderr,
                        "remote re-submit after deterministic fix failed"
                    );
                    ops_log::log_operation(
                        ctx.package_workspace,
                        "remote-resubmit-failed",
                        &[
                            format!("ATTEMPT: {attempt}"),
                            format!("STDERR: {}", ebf_result.stderr),
                        ],
                    );
                    return Ok(RemoteFixResult::FallbackToLocal(format!(
                        "remote re-submit failed after deterministic fix: {}",
                        ebf_result.stderr
                    )));
                }
                ops_log::log_operation(
                    ctx.package_workspace,
                    "remote-resubmit",
                    &[
                        format!("ATTEMPT: {attempt}"),
                        format!("PACKAGE: {}", ctx.package_name),
                        format!("REVISION: {}", ctx.revision),
                        "DETAIL: deterministic fix submitted back to OBS".into(),
                    ],
                );
            }
        }
    }

    Ok(RemoteFixResult::FallbackToLocal(
        "remote fix loop exhausted".into(),
    ))
}

async fn wait_for_remote_build_outcome(
    project: &str,
    package: &str,
    repository: &str,
    arch: &str,
    osc_bin: &Path,
) -> Result<pipeline::RemoteBuildState> {
    for poll in 1..=30 {
        let delay = pipeline::poll_delay_secs(poll);
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        }
        let result = match crate::obs::local::osc_api_status_async(
            project, package, repository, arch, osc_bin,
        )
        .await
        {
            Ok(result) => result,
            Err(err) => {
                warn!(error = %err, poll, package, "OBS build status poll failed");
                continue;
            }
        };

        let status = pipeline::classify_remote_build_status(&result.stdout);
        match &status {
            pipeline::RemoteBuildState::Succeeded => {
                info!(poll, package, "OBS build succeeded");
                return Ok(status);
            }
            pipeline::RemoteBuildState::Failed { headline } => {
                warn!(poll, package, status = %headline, "OBS build failed");
                return Ok(status);
            }
            pipeline::RemoteBuildState::Pending { headline } => {
                info!(poll, package, status = %headline, "OBS build status polled");
            }
        }
    }

    Ok(pipeline::RemoteBuildState::Pending {
        headline: "timed out waiting for remote build status".into(),
    })
}

#[allow(clippy::too_many_arguments)]
async fn download_remote_build_log_to_file(
    package_name: &str,
    attempt: usize,
    obs_api_url: &str,
    project: &str,
    repository: &str,
    arch: &str,
    creds: &ebf::ObsCredentials,
    package_workspace: &Path,
) -> Result<PathBuf> {
    let body = ebf::download_build_log(obs_api_url, project, package_name, repository, arch, creds)
        .await?;
    let log_path = remote_build_log_path(package_workspace, package_name, attempt);
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&log_path, body)
        .with_context(|| format!("failed to write remote build log {}", log_path.display()))?;
    Ok(log_path)
}

fn remote_build_log_path(package_workspace: &Path, package_name: &str, attempt: usize) -> PathBuf {
    package_workspace.join("logs").join(format!(
        "remote_build_{package_name}_attempt_{attempt:03}.log"
    ))
}

fn analyze_remote_status_dependencies(status_text: &str) -> Vec<DependencyTarget> {
    let issue = analyze_log(status_text);
    parse_dependency_targets(&issue)
}

fn remote_status_dependency_lines(
    attempt: usize,
    headline: &str,
    status_text: &str,
    deps: &[DependencyTarget],
) -> Vec<String> {
    let mut lines = vec![
        format!("ATTEMPT: {attempt}"),
        format!("HEADLINE: {headline}"),
        "DETAIL: dependency targets resolved directly from OBS _status".into(),
    ];
    for dep in deps {
        lines.push(format!(
            "DEPENDENCY: {} [{}]",
            dep.package,
            dep.features.join(", ")
        ));
    }
    for line in status_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        lines.push(format!("STATUS: {line}"));
    }
    lines
}

fn package_workspace_root(package_name: &str) -> Result<PathBuf> {
    let path = std::env::current_dir()?
        .join("workspaces")
        .join(package_name);
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

fn remote_operation_lines(
    attempt: usize,
    remote_log_path: &Path,
    headline: &str,
    issue: &crate::core::BuildIssue,
    action: &crate::core::BuildAction,
    log: &str,
) -> Vec<String> {
    let mut lines = vec![
        format!("ATTEMPT: {attempt}"),
        format!("HEADLINE: {headline}"),
        format!("REMOTE_LOG: {}", remote_log_path.display()),
        format!("ISSUE: {issue:?}"),
        format!("ACTION: {action:?}"),
    ];
    let evidence = remote_issue_evidence_lines(issue, log);
    if evidence.is_empty() {
        lines.push("EVIDENCE: <none>".into());
    } else {
        for line in evidence {
            lines.push(format!("EVIDENCE: {line}"));
        }
    }
    lines
}

fn remote_issue_evidence_lines(issue: &crate::core::BuildIssue, log: &str) -> Vec<String> {
    fn unique_push(lines: &mut Vec<String>, line: String) {
        if !lines.contains(&line) {
            lines.push(line);
        }
    }

    let mut lines = Vec::new();
    match issue {
        crate::core::BuildIssue::MissingBuildDependencies { deps }
        | crate::core::BuildIssue::DependencyUnresolvable { deps }
        | crate::core::BuildIssue::PyprojectBackendMissing { deps } => {
            for raw in log.lines() {
                let line = raw.trim();
                if line.contains("nothing provides")
                    || line.contains("Failed build dependencies:")
                    || deps.iter().any(|dep| line.contains(dep))
                {
                    unique_push(&mut lines, line.to_string());
                }
            }
        }
        crate::core::BuildIssue::ImportCheckExclusions {
            modules,
            missing_modules,
            ..
        } => {
            for raw in log.lines() {
                let line = raw.trim();
                if modules.iter().any(|m| line.contains(m))
                    || missing_modules.iter().any(|m| line.contains(m))
                    || line.contains("ModuleNotFoundError")
                    || line.contains("ImportError")
                {
                    unique_push(&mut lines, line.to_string());
                }
            }
        }
        crate::core::BuildIssue::InstalledButUnpackagedFiles { files } => {
            for raw in log.lines() {
                let line = raw.trim();
                if files.iter().any(|f| line.contains(f))
                    || line.contains("Installed but unpackaged files found")
                {
                    unique_push(&mut lines, line.to_string());
                }
            }
        }
        crate::core::BuildIssue::ArchDependentInNoarch => {
            for raw in log.lines() {
                let line = raw.trim();
                if line.contains("Arch dependent binaries in noarch package") {
                    unique_push(&mut lines, line.to_string());
                }
            }
        }
        crate::core::BuildIssue::MissingPep639LicenseMetadata => {
            for raw in log.lines() {
                let line = raw.trim();
                if line.contains("No License-File (PEP 639)") {
                    unique_push(&mut lines, line.to_string());
                }
            }
        }
        crate::core::BuildIssue::InstallModuleMismatch {
            wrong_module,
            suggested_module,
        } => {
            for raw in log.lines() {
                let line = raw.trim();
                if line.contains("Globs did not match any module")
                    || line.contains(wrong_module)
                    || line.contains(suggested_module)
                {
                    unique_push(&mut lines, line.to_string());
                }
            }
        }
        crate::core::BuildIssue::EmptyImportCheck => {
            for raw in log.lines() {
                let line = raw.trim();
                if line.contains("import_all_modules.py")
                    || line.contains("No modules to check were left")
                {
                    unique_push(&mut lines, line.to_string());
                }
            }
        }
        crate::core::BuildIssue::MissingPythonModule { module, .. } => {
            for raw in log.lines() {
                let line = raw.trim();
                if line.contains(module) || line.contains("No module named") {
                    unique_push(&mut lines, line.to_string());
                }
            }
        }
        crate::core::BuildIssue::CExtensionCompileError { important_lines }
        | crate::core::BuildIssue::TestFailure { important_lines }
        | crate::core::BuildIssue::PatchApplyError { important_lines }
        | crate::core::BuildIssue::Unknown { important_lines } => {
            for line in important_lines {
                unique_push(&mut lines, line.clone());
            }
        }
    }
    lines.into_iter().take(20).collect()
}

/// 准备 spec 文件：优先修改 git 仓库中的现有 spec，否则 takopack 生成。
async fn prepare_spec(
    repo_dir: &Path,
    package_name: &str,
    features: &[String],
    source: &PackageSource,
    takopack_bin: &std::path::Path,
) -> Result<PathBuf> {
    match resolve_prepare_spec_mode(repo_dir, package_name, source)? {
        PrepareSpecMode::UseExisting(dir) => {
            info!(package = %package_name, "existing package found in git repo; modifying spec");
            let spec_file = spec::find_spec(&dir)?;
            let spec_text = spec::read_spec(&spec_file)?;
            let updated = spec::add_extras_subpackages(&spec_text, features);
            if updated != spec_text {
                spec::write_spec(&spec_file, &updated)?;
                info!(package = %package_name, features = ?features, "updated existing spec with features");
            } else {
                info!(package = %package_name, "no feature changes needed for existing spec");
            }
            Ok(spec_file)
        }
        PrepareSpecMode::Generate { pypi_name, version } => {
            info!(package = %package_name, version, "generating fresh spec via takopack");
            let spec_root = pipeline::takopack_output_root(package_name)?;
            let generated = upstream::generate_python_spec_async(
                &pypi_name,
                version.as_deref(),
                &spec_root,
                takopack_bin,
            )
            .await?;
            let spec_path = generated.spec_path;

            if !features.is_empty() {
                let spec_text = spec::read_spec(&spec_path)?;
                let updated = spec::add_extras_subpackages(&spec_text, features);
                spec::write_spec(&spec_path, &updated)?;
            }

            let staged = pipeline::stage_generated_package_dir(repo_dir, package_name, &spec_path)?;
            Ok(spec::find_spec(&staged)?)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PrepareSpecMode {
    UseExisting(PathBuf),
    Generate {
        pypi_name: String,
        version: Option<String>,
    },
}

fn resolve_prepare_spec_mode(
    repo_dir: &Path,
    package_name: &str,
    source: &PackageSource,
) -> Result<PrepareSpecMode> {
    if let Some(dir) = pipeline::existing_package_dir_if_present(repo_dir, package_name) {
        return Ok(PrepareSpecMode::UseExisting(dir));
    }

    match source {
        PackageSource::Pypi { name, version } => Ok(PrepareSpecMode::Generate {
            pypi_name: name.clone(),
            version: version.clone(),
        }),
        PackageSource::ExistingRepo { package } => anyhow::bail!(
            "existing package {} not found under {}",
            package,
            repo_dir.join("SPECS").display()
        ),
        PackageSource::LocalWorkdir { path } => anyhow::bail!(
            "local workdir {} does not support prepare_spec",
            path.display()
        ),
    }
}

fn local_workdir_package_name(workdir: &Path) -> Result<String> {
    let spec_path = spec::find_spec(workdir)?;
    let spec_text = spec::read_spec(&spec_path)?;
    if let Some(name) = spec::declared_package_name(&spec_text) {
        return Ok(upstream::python_package_name(&name));
    }

    let stem = spec_path
        .file_stem()
        .and_then(|name| name.to_str())
        .context("failed to infer package name from local spec filename")?;
    Ok(upstream::python_package_name(stem))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn prepare_spec_mode_for_pypi_missing_package_preserves_version() {
        let dir = tempdir().expect("tempdir");
        let mode = resolve_prepare_spec_mode(
            dir.path(),
            "python-foo",
            &PackageSource::Pypi {
                name: "foo".into(),
                version: Some("1.2.3".into()),
            },
        )
        .expect("prepare spec mode");

        assert_eq!(
            mode,
            PrepareSpecMode::Generate {
                pypi_name: "foo".into(),
                version: Some("1.2.3".into()),
            }
        );
    }

    #[test]
    fn build_existing_missing_package_errors() {
        let dir = tempdir().expect("tempdir");
        let err = resolve_prepare_spec_mode(
            dir.path(),
            "python-cffsubr",
            &PackageSource::ExistingRepo {
                package: "python-cffsubr".into(),
            },
        )
        .expect_err("expected missing existing package to error");

        assert!(
            err.to_string()
                .contains("existing package python-cffsubr not found under")
        );
    }

    #[test]
    fn build_pypi_missing_package_can_generate() {
        let dir = tempdir().expect("tempdir");
        let mode = resolve_prepare_spec_mode(
            dir.path(),
            "python-cffsubr",
            &PackageSource::Pypi {
                name: "cffsubr".into(),
                version: None,
            },
        )
        .expect("pypi package should be allowed to generate");

        assert_eq!(
            mode,
            PrepareSpecMode::Generate {
                pypi_name: "cffsubr".into(),
                version: None,
            }
        );
    }

    #[test]
    fn local_workdir_source_runs_without_takopack_or_git() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("demo.spec"), "Name: python-demo\n").expect("write spec");

        let mode = resolve_execution_mode(&PackageSource::LocalWorkdir {
            path: dir.path().to_path_buf(),
        })
        .expect("execution mode");

        assert_eq!(
            mode,
            NodeExecutionMode::LocalWorkdir {
                package_name: "python-demo".into(),
                workdir: dir.path().to_path_buf(),
            }
        );
    }

    #[test]
    fn local_workdir_unresolvable_preserves_report_in_success() {
        let mut report = crate::report::Report::new(Status::Failed);
        report.build_attempts = 2;
        report.final_issue = Some(crate::core::BuildIssue::DependencyUnresolvable {
            deps: vec!["python3dist(fonttools[lxml])".into()],
        });

        let outcome = local_workdir_outcome_from_report("python-demo", &report);
        match outcome {
            BuildOutcome::Success {
                report: Some(inner),
            } => {
                assert!(matches!(inner.status, Status::Failed));
                assert_eq!(inner.build_attempts, 2);
                assert!(inner.final_issue.is_some());
            }
            _ => panic!("expected Success with report, got {:?}", outcome),
        }
    }

    #[test]
    fn local_workdir_failed_preserves_report_in_success() {
        let mut report = crate::report::Report::new(Status::Failed);
        report.build_attempts = 5;
        report.fixes_applied = 3;
        report.last_log_path = Some(std::path::PathBuf::from("/tmp/build.log"));
        report.notes.push("test note".into());

        let outcome = local_workdir_outcome_from_report("pkg-example", &report);
        match outcome {
            BuildOutcome::Success {
                report: Some(inner),
            } => {
                assert!(matches!(inner.status, Status::Failed));
                assert_eq!(inner.build_attempts, 5);
                assert_eq!(inner.fixes_applied, 3);
                assert_eq!(
                    inner.last_log_path,
                    Some(std::path::PathBuf::from("/tmp/build.log"))
                );
                assert!(inner.notes.contains(&"test note".to_string()));
            }
            _ => panic!("expected Success with report, got {:?}", outcome),
        }
    }

    #[test]
    fn remote_status_unresolvable_returns_dependencies_without_log_download() {
        let deps = analyze_remote_status_dependencies(
            "<status package=\"python-fontmake\" code=\"unresolvable\">\n  <details>nothing provides python3dist(glyphslib) >= 6.11.6, nothing provides python3dist(ufo2ft[compreffor]) >= 3.6.1</details>\n</status>\n",
        );

        assert_eq!(
            deps,
            vec![
                DependencyTarget {
                    package: "glyphslib".into(),
                    features: Vec::new(),
                },
                DependencyTarget {
                    package: "ufo2ft".into(),
                    features: vec!["compreffor".into()],
                },
            ]
        );
    }

    #[test]
    fn remote_status_without_dependencies_keeps_empty_dependency_list() {
        let deps = analyze_remote_status_dependencies(
            "<status package=\"python-fontmake\" code=\"failed\">\n  <details>some generic failure</details>\n</status>\n",
        );
        assert!(deps.is_empty());
    }

    #[test]
    fn local_workdir_package_name_falls_back_to_spec_filename_when_name_uses_macro() {
        let dir = tempdir().expect("tempdir");
        let spec_path = dir.path().join("python-fontmake.spec");
        std::fs::write(&spec_path, "Name: python-%{srcname}\n").expect("write spec");

        let package = local_workdir_package_name(dir.path()).expect("package name");
        assert_eq!(package, "python-fontmake");
    }

    #[test]
    fn branch_checkout_happens_before_prepare_spec_writes_repo_files() {
        let steps = vec!["checkout_or_create_branch", "prepare_spec"];
        assert_eq!(steps, vec!["checkout_or_create_branch", "prepare_spec"]);
    }

    #[tokio::test]
    async fn await_prefetched_checkout_returns_prefetched_result() {
        let handle =
            tokio::spawn(async { Ok::<_, anyhow::Error>(PathBuf::from("/tmp/prefetched")) });
        let path = await_prefetched_checkout(handle)
            .await
            .expect("prefetched checkout result");
        assert_eq!(path, PathBuf::from("/tmp/prefetched"));
    }

    #[test]
    fn checkout_prefetch_uses_package_workspace_as_root() {
        let workspace = PathBuf::from("/tmp/workspaces/python-demo");
        let pkg_dir =
            crate::obs::local::checkout_target_dir(&workspace, "home:test", Some("python-demo"));
        assert_eq!(pkg_dir, workspace.join("home:test").join("python-demo"));
    }

    #[test]
    fn remote_build_log_path_includes_attempt_and_package() {
        let cwd = std::env::current_dir().expect("cwd");
        let workspace = cwd.join("workspaces").join("python-fontmath");
        let path = remote_build_log_path(&workspace, "python-fontmath", 2);
        assert_eq!(
            path,
            cwd.join("workspaces")
                .join("python-fontmath")
                .join("logs")
                .join("remote_build_python-fontmath_attempt_002.log")
        );
    }
}
