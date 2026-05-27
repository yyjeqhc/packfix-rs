use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use tracing::{info, warn};

use crate::core::graph::PackageSource;
use crate::core::scheduler::BuildRequest;
use crate::{
    cli::Cli,
    config, git,
    obs::{api as ebf, local as osc},
    report::{self},
    spec, upstream,
};

#[derive(Debug, Clone)]
pub struct BuildPythonPackageRequest {
    pub pypi_name: String,
    pub version: Option<String>,
    pub obs_project: Option<String>,
    pub revision: Option<String>,
    pub feature_declarations: Vec<String>,
}

pub async fn build_python_package(
    request: BuildPythonPackageRequest,
    cli: &Cli,
    cfg: Option<&config::PackfixConfig>,
) -> Result<report::Report> {
    let root = build_scheduler_request(&request);
    let BuildPythonPackageRequest {
        pypi_name,
        obs_project,
        revision,
        ..
    } = request;

    let project = resolve_project(obs_project, cli, cfg);
    let rev = revision.unwrap_or_else(|| pypi_name.clone());

    let scheduler = crate::core::scheduler::BuildScheduler::new(cli.clone(), cfg.cloned());
    let results = scheduler.run(vec![root], project, rev).await?;

    node_results_to_report(&pypi_name, results)
}

pub async fn build_python_packages(
    requests: Vec<BuildPythonPackageRequest>,
    cli: &Cli,
    cfg: Option<&config::PackfixConfig>,
) -> Result<Vec<report::Report>> {
    let first = requests
        .first()
        .context("at least one build request is required")?;
    let project = resolve_project(first.obs_project.clone(), cli, cfg);
    let revision = if let Some(revision) = &first.revision {
        revision.clone()
    } else if requests.len() == 1 {
        first.pypi_name.clone()
    } else {
        default_multi_build_revision()
    };

    let roots: Vec<BuildRequest> = requests.iter().map(build_scheduler_request).collect();
    let root_inputs: Vec<String> = requests
        .iter()
        .map(|request| request.pypi_name.clone())
        .collect();

    let scheduler = crate::core::scheduler::BuildScheduler::new(cli.clone(), cfg.cloned());
    let results = scheduler.run(roots, project, revision).await?;

    node_results_to_reports(&root_inputs, results)
}

pub async fn build_existing_python_package(
    package: String,
    obs_project: Option<String>,
    revision: Option<String>,
    cli: &Cli,
    cfg: Option<&config::PackfixConfig>,
    _build_stack: Vec<String>,
) -> Result<report::Report> {
    let project = resolve_project(obs_project, cli, cfg);
    let normalized_package = normalize_existing_repo_package(&package);
    let rev = revision.unwrap_or_else(|| default_existing_revision(&normalized_package));

    let scheduler = crate::core::scheduler::BuildScheduler::new(cli.clone(), cfg.cloned());
    let results = scheduler
        .run(
            vec![build_existing_scheduler_request(&package)],
            project,
            rev,
        )
        .await?;

    node_results_to_report(&package, results)
}

pub async fn fix_local_workdir_with_dependencies(
    workdir: PathBuf,
    cli: &Cli,
    cfg: Option<&config::PackfixConfig>,
) -> Result<report::Report> {
    let root = build_fix_scheduler_request(&workdir)?;
    let root_package = root.package.clone();
    let project = resolve_project(None, cli, cfg);
    let revision = default_existing_revision(&root_package);

    let scheduler = crate::core::scheduler::BuildScheduler::new(cli.clone(), cfg.cloned());
    let results = scheduler.run(vec![root], project, revision).await?;
    let mut report = node_results_to_report(&root_package, results)?;
    report.operation_log_path = Some(workdir.join("logs").join("packfix_operations.log"));
    Ok(report)
}

pub async fn build_existing_python_packages(
    packages: Vec<String>,
    obs_project: Option<String>,
    revision: Option<String>,
    cli: &Cli,
    cfg: Option<&config::PackfixConfig>,
) -> Result<Vec<report::Report>> {
    let project = resolve_project(obs_project, cli, cfg);
    let rev = if let Some(revision) = revision {
        revision
    } else if packages.len() == 1 {
        default_existing_revision(&normalize_existing_repo_package(&packages[0]))
    } else {
        default_multi_build_revision()
    };

    let roots: Vec<BuildRequest> = packages
        .iter()
        .map(|p| build_existing_scheduler_request(p))
        .collect();
    let root_inputs: Vec<String> = packages.to_vec();

    let scheduler = crate::core::scheduler::BuildScheduler::new(cli.clone(), cfg.cloned());
    let results = scheduler.run(roots, project, rev).await?;

    node_results_to_reports(&root_inputs, results)
}

