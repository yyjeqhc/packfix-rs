use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use similar::TextDiff;
use tracing::{info, warn};

use crate::{
    cli::Cli,
    core::BuildIssue,
    core::resources::LocalBuildPool,
    fix,
    fix::analyzer::analyze_log,
    fix::fixer::{apply_action, decide_action},
    obs::local as osc,
    report::{AppliedFix, Report, Status},
    spec, upstream,
    utils::{llm::TextSuggestion, ops_log},
};

#[derive(Debug, Clone)]
pub enum WorkflowMode {
    New {
        pypi_name: String,
        version: Option<String>,
    },
    Fix {
        workdir: PathBuf,
    },
    DryRun {
        workdir: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct WorkflowConfig {
    pub mode: WorkflowMode,
    pub project: Option<String>,
    pub repository: String,
    pub arch: String,
    pub max_retries: usize,
    pub takopack_bin: PathBuf,
    pub osc_bin: PathBuf,
    pub ollama_host: String,
    pub ollama_port: u16,
    pub model: String,
    pub apply_text: bool,
    pub local_build_pool: Option<Arc<LocalBuildPool>>,
    pub llm_description: bool,
    pub llm_semaphore: Option<crate::core::resources::LlmSemaphore>,
    // Resolved description config (filled by resolve_description_from_config).
    pub description_system_prompt: String,
    pub description_user_prompt: String,
    pub description_timeout_secs: u64,
    pub description_max_context_chars: usize,
    pub description_num_predict: i32,
    pub description_temperature: f32,
}

impl WorkflowConfig {
    pub fn from_cli(cli: &Cli, mode: WorkflowMode) -> Self {
        Self {
            mode,
            project: cli.project.clone(),
            repository: cli.repository.clone(),
            arch: cli.arch.clone(),
            max_retries: cli.max_retries,
            takopack_bin: cli.takopack_bin.clone(),
            osc_bin: cli.osc_bin.clone(),
            ollama_host: cli.ollama_host.clone(),
            ollama_port: cli.ollama_port,
            model: cli.model.clone(),
            apply_text: cli.apply_text,
            local_build_pool: None,
            llm_description: if cli.no_llm_description {
                false
            } else {
                cli.llm_description
            },
            llm_semaphore: None,
            description_system_prompt: crate::config::resolve_description_system_prompt(None),
            description_user_prompt: crate::config::resolve_description_user_prompt(None),
            description_timeout_secs: crate::config::resolve_description_timeout_secs(None),
            description_max_context_chars: crate::config::resolve_description_max_context_chars(
                None,
            ),
            description_num_predict: crate::config::resolve_description_num_predict(None),
            description_temperature: crate::config::resolve_description_temperature(None),
        }
    }

    /// Override description fields from a loaded `PackfixConfig`.
    pub fn with_description_config(mut self, cfg: Option<&crate::config::PackfixConfig>) -> Self {
        self.description_system_prompt = crate::config::resolve_description_system_prompt(cfg);
        self.description_user_prompt = crate::config::resolve_description_user_prompt(cfg);
        self.description_timeout_secs = crate::config::resolve_description_timeout_secs(cfg);
        self.description_max_context_chars =
            crate::config::resolve_description_max_context_chars(cfg);
        self.description_num_predict = crate::config::resolve_description_num_predict(cfg);
        self.description_temperature = crate::config::resolve_description_temperature(cfg);
        self
    }
}

pub async fn run_workflow(config: WorkflowConfig) -> Result<Report> {
    let (workdir, spec_path, package_name, mut report) = prepare(&config).await?;
    report.spec_path = Some(spec_path.clone());
    report.package_name = package_name;
    report.operation_log_path = Some(ops_log::operation_log_path(&workdir));
    ops_log::log_operation(
        &workdir,
        "workflow-start",
        &[
            format!("MODE: {:?}", config.mode),
            format!("SPEC: {}", spec_path.display()),
            format!("REPOSITORY: {}", config.repository),
            format!("ARCH: {}", config.arch),
        ],
    );

    let spec_text = spec::read_spec(&spec_path)?;
    let normalized = spec::normalize_spec(&spec_text);
    if matches!(config.mode, WorkflowMode::DryRun { .. }) {
        report.status = Status::DryRunComplete;
        report
            .notes
            .push("dry-run: found spec, read spec; osc build skipped".into());
        if normalized != spec_text {
            report
                .notes
                .push("dry-run: spec would be normalized".into());
        }
        ops_log::log_operation(
            &workdir,
            "workflow-dry-run",
            &[
                format!("STATUS: {:?}", report.status),
                "DETAIL: osc build skipped".into(),
            ],
        );
        return Ok(report);
    }

    if normalized != spec_text {
        spec::write_spec(&spec_path, &normalized)?;
        ops_log::log_operation(
            &workdir,
            "spec-normalized",
            &[
                format!("SPEC: {}", spec_path.display()),
                "DETAIL: normalized spec formatting before build".into(),
            ],
        );
    }

    let pkg_label = report.package_name.clone().unwrap_or_else(|| "?".into());

    // LLM %description generation — happens after source extraction, before
    // buildroot slot acquisition, so it does not block build concurrency.
    if config.llm_description {
        try_llm_description(&config, &workdir, &spec_path, &pkg_label, &mut report).await;
    }

    // Acquire a build-root slot from the pool (if one is configured).
    // Held during osc_build, released immediately on success (so the slot is
    // not wasted on LLM / post-processing).  On failure the slot is kept for
    // the retry attempt so that the same --root is reused.
    let mut build_slot: Option<crate::core::resources::LocalBuildSlot> =
        if let Some(pool) = &config.local_build_pool {
            Some(pool.acquire().await?)
        } else {
            None
        };
    let build_root = build_slot.as_ref().map(|s| s.root().to_path_buf());
    if let Some(ref slot) = build_slot {
        info!(
            package = %pkg_label,
            slot_root = %slot.root().display(),
            "slot acquired"
        );
    }

    let build_start = std::time::Instant::now();
    for attempt in 1..=config.max_retries + 1 {
        report.build_attempts = attempt;
        info!(
            package = %pkg_label,
            attempt,
            max_attempts = config.max_retries + 1,
            "attempt started"
        );
        let build = osc::osc_build_async(
            &workdir,
            &spec_path,
            &config.repository,
            &config.arch,
            config.project.as_deref(),
            &config.osc_bin,
            attempt,
            build_root.as_deref(),
        )
        .await?;
        report.last_log_path = Some(build.log_path.clone());
        ops_log::log_operation(
            &workdir,
            "build-attempt",
            &[
                format!("ATTEMPT: {attempt}"),
                format!("SPEC: {}", spec_path.display()),
                format!("BUILD_LOG: {}", build.log_path.display()),
                format!("RETURNCODE: {}", build.returncode),
                format!("SUCCESS: {}", build.success),
            ],
        );
        if build.success {
            report.status = Status::BuildSuccess;
            let elapsed_ms = build_start.elapsed().as_millis();
            info!(
                package = %pkg_label,
                attempt,
                elapsed_ms,
                "local build succeeded"
            );
            // Release the slot immediately — do not hold it during the LLM
            // call so other packages can start building.
            if let Some(slot) = build_slot.take() {
                info!(
                    package = %pkg_label,
                    slot_root = %slot.root().display(),
                    "slot released (post-processing without slot)"
                );
                drop(slot);
            }
            let llm_start = std::time::Instant::now();
            add_text_suggestion(&config, &workdir, &spec_path, &mut report).await;
            info!(
                package = %pkg_label,
                llm_ms = llm_start.elapsed().as_millis(),
                "post-processing done"
            );
            ops_log::log_operation(
                &workdir,
                "build-success",
                &[
                    format!("ATTEMPT: {attempt}"),
                    format!("STATUS: {:?}", report.status),
                    format!("BUILD_LOG: {}", build.log_path.display()),
                ],
            );
            return Ok(report);
        }
        report.notes.push(format!(
            "osc build failed with rc={} (stdout {} bytes, stderr {} bytes)",
            build.returncode,
            build.stdout.len(),
            build.stderr.len()
        ));

        let log = std::fs::read_to_string(&build.log_path)?;
        let issue = fix::contextualize_issue(&workdir, analyze_log(&log))?;
        if let BuildIssue::InstallModuleMismatch {
            ref wrong_module,
            ref suggested_module,
        } = issue
            && wrong_module != suggested_module
        {
            report.notes.push(format!(
                "install module mismatch refined from '{wrong_module}' to '{suggested_module}' using source archive layout"
            ));
        }
        let action = decide_action(&issue);
        let evidence_lines = issue_evidence_lines(&issue, &log);
        info!(
            package = %pkg_label,
            attempt,
            issue = ?issue,
            action = ?action,
            evidence_count = evidence_lines.len(),
            build_log = %build.log_path.display(),
            "attempt failed"
        );
        let max_console_evidence = 5;
        if evidence_lines.is_empty() {
            warn!(attempt, "no focused evidence lines extracted for issue");
        } else {
            for (i, line) in evidence_lines.iter().enumerate() {
                if i >= max_console_evidence {
                    warn!(
                        attempt,
                        remaining = evidence_lines.len() - i,
                        build_log = %build.log_path.display(),
                        "{} more evidence lines in build log",
                        evidence_lines.len() - i
                    );
                    break;
                }
                info!(attempt, evidence = %line, "issue evidence");
            }
        }
        report.final_issue = Some(issue.clone());
        report.final_action = Some(action.clone());
        report.notes.push(format!("classified issue: {issue:?}"));
        report.notes.push(format!("selected action: {action:?}"));
        for line in &evidence_lines {
            report.notes.push(format!("issue evidence: {line}"));
        }
        let mut operation_lines = vec![
            format!("ATTEMPT: {attempt}"),
            format!("BUILD_LOG: {}", build.log_path.display()),
            format!("ISSUE: {issue:?}"),
            format!("ACTION: {action:?}"),
        ];
        if evidence_lines.is_empty() {
            operation_lines.push("EVIDENCE: <none>".into());
        } else {
            for line in &evidence_lines {
                operation_lines.push(format!("EVIDENCE: {line}"));
            }
        }
        ops_log::log_operation(&workdir, "build-failure-analysis", &operation_lines);

        if action.is_need_human() {
            report.status = Status::NeedHuman;
            ops_log::log_operation(
                &workdir,
                "build-stop-need-human",
                &[
                    format!("ATTEMPT: {attempt}"),
                    format!("ISSUE: {issue:?}"),
                    "DETAIL: no safe deterministic fix available".into(),
                ],
            );
            return Ok(report);
        }
        if attempt > config.max_retries {
            report.status = Status::NeedHuman;
            report.notes.push(format!(
                "max retries ({}) exceeded after build attempt {attempt}",
                config.max_retries
            ));
            ops_log::log_operation(
                &workdir,
                "build-stop-max-retries",
                &[
                    format!("ATTEMPT: {attempt}"),
                    format!("MAX_RETRIES: {}", config.max_retries),
                    format!("ISSUE: {issue:?}"),
                ],
            );
            return Ok(report);
        }

        let before = spec::read_spec(&spec_path)?;
        let after = apply_action(&before, &action)?;
        if before == after {
            report.status = Status::NeedHuman;
            report.notes.push(
                "deterministic action produced no spec change; stopping to avoid repeated builds"
                    .into(),
            );
            ops_log::log_operation(
                &workdir,
                "build-stop-no-spec-change",
                &[
                    format!("ATTEMPT: {attempt}"),
                    format!("ACTION: {action:?}"),
                    "DETAIL: deterministic action produced no spec change".into(),
                ],
            );
            return Ok(report);
        } else {
            spec::write_spec(&spec_path, &after)?;
            let diff = TextDiff::from_lines(&before, &after)
                .unified_diff()
                .header("spec.before", "spec.after")
                .to_string();
            info!(
                package = %report.package_name.as_deref().unwrap_or("?"),
                attempt,
                action = ?action,
                "fix applied"
            );
            report.applied_fixes.push(AppliedFix { action, diff });
            report.fixes_applied = report.applied_fixes.len();
            let applied = report
                .applied_fixes
                .last()
                .expect("applied fix must exist after push");
            ops_log::log_operation(
                &workdir,
                "spec-updated",
                &[
                    format!("ATTEMPT: {attempt}"),
                    format!("ACTION: {:?}", applied.action),
                    format!("SPEC: {}", spec_path.display()),
                    "DIFF:".into(),
                    applied.diff.clone(),
                ],
            );
        }
    }

    report.status = Status::Failed;
    ops_log::log_operation(
        &workdir,
        "workflow-failed",
        &[format!("STATUS: {:?}", report.status)],
    );
    Ok(report)
}

fn issue_evidence_lines(issue: &BuildIssue, log: &str) -> Vec<String> {
    fn unique_push(lines: &mut Vec<String>, line: String) {
        if !lines.contains(&line) {
            lines.push(line);
        }
    }

    let mut lines = Vec::new();
    match issue {
        BuildIssue::MissingBuildDependencies { deps } => {
            for raw in log.lines() {
                let line = raw.trim();
                if deps.iter().any(|dep| line.contains(dep)) {
                    unique_push(&mut lines, line.to_string());
                }
            }
        }
        BuildIssue::DependencyUnresolvable { deps } => {
            for raw in log.lines() {
                let line = raw.trim();
                if line.contains("nothing provides")
                    || line.contains("unresolvable")
                    || deps.iter().any(|dep| line.contains(dep))
                {
                    unique_push(&mut lines, line.to_string());
                }
            }
        }
        BuildIssue::ImportCheckExclusions {
            modules,
            missing_modules,
            exclusions: _,
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
        BuildIssue::InstalledButUnpackagedFiles { files } => {
            for raw in log.lines() {
                let line = raw.trim();
                if files.iter().any(|f| line.contains(f))
                    || line.contains("Installed but unpackaged files found")
                {
                    unique_push(&mut lines, line.to_string());
                }
            }
        }
        BuildIssue::ArchDependentInNoarch => {
            for raw in log.lines() {
                let line = raw.trim();
                if line.contains("Arch dependent binaries in noarch package") {
                    unique_push(&mut lines, line.to_string());
                }
            }
        }
        BuildIssue::MissingPep639LicenseMetadata => {
            for raw in log.lines() {
                let line = raw.trim();
                if line.contains("No License-File (PEP 639)") {
                    unique_push(&mut lines, line.to_string());
                }
            }
        }
        BuildIssue::InstallModuleMismatch {
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
        BuildIssue::EmptyImportCheck => {
            for raw in log.lines() {
                let line = raw.trim();
                if line.contains("import_all_modules.py")
                    || line.contains("No modules to check were left")
                {
                    unique_push(&mut lines, line.to_string());
                }
            }
        }
        BuildIssue::MissingPythonModule {
            module,
            import_context,
        } => {
            for raw in log.lines() {
                let line = raw.trim();
                if line.contains(module)
                    || line.contains("No module named")
                    || import_context
                        .as_ref()
                        .is_some_and(|ctx| line.contains(ctx.as_str()))
                {
                    unique_push(&mut lines, line.to_string());
                }
            }
        }
        BuildIssue::PyprojectBackendMissing { deps } => {
            for raw in log.lines() {
                let line = raw.trim();
                if deps.iter().any(|dep| line.contains(dep)) {
                    unique_push(&mut lines, line.to_string());
                }
            }
        }
        BuildIssue::CExtensionCompileError { important_lines }
        | BuildIssue::TestFailure { important_lines }
        | BuildIssue::PatchApplyError { important_lines }
        | BuildIssue::Unknown { important_lines } => {
            for line in important_lines {
                unique_push(&mut lines, line.clone());
            }
        }
    }

    lines.into_iter().take(20).collect()
}

/// Attempt to generate and apply an LLM %description before the first build.
///
/// Reads `source_extract_dir` from `.packfix/state.json`, acquires the LLM
/// semaphore (serialising across packages), calls the describe pipeline, and
/// updates the spec if the result is non-empty.
async fn try_llm_description(
    config: &WorkflowConfig,
    workdir: &Path,
    spec_path: &Path,
    pkg_label: &str,
    report: &mut Report,
) {
    use tracing::info;

    info!(package = %pkg_label, "llm description queued");

    // Find the state.json — try the workdir itself (Fix/DryRun local dirs)
    // and also the workspace parent two levels up (remote checkout flow).
    let state = find_state_json(workdir);
    let Some(source_extract_dir) = state.and_then(|s| s.source_extract_dir) else {
        report
            .notes
            .push("llm-description skipped: no source extract dir".into());
        ops_log::log_operation(
            workdir,
            "llm-description",
            &["STATUS: skipped (no source_extract_dir in state.json)".into()],
        );
        return;
    };

    if !source_extract_dir.exists() {
        report
            .notes
            .push("llm-description skipped: source extract dir not found".into());
        ops_log::log_operation(
            workdir,
            "llm-description",
            &[format!(
                "STATUS: skipped (dir not found: {})",
                source_extract_dir.display()
            )],
        );
        return;
    }

    // Acquire the LLM semaphore (serialise across packages).
    let _permit = if let Some(ref sem) = config.llm_semaphore {
        info!(package = %pkg_label, "llm description started (waiting for permit)");
        match sem.acquire().await {
            Ok(permit) => {
                info!(package = %pkg_label, "llm description started");
                Some(permit)
            }
            Err(_closed) => {
                info!(package = %pkg_label, "llm description started (semaphore closed)");
                None
            }
        }
    } else {
        info!(package = %pkg_label, "llm description started (no semaphore)");
        None
    };

    let start = std::time::Instant::now();

    let normalized_host = crate::describe::normalize_ollama_host(&config.ollama_host);

    let description = tokio::time::timeout(
        std::time::Duration::from_secs(config.description_timeout_secs),
        crate::describe::generate_description_core(
            &source_extract_dir,
            &normalized_host,
            config.ollama_port,
            &config.model,
            &config.description_system_prompt,
            &config.description_user_prompt,
            config.description_max_context_chars,
            config.description_num_predict,
            config.description_temperature,
        ),
    )
    .await
    .unwrap_or_else(|_| {
        report.notes.push("llm-description skipped: timeout".into());
        String::new()
    });

    let elapsed_ms = start.elapsed().as_millis();

    if description.is_empty() {
        report.notes.push("llm-description skipped: empty".into());
        ops_log::log_operation(
            workdir,
            "llm-description",
            &[
                "STATUS: skipped (LLM returned empty)".into(),
                format!("ELAPSED_MS: {elapsed_ms}"),
            ],
        );
        info!(package = %pkg_label, elapsed_ms, "llm description skipped: reason=empty");
        return;
    }

    // Read current spec, replace %description, write back.
    match spec::read_spec(spec_path) {
        Ok(before) => {
            let after = spec::update_description(&before, &description);
            if after == before {
                report
                    .notes
                    .push("llm-description skipped: no change".into());
                return;
            }
            if let Err(e) = spec::write_spec(spec_path, &after) {
                warn!(package = %pkg_label, error = %e, "failed to write spec with llm description");
                report
                    .notes
                    .push(format!("llm-description skipped: write error: {e}"));
                return;
            }
            report.notes.push("llm-description updated".into());
            ops_log::log_operation(
                workdir,
                "llm-description",
                &[
                    "STATUS: updated".into(),
                    format!("DESCRIPTION: {description}"),
                    format!("ELAPSED_MS: {elapsed_ms}"),
                ],
            );
            info!(package = %pkg_label, elapsed_ms, "llm description updated");
        }
        Err(e) => {
            warn!(package = %pkg_label, error = %e, "failed to read spec for llm description");
            report
                .notes
                .push(format!("llm-description skipped: read error: {e}"));
        }
    }
}

/// Look for `.packfix/state.json` in `workdir` and (for remote checkout flow)
/// two levels up from the OBS checkout dir.
fn find_state_json(workdir: &Path) -> Option<crate::extract::PackfixState> {
    // Try workdir first (Fix/DryRun local dirs)
    if let Ok(Some(s)) = crate::extract::read_state(workdir) {
        return Some(s);
    }
    // Try two levels up (workspaces/<pkg>/ from checkout dir at
    // workspaces/<pkg>/<project>/<pkg_name>/)
    if let Some(grandparent) = workdir.parent().and_then(|p| p.parent())
        && let Ok(Some(s)) = crate::extract::read_state(grandparent)
    {
        return Some(s);
    }
    None
}

pub async fn summarize_workdir(
    workdir: PathBuf,
    host: String,
    port: u16,
    model: String,
    apply_text: bool,
) -> Result<Report> {
    let spec_path = spec::find_spec(&workdir)?;
    let mut report = Report::new(Status::BuildSuccess);
    report.spec_path = Some(spec_path.clone());
    let suggestion = summarize_for_silent(&workdir, host, port, model).await;
    if apply_text {
        apply_text_suggestion(&spec_path, &suggestion)?;
        report
            .notes
            .push("applied summary/description suggestions".into());
    }
    report.llm_text_suggestion = Some(suggestion);
    Ok(report)
}

async fn prepare(config: &WorkflowConfig) -> Result<(PathBuf, PathBuf, Option<String>, Report)> {
    match &config.mode {
        WorkflowMode::New { pypi_name, version } => {
            let workdir = std::env::current_dir()?.join("workspaces").join(pypi_name);
            std::fs::create_dir_all(&workdir)?;
            let generated = upstream::generate_python_spec_async(
                pypi_name,
                version.as_deref(),
                &workdir,
                &config.takopack_bin,
            )
            .await?;
            let package_name = upstream::python_package_name(pypi_name);
            let mut report = Report::new(Status::Failed);
            report.notes.push(format!(
                "takopack output: {}; log: {}",
                generated.output_dir.display(),
                generated.log_path.display()
            ));
            Ok((workdir, generated.spec_path, Some(package_name), report))
        }
        WorkflowMode::Fix { workdir } | WorkflowMode::DryRun { workdir } => {
            let spec_path = spec::find_spec(workdir)?;
            let effective_workdir = spec_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| workdir.clone());
            crate::extract::extract_source_if_present(&effective_workdir, &effective_workdir);
            let package_name = spec::read_spec(&spec_path)
                .ok()
                .and_then(|text| spec::declared_package_name(&text))
                .or_else(|| {
                    // Fall back to spec filename (e.g. "python-fontmake.spec" →
                    // "python-fontmake") when Name: contains a macro like %{srcname}.
                    // Strips `_service:obs_scm:` prefix from service-generated filenames.
                    spec::spec_file_basename(&spec_path)
                })
                .map(|name| upstream::python_package_name(&name));
            Ok((
                effective_workdir,
                spec_path,
                package_name,
                Report::new(Status::Failed),
            ))
        }
    }
}

async fn add_text_suggestion(
    config: &WorkflowConfig,
    workdir: &Path,
    spec_path: &Path,
    report: &mut Report,
) {
    let suggestion = summarize_for_silent(
        workdir,
        config.ollama_host.clone(),
        config.ollama_port,
        config.model.clone(),
    )
    .await;
    if !suggestion.notes.is_empty() {
        for note in &suggestion.notes {
            report.notes.push(format!("llm: {note}"));
        }
    }
    if config.apply_text
        && let Err(err) = apply_text_suggestion(spec_path, &suggestion)
    {
        report
            .notes
            .push(format!("failed to apply text suggestion: {err}"));
    }
    report.llm_text_suggestion = Some(suggestion);
}

async fn summarize_for_silent(
    workdir: &Path,
    host: String,
    port: u16,
    model: String,
) -> TextSuggestion {
    let metadata_text = read_first_matching(workdir, &["PKG-INFO", "METADATA"]);
    let readme_text = read_first_prefix(workdir, "README");

    // Strip http:// / https:// prefix so the host string is a plain hostname
    // that can be resolved by DNS.  "http://localhost" → "localhost".
    let host_clean = host
        .strip_prefix("http://")
        .or_else(|| host.strip_prefix("https://"))
        .unwrap_or(&host);

    // Quick TCP pre-check: if the port is closed, skip the LLM call entirely.
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;
    let tcp_timeout = Duration::from_millis(300);
    if let Ok(mut addrs_iter) = (host_clean, port).to_socket_addrs()
        && let Some(sock) = addrs_iter.next()
        && TcpStream::connect_timeout(&sock, tcp_timeout).is_err()
    {
        return crate::utils::llm::TextSuggestion::default();
    }

    // Wrap the LLM call in a short timeout so a dead server does not block
    // the build pipeline for the ollama_rs default (30 s).
    let llm_timeout = Duration::from_secs(2);
    tokio::time::timeout(
        llm_timeout,
        crate::utils::llm::summarize_spec_text_silent(
            host,
            port,
            model,
            metadata_text,
            readme_text,
        ),
    )
    .await
    .unwrap_or_default()
}

fn apply_text_suggestion(spec_path: &Path, suggestion: &TextSuggestion) -> Result<()> {
    let mut text = spec::read_spec(spec_path)?;
    if let Some(summary) = &suggestion.summary {
        text = spec::update_summary(&text, summary);
    }
    if let Some(description) = &suggestion.description {
        text = spec::update_description(&text, description);
    }
    spec::write_spec(spec_path, &text)?;
    Ok(())
}

fn read_first_matching(workdir: &Path, names: &[&str]) -> String {
    for name in names {
        for path in walk(workdir) {
            if path.file_name().and_then(|s| s.to_str()) == Some(name)
                && let Ok(text) = std::fs::read_to_string(path)
            {
                return text;
            }
        }
    }
    String::new()
}

fn read_first_prefix(workdir: &Path, prefix: &str) -> String {
    for path in walk(workdir) {
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|name| name.starts_with(prefix))
            && let Ok(text) = std::fs::read_to_string(path)
        {
            return text;
        }
    }
    String::new()
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn workflow_dry_run_does_not_modify_spec() {
        let dir = tempdir().expect("tempdir");
        let spec_path = dir.path().join("demo.spec");
        let original = "Name: demo  \n%description\nx";
        std::fs::write(&spec_path, original).expect("write spec");

        let cli = crate::cli::Cli {
            command: crate::cli::Command::DryRun {
                workdir: dir.path().to_path_buf(),
            },
            project: None,
            repository: "x64".into(),
            arch: "x86_64".into(),
            max_retries: 0,
            max_dep_depth: 3,
            takopack_bin: "takopack".into(),
            osc_bin: "osc".into(),
            ollama_host: "http://localhost".into(),
            ollama_port: 11434,
            model: "qwen3:8b".into(),
            apply_text: false,
            json: false,
            obs_api_url: None,
            repo_url: None,
            oscrc_path: None,
            default_obs_project: None,
            llm_description: false,
            no_llm_description: false,
        };
        let report = run_workflow(WorkflowConfig::from_cli(
            &cli,
            WorkflowMode::DryRun {
                workdir: dir.path().to_path_buf(),
            },
        ))
        .await
        .expect("dry run report");

        let after = std::fs::read_to_string(&spec_path).expect("read spec");
        assert_eq!(after, original);
        assert!(matches!(report.status, Status::DryRunComplete));
        let operations_log = dir.path().join("logs").join("packfix_operations.log");
        let operations_text =
            std::fs::read_to_string(&operations_log).expect("read operations log");
        assert!(operations_text.contains("workflow-start"));
        assert!(operations_text.contains("workflow-dry-run"));
        assert!(
            report
                .notes
                .iter()
                .any(|note| note == "dry-run: spec would be normalized")
        );
    }

    #[test]
    fn workflow_action_no_spec_change_returns_need_human() {
        let before = "BuildOption(install):  -L\n%description\nx\n";
        let action = crate::core::BuildAction::FixBuildOptionInstall {
            arg: "-L".into(),
            reason: "already present".into(),
        };
        let after = apply_action(before, &action).expect("apply action");
        assert_eq!(before, after);
    }

    #[test]
    fn workflow_config_from_cli_defaults_to_no_local_build_pool() {
        let cli = crate::cli::Cli {
            command: crate::cli::Command::ConfigShow,
            project: None,
            repository: "x64".into(),
            arch: "x86_64".into(),
            max_retries: 2,
            max_dep_depth: 3,
            takopack_bin: "takopack".into(),
            osc_bin: "osc".into(),
            ollama_host: "http://localhost".into(),
            ollama_port: 11434,
            model: "qwen3:8b".into(),
            apply_text: false,
            json: false,
            obs_api_url: None,
            repo_url: None,
            oscrc_path: None,
            default_obs_project: None,
            llm_description: false,
            no_llm_description: false,
        };

        let cfg = WorkflowConfig::from_cli(
            &cli,
            WorkflowMode::DryRun {
                workdir: PathBuf::from("."),
            },
        );
        assert!(cfg.local_build_pool.is_none());
    }

    #[test]
    fn llm_description_can_be_disabled() {
        let cli = crate::cli::Cli {
            command: crate::cli::Command::ConfigShow,
            project: None,
            repository: "x64".into(),
            arch: "x86_64".into(),
            max_retries: 2,
            max_dep_depth: 3,
            takopack_bin: "takopack".into(),
            osc_bin: "osc".into(),
            ollama_host: "http://localhost".into(),
            ollama_port: 11434,
            model: "qwen3:8b".into(),
            apply_text: false,
            json: false,
            obs_api_url: None,
            repo_url: None,
            oscrc_path: None,
            default_obs_project: None,
            llm_description: false,
            no_llm_description: false,
        };
        let cfg = WorkflowConfig::from_cli(
            &cli,
            WorkflowMode::DryRun {
                workdir: PathBuf::from("."),
            },
        );
        assert!(!cfg.llm_description);
    }

    #[test]
    fn llm_description_defaults_to_true() {
        // `from_cli` reads `cli.llm_description` which defaults to true via
        // `#[arg(default_value_t = true)]`; struct construction should pass
        // true to match.
        let cli = crate::cli::Cli {
            command: crate::cli::Command::ConfigShow,
            project: None,
            repository: "x64".into(),
            arch: "x86_64".into(),
            max_retries: 2,
            max_dep_depth: 3,
            takopack_bin: "takopack".into(),
            osc_bin: "osc".into(),
            ollama_host: "http://localhost".into(),
            ollama_port: 11434,
            model: "qwen3:8b".into(),
            apply_text: false,
            json: false,
            obs_api_url: None,
            repo_url: None,
            oscrc_path: None,
            default_obs_project: None,
            llm_description: true,
            no_llm_description: false,
        };
        let cfg = WorkflowConfig::from_cli(
            &cli,
            WorkflowMode::DryRun {
                workdir: PathBuf::from("."),
            },
        );
        assert!(cfg.llm_description);
    }

    #[tokio::test]
    async fn llm_semaphore_has_one_permit() {
        let sem = crate::core::resources::LlmSemaphore::new(tokio::sync::Semaphore::new(1));
        let p1 = sem.acquire().await;
        assert!(p1.is_ok());
        // Second acquire should not succeed immediately — try_acquire fails.
        assert!(sem.try_acquire().is_err());
        drop(p1);
        // After releasing, a new acquire should succeed.
        assert!(sem.try_acquire().is_ok());
    }
}
