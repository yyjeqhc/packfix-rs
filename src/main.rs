mod cli;
mod config;
mod core;
mod describe;
mod extract;
mod fix;
mod git;
mod obs;
mod pipeline;
mod report;
mod spec;
mod upstream;
mod utils;
mod workflow;

use crate::core::AnalysisOutput;
use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use workflow::{WorkflowConfig, WorkflowMode};

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

fn arg_provided(args: &[String], flag: &str) -> bool {
    args.iter()
        .any(|arg| arg == flag || arg.starts_with(&format!("{flag}=")))
}

fn apply_cli_config_defaults(cli: &mut Cli, cfg: Option<&config::PackfixConfig>) {
    let args: Vec<String> = std::env::args().collect();

    if !arg_provided(&args, "--repository")
        && let Some(v) = cfg
            .and_then(|c| c.build.as_ref())
            .and_then(|b| b.repository.clone())
    {
        cli.repository = v;
    }
    if !arg_provided(&args, "--arch")
        && let Some(v) = cfg
            .and_then(|c| c.build.as_ref())
            .and_then(|b| b.arch.clone())
    {
        cli.arch = v;
    }
    if !arg_provided(&args, "--max-retries")
        && let Some(v) = cfg
            .and_then(|c| c.build.as_ref())
            .and_then(|b| b.max_retries)
    {
        cli.max_retries = v;
    }
    if !arg_provided(&args, "--max-dep-depth")
        && let Some(v) = cfg
            .and_then(|c| c.build.as_ref())
            .and_then(|b| b.max_dep_depth)
    {
        cli.max_dep_depth = v;
    }
    if !arg_provided(&args, "--ollama-host")
        && let Some(v) = cfg
            .and_then(|c| c.llm.as_ref())
            .and_then(|l| l.host.clone())
    {
        cli.ollama_host = v;
    }
    if !arg_provided(&args, "--ollama-port")
        && let Some(v) = cfg.and_then(|c| c.llm.as_ref()).and_then(|l| l.port)
    {
        cli.ollama_port = v;
    }
    if !arg_provided(&args, "--model")
        && let Some(v) = cfg
            .and_then(|c| c.llm.as_ref())
            .and_then(|l| l.model.clone())
    {
        cli.model = v;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let mut cli = Cli::parse();
    let cfg = config::load_config();
    apply_cli_config_defaults(&mut cli, cfg.as_ref());

    match cli.command.clone() {
        Command::Build {
            pypi_names,
            version,
            obs_project,
            revision,
        } => {
            if pypi_names.is_empty() {
                anyhow::bail!("at least one package name is required");
            }
            if pypi_names.len() > 1 && version.is_some() {
                anyhow::bail!("--version only supports a single package build");
            }

            let requests: Vec<pipeline::BuildPythonPackageRequest> = pypi_names
                .iter()
                .map(|pypi_name| pipeline::BuildPythonPackageRequest {
                    pypi_name: pypi_name.clone(),
                    version: version.clone(),
                    obs_project: obs_project.clone(),
                    revision: revision.clone(),
                    feature_declarations: Vec::new(),
                })
                .collect();

            if requests.len() == 1 {
                let report = pipeline::build_python_package(
                    requests.into_iter().next().expect("single request"),
                    &cli,
                    cfg.as_ref(),
                )
                .await?;
                report::print_report(&report, cli.json)?;
            } else {
                let reports = pipeline::build_python_packages(requests, &cli, cfg.as_ref()).await?;
                report::print_reports(&reports, cli.json)?;
            }

            info!("build pipeline complete");
        }

        Command::BuildExisting {
            packages,
            obs_project,
            revision,
        } => {
            if packages.is_empty() {
                anyhow::bail!("at least one package is required");
            }
            if packages.len() == 1 {
                let report = pipeline::build_existing_python_package(
                    packages.into_iter().next().expect("single package"),
                    obs_project,
                    revision,
                    &cli,
                    cfg.as_ref(),
                    Vec::new(),
                )
                .await?;
                report::print_report(&report, cli.json)?;
            } else {
                let reports = pipeline::build_existing_python_packages(
                    packages,
                    obs_project,
                    revision,
                    &cli,
                    cfg.as_ref(),
                )
                .await?;
                report::print_reports(&reports, cli.json)?;
            }

            info!("build pipeline complete");
        }

        Command::AnalyzeLog { log_file } => {
            let log = std::fs::read_to_string(log_file)?;
            let issue = fix::analyzer::analyze_log(&log);
            let action = fix::fixer::decide_action(&issue);
            let output = AnalysisOutput {
                issue: issue.clone(),
                action,
            };
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!("issue:\n{issue:#?}\n\naction:\n{:#?}", output.action);
            }
        }

        Command::DryRun { workdir } => {
            let report = workflow::run_workflow(WorkflowConfig::from_cli(
                &cli,
                WorkflowMode::DryRun { workdir },
            ))
            .await?;
            report::print_report(&report, cli.json)?;
        }

        Command::Fix { workdirs } => {
            if workdirs.is_empty() {
                anyhow::bail!("at least one workdir is required");
            }
            if workdirs.len() == 1 {
                let report = pipeline::fix_local_workdir_with_dependencies(
                    workdirs.into_iter().next().expect("single workdir"),
                    &cli,
                    cfg.as_ref(),
                )
                .await?;
                report::print_report(&report, cli.json)?;
            } else {
                let reports =
                    pipeline::fix_local_workdirs_with_dependencies(workdirs, &cli, cfg.as_ref())
                        .await?;
                report::print_reports(&reports, cli.json)?;
            }
        }

        Command::New { pypi_name, version } => {
            let report = workflow::run_workflow(WorkflowConfig::from_cli(
                &cli,
                WorkflowMode::New { pypi_name, version },
            ))
            .await?;
            report::print_report(&report, cli.json)?;
        }

        Command::Summarize { workdir } => {
            let report = workflow::summarize_workdir(
                workdir,
                cli.ollama_host,
                cli.ollama_port,
                cli.model,
                cli.apply_text,
            )
            .await?;
            report::print_report(&report, cli.json)?;
        }

        Command::Describe { dir, output } => {
            let system_prompt = config::resolve_description_system_prompt(cfg.as_ref());
            let user_prompt = config::resolve_description_user_prompt(cfg.as_ref());
            let timeout_secs = config::resolve_description_timeout_secs(cfg.as_ref());
            let max_context_chars = config::resolve_description_max_context_chars(cfg.as_ref());
            let num_predict = config::resolve_description_num_predict(cfg.as_ref());
            let temperature = config::resolve_description_temperature(cfg.as_ref());
            let description = describe::run_describe(
                &dir,
                &cli.ollama_host,
                cli.ollama_port,
                &cli.model,
                &system_prompt,
                &user_prompt,
                timeout_secs,
                max_context_chars,
                num_predict,
                temperature,
            )
            .await;
            if description.is_empty() {
                warn!("LLM returned empty description for {}", dir.display());
                println!();
            } else {
                println!("{description}");
            }
            if let Some(out_path) = output {
                std::fs::write(&out_path, &description)?;
                info!(output = %out_path.display(), "description written to file");
            }
        }

        Command::ConfigShow => {
            config::display_config(cfg.as_ref(), cli.json)?;
        }

        Command::Checkout {
            project,
            package,
            output,
        } => {
            let result = obs::local::osc_checkout(
                &project,
                package.as_deref(),
                output.as_deref(),
                &cli.osc_bin,
            )?;
            if result.success {
                // perform `osc up -S` on the checked-out dir to fetch actual files
                let checkout_root = match output.as_deref() {
                    Some(d) => d.to_path_buf(),
                    None => std::env::current_dir()?,
                };
                let out_dir =
                    obs::local::checkout_target_dir(&checkout_root, &project, package.as_deref());
                let update = obs::local::osc_update(&out_dir, &cli.osc_bin)?;
                if !update.success {
                    anyhow::bail!(
                        "update after checkout failed (rc={}): {}",
                        update.returncode,
                        update.stderr
                    );
                }
                obs::local::sanitize_checkout_dir(&out_dir)?;
                extract::extract_source_if_present(&out_dir, &checkout_root);
                info!(workdir = %out_dir.display(), "checkout succeeded");
            } else {
                anyhow::bail!(
                    "checkout failed (rc={}): {}",
                    result.returncode,
                    result.stderr
                );
            }
        }

        Command::Update {
            target,
            obs_project,
        } => {
            let target_path = std::path::PathBuf::from(&target);
            if target_path.exists() {
                let result = obs::local::osc_update(&target_path, &cli.osc_bin)?;
                if result.success {
                    info!(workdir = %target_path.display(), "update succeeded");
                } else {
                    anyhow::bail!(
                        "update failed (rc={}): {}",
                        result.returncode,
                        result.stderr
                    );
                }
            } else {
                let report = pipeline::update_existing_python_package(
                    target,
                    obs_project,
                    &cli,
                    cfg.as_ref(),
                )
                .await?;
                report::print_report(&report, cli.json)?;
                info!("build pipeline complete");
            }
        }

        Command::Status {
            project,
            package,
            repository,
            arch,
        } => {
            let repo = repository.as_deref().unwrap_or(&cli.repository);
            let arch = arch.as_deref().unwrap_or(&cli.arch);
            let result = obs::local::osc_api_status(&project, &package, repo, arch, &cli.osc_bin)?;
            info!(
                project = %project,
                package = %package,
                repository = %repo,
                arch = %arch,
                status = %result.stdout.trim(),
                "osc status"
            );
        }

        Command::Submit {
            revision,
            components,
            obs_project,
        } => {
            let project = obs_project
                .or(cli.default_obs_project.clone())
                .unwrap_or_else(|| config::resolve_default_obs_project(None, cfg.as_ref()));
            let obs_api_url = config::resolve_obs_api_url(cli.obs_api_url.as_deref(), cfg.as_ref());
            let repo_url = config::resolve_repo_url(cli.repo_url.as_deref(), cfg.as_ref());
            let oscrc_path = config::resolve_oscrc_path(cli.oscrc_path.as_ref(), cfg.as_ref());
            let creds = obs::api::read_osc_credentials(&oscrc_path)?;

            let result = obs::api::ebf_submit(
                &project,
                &revision,
                &components,
                &obs_api_url,
                &repo_url,
                &creds,
            )
            .await?;
            pipeline::ensure_ebf_success(&result)?;
            info!(
                project = %project,
                revision = %revision,
                success = result.success_count,
                total = result.total_count,
                "ebf submit finished"
            );
            info!(ebf_stdout = %result.stdout.trim(), "ebf submit details");
            if !result.stderr.trim().is_empty() {
                warn!(ebf_stderr = %result.stderr.trim(), "ebf submit stderr");
            }
        }
    }

    Ok(())
}