pub async fn fix_local_workdirs_with_dependencies(
    workdirs: Vec<PathBuf>,
    cli: &Cli,
    cfg: Option<&config::PackfixConfig>,
) -> Result<Vec<report::Report>> {
    let roots: Vec<BuildRequest> = workdirs
        .iter()
        .map(|d| build_fix_scheduler_request(d))
        .collect::<Result<Vec<_>>>()?;

    let root_inputs: Vec<String> = roots.iter().map(|r| r.package.clone()).collect();
    let first_input = root_inputs.first().cloned().unwrap_or_default();
    let project = resolve_project(None, cli, cfg);
    let revision = if roots.len() == 1 {
        default_existing_revision(&first_input)
    } else {
        default_multi_build_revision()
    };

    let scheduler = crate::core::scheduler::BuildScheduler::new(cli.clone(), cfg.cloned());
    let results = scheduler.run(roots, project, revision).await?;

    node_results_to_reports(&root_inputs, results)
}

pub async fn update_existing_python_package(
    package: String,
    obs_project: Option<String>,
    cli: &Cli,
    cfg: Option<&config::PackfixConfig>,
) -> Result<report::Report> {
    let normalized_package = normalize_existing_repo_package(&package);
    let branch = update_branch_name(&normalized_package);
    let repo_dir = config::resolve_repo_dir(cfg);

    git::checkout_or_create_branch(&repo_dir, &branch).await?;

    let spec_dir = existing_package_dir(&repo_dir, &normalized_package)?;
    let spec_path = spec::find_spec(&spec_dir)?;
    let original_spec = spec::read_spec(&spec_path)?;
    ensure_pythonhosted_source(&original_spec, &normalized_package)?;

    let pypi_name = infer_update_pypi_name(&original_spec, &normalized_package)?;
    let current_version = spec::tag_value(&original_spec, "Version")
        .context("existing spec is missing Version tag")?;

    let reference = generate_update_reference_spec(&normalized_package, &pypi_name, cli).await?;
    let reference_spec = spec::read_spec(&reference.spec_path)?;
    let latest_version = spec::tag_value(&reference_spec, "Version")
        .context("generated reference spec is missing Version tag")?;
    let remote_asset_line = extract_remote_asset_line(&reference_spec)
        .context("generated reference spec is missing #!RemoteAsset line")?;

    let version_updated = is_newer_python_version(&latest_version, &current_version);
    let mut updated_spec = original_spec.clone();
    if version_updated {
        updated_spec = spec::update_version(&updated_spec, &latest_version);
    }
    updated_spec = spec::ensure_remote_asset(&updated_spec, &remote_asset_line);
    updated_spec = spec::ensure_versioned_python_provides(&updated_spec);
    updated_spec = spec::ensure_buildarch_noarch(&updated_spec);
    updated_spec = spec::ensure_autochangelog_macro(&updated_spec);

    if updated_spec != original_spec {
        spec::write_spec(&spec_path, &updated_spec)?;
        let rel = spec_path
            .strip_prefix(&repo_dir)
            .context("updated spec path must be inside repo dir")?
            .display()
            .to_string();
        git::add(&repo_dir, &[&rel]).await?;
        let commit_message = update_commit_message(&normalized_package, version_updated);
        let committed = git::commit_if_staged(&repo_dir, &commit_message).await?;
        if committed {
            info!(
                package = %normalized_package,
                version_updated,
                "update spec changes committed before build"
            );
        } else {
            info!(package = %normalized_package, "no staged update spec changes to commit");
        }
    } else {
        info!(package = %normalized_package, "update produced no spec text changes before build");
    }

    build_existing_python_package(
        normalized_package,
        obs_project,
        Some(branch),
        cli,
        cfg,
        Vec::new(),
    )
    .await
}

fn build_scheduler_request(request: &BuildPythonPackageRequest) -> BuildRequest {
    BuildRequest {
        package: upstream::python_package_name(&request.pypi_name),
        features: request.feature_declarations.clone(),
        source: PackageSource::Pypi {
            name: request.pypi_name.clone(),
            version: request.version.clone(),
        },
    }
}

fn build_existing_scheduler_request(package: &str) -> BuildRequest {
    let package = normalize_existing_repo_package(package);
    BuildRequest {
        package: package.clone(),
        features: Vec::new(),
        source: PackageSource::ExistingRepo { package },
    }
}

fn build_fix_scheduler_request(workdir: &Path) -> Result<BuildRequest> {
    let package = infer_local_workdir_package_name(workdir)?;
    Ok(BuildRequest {
        package,
        features: Vec::new(),
        source: PackageSource::LocalWorkdir {
            path: workdir.to_path_buf(),
        },
    })
}

fn infer_local_workdir_package_name(workdir: &Path) -> Result<String> {
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

fn normalize_existing_repo_package(package: &str) -> String {
    upstream::python_package_name(package)
}

fn update_branch_name(package: &str) -> String {
    format!("update_{package}")
}

fn update_commit_message(package: &str, version_updated: bool) -> String {
    if version_updated {
        format!("SPECS: {package}: Update version and format spec.")
    } else {
        format!("SPECS: {package}: Format spec.")
    }
}

fn ensure_pythonhosted_source(spec_text: &str, package_name: &str) -> Result<()> {
    if spec_text.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with("Source") && trimmed.contains("files.pythonhosted.org")
    }) {
        return Ok(());
    }

    anyhow::bail!(
        "package {} is not sourced from files.pythonhosted.org; update only supports PyPI source packages",
        package_name
    )
}

fn infer_update_pypi_name(spec_text: &str, package_name: &str) -> Result<String> {
    if let Some(pypi_name) = global_macro_value(spec_text, "pypi_name") {
        return Ok(pypi_name);
    }
    if let Some(srcname) = global_macro_value(spec_text, "srcname") {
        return Ok(srcname);
    }
    Ok(upstream::python_dist_name(package_name))
}

fn global_macro_value(spec_text: &str, name: &str) -> Option<String> {
    let prefix = format!("%global {name} ");
    spec_text.lines().find_map(|line| {
        line.trim_start()
            .strip_prefix(&prefix)
            .map(str::trim)
            .filter(|value| !value.is_empty() && !value.contains("%{"))
            .map(String::from)
    })
}

async fn generate_update_reference_spec(
    package_name: &str,
    pypi_name: &str,
    cli: &Cli,
) -> Result<upstream::TakopackResult> {
    let output_root = takopack_output_root(package_name)?;
    upstream::generate_python_spec_async(pypi_name, None, &output_root, &cli.takopack_bin).await
}

fn extract_remote_asset_line(spec_text: &str) -> Option<String> {
    spec_text
        .lines()
        .find(|line| line.trim_start().starts_with("#!RemoteAsset"))
        .map(String::from)
}

fn is_newer_python_version(candidate: &str, current: &str) -> bool {
    compare_python_versions(candidate, current).is_gt()
}

fn compare_python_versions(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let left_tokens = version_tokens(left);
    let right_tokens = version_tokens(right);
    let max_len = left_tokens.len().max(right_tokens.len());

    for idx in 0..max_len {
        let left_token = left_tokens.get(idx);
        let right_token = right_tokens.get(idx);
        match (left_token, right_token) {
            (Some(VersionToken::Number(a)), Some(VersionToken::Number(b))) => match a.cmp(b) {
                Ordering::Equal => {}
                ordering => return ordering,
            },
            (Some(VersionToken::Text(a)), Some(VersionToken::Text(b))) => match a.cmp(b) {
                Ordering::Equal => {}
                ordering => return ordering,
            },
            (Some(VersionToken::Number(_)), Some(VersionToken::Text(_))) => {
                return Ordering::Greater;
            }
            (Some(VersionToken::Text(_)), Some(VersionToken::Number(_))) => {
                return Ordering::Less;
            }
            (Some(token), None) => {
                if !token.is_zero_like() {
                    return Ordering::Greater;
                }
            }
            (None, Some(token)) => {
                if !token.is_zero_like() {
                    return Ordering::Less;
                }
            }
            (None, None) => break,
        }
    }

    Ordering::Equal
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionToken {
    Number(u64),
    Text(String),
}

impl VersionToken {
    fn is_zero_like(&self) -> bool {
        match self {
            Self::Number(value) => *value == 0,
            Self::Text(text) => text.is_empty(),
        }
    }
}

fn version_tokens(version: &str) -> Vec<VersionToken> {
    let mut tokens = Vec::new();
    let mut chars = version.chars().peekable();

    while let Some(ch) = chars.peek().copied() {
        if ch.is_ascii_digit() {
            let mut number = String::new();
            while let Some(next) = chars.peek().copied() {
                if next.is_ascii_digit() {
                    number.push(next);
                    let _ = chars.next();
                } else {
                    break;
                }
            }
            tokens.push(VersionToken::Number(number.parse().unwrap_or(0)));
            continue;
        }

        if ch.is_ascii_alphabetic() {
            let mut text = String::new();
            while let Some(next) = chars.peek().copied() {
                if next.is_ascii_alphabetic() {
                    text.push(next.to_ascii_lowercase());
                    let _ = chars.next();
                } else {
                    break;
                }
            }
            tokens.push(VersionToken::Text(text));
            continue;
        }

        let _ = chars.next();
    }

    tokens
}

pub(crate) fn existing_package_dir(repo_dir: &Path, package_name: &str) -> Result<PathBuf> {
    existing_package_dir_if_present(repo_dir, package_name).ok_or_else(|| {
        anyhow::anyhow!(
            "package {} not found under {}",
            package_name,
            repo_dir.join("SPECS").display()
        )
    })
}

pub(crate) fn existing_package_dir_if_present(
    repo_dir: &Path,
    package_name: &str,
) -> Option<PathBuf> {
    let path = repo_dir.join("SPECS").join(package_name);
    path.exists().then_some(path)
}

fn default_existing_revision(package_name: &str) -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("rebuild-{package_name}-{stamp}")
}

fn default_multi_build_revision() -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("build-batch-{stamp}")
}

fn resolve_project(
    obs_project: Option<String>,
    cli: &Cli,
    cfg: Option<&config::PackfixConfig>,
) -> String {
    obs_project
        .or(cli.default_obs_project.clone())
        .unwrap_or_else(|| config::resolve_default_obs_project(None, cfg))
}

pub(crate) async fn checkout_remote_package(
    checkout_root: &Path,
    project: &str,
    package_name: &str,
    repository: &str,
    arch: &str,
    osc_bin: &Path,
) -> Result<PathBuf> {
    info!(project = %project, package = %package_name, "waiting for OBS source checkout readiness");
    let mut ready = false;
    for poll in 1..=30 {
        let delay = poll_delay_secs(poll);
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        }
        match osc::osc_api_status_async(project, package_name, repository, arch, osc_bin).await {
            Ok(result) => {
                let s = &result.stdout;
                if is_source_checkout_ready(s) {
                    ready = true;
                    info!(poll, "OBS source checkout is ready");
                    break;
                }
                info!(
                    poll,
                    status = %s.lines().next().unwrap_or("unknown"),
                    "OBS source status polled"
                );
            }
            Err(e) => warn!(error = %e, poll, "OBS source status poll failed"),
        }
    }
    if !ready {
        anyhow::bail!(
            "timed out waiting for OBS source checkout readiness for {project}/{package_name}"
        );
    }

    let pkg_dir = osc::checkout_target_dir(checkout_root, project, Some(package_name));
    if pkg_dir.exists() {
        info!(
            workdir = %pkg_dir.display(),
            "existing osc working copy detected; removing before checkout"
        );
        std::fs::remove_dir_all(&pkg_dir).with_context(|| {
            format!(
                "failed to remove existing osc working copy before checkout: {}",
                pkg_dir.display()
            )
        })?;
    }

    info!(project = %project, package = %package_name, "starting osc checkout");
    let checkout =
        osc::osc_checkout_async(project, Some(package_name), Some(checkout_root), osc_bin).await?;
    if !checkout.success {
        anyhow::bail!(
            "osc checkout failed for {project}/{package_name} (rc={}): {}",
            checkout.returncode,
            checkout.stderr
        );
    }
    info!(workdir = %pkg_dir.display(), "starting osc update");
    let update = osc::osc_update_async(&pkg_dir, osc_bin).await?;
    if !update.success {
        anyhow::bail!(
            "osc update failed for {} (rc={}): {}",
            pkg_dir.display(),
            update.returncode,
            update.stderr
        );
    }
    osc::sanitize_checkout_dir(&pkg_dir)?;
    crate::extract::extract_source_if_present(&pkg_dir, checkout_root);
    info!(workdir = %pkg_dir.display(), "checkout directory ready");
    Ok(pkg_dir)
}

pub(crate) fn copy_spec_back(
    repo_dir: &Path,
    spec_path: &Path,
    package_name: &str,
) -> Result<PathBuf> {
    let target_dir = existing_package_dir(repo_dir, package_name)?;
    let target_spec = spec::find_spec(&target_dir)?;
    std::fs::copy(spec_path, &target_spec).with_context(|| {
        format!(
            "failed to copy {} to {}",
            spec_path.display(),
            target_spec.display()
        )
    })?;
    Ok(target_spec)
}

pub(crate) fn takopack_output_root(package_name: &str) -> Result<PathBuf> {
    let root = std::env::current_dir()?
        .join("workspaces")
        .join("takopack")
        .join(package_name);
    if root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    std::fs::create_dir_all(&root)?;
    Ok(root)
}

pub(crate) fn stage_generated_package_dir(
    repo_dir: &Path,
    package_name: &str,
    spec_path: &Path,
) -> Result<PathBuf> {
    let generated_dir = spec_path
        .parent()
        .context("generated spec must have parent directory")?;
    let target_dir = repo_dir.join("SPECS").join(package_name);
    if target_dir.exists() && !is_dir_empty(&target_dir) {
        anyhow::bail!(
            "refusing to overwrite existing non-empty package directory {}; \
             remove it manually if you want to regenerate",
            target_dir.display()
        );
    }
    copy_dir_all(generated_dir, &target_dir)?;
    Ok(target_dir)
}

fn is_dir_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
}

pub(crate) fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

pub(crate) fn ensure_ebf_success(result: &ebf::EbfSubmitResult) -> Result<()> {
    if result.success {
        return Ok(());
    }
    anyhow::bail!(
        "ebf submit failed: {}/{} succeeded; stderr: {}",
        result.success_count,
        result.total_count,
        result.stderr
    )
}

pub(crate) fn is_source_checkout_ready(status: &str) -> bool {
    let trimmed = status.trim();
    !trimmed.is_empty() && !trimmed.contains("blocked") && !trimmed.contains("broken")
}

pub(crate) fn poll_delay_secs(poll_index: usize) -> u64 {
    match poll_index {
        0 | 1 => 0,
        2 => 5,
        3 => 10,
        4 => 15,
        _ => 20,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteBuildState {
    Succeeded,
    Failed { headline: String },
    Pending { headline: String },
}

pub(crate) fn classify_remote_build_status(status_text: &str) -> RemoteBuildState {
    let headline = status_text
        .lines()
        .next()
        .unwrap_or("unknown")
        .trim()
        .to_string();
    // Failure/broken states take priority over succeeded.  A status output that
    // mentions both (e.g. multi-package query, or historical log lines) must not
    // be classified as Succeeded.
    if status_text.contains("failed")
        || status_text.contains("broken")
        || status_text.contains("unresolvable")
        || status_text.contains("disabled")
        || status_text.contains("excluded")
    {
        return RemoteBuildState::Failed { headline };
    }
    if status_text.contains("succeeded") {
        return RemoteBuildState::Succeeded;
    }
    RemoteBuildState::Pending { headline }
}

pub(crate) async fn wait_for_remote_build_success(
    project: &str,
    package: &str,
    repository: &str,
    arch: &str,
    osc_bin: &Path,
) -> Result<()> {
    for poll in 1..=30 {
        let delay = poll_delay_secs(poll);
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        }
        let result =
            match osc::osc_api_status_async(project, package, repository, arch, osc_bin).await {
                Ok(result) => result,
                Err(err) => {
                    warn!(error = %err, poll, "OBS build status poll failed");
                    continue;
                }
            };

        let status_text = result.stdout;
        match classify_remote_build_status(&status_text) {
            RemoteBuildState::Succeeded => {
                info!(poll, "OBS build succeeded");
                return Ok(());
            }
            RemoteBuildState::Failed { headline } => {
                anyhow::bail!("remote build failed: {headline}");
            }
            RemoteBuildState::Pending { headline } => {
                info!(poll, status = %headline, "OBS build status polled");
            }
        }
    }

    anyhow::bail!("timed out waiting for remote build success")
}

fn node_results_to_report(
    root_package_input: &str,
    results: Vec<crate::core::scheduler::NodeResult>,
) -> Result<report::Report> {
    let mut root_packages = BTreeSet::new();
    root_packages.insert(upstream::python_package_name(root_package_input));
    node_results_to_report_with_root_set(root_package_input, &root_packages, results)
}

fn node_results_to_reports(
    root_package_inputs: &[String],
    results: Vec<crate::core::scheduler::NodeResult>,
) -> Result<Vec<report::Report>> {
    let root_packages: BTreeSet<String> = root_package_inputs
        .iter()
        .map(|input| upstream::python_package_name(input))
        .collect();
    root_package_inputs
        .iter()
        .map(|input| node_results_to_report_with_root_set(input, &root_packages, results.clone()))
        .collect()
}

fn node_results_to_report_with_root_set(
    root_package_input: &str,
    root_packages: &BTreeSet<String>,
    results: Vec<crate::core::scheduler::NodeResult>,
) -> Result<report::Report> {
    let root_package = upstream::python_package_name(root_package_input);
    let root_result = results
        .iter()
        .find(|r| r.package == root_package)
        .context("root node result not found in scheduler output")?;

    let mut report = report::Report::new(match &root_result.outcome {
        crate::core::engine::BuildOutcome::Success { .. } => report::Status::BuildSuccess,
        _ => report::Status::Failed,
    });
    report.package_name = Some(root_package.clone());
    report.operation_log_path = Some(
        std::env::current_dir()?
            .join("workspaces")
            .join(&root_package)
            .join("logs")
            .join("packfix_operations.log"),
    );

    match &root_result.outcome {
        crate::core::engine::BuildOutcome::Failed(reason) => {
            report.notes.push(format!("root build failed: {reason}"));
        }
        crate::core::engine::BuildOutcome::NeedsDependencies(deps) => {
            report.notes.push(format!(
                "root build needs dependencies: {}",
                deps.iter()
                    .map(|d| d.package.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        crate::core::engine::BuildOutcome::Success { report: inner } => {
            if let Some(inner) = inner {
                report.build_attempts = inner.build_attempts;
                report.fixes_applied = inner.fixes_applied;
                report.last_log_path = inner.last_log_path.clone();
                report.notes.extend(inner.notes.clone());
                report.notes.push(format!(
                    "local build succeeded after {} attempts, {} fixes applied",
                    inner.build_attempts, inner.fixes_applied
                ));
            } else {
                report.notes.push("root build succeeded".into());
            }
        }
    }

    for r in &results {
        if r.package != root_package && !root_packages.contains(&r.package) {
            let status = match &r.outcome {
                crate::core::engine::BuildOutcome::Success { .. } => "success",
                crate::core::engine::BuildOutcome::Failed(reason) => {
                    report
                        .notes
                        .push(format!("  dependency {}: failed ({})", r.package, reason));
                    continue;
                }
                crate::core::engine::BuildOutcome::NeedsDependencies(_) => "needs-deps",
            };
            report
                .notes
                .push(format!("  dependency {}: {}", r.package, status));
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn build_request_uses_rpm_key_but_pypi_source_name() {
        let request = BuildPythonPackageRequest {
            pypi_name: "foo".into(),
            version: Some("1.2.3".into()),
            obs_project: None,
            revision: None,
            feature_declarations: vec!["lxml".into()],
        };

        let root = build_scheduler_request(&request);
        assert_eq!(root.package, "python-foo");
        assert_eq!(root.features, vec!["lxml"]);
        assert_eq!(
            root.source,
            PackageSource::Pypi {
                name: "foo".into(),
                version: Some("1.2.3".into())
            }
        );
    }

    #[test]
    fn build_existing_normalizes_plain_name_to_rpm_name() {
        let root = build_existing_scheduler_request("cffsubr");
        assert_eq!(root.package, "python-cffsubr");
        assert_eq!(
            root.source,
            PackageSource::ExistingRepo {
                package: "python-cffsubr".into()
            }
        );
    }

    #[test]
    fn fix_command_build_request_uses_local_workdir_source() {
        let dir = tempdir().expect("tempdir");
        let spec_path = dir.path().join("demo.spec");
        std::fs::write(&spec_path, "Name: python-demo\n").expect("write spec");

        let request = build_fix_scheduler_request(dir.path()).expect("fix build request");
        assert_eq!(request.package, "python-demo");
        assert_eq!(
            request.source,
            PackageSource::LocalWorkdir {
                path: dir.path().to_path_buf()
            }
        );
    }

    #[test]
    fn copy_spec_back_targets_repo_package_dir() {
        let dir = tempdir().expect("tempdir");
        let repo_dir = dir.path().join("repo");
        let pkg_dir = repo_dir.join("SPECS").join("python-demo");
        std::fs::create_dir_all(&pkg_dir).expect("create package dir");

        let repo_spec = pkg_dir.join("python-demo.spec");
        std::fs::write(&repo_spec, "Name: python-demo\n").expect("write repo spec");

        let local_checkout = dir.path().join("checkout");
        std::fs::create_dir_all(&local_checkout).expect("create checkout dir");
        let local_spec = local_checkout.join("python-demo.spec");
        std::fs::write(&local_spec, "Name: python-demo\nRelease: 2\n").expect("write local spec");

        let copied = copy_spec_back(&repo_dir, &local_spec, "python-demo").expect("copy spec");
        assert_eq!(copied, repo_spec);
        let content = std::fs::read_to_string(&repo_spec).expect("read copied spec");
        assert!(content.contains("Release: 2"));
    }

    #[test]
    fn ensure_ebf_success_returns_err_on_failure() {
        let result = ebf::EbfSubmitResult {
            success: false,
            returncode: 1,
            log_path: PathBuf::from("logs/test.log"),
            stdout: String::new(),
            stderr: "boom".into(),
            success_count: 0,
            total_count: 2,
        };
        let err = ensure_ebf_success(&result).expect_err("expected ebf failure");
        assert!(err.to_string().contains("0/2"));
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn source_checkout_ready_keeps_current_semantics() {
        assert!(!is_source_checkout_ready("blocked"));
        assert!(!is_source_checkout_ready("broken"));
        assert!(is_source_checkout_ready("building"));
        assert!(is_source_checkout_ready("succeeded"));
        assert!(is_source_checkout_ready("finished"));
        assert!(is_source_checkout_ready("  building  "));
        assert!(!is_source_checkout_ready(""));
    }

    #[test]
    fn poll_delay_schedule_is_0_5_10_15_20() {
        assert_eq!(poll_delay_secs(1), 0);
        assert_eq!(poll_delay_secs(2), 5);
        assert_eq!(poll_delay_secs(3), 10);
        assert_eq!(poll_delay_secs(4), 15);
        assert_eq!(poll_delay_secs(5), 20);
        assert_eq!(poll_delay_secs(6), 20);
    }

    #[test]
    fn classify_status_succeeded() {
        assert_eq!(
            classify_remote_build_status("succeeded"),
            RemoteBuildState::Succeeded
        );
    }

    #[test]
    fn classify_status_succeeded_with_extra_text() {
        assert_eq!(
            classify_remote_build_status("succeeded\nmore text"),
            RemoteBuildState::Succeeded
        );
    }

    #[test]
    fn classify_status_failed() {
        assert_eq!(
            classify_remote_build_status("failed"),
            RemoteBuildState::Failed {
                headline: "failed".into()
            }
        );
    }

    #[test]
    fn classify_status_broken() {
        assert_eq!(
            classify_remote_build_status("broken: dependency issue"),
            RemoteBuildState::Failed {
                headline: "broken: dependency issue".into()
            }
        );
    }

    #[test]
    fn classify_status_unresolvable() {
        assert_eq!(
            classify_remote_build_status("unresolvable: nothing provides foo"),
            RemoteBuildState::Failed {
                headline: "unresolvable: nothing provides foo".into()
            }
        );
    }

    #[test]
    fn classify_status_failed_takes_priority_over_succeeded() {
        // Multi-line output where one line says succeeded and another says failed
        assert_eq!(
            classify_remote_build_status("succeeded\nfailed"),
            RemoteBuildState::Failed {
                headline: "succeeded".into()
            }
        );
    }

    #[test]
    fn classify_status_broken_takes_priority_over_succeeded() {
        assert_eq!(
            classify_remote_build_status("foo succeeded\nbar broken"),
            RemoteBuildState::Failed {
                headline: "foo succeeded".into()
            }
        );
    }

    #[test]
    fn classify_status_building_is_pending() {
        assert_eq!(
            classify_remote_build_status("building"),
            RemoteBuildState::Pending {
                headline: "building".into()
            }
        );
    }

    #[test]
    fn classify_status_scheduled_is_pending() {
        assert_eq!(
            classify_remote_build_status("scheduled"),
            RemoteBuildState::Pending {
                headline: "scheduled".into()
            }
        );
    }

    #[test]
    fn classify_status_running_is_pending() {
        assert_eq!(
            classify_remote_build_status("running"),
            RemoteBuildState::Pending {
                headline: "running".into()
            }
        );
    }

    #[test]
    fn classify_status_disabled_is_failed() {
        assert_eq!(
            classify_remote_build_status("disabled"),
            RemoteBuildState::Failed {
                headline: "disabled".into()
            }
        );
    }

    #[test]
    fn classify_status_excluded_is_failed() {
        assert_eq!(
            classify_remote_build_status("excluded"),
            RemoteBuildState::Failed {
                headline: "excluded".into()
            }
        );
    }

    #[test]
    fn classify_status_empty_is_pending() {
        assert_eq!(
            classify_remote_build_status(""),
            RemoteBuildState::Pending {
                headline: "unknown".into()
            }
        );
    }

    #[test]
    fn classify_status_unknown_output_is_pending() {
        assert_eq!(
            classify_remote_build_status("some random text"),
            RemoteBuildState::Pending {
                headline: "some random text".into()
            }
        );
    }

    #[test]
    fn node_results_to_report_finds_pypi_root_result() {
        let report = node_results_to_report(
            "fonttools",
            vec![
                crate::core::scheduler::NodeResult {
                    package: "python-fonttools".into(),
                    outcome: crate::core::engine::BuildOutcome::Success { report: None },
                },
                crate::core::scheduler::NodeResult {
                    package: "python-lxml".into(),
                    outcome: crate::core::engine::BuildOutcome::Success { report: None },
                },
            ],
        )
        .expect("report");

        assert_eq!(report.package_name.as_deref(), Some("python-fonttools"));
        assert!(matches!(report.status, crate::report::Status::BuildSuccess));
    }

    #[test]
    fn multi_root_reports_skip_other_roots_in_dependency_notes() {
        let reports = node_results_to_reports(
            &["fonttools".into(), "ufolib2".into()],
            vec![
                crate::core::scheduler::NodeResult {
                    package: "python-fonttools".into(),
                    outcome: crate::core::engine::BuildOutcome::Success { report: None },
                },
                crate::core::scheduler::NodeResult {
                    package: "python-ufolib2".into(),
                    outcome: crate::core::engine::BuildOutcome::Success { report: None },
                },
                crate::core::scheduler::NodeResult {
                    package: "python-lxml".into(),
                    outcome: crate::core::engine::BuildOutcome::Success { report: None },
                },
            ],
        )
        .expect("reports");

        assert_eq!(reports.len(), 2);
        assert!(
            reports[0]
                .notes
                .iter()
                .all(|note| !note.contains("python-ufolib2"))
        );
        assert!(
            reports[1]
                .notes
                .iter()
                .all(|note| !note.contains("python-fonttools"))
        );
    }

    #[test]
    fn update_branch_name_uses_package_prefix() {
        assert_eq!(
            update_branch_name("python-authlib"),
            "update_python-authlib"
        );
    }

    #[test]
    fn update_commit_message_reflects_version_change() {
        assert_eq!(
            update_commit_message("python-authlib", true),
            "SPECS: python-authlib: Update version and format spec."
        );
        assert_eq!(
            update_commit_message("python-authlib", false),
            "SPECS: python-authlib: Format spec."
        );
    }

    #[test]
    fn infer_update_pypi_name_prefers_pypi_name_macro() {
        let spec_text = "%global pypi_name nvidia_ml_py\n%global srcname nvidia-ml-py\n";
        assert_eq!(
            infer_update_pypi_name(spec_text, "python-nvidia-ml-py").expect("pypi name"),
            "nvidia_ml_py"
        );
    }

    #[test]
    fn ensure_pythonhosted_source_accepts_source_without_index_suffix() {
        let spec_text = "Source:         https://files.pythonhosted.org/packages/source/a/authlib/authlib-1.0.tar.gz\n";
        ensure_pythonhosted_source(spec_text, "python-authlib").expect("pythonhosted source");
    }

    #[test]
    fn compare_python_versions_detects_newer_release() {
        assert!(is_newer_python_version("1.7.0", "1.6.9"));
        assert!(!is_newer_python_version("1.7.0", "1.7.0"));
        assert!(!is_newer_python_version("1.6.9", "1.7.0"));
    }

    #[test]
    fn default_multi_build_revision_has_batch_prefix() {
        assert!(default_multi_build_revision().starts_with("build-batch-"));
    }

    // ── multi-package BuildExisting / multi-workdir Fix ────────────────

    #[test]
    fn build_existing_scheduler_requests_normalize_multiple_packages() {
        let reqs: Vec<BuildRequest> = ["cffsubr", "arrow"]
            .iter()
            .map(|p| build_existing_scheduler_request(p))
            .collect();

        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].package, "python-cffsubr");
        assert_eq!(
            reqs[0].source,
            PackageSource::ExistingRepo {
                package: "python-cffsubr".into()
            }
        );
        assert_eq!(reqs[1].package, "python-arrow");
        assert_eq!(
            reqs[1].source,
            PackageSource::ExistingRepo {
                package: "python-arrow".into()
            }
        );
    }

    #[test]
    fn fix_scheduler_requests_for_multiple_workdirs() {
        let dir1 = tempdir().expect("tempdir1");
        std::fs::write(dir1.path().join("demo.spec"), "Name: python-demo\n").expect("write spec");

        let dir2 = tempdir().expect("tempdir2");
        std::fs::write(dir2.path().join("example.spec"), "Name: python-example\n")
            .expect("write spec");

        let reqs: Vec<BuildRequest> = [dir1.path().to_path_buf(), dir2.path().to_path_buf()]
            .iter()
            .map(|d| build_fix_scheduler_request(d))
            .collect::<Result<Vec<_>>>()
            .expect("build fix requests");

        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].package, "python-demo");
        assert!(matches!(reqs[0].source, PackageSource::LocalWorkdir { .. }));
        assert_eq!(reqs[1].package, "python-example");
        assert!(matches!(reqs[1].source, PackageSource::LocalWorkdir { .. }));
    }

    #[test]
    fn node_results_to_reports_maps_each_root_to_its_own_report() {
        let reports = node_results_to_reports(
            &["fonttools".to_string(), "lxml".to_string()],
            vec![
                crate::core::scheduler::NodeResult {
                    package: "python-fonttools".into(),
                    outcome: crate::core::engine::BuildOutcome::Success { report: None },
                },
                crate::core::scheduler::NodeResult {
                    package: "python-lxml".into(),
                    outcome: crate::core::engine::BuildOutcome::Failed("oops".into()),
                },
            ],
        )
        .expect("reports");

        assert_eq!(reports.len(), 2);
        assert!(matches!(reports[0].status, report::Status::BuildSuccess));
        assert!(matches!(reports[1].status, report::Status::Failed));
    }

    #[test]
    fn node_results_missing_root_produces_error() {
        let err = node_results_to_reports(
            &["fonttools".to_string(), "nonexistent".to_string()],
            vec![crate::core::scheduler::NodeResult {
                package: "python-fonttools".into(),
                outcome: crate::core::engine::BuildOutcome::Success { report: None },
            }],
        )
        .expect_err("missing root should error");

        assert!(
            err.to_string().contains("root node result not found"),
            "expected 'root node result not found' error, got: {err}"
        );
    }

    #[test]
    fn stage_generated_creates_target_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let generated = repo.join("gen").join("mypkg");
        std::fs::create_dir_all(&generated).unwrap();
        std::fs::write(generated.join("python-mypkg.spec"), "Name: python-mypkg\n").unwrap();

        let staged =
            stage_generated_package_dir(repo, "python-mypkg", &generated.join("python-mypkg.spec"))
                .unwrap();
        assert!(staged.join("python-mypkg.spec").exists());
    }

    #[test]
    fn stage_generated_succeeds_when_target_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let target = repo.join("SPECS").join("python-mypkg");
        std::fs::create_dir_all(&target).unwrap(); // empty dir
        let generated = repo.join("gen");
        std::fs::create_dir_all(&generated).unwrap();
        std::fs::write(generated.join("python-mypkg.spec"), "Name: python-mypkg\n").unwrap();

        let staged =
            stage_generated_package_dir(repo, "python-mypkg", &generated.join("python-mypkg.spec"))
                .unwrap();
        assert!(staged.join("python-mypkg.spec").exists());
    }

    #[test]
    fn stage_generated_errors_when_target_is_nonempty() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let target = repo.join("SPECS").join("python-mypkg");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("existing-file.txt"), "important data\n").unwrap();

        let generated = repo.join("gen");
        std::fs::create_dir_all(&generated).unwrap();
        std::fs::write(generated.join("python-mypkg.spec"), "Name: python-mypkg\n").unwrap();

        let err =
            stage_generated_package_dir(repo, "python-mypkg", &generated.join("python-mypkg.spec"))
                .unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains("refusing to overwrite"), "got: {msg}");
        assert!(
            msg.contains("python-mypkg"),
            "error must include path, got: {msg}"
        );
    }

    #[test]
    fn stage_generated_error_preserves_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let target = repo.join("SPECS").join("python-mypkg");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("precious.txt"), "do not delete\n").unwrap();

        let generated = repo.join("gen");
        std::fs::create_dir_all(&generated).unwrap();
        std::fs::write(generated.join("python-mypkg.spec"), "Name: python-mypkg\n").unwrap();

        let _ =
            stage_generated_package_dir(repo, "python-mypkg", &generated.join("python-mypkg.spec"));

        // The existing file must still be there
        assert_eq!(
            std::fs::read_to_string(target.join("precious.txt")).unwrap(),
            "do not delete\n"
        );
    }
}
